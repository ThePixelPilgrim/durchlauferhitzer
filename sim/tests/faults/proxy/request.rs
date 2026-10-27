//! Random client requests for the proxy net, and the model of each: a
//! parse error, an answer of the core's own, a body the core must refuse,
//! or exactly the bytes the backend must receive (FR-PROXY-04…11). The
//! model is written from the spec, independently of the core's renderer.

use super::*;

/// One header line as the client sends it (value with its OWS).
#[derive(Debug, Clone)]
pub(crate) struct Line {
    pub(crate) name: String,
    pub(crate) value: Vec<u8>,
}

fn line(name: &str, value: &[u8]) -> Line {
    Line {
        name: name.to_owned(),
        value: value.to_vec(),
    }
}

/// One generated request and what the core must make of it.
#[derive(Debug)]
pub(crate) struct Request {
    /// Head and body as the client sends them.
    pub(crate) wire: Vec<u8>,
    pub(crate) head_len: usize,
    pub(crate) summary: RequestSummary,
    pub(crate) head_only: bool,
    pub(crate) http10: bool,
    pub(crate) persistence: Persistence,
    pub(crate) verdict: Verdict,
    /// The request's body is chunked (pipelining right behind it is a
    /// smuggling probe).
    pub(crate) chunked: bool,
    /// For a chunked body: where each body byte ends a unit the core may
    /// forward (a data byte, a complete size line or CRLF).
    pub(crate) chunk_ends: Vec<usize>,
    /// Coverage tag of the smuggling or edge case it carries, if any.
    pub(crate) tag: Option<&'static str>,
}

#[derive(Debug)]
pub(crate) enum Verdict {
    /// A head the parser must reject: logged `Rejected { Http(e) }`,
    /// answered `e.status()` with a body, closed.
    Parse(HttpError),
    /// The core answers itself (route miss, 413, duplicate
    /// `X-Forwarded-For`), logged as a `Response`.
    Answer(Status, Persistence),
    /// A chunked body the core must refuse: 400 before connecting, or a cut
    /// once connected. Every forwarded byte lies before `bound` (an offset
    /// into the body).
    BadBody {
        errors: Vec<RequestBodyError>,
        fwd: Forward,
        bound: usize,
    },
    Forward(Forward),
}

/// A request that is forwarded: to which route, and as which bytes.
#[derive(Debug, Clone)]
pub(crate) struct Forward {
    pub(crate) route: usize,
    /// The rewritten head, then the client's body verbatim.
    pub(crate) bytes: Vec<u8>,
    pub(crate) head_len: usize,
    /// The client waits for `100 Continue` (HTTP/1.1, one valid `Expect`,
    /// a body).
    pub(crate) expect100: bool,
}

/// The body framing of a generated request.
enum Body {
    None,
    Length { zeros: usize, payload: Vec<u8> },
    Chunked(Vec<u8>),
}

/// A defect a request may carry (at most one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Defect {
    ClTe,
    DupCl,
    BadCl,
    BadTe,
    TeTwice,
    ConnFraming,
    ObsText,
    CtlInValue,
    DupHost,
    DupXff,
    ExpectOther,
    ExpectTwice,
    ExpectList,
    Chunk(ChunkDefect),
    HiddenRequest,
}

fn random_case(rng: &mut fastrand::Rng, name: &str) -> String {
    match rng.u8(0..4) {
        0 => name.to_ascii_uppercase(),
        1 => name.to_ascii_lowercase(),
        _ => name.to_owned(),
    }
}

/// A random body length: mostly small, sometimes past a buffer, rarely
/// large.
pub(crate) fn body_len(rng: &mut fastrand::Rng) -> usize {
    match rng.u8(0..20) {
        0..10 => rng.usize(0..64),
        10..16 => rng.usize(64..3000),
        16..19 => rng.usize(3000..40_000),
        _ => rng.usize(40_000..300_000),
    }
}

/// Extra header lines a client may send; hop-by-hop ones among them.
const EXTRA: &[(&str, &[u8])] = &[
    ("Accept", b"*/*"),
    ("User-Agent", b"  sim/1.0\t"),
    ("X-Custom", b"a\tb c"),
    ("Cookie", b"k=v; k2=v2"),
    ("X-Empty", b""),
    ("Connection", b"keep-alive"),
    ("Connection", b"close"),
    ("Keep-Alive", b"timeout=5"),
    ("Proxy-Authorization", b"Basic eDp5"),
    ("Proxy-Connection", b"keep-alive"),
    ("TE", b"trailers"),
    ("Trailer", b"X-T"),
    ("Upgrade", b"websocket"),
    ("X-Forwarded-For", b"203.0.113.7"),
    ("X-Forwarded-For", b" 198.51.100.2, 203.0.113.9 "),
    ("X-Forwarded-For", b""),
    ("X-Forwarded-Proto", b"https"),
];

/// Request paths below a route prefix.
const RESTS: &[&str] = &[
    "",
    "/",
    "/x",
    "/x/",
    "/a/b.c",
    "/sp%20ace",
    "/%C3%A4/z",
    "/v1/items/42",
];
const QUERIES: &[&str] = &["", "", "?", "?a=1", "?q=%2F;x&y=%41"];
const METHODS: &[&str] = &[
    "GET", "GET", "HEAD", "POST", "POST", "PUT", "DELETE", "PATCH", "M-SEARCH",
];

fn method_of(name: &str) -> Method {
    match name {
        "GET" => Method::Get,
        "HEAD" => Method::Head,
        other => Method::Other(other.to_owned()),
    }
}

/// What the generator may draw for one request.
pub(crate) struct Allow {
    /// Body defects (they need a backend that accepts).
    pub(crate) body_defects: bool,
    /// Requests only to proxy routes, well-formed and forwarded (for timed
    /// scenarios), with a body if `need_body`.
    pub(crate) forward_only: bool,
    pub(crate) need_body: bool,
}

/// A random request from `peer` under `net`.
pub(crate) fn random_proxy_request(
    rng: &mut fastrand::Rng,
    net: &NetConfig,
    peer: &str,
    allow: &Allow,
) -> Request {
    let http10 = !allow.forward_only && rng.u8(0..8) == 0;
    let method = rng.choice(METHODS).copied().unwrap_or("GET");
    // 0, 1: the proxy routes; 2: the static route; 3: no route; 4: other
    // host.
    let target = if allow.forward_only {
        rng.usize(0..2)
    } else {
        match rng.u8(0..20) {
            0..10 => 0,
            10..17 => 1,
            17 => 2,
            18 => 3,
            _ => 4,
        }
    };
    let rest = rng.choice(RESTS).copied().unwrap_or("");
    let path = match target {
        0 | 1 => format!("{}{rest}", net.routes[target].prefix),
        2 => format!("/files{}", if rest.is_empty() { "/f" } else { rest }),
        _ => format!("/nothing{rest}"),
    };
    let query = rng.choice(QUERIES).copied().unwrap_or("");
    let host = if target == 4 {
        "other.test"
    } else {
        "example.test"
    };
    let host_sent = match rng.u8(0..4) {
        0 => host.to_ascii_uppercase(),
        _ => host.to_owned(),
    };
    let mut defect = None;
    if !allow.forward_only && rng.u8(0..4) == 0 {
        defect = Some(match rng.u8(0..16) {
            0 => Defect::ClTe,
            1 => Defect::DupCl,
            2 => Defect::BadCl,
            3 => Defect::BadTe,
            4 => Defect::TeTwice,
            5 => Defect::ConnFraming,
            6 => Defect::ObsText,
            7 => Defect::CtlInValue,
            8 => Defect::DupHost,
            9 => Defect::DupXff,
            10 => Defect::ExpectOther,
            11 => Defect::ExpectTwice,
            12 => Defect::ExpectList,
            13 => Defect::HiddenRequest,
            _ if allow.body_defects => Defect::Chunk(ChunkDefect::random(rng)),
            _ => Defect::HiddenRequest,
        });
    }
    // Body framing.
    let wants_body = allow.need_body
        || match defect {
            Some(
                Defect::ClTe
                | Defect::DupCl
                | Defect::BadCl
                | Defect::BadTe
                | Defect::TeTwice
                | Defect::Chunk(_)
                | Defect::HiddenRequest,
            ) => true,
            Some(_) | None => match method {
                "POST" | "PUT" | "PATCH" => rng.u8(0..5) != 0,
                _ => rng.u8(0..6) == 0,
            },
        };
    let mut chunk_info = None;
    let body = if !wants_body {
        Body::None
    } else {
        let mut len = body_len(rng);
        if allow.need_body && len == 0 {
            len = 1;
        }
        let bytes = if defect == Some(Defect::HiddenRequest) {
            let mut p = b"GET /hidden HTTP/1.1\r\nHost: example.test\r\n\r\n".to_vec();
            p.extend(payload(rng, len % 64));
            p
        } else {
            payload(rng, len)
        };
        let chunk = match defect {
            Some(Defect::ClTe | Defect::BadTe | Defect::TeTwice) => true,
            Some(Defect::Chunk(_)) => true,
            Some(Defect::DupCl | Defect::BadCl) => false,
            Some(_) | None => rng.u8(0..5) < 2 && !http10,
        };
        if chunk {
            let c = chunked(
                rng,
                &bytes,
                match defect {
                    Some(Defect::Chunk(d)) => Some(d),
                    Some(_) | None => None,
                },
            );
            let wire = c.wire.clone();
            chunk_info = Some(c);
            Body::Chunked(wire)
        } else {
            let zeros = if rng.u8(0..5) == 0 {
                rng.usize(1..4)
            } else {
                0
            };
            Body::Length {
                zeros,
                payload: bytes,
            }
        }
    };
    let has_body = match &body {
        Body::None => false,
        Body::Length { payload, .. } => !payload.is_empty(),
        Body::Chunked(_) => true,
    };
    // Header lines.
    let mut lines = vec![line(&random_case(rng, "Host"), host_sent.as_bytes())];
    for _ in 0..rng.usize(0..4) {
        if let Some(&(name, value)) = rng.choice(EXTRA) {
            let mut l = line(&random_case(rng, name), value);
            // A header named in `Connection` is dropped with it.
            if name == "Connection" && rng.u8(0..3) == 0 {
                l.value = b"X-Drop, keep-alive".to_vec();
                lines.push(line("X-Drop", b"secret"));
            } else if name == "Connection" && rng.u8(0..6) == 0 {
                l.value = b"X-Forwarded-For".to_vec();
            }
            lines.push(l);
        }
    }
    if defect == Some(Defect::DupXff) {
        lines.push(line("X-Forwarded-For", b"203.0.113.1"));
        lines.push(line(&random_case(rng, "x-forwarded-for"), b"203.0.113.2"));
    }
    let mut expect_lines = 0;
    let mut expect_ok = false;
    match defect {
        Some(Defect::ExpectOther) => {
            lines.push(line("Expect", b"200-ok"));
            expect_lines = 1;
        }
        Some(Defect::ExpectList) => {
            lines.push(line("Expect", b"100-continue, foo"));
            expect_lines = 1;
        }
        Some(Defect::ExpectTwice) => {
            lines.push(line("Expect", b"100-continue"));
            lines.push(line("Expect", b"100-continue"));
            expect_lines = 2;
        }
        Some(_) | None => {
            if (has_body && rng.u8(0..3) == 0) || rng.u8(0..20) == 0 {
                lines.push(line(&random_case(rng, "Expect"), b"100-continue"));
                expect_lines = 1;
                expect_ok = true;
            }
        }
    }
    match &body {
        Body::None => {
            if rng.u8(0..8) == 0 {
                lines.push(line("Content-Length", b"0"));
            }
        }
        Body::Length { zeros, payload } => {
            let value = format!("{}{}", "0".repeat(*zeros), payload.len());
            lines.push(line(&random_case(rng, "Content-Length"), value.as_bytes()));
            match defect {
                Some(Defect::DupCl) => lines.push(line("Content-Length", value.as_bytes())),
                Some(Defect::BadCl) => {
                    let last = lines.len() - 1;
                    lines[last].value = format!("+{}", payload.len()).into_bytes();
                }
                Some(_) | None => {}
            }
        }
        Body::Chunked(_) => {
            let value: &[u8] = match (defect, rng.u8(0..4)) {
                (Some(Defect::BadTe), _) => b"gzip, chunked",
                (_, 0) => b"Chunked",
                _ => b"chunked",
            };
            lines.push(line(&random_case(rng, "Transfer-Encoding"), value));
            match defect {
                Some(Defect::ClTe) => lines.push(line("Content-Length", b"5")),
                Some(Defect::TeTwice) => lines.push(line("Transfer-Encoding", b"chunked")),
                Some(_) | None => {}
            }
        }
    }
    match defect {
        Some(Defect::ConnFraming) => {
            let named: &[u8] = match rng.u8(0..3) {
                0 => b"Content-Length",
                1 => b"close, transfer-encoding",
                _ => b"HOST",
            };
            lines.push(line("Connection", named));
        }
        Some(Defect::ObsText) => lines.push(line("X-Name", b"caf\xc3\xa9")),
        Some(Defect::CtlInValue) => lines.push(line("X-Name", b"a\x01b")),
        Some(Defect::DupHost) => lines.push(line("Host", b"example.test")),
        Some(_) | None => {}
    }
    // Shuffle all but `Host` a little: order must be kept on the way.
    if lines.len() > 2 && rng.bool() {
        let i = rng.usize(0..lines.len());
        let l = lines.remove(i);
        lines.push(l);
    }

    // The wire bytes.
    let version = if http10 { "HTTP/1.0" } else { "HTTP/1.1" };
    let mut wire = format!("{method} {path}{query} {version}\r\n").into_bytes();
    for l in &lines {
        wire.extend_from_slice(l.name.as_bytes());
        wire.extend_from_slice(b":");
        if rng.u8(0..4) != 0 {
            wire.push(b' ');
        }
        wire.extend_from_slice(&l.value);
        wire.extend_from_slice(b"\r\n");
    }
    // Now and then a head right at `max_head` (FR-HTTP-06): a padding
    // line brings it to within 3 bytes of the limit, either side.
    let mut too_large = false;
    let mut padded = false;
    if defect.is_none() && rng.u8(0..12) == 0 {
        let target = net.max_head as i64 + rng.i64(-3..=3);
        let pad = target - wire.len() as i64 - 2 - "X-Big: \r\n".len() as i64;
        if pad > 0 {
            let value = vec![b'b'; pad as usize];
            wire.extend_from_slice(b"X-Big: ");
            wire.extend_from_slice(&value);
            wire.extend_from_slice(b"\r\n");
            lines.push(line("X-Big", &value));
            too_large = wire.len() + 2 > net.max_head;
            padded = true;
        }
    }
    wire.extend_from_slice(b"\r\n");
    let head_len = wire.len();
    match &body {
        Body::None => {}
        Body::Length { payload, .. } => wire.extend_from_slice(payload),
        Body::Chunked(w) => wire.extend_from_slice(w),
    }

    // The model.
    let tokens = connection_tokens(&lines);
    let close = http10 || tokens.iter().any(|t| t == "close");
    let persistence = if close {
        Persistence::Close
    } else {
        Persistence::KeepAlive
    };
    let summary = RequestSummary {
        method: method_of(method),
        host: Some(host.to_owned()),
        path: path.clone(),
    };
    let tag = defect.map(|d| match d {
        Defect::ClTe => "CL+TE",
        Defect::DupCl => "duplicate CL",
        Defect::BadCl => "bad CL",
        Defect::BadTe => "unsupported TE",
        Defect::TeTwice => "TE twice",
        Defect::ConnFraming => "Connection names framing",
        Defect::ObsText => "obs-text value",
        Defect::CtlInValue => "control byte in value",
        Defect::DupHost => "duplicate Host",
        Defect::DupXff => "duplicate X-Forwarded-For",
        Defect::ExpectOther | Defect::ExpectTwice | Defect::ExpectList => "bad Expect",
        Defect::Chunk(c) => c.tag(),
        Defect::HiddenRequest => "request hidden in a body",
    });
    let tag = match (too_large, padded) {
        (true, _) => Some("request head just too large"),
        (false, true) => Some("request head at the limit"),
        (false, false) => tag,
    };
    let verdict = if too_large {
        Verdict::Parse(HttpError::HeadTooLarge)
    } else {
        judge(&Judged {
            net,
            defect,
            http10,
            target,
            path: &path,
            query,
            method,
            lines: &lines,
            tokens: &tokens,
            body: &body,
            chunk: chunk_info.as_ref(),
            has_body,
            expect_lines,
            expect_ok,
            peer,
            persistence,
        })
    };
    let chunked = matches!(body, Body::Chunked(_));
    let chunk_ends = chunk_info.as_ref().map(unit_ends).unwrap_or_default();
    Request {
        wire,
        head_len,
        summary,
        head_only: method == "HEAD",
        http10,
        persistence,
        verdict,
        chunked,
        chunk_ends,
        tag,
    }
}

/// The tokens of every `Connection` line, lowercased.
fn connection_tokens(lines: &[Line]) -> Vec<String> {
    lines
        .iter()
        .filter(|l| l.name.eq_ignore_ascii_case("connection"))
        .flat_map(|l| {
            String::from_utf8_lossy(&l.value)
                .split(',')
                .map(|t| t.trim().to_ascii_lowercase())
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Optional whitespace (SP, HTAB) trimmed on both sides.
pub(crate) fn trim_ows(v: &[u8]) -> &[u8] {
    let start = v
        .iter()
        .position(|b| *b != b' ' && *b != b'\t')
        .unwrap_or(v.len());
    let end = v
        .iter()
        .rposition(|b| *b != b' ' && *b != b'\t')
        .map_or(start, |e| e + 1);
    &v[start..end.max(start)]
}

/// The hop-by-hop list of FR-PROXY-06/-12, plus the names `Connection`
/// lists.
pub(crate) fn is_hop(name_lower: &str, tokens: &[String]) -> bool {
    [
        "connection",
        "keep-alive",
        "te",
        "trailer",
        "upgrade",
        "expect",
    ]
    .contains(&name_lower)
        || name_lower.starts_with("proxy-")
        || tokens.iter().any(|t| t == name_lower)
}

struct Judged<'a> {
    net: &'a NetConfig,
    defect: Option<Defect>,
    http10: bool,
    target: usize,
    path: &'a str,
    query: &'a str,
    method: &'a str,
    lines: &'a [Line],
    tokens: &'a [String],
    body: &'a Body,
    chunk: Option<&'a Chunked>,
    has_body: bool,
    expect_lines: usize,
    expect_ok: bool,
    peer: &'a str,
    persistence: Persistence,
}

fn judge(j: &Judged) -> Verdict {
    // Line checks (FR-HTTP-05), then the head checks in their order:
    // FR-HTTP-09a, Host, framing (FR-HTTP-08), `Expect` (FR-PROXY-25).
    let parse = match j.defect {
        Some(Defect::ObsText | Defect::CtlInValue) => Some(HttpError::BadHeaderValue),
        Some(Defect::ConnFraming) => Some(HttpError::ConnectionNamesFraming),
        Some(Defect::DupHost) => Some(HttpError::DuplicateHost),
        Some(Defect::ClTe | Defect::BadTe | Defect::TeTwice | Defect::Chunk(_)) if j.http10 => {
            Some(HttpError::TransferEncodingInHttp10)
        }
        Some(Defect::ClTe) => Some(HttpError::ContentLengthWithTransferEncoding),
        Some(Defect::DupCl) => Some(HttpError::DuplicateContentLength),
        Some(Defect::BadCl) => Some(HttpError::BadContentLength),
        Some(Defect::BadTe | Defect::TeTwice) => Some(HttpError::UnsupportedTransferEncoding),
        Some(_) | None if j.http10 && matches!(j.body, Body::Chunked(_)) => {
            Some(HttpError::TransferEncodingInHttp10)
        }
        Some(Defect::ExpectOther | Defect::ExpectTwice | Defect::ExpectList) if !j.http10 => {
            Some(HttpError::BadExpect)
        }
        Some(_) | None => None,
    };
    if let Some(e) = parse {
        return Verdict::Parse(e);
    }
    let body_close = if j.has_body {
        Persistence::Close
    } else {
        j.persistence
    };
    // The static route takes only GET and HEAD, without a body
    // (FR-STATIC-03, -04); nothing else matches.
    if j.target == 2 && !matches!(j.method, "GET" | "HEAD") {
        return Verdict::Answer(Status::MethodNotAllowed, body_close);
    }
    if j.target == 2 && j.has_body {
        return Verdict::Answer(Status::ContentTooLarge, Persistence::Close);
    }
    if j.target >= 2 {
        return Verdict::Answer(Status::NotFound, body_close);
    }
    let route = &j.net.routes[j.target];
    let xff_lines = j
        .lines
        .iter()
        .filter(|l| l.name.eq_ignore_ascii_case("x-forwarded-for"))
        .count();
    if xff_lines > 1 {
        return Verdict::Answer(Status::BadRequest, Persistence::Close);
    }
    if let (Body::Length { payload, .. }, Some(max)) = (j.body, route.max_request_body) {
        if payload.len() as u64 > max {
            return Verdict::Answer(Status::ContentTooLarge, Persistence::Close);
        }
    }
    let fwd = forward(j, route);
    // A chunked body: its defect, or the byte that passes the limit.
    if let Some(c) = j.chunk {
        let over = route
            .max_request_body
            .and_then(|max| c.payload_at.get(max as usize).copied());
        let mut errors = Vec::new();
        let mut bound = usize::MAX;
        if let (Some(Defect::Chunk(d)), Some(at)) = (j.defect, c.defect_at) {
            errors.push(RequestBodyError::Chunked(d.error()));
            bound = at;
        }
        if let Some(at) = over.filter(|at| *at < bound || errors.is_empty()) {
            errors.push(RequestBodyError::TooLarge);
            bound = bound.min(at);
        }
        if !errors.is_empty() {
            return Verdict::BadBody { errors, fwd, bound };
        }
    }
    Verdict::Forward(fwd)
}

/// The request the backend must receive (FR-PROXY-04…11).
fn forward(j: &Judged, route: &RouteConfig) -> Forward {
    let path = if route.strip {
        let rest = &j.path[route.prefix.len()..];
        if rest.is_empty() { "/" } else { rest }
    } else {
        j.path
    };
    let version = if j.http10 { "HTTP/1.0" } else { "HTTP/1.1" };
    let mut out = format!("{} {path}{} {version}\r\n", j.method, j.query).into_bytes();
    let mut xff = None;
    for l in j.lines {
        let name = l.name.to_ascii_lowercase();
        if is_hop(&name, j.tokens) || name == "x-forwarded-proto" {
            continue;
        }
        let value = trim_ows(&l.value);
        if name == "x-forwarded-for" {
            xff = Some(value.to_vec()).filter(|v| !v.is_empty());
            continue;
        }
        let value = if name == "content-length" {
            let digits = String::from_utf8_lossy(value);
            let trimmed = digits.trim_start_matches('0');
            if trimmed.is_empty() {
                "0".to_owned()
            } else {
                trimmed.to_owned()
            }
            .into_bytes()
        } else {
            value.to_vec()
        };
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(&value);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"x-forwarded-for: ");
    if let Some(v) = xff {
        out.extend_from_slice(&v);
        out.extend_from_slice(b", ");
    }
    out.extend_from_slice(j.peer.as_bytes());
    out.extend_from_slice(b"\r\nx-forwarded-proto: http\r\nconnection: close\r\n\r\n");
    let head_len = out.len();
    match j.body {
        Body::None => {}
        Body::Length { payload, .. } => out.extend_from_slice(payload),
        Body::Chunked(w) => out.extend_from_slice(w),
    }
    Forward {
        route: j.target,
        bytes: out,
        head_len,
        expect100: !j.http10 && j.expect_ok && j.expect_lines == 1 && j.has_body,
    }
}
