//! Random backend responses, hostile ones among them, and the model of
//! what the client must get for each (FR-PROXY-12…24): a 502, or the
//! rewritten head and exactly the backend's body bytes, maybe cut.

use super::*;

/// How a script ends once its bytes (up to the cut point) are sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ending {
    /// Everything is sent; then maybe an end of stream.
    Done { eof: bool },
    /// The first `n` bytes are sent, then the backend resets.
    Reset(usize),
    /// The first `n` bytes are sent, then the backend closes.
    Eof(usize),
}

/// What a backend sends.
#[derive(Debug)]
pub(crate) struct Script {
    pub(crate) wire: Vec<u8>,
    /// Where the final head ends (after any interim heads).
    pub(crate) head_end: usize,
    /// Where the body ends (bytes after it are extra).
    pub(crate) body_end: usize,
    /// Where each interim head ends.
    pub(crate) interim_ends: Vec<usize>,
    pub(crate) ending: Ending,
    pub(crate) reply: Reply,
    /// Coverage tag of the hostile or edge case it carries, if any.
    pub(crate) tag: Option<&'static str>,
}

/// What the client must get.
#[derive(Debug, Clone)]
pub(crate) enum Reply {
    /// A 502 before any response byte, for this failure.
    Fail(BackendFailure),
    Relay(Relay),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyKind {
    None,
    Length,
    Chunked,
    UntilClose,
}

#[derive(Debug, Clone)]
pub(crate) struct Relay {
    pub(crate) status: u16,
    /// The rewritten head as the client gets it.
    pub(crate) head: Vec<u8>,
    /// The body bytes the client gets (up to `fails`' or `cut`'s offset).
    pub(crate) body: Vec<u8>,
    pub(crate) kind: BodyKind,
    pub(crate) persistence: Persistence,
    /// The body breaks a rule at this offset into `body`: a cut, or a 502
    /// when the offending bytes came with the head.
    pub(crate) fails: Option<(Vec<BackendFailure>, usize)>,
    /// The backend fails after this many body bytes: a cut.
    pub(crate) cut: Option<(BackendFailure, usize)>,
}

/// What the response answers.
pub(crate) struct Ctx {
    pub(crate) head_only: bool,
    pub(crate) http10: bool,
    pub(crate) persistence: Persistence,
    /// The backend answers before the request body is complete.
    pub(crate) early: bool,
    pub(crate) limit: Option<u64>,
    pub(crate) max_head: usize,
    /// Only a well-formed response without failures (for timed
    /// scenarios), with a body if `need_body`.
    pub(crate) valid_only: bool,
    pub(crate) need_body: bool,
    /// Allow a large body (the client's window is not tiny).
    pub(crate) large: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadDefect {
    BareLf,
    CrInReason,
    ObsTextReason,
    LfInValue,
    CrInValue,
    ObsTextValue,
    ObsFold,
    BadName,
    BadVersion,
    BadCode,
    NoReasonSpace,
    ClTe,
    DupCl,
    BadCl,
    BadTe,
    TeInHttp10,
    ConnFraming,
    Upgrade101,
    TooManyInterim,
    HeadTooLarge,
    TooManyHeaders,
    TeTwice,
    CtlInValue,
    ClOverflow,
    BadInterim,
}

impl HeadDefect {
    fn error(self) -> ResponseError {
        match self {
            HeadDefect::BareLf | HeadDefect::LfInValue => ResponseError::BareLf,
            HeadDefect::CrInReason
            | HeadDefect::ObsTextReason
            | HeadDefect::BadCode
            | HeadDefect::NoReasonSpace => ResponseError::BadStatusLine,
            HeadDefect::CrInValue
            | HeadDefect::ObsTextValue
            | HeadDefect::CtlInValue
            | HeadDefect::BadInterim => ResponseError::BadHeaderValue,
            HeadDefect::ObsFold => ResponseError::ObsFold,
            HeadDefect::BadName => ResponseError::BadHeaderName,
            HeadDefect::BadVersion => ResponseError::BadVersion,
            HeadDefect::ClTe => ResponseError::ContentLengthWithTransferEncoding,
            HeadDefect::DupCl => ResponseError::DuplicateContentLength,
            HeadDefect::BadCl | HeadDefect::ClOverflow => ResponseError::BadContentLength,
            HeadDefect::BadTe | HeadDefect::TeInHttp10 | HeadDefect::TeTwice => {
                ResponseError::UnsupportedTransferEncoding
            }
            HeadDefect::ConnFraming => ResponseError::ConnectionNamesFraming,
            HeadDefect::Upgrade101 => ResponseError::SwitchingProtocols,
            HeadDefect::TooManyInterim => ResponseError::TooManyInterimHeads,
            HeadDefect::HeadTooLarge => ResponseError::HeadTooLarge,
            HeadDefect::TooManyHeaders => ResponseError::TooManyHeaders,
        }
    }

    fn random(rng: &mut fastrand::Rng) -> HeadDefect {
        match rng.u8(0..25) {
            21 => HeadDefect::TeTwice,
            22 => HeadDefect::CtlInValue,
            23 => HeadDefect::ClOverflow,
            24 => HeadDefect::BadInterim,
            0 => HeadDefect::BareLf,
            1 => HeadDefect::CrInReason,
            2 => HeadDefect::ObsTextReason,
            3 => HeadDefect::LfInValue,
            4 => HeadDefect::CrInValue,
            5 => HeadDefect::ObsTextValue,
            6 => HeadDefect::ObsFold,
            7 => HeadDefect::BadName,
            8 => HeadDefect::BadVersion,
            9 => HeadDefect::BadCode,
            10 => HeadDefect::NoReasonSpace,
            11 => HeadDefect::ClTe,
            12 => HeadDefect::DupCl,
            13 => HeadDefect::BadCl,
            14 => HeadDefect::BadTe,
            15 => HeadDefect::TeInHttp10,
            16 => HeadDefect::ConnFraming,
            17 => HeadDefect::Upgrade101,
            18 => HeadDefect::TooManyInterim,
            19 => HeadDefect::HeadTooLarge,
            _ => HeadDefect::TooManyHeaders,
        }
    }
}

const CODES: &[(u16, &str)] = &[
    (200, "OK"),
    (200, "OK"),
    (200, "Fine\tDandy"),
    (201, "Created"),
    (204, "No Content"),
    (304, "Not Modified"),
    (404, ""),
    (500, "Internal Server Error"),
    (299, "Custom"),
    (418, "I'm a teapot"),
];

/// Header lines a backend may send; hop-by-hop ones among them.
const EXTRA: &[(&str, &[u8])] = &[
    ("Server", b"backend/1"),
    ("Date", b"Tue, 27 Oct 2026 10:00:00 GMT"),
    ("Content-Type", b"text/plain"),
    ("X-Backend", b"  spaced\t "),
    ("Set-Cookie", b"a=b; Path=/"),
    ("Connection", b"keep-alive"),
    ("Keep-Alive", b"timeout=5"),
    ("Proxy-Authenticate", b"Basic"),
    ("Upgrade", b"h2c"),
    ("TE", b"trailers"),
    ("Trailer", b"X-T"),
    ("Expect", b"100-continue"),
];

/// Bytes after a response's end: another response, or junk.
const AFTER: &[&[u8]] = &[
    b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nevil",
    b"\r\n",
    b"garbage after the body",
];

fn header(out: &mut Vec<u8>, rng: &mut fastrand::Rng, name: &str, value: &[u8]) {
    out.extend_from_slice(name.as_bytes());
    out.push(b':');
    if rng.u8(0..4) != 0 {
        out.push(b' ');
    }
    out.extend_from_slice(value);
    out.extend_from_slice(b"\r\n");
}

/// A random response body length; large ones only when allowed.
fn response_len(rng: &mut fastrand::Rng, large: bool) -> usize {
    match rng.u8(0..30) {
        0..15 => rng.usize(0..64),
        15..24 => rng.usize(64..4000),
        24..28 => rng.usize(4000..70_000),
        _ if large => rng.usize(70_000..=2 * 1024 * 1024),
        _ => rng.usize(4000..20_000),
    }
}

/// A random backend response for `ctx`, and its model.
pub(crate) fn random_script(rng: &mut fastrand::Rng, ctx: &Ctx) -> Script {
    let defect = (!ctx.valid_only && rng.u8(0..6) == 0).then(|| HeadDefect::random(rng));
    let body_defect = (!ctx.valid_only && defect.is_none() && rng.u8(0..12) == 0)
        .then(|| ChunkDefect::random(rng));
    let mut wire = Vec::new();
    // Interim heads.
    let interim = match defect {
        Some(HeadDefect::TooManyInterim) => 9,
        Some(HeadDefect::BadInterim) => rng.usize(1..3),
        Some(_) | None => match rng.u8(0..40) {
            0..26 => 0,
            26..38 => rng.usize(1..3),
            _ => 8,
        },
    };
    let mut interim_ends = Vec::new();
    for i in 0..interim {
        // The defect in the last interim head.
        if defect == Some(HeadDefect::BadInterim) && i + 1 == interim {
            wire.extend_from_slice(b"HTTP/1.1 103 Early Hints\r\nLink: </caf\xc3\xa9>\r\n\r\n");
        } else if i % 2 == 0 {
            wire.extend_from_slice(b"HTTP/1.1 100 Continue\r\n\r\n");
        } else {
            wire.extend_from_slice(
                b"HTTP/1.1 103 Early Hints\r\nLink: </s.css>; rel=preload\r\n\r\n",
            );
        }
        interim_ends.push(wire.len());
    }
    let head_start = wire.len();
    let (mut code, reason) = rng.choice(CODES).copied().unwrap_or((200, "OK"));
    // No framing header (`UntilClose`) is the end of stream, unless the
    // status or method rule out a body.
    let mut framing = match rng.u8(0..10) {
        0..5 => BodyKind::Length,
        5..8 => BodyKind::Chunked,
        _ => BodyKind::UntilClose,
    };
    if ctx.need_body && matches!(code, 204 | 304) {
        code = 200;
    }
    match defect {
        Some(HeadDefect::ClTe | HeadDefect::DupCl | HeadDefect::BadCl | HeadDefect::ClOverflow) => {
            framing = BodyKind::Length;
        }
        Some(HeadDefect::BadTe | HeadDefect::TeTwice) => framing = BodyKind::Chunked,
        Some(_) | None => {}
    }
    if body_defect.is_some() {
        framing = BodyKind::Chunked;
    }
    let http10 =
        framing != BodyKind::Chunked && defect != Some(HeadDefect::ClTe) && rng.u8(0..6) == 0;
    // The status line.
    let mut reason = reason.as_bytes().to_vec();
    let mut version: &[u8] = if http10 { b"HTTP/1.0" } else { b"HTTP/1.1" };
    match defect {
        Some(HeadDefect::CrInReason) => reason = b"O\rK".to_vec(),
        Some(HeadDefect::ObsTextReason) => reason = b"Gr\xc3\xbc\xc3\x9fe".to_vec(),
        Some(HeadDefect::BadVersion) => version = b"HTTP/2.0",
        Some(HeadDefect::TeInHttp10) => {
            version = b"HTTP/1.0";
            framing = BodyKind::Chunked;
        }
        Some(_) | None => {}
    }
    wire.extend_from_slice(version);
    match defect {
        Some(HeadDefect::BadCode) => wire.extend_from_slice(b" 20 OK"),
        Some(HeadDefect::NoReasonSpace) => wire.extend_from_slice(b" 200"),
        Some(HeadDefect::Upgrade101) => wire.extend_from_slice(b" 101 Switching Protocols"),
        Some(_) | None => {
            wire.extend_from_slice(format!(" {code} ").as_bytes());
            wire.extend_from_slice(&reason);
        }
    }
    wire.extend_from_slice(if defect == Some(HeadDefect::BareLf) {
        b"\n"
    } else {
        b"\r\n"
    });
    // Header lines (name, trimmed value) as the client must get them.
    let mut lines: Vec<(String, Vec<u8>)> = Vec::new();
    let mut push = |wire: &mut Vec<u8>, rng: &mut fastrand::Rng, name: &str, value: &[u8]| {
        header(wire, rng, name, value);
        lines.push((name.to_ascii_lowercase(), trim_ows(value).to_vec()));
    };
    for _ in 0..rng.usize(0..4) {
        if let Some(&(name, value)) = rng.choice(EXTRA) {
            push(&mut wire, rng, name, value);
        }
    }
    if rng.u8(0..5) == 0 {
        push(&mut wire, rng, "Connection", b"X-Secret, keep-alive");
        push(&mut wire, rng, "X-Secret", b"internal");
    }
    match defect {
        Some(HeadDefect::LfInValue) => wire.extend_from_slice(b"X-Split: a\nInjected: b\r\n"),
        Some(HeadDefect::CrInValue) => wire.extend_from_slice(b"X-Split: a\rInjected: b\r\n"),
        Some(HeadDefect::ObsTextValue) => wire.extend_from_slice(b"X-Name: caf\xc3\xa9\r\n"),
        Some(HeadDefect::CtlInValue) => wire.extend_from_slice(b"X-Ctl: a\x7fb\x1bc\r\n"),
        Some(HeadDefect::ObsFold) => wire.extend_from_slice(b"X-Fold: a\r\n b\r\n"),
        Some(HeadDefect::BadName) => wire.extend_from_slice(b"Bad Name: x\r\n"),
        Some(HeadDefect::ConnFraming) => wire.extend_from_slice(b"Connection: Content-Length\r\n"),
        Some(HeadDefect::HeadTooLarge) => {
            wire.extend_from_slice(b"X-Pad: ");
            wire.extend(std::iter::repeat_n(b'p', ctx.max_head));
            wire.extend_from_slice(b"\r\n");
        }
        Some(HeadDefect::TooManyHeaders) => {
            for i in 0..=100 {
                wire.extend_from_slice(format!("X-{i}: v\r\n").as_bytes());
            }
        }
        Some(_) | None => {}
    }
    // The body and its framing header.
    let len = if ctx.need_body {
        response_len(rng, ctx.large).max(1)
    } else {
        response_len(rng, ctx.large)
    };
    let body = payload(rng, len);
    let mut body_wire = body.clone();
    let mut chunk = None;
    let mut cl_value = None;
    match framing {
        BodyKind::Length => {
            let zeros = if rng.u8(0..5) == 0 {
                rng.usize(1..3)
            } else {
                0
            };
            let value = format!("{}{}", "0".repeat(zeros), body.len());
            match defect {
                Some(HeadDefect::BadCl) => header(&mut wire, rng, "Content-Length", b"1 2"),
                Some(HeadDefect::ClOverflow) => {
                    header(&mut wire, rng, "Content-Length", b"18446744073709551616");
                }
                Some(HeadDefect::DupCl) => {
                    header(&mut wire, rng, "Content-Length", value.as_bytes());
                    header(&mut wire, rng, "content-length", value.as_bytes());
                }
                Some(HeadDefect::ClTe) => {
                    header(&mut wire, rng, "Content-Length", value.as_bytes());
                    header(&mut wire, rng, "Transfer-Encoding", b"chunked");
                }
                Some(_) | None => push(&mut wire, rng, "Content-Length", value.as_bytes()),
            }
            cl_value = Some(body.len() as u64);
        }
        BodyKind::Chunked => {
            let c = chunked(rng, &body, body_defect);
            body_wire = c.wire.clone();
            match defect {
                Some(HeadDefect::BadTe) => header(&mut wire, rng, "Transfer-Encoding", b"identity"),
                Some(HeadDefect::TeTwice) => {
                    header(&mut wire, rng, "Transfer-Encoding", b"chunked");
                    header(&mut wire, rng, "transfer-encoding", b"chunked");
                }
                Some(_) | None => push(&mut wire, rng, "Transfer-Encoding", b"chunked"),
            }
            chunk = Some(c);
        }
        BodyKind::UntilClose | BodyKind::None => {}
    }
    // Now and then a head right at `max_head` (FR-PROXY-14): a padding
    // line brings it to within 3 bytes of the limit, either side.
    let mut too_large = false;
    let mut padded = false;
    if defect.is_none() && rng.u8(0..12) == 0 {
        let target = ctx.max_head as i64 + rng.i64(-3..=3);
        let pad = target - (wire.len() - head_start) as i64 - 2 - "X-Pad: \r\n".len() as i64;
        if pad > 0 {
            let value = vec![b'p'; pad as usize];
            push(&mut wire, rng, "X-Pad", &value);
            padded = true;
            // `push` may leave the space after the colon out.
            too_large = wire.len() - head_start + 2 > ctx.max_head;
        }
    }
    wire.extend_from_slice(b"\r\n");
    let head_end = wire.len();
    // A 204, 304 or a response to HEAD has no body, whatever it declares.
    let kind = if ctx.head_only || matches!(code, 204 | 304) || cl_value == Some(0) {
        BodyKind::None
    } else {
        framing
    };
    let sent_body: &[u8] = match (framing, kind) {
        (BodyKind::None, _) => &[],
        (_, BodyKind::None) if rng.bool() => &[],
        (BodyKind::Length | BodyKind::Chunked | BodyKind::UntilClose, _) => &body_wire,
    };
    wire.extend_from_slice(sent_body);
    let body_end = match kind {
        BodyKind::None => head_end,
        BodyKind::Length | BodyKind::Chunked | BodyKind::UntilClose => wire.len(),
    };
    if kind != BodyKind::UntilClose && rng.u8(0..5) == 0 {
        wire.extend_from_slice(rng.choice(AFTER).copied().unwrap_or_default());
    }
    let mut tag = defect.map(|d| match d {
        HeadDefect::BareLf | HeadDefect::LfInValue => "LF in a backend head",
        HeadDefect::CrInReason | HeadDefect::CrInValue => "CR in a backend head",
        HeadDefect::ObsTextReason | HeadDefect::ObsTextValue => "obs-text from a backend",
        HeadDefect::ClTe | HeadDefect::DupCl | HeadDefect::BadCl | HeadDefect::BadTe => {
            "bad backend framing"
        }
        HeadDefect::TeInHttp10 => "TE in an HTTP/1.0 response",
        HeadDefect::TeTwice => "duplicate backend TE",
        HeadDefect::CtlInValue => "control byte from a backend",
        HeadDefect::ClOverflow => "backend CL overflow",
        HeadDefect::BadInterim => "defect in an interim head",
        HeadDefect::ConnFraming => "backend Connection names framing",
        HeadDefect::Upgrade101 => "101 from a backend",
        HeadDefect::TooManyInterim => "too many interim heads",
        HeadDefect::HeadTooLarge | HeadDefect::TooManyHeaders => "backend head too large",
        HeadDefect::ObsFold
        | HeadDefect::BadName
        | HeadDefect::BadVersion
        | HeadDefect::BadCode
        | HeadDefect::NoReasonSpace => "malformed backend head",
    });
    if body_end < wire.len() && tag.is_none() {
        tag = Some("bytes after a backend response");
    }
    // Endings: resets and ends of stream at random points (not together
    // with a body that breaks a rule).
    let overflow = matches!(kind, BodyKind::Chunked | BodyKind::UntilClose)
        && ctx.limit.is_some_and(|max| body.len() as u64 > max);
    let ending =
        if ctx.valid_only || defect.is_some() || body_defect.is_some() || overflow || too_large {
            Ending::Done {
                eof: kind == BodyKind::UntilClose || rng.bool(),
            }
        } else {
            match rng.u8(0..10) {
                0 => Ending::Reset(rng.usize(0..=body_end)),
                1 if kind != BodyKind::UntilClose => Ending::Eof(rng.usize(0..=body_end)),
                _ => Ending::Done {
                    eof: kind == BodyKind::UntilClose || rng.bool(),
                },
            }
        };
    // The model.
    let reply = if let Some(d) = defect {
        Reply::Fail(BackendFailure::BadResponse(d.error()))
    } else if too_large {
        Reply::Fail(BackendFailure::BadResponse(ResponseError::HeadTooLarge))
    } else if ctx.http10 && framing == BodyKind::Chunked {
        Reply::Fail(BackendFailure::TransferEncodingToHttp10)
    } else if let (BodyKind::Length, Some(n), Some(max)) = (kind, cl_value, ctx.limit) {
        if n > max {
            Reply::Fail(BackendFailure::BodyTooLarge)
        } else {
            relay(
                ctx,
                code,
                &reason,
                &lines,
                kind,
                &body,
                &body_wire,
                chunk.as_ref(),
                body_defect,
                ending,
                head_end,
                body_end,
            )
        }
    } else {
        relay(
            ctx,
            code,
            &reason,
            &lines,
            kind,
            &body,
            &body_wire,
            chunk.as_ref(),
            body_defect,
            ending,
            head_end,
            body_end,
        )
    };
    // A failure before the head is complete is a 502 too.
    let reply = match ending {
        Ending::Reset(k) if k < head_end => Reply::Fail(BackendFailure::Reset),
        Ending::Eof(k) if k < head_end => Reply::Fail(BackendFailure::ClosedEarly),
        Ending::Reset(_) | Ending::Eof(_) | Ending::Done { .. } => reply,
    };
    Script {
        wire,
        head_end,
        body_end,
        interim_ends,
        ending,
        reply,
        tag: match (too_large, padded) {
            (true, _) => Some("backend head just too large"),
            (false, true) => Some("backend head at the limit"),
            (false, false) => tag,
        },
    }
}

#[allow(clippy::too_many_arguments, reason = "one call site per branch")]
fn relay(
    ctx: &Ctx,
    code: u16,
    reason: &[u8],
    lines: &[(String, Vec<u8>)],
    kind: BodyKind,
    body: &[u8],
    body_wire: &[u8],
    chunk: Option<&Chunked>,
    body_defect: Option<ChunkDefect>,
    ending: Ending,
    head_end: usize,
    body_end: usize,
) -> Reply {
    let persistence = if ctx.early || kind == BodyKind::UntilClose {
        Persistence::Close
    } else {
        ctx.persistence
    };
    let tokens: Vec<String> = lines
        .iter()
        .filter(|(n, _)| n == "connection")
        .flat_map(|(_, v)| {
            String::from_utf8_lossy(v)
                .split(',')
                .map(|t| t.trim().to_ascii_lowercase())
                .collect::<Vec<_>>()
        })
        .collect();
    let mut head = format!("HTTP/1.1 {code} ").into_bytes();
    head.extend_from_slice(reason);
    head.extend_from_slice(b"\r\n");
    let mut backend_date = false;
    for (name, value) in lines {
        if is_hop(name, &tokens) {
            continue;
        }
        backend_date |= name == "date";
        head.extend_from_slice(name.as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value);
        head.extend_from_slice(b"\r\n");
    }
    // FR-PROXY-12, FR-HTTP-22a: the backend's `Date` passes through; a
    // response without one gets the core's, behind the backend's headers.
    if !backend_date {
        head.extend_from_slice(format!("date: {}\r\n", NET_DATE.1).as_bytes());
    }
    if persistence == Persistence::Close {
        head.extend_from_slice(b"connection: close\r\n");
    }
    head.extend_from_slice(b"\r\n");
    let forwarded: Vec<u8> = match kind {
        BodyKind::None => Vec::new(),
        BodyKind::Length | BodyKind::UntilClose => body.to_vec(),
        BodyKind::Chunked => body_wire.to_vec(),
    };
    let mut fails = None;
    match (kind, chunk) {
        (BodyKind::Chunked, Some(c)) => {
            let over = ctx
                .limit
                .and_then(|max| c.payload_at.get(max as usize).copied());
            let mut errors = Vec::new();
            let mut bound = usize::MAX;
            if let (Some(at), Some(d)) = (c.defect_at, body_defect) {
                errors.push(BackendFailure::BadChunk(d.error()));
                bound = at;
            }
            if let Some(at) = over.filter(|at| *at < bound || errors.is_empty()) {
                errors.push(BackendFailure::BodyTooLarge);
                bound = bound.min(at);
            }
            if !errors.is_empty() {
                fails = Some((errors, bound));
            }
        }
        (BodyKind::UntilClose, _) => {
            if let Some(max) = ctx.limit.filter(|max| body.len() as u64 > *max) {
                fails = Some((vec![BackendFailure::BodyTooLarge], max as usize));
            }
        }
        (BodyKind::None | BodyKind::Length | BodyKind::Chunked, _) => {}
    }
    let complete = |k: usize| k >= body_end && kind != BodyKind::UntilClose;
    let cut = match ending {
        Ending::Reset(k) if !complete(k) && kind != BodyKind::None => {
            Some((BackendFailure::Reset, k.saturating_sub(head_end)))
        }
        Ending::Eof(k) if !complete(k) && kind != BodyKind::None => {
            Some((BackendFailure::ClosedEarly, k.saturating_sub(head_end)))
        }
        Ending::Reset(_) | Ending::Eof(_) | Ending::Done { .. } => None,
    };
    Reply::Relay(Relay {
        status: code,
        head,
        body: forwarded,
        kind,
        persistence,
        fails,
        cut,
    })
}
