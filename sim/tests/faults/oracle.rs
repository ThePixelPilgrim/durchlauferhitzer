//! The oracle: what each client must receive, given the logs and the
//! tree, and what the runs must cover.

use super::*;

/// What the generator reached, summed over all seeds.
#[derive(Default)]
pub(crate) struct Coverage {
    pub(crate) statuses: [usize; STATUSES.len()],
    pub(crate) http: usize,
    pub(crate) header_timeout: usize,
    pub(crate) send_timeout: usize,
    pub(crate) disk_timeout: usize,
    pub(crate) limit_connections: usize,
    pub(crate) limit_per_ip: usize,
    pub(crate) pipelined: usize,
    pub(crate) aborted_truncated: usize,
    pub(crate) aborted_changed: usize,
    /// Aborts on the first chunk's stamp, possibly before the head was out.
    pub(crate) aborted_first_chunk: usize,
    /// Complete `GET` bodies of more than one chunk.
    pub(crate) multi_chunk: usize,
    /// Complete `GET` bodies of files that grew under `allow_growth`.
    pub(crate) grown: usize,
    pub(crate) head_ok: usize,
    /// 200s and 301s rendered as closing.
    pub(crate) close_ok: usize,
    pub(crate) close_redirect: usize,
    /// Directory redirects answered 414 because they would not fit the
    /// send buffer.
    pub(crate) redirect_414: usize,
    /// Paths rejected for an encoded unreserved character.
    pub(crate) encoded_unreserved: usize,
    /// Responses on the `.well-known` route.
    pub(crate) well_known: usize,
    /// Directory redirects that kept a query.
    pub(crate) redirect_query: usize,
    /// 200s on the `/@%C3%A4` route through an escaped or re-cased
    /// spelling of its segment.
    pub(crate) respelled_route: usize,
    /// The drains and aborts (`shutdown.rs`).
    pub(crate) shutdown: ShutdownCoverage,
    /// Peer groups (`admission.rs`).
    pub(crate) admission: AdmissionCoverage,
}

/// `grown` must be well clear of zero, so a refactor cannot silently lose
/// it.
pub(crate) const MIN_GROWN: usize = 50;

impl Coverage {
    pub(crate) fn check(&self) {
        let named = [
            ("Rejected(Http)", self.http),
            ("Rejected(Timeout(Header))", self.header_timeout),
            ("Rejected(Timeout(Send))", self.send_timeout),
            ("Rejected(Timeout(Disk))", self.disk_timeout),
            ("Rejected(Limit(Connections))", self.limit_connections),
            ("Rejected(Limit(ConnectionsPerIp))", self.limit_per_ip),
            (
                "a group shared by two addresses",
                self.admission.shared_group,
            ),
            (
                "a refusal dropped for its group's refused cap",
                self.admission.group_dropped,
            ),
            ("pipelined pairs", self.pipelined),
            ("FileAborted(Truncated)", self.aborted_truncated),
            ("FileAborted(Changed)", self.aborted_changed),
            ("FileAborted on the first chunk", self.aborted_first_chunk),
            ("multi-chunk 200 bodies", self.multi_chunk),
            ("grown files sent under allow_growth", self.grown),
            ("HEAD 200", self.head_ok),
            ("closing 200", self.close_ok),
            ("closing 301", self.close_redirect),
            ("414 instead of a redirect", self.redirect_414),
            (
                "400 for an encoded unreserved character",
                self.encoded_unreserved,
            ),
            (".well-known responses", self.well_known),
            ("redirects with a query", self.redirect_query),
            (
                "200s through a respelled route segment",
                self.respelled_route,
            ),
        ];
        assert!(
            self.grown >= MIN_GROWN,
            "only {} grown files sent under allow_growth (want {MIN_GROWN})",
            self.grown
        );
        for (status, n) in STATUSES.iter().zip(self.statuses) {
            assert!(n > 0, "no {} response in any seed", model_code(*status));
        }
        for (what, n) in named {
            assert!(n > 0, "no {what} in any seed");
        }
        self.shutdown.check();
    }
}

/// One complete received response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Seen {
    pub(crate) status: Status,
    pub(crate) persistence: Persistence,
    pub(crate) body: BodyMode,
}

/// One rendering a logged response may have been sent as.
pub(crate) struct Rendering {
    pub(crate) persistence: Persistence,
    /// A redirect whose `Location` kept a query.
    pub(crate) with_query: bool,
    /// Length of the head (for a file response: where the body starts).
    pub(crate) head_len: usize,
    pub(crate) bytes: Vec<u8>,
}

/// A response the core logged for a connection, and how it may look.
pub(crate) struct Announced<'t> {
    pub(crate) status: Status,
    pub(crate) body: BodyMode,
    pub(crate) renderings: Vec<Rendering>,
    /// For a 200: the file and its route's policy.
    pub(crate) file: Option<(&'t ModelFile, Policy)>,
}

pub(crate) fn fixed_renderings(
    status: Status,
    persistences: &[Persistence],
    body: BodyMode,
) -> Vec<Rendering> {
    persistences
        .iter()
        .map(|&persistence| {
            let bytes = fixed(status, persistence, body);
            let head = fixed(status, persistence, BodyMode::HeadOnly);
            Rendering {
                persistence,
                with_query: false,
                head_len: head.len(),
                bytes,
            }
        })
        .collect()
}

/// A directory redirect of `path` with each query the generator can
/// produce, in each persistence: the renderings that fit the send buffer
/// (a `301`), and whether any would not (a `414` instead).
pub(crate) fn redirect_renderings(path: &str, body: BodyMode) -> (Vec<Rendering>, bool) {
    let mut fitting = Vec::new();
    let mut overflow = false;
    for query in all_queries() {
        let location = format!("{path}/{query}");
        for persistence in BOTH {
            // FR-STATIC-07: a redirect that does not fit the send buffer
            // (1 KiB, NFR-RES-01) is a 414 instead.
            let bytes = model_redirect(&location, persistence, body, Some(NET_DATE.1));
            if bytes.len() > SEND_CAPACITY {
                overflow = true;
                continue;
            }
            let head = model_redirect(&location, persistence, BodyMode::HeadOnly, Some(NET_DATE.1));
            fitting.push(Rendering {
                persistence,
                with_query: !query.is_empty(),
                head_len: head.len(),
                bytes,
            });
        }
    }
    (fitting, overflow)
}

pub(crate) const BOTH: [Persistence; 2] = [Persistence::KeepAlive, Persistence::Close];

/// Whether a request logged during the drain may be its `503`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusable {
    Yes,
    /// Its file was being opened when the drain began, so it was parsed
    /// before: it gets the tree's status (with close), not the 503.
    No,
}

/// The response `log` (written in `phase`) announces, if it announces
/// one, with the exact bytes it may be sent as. A logged request status
/// that does not follow from the tree is an error; during the drain a
/// request may be answered `503` with close instead (FR-PROC-05), unless
/// it was parsed before the drain (`refusable`).
pub(crate) fn announced<'t>(
    log: &Log,
    tree: &'t Tree,
    phase: Phase,
    refusable: Refusable,
) -> Result<Option<Announced<'t>>, String> {
    let (status, request) = match log {
        Log::Response {
            status, request, ..
        } => (*status, request.as_ref()),
        Log::Rejected { reason, .. } => {
            let status = match reason {
                // Its request line is checked against the request it
                // answers (`check_request_line`).
                RejectReason::Http { error, .. } => model_http_status(error),
                RejectReason::Timeout(TimeoutKind::Header | TimeoutKind::RequestBody) => {
                    Status::RequestTimeout
                }
                RejectReason::Timeout(TimeoutKind::Send | TimeoutKind::Disk) => return Ok(None),
                RejectReason::Body(RequestBodyError::Chunked(_)) => Status::BadRequest,
                // A body over the limit, or one the client ended early, is
                // cut, not answered.
                RejectReason::Body(RequestBodyError::TooLarge | RequestBodyError::ClosedEarly) => {
                    return Ok(None);
                }
                // An exchange the abort cut: nothing is answered.
                RejectReason::ShuttingDown if phase == Phase::AbortCut => return Ok(None),
                RejectReason::Limit(_) | RejectReason::ShuttingDown => Status::ServiceUnavailable,
                // Not emitted in this stage; a route miss is answered 404.
                RejectReason::Route(_) => Status::NotFound,
            };
            // Refusals and rejections always close, and have a body.
            return Ok(Some(Announced {
                status,
                body: BodyMode::Full,
                renderings: fixed_renderings(status, &[Persistence::Close], BodyMode::Full),
                file: None,
            }));
        }
        // The 502 that follows announces the response.
        Log::Reset { .. } | Log::FileAborted { .. } | Log::BackendFailed { .. } => {
            return Ok(None);
        }
        Log::ProxiedResponse { .. } => {
            return Err(format!("{log:?}, but every backend refuses"));
        }
    };
    let body = match request.map(|r| &r.method) {
        Some(Method::Head) => BodyMode::HeadOnly,
        Some(Method::Get | Method::Other(_)) | None => BodyMode::Full,
    };
    if phase == Phase::Draining
        && status == Status::ServiceUnavailable
        && refusable == Refusable::Yes
    {
        if request.is_none() {
            return Err(format!("{log:?}: a drain's 503 without its request"));
        }
        return Ok(Some(Announced {
            status,
            body,
            renderings: fixed_renderings(status, &[Persistence::Close], body),
            file: None,
        }));
    }
    let Some(request) = request.filter(|r| r.host.as_deref() == Some("example.test")) else {
        return Ok(Some(Announced {
            status,
            body,
            renderings: fixed_renderings(status, &BOTH, body),
            file: None,
        }));
    };
    let path = request.path.as_str();
    if has_encoded_unreserved(path) {
        return Err(format!("{path} answered, but it must be rejected (400)"));
    }
    let expected = tree.expected_status(&request.method, path);
    // A redirect too long for the send buffer is answered 414 instead.
    let redirect_too_long = expected == Status::MovedPermanently && status == Status::UriTooLong;
    if status != expected && !redirect_too_long {
        return Err(format!(
            "{:?} {path} logged {} but the tree says {}",
            request.method,
            model_code(status),
            model_code(expected)
        ));
    }
    let announced = match (status, tree.resolve(path)) {
        (Status::Ok, Expect::File(f, policy)) => {
            let renderings = BOTH
                .iter()
                .map(|&persistence| {
                    let mut bytes =
                        model_file_head(f.content_type, f.size(), persistence, Some(NET_DATE.1));
                    let head_len = bytes.len();
                    match body {
                        BodyMode::Full => bytes.extend_from_slice(&f.bytes),
                        BodyMode::HeadOnly => {}
                    }
                    Rendering {
                        persistence,
                        with_query: false,
                        head_len,
                        bytes,
                    }
                })
                .collect();
            Announced {
                status,
                body,
                renderings,
                file: Some((f, policy)),
            }
        }
        (Status::MovedPermanently, _) => {
            let (renderings, _) = redirect_renderings(path, body);
            if renderings.is_empty() {
                return Err(format!("301 for {path}, which never fits the send buffer"));
            }
            Announced {
                status,
                body,
                renderings,
                file: None,
            }
        }
        (Status::UriTooLong, _) => {
            let (_, overflow) = redirect_renderings(path, body);
            if !overflow {
                return Err(format!("414 for {path}, whose redirect always fits"));
            }
            Announced {
                status,
                body,
                renderings: fixed_renderings(status, &BOTH, body),
                file: None,
            }
        }
        _ => Announced {
            status,
            body,
            renderings: fixed_renderings(status, &BOTH, body),
            file: None,
        },
    };
    Ok(Some(announced))
}

/// Where the parser finds an error: in the request line, after a valid
/// one, or (a head over its limit, a bare LF) in either.
enum Found {
    RequestLine,
    Headers,
    Either,
}

/// The raw path of `template`'s request target, without the query.
fn template_path(template: &[u8]) -> String {
    let line = String::from_utf8_lossy(template);
    let target = line.split(' ').nth(1).unwrap_or_default();
    target.split('?').next().unwrap_or_default().to_owned()
}

/// FR-LOG-07: a parse error of `template` carries the request line
/// exactly when it was found after a valid one, with the template's raw
/// path. An error that can be in either place is placed by the template:
/// after the request line if that line ended with CRLF (a bare LF ending
/// it is the error itself; a head over its limit with a valid request
/// line is found in the headers).
fn check_request_line(
    error: &HttpError,
    request: Option<&RequestLine>,
    template: &[u8],
) -> Result<(), String> {
    let found = match error {
        HttpError::RequestLineTooLong
        | HttpError::BadRequestLine
        | HttpError::BadMethod
        | HttpError::BadVersion
        | HttpError::UnsupportedVersion
        | HttpError::BadTarget(_) => Found::RequestLine,
        HttpError::TooManyHeaders
        | HttpError::BadHeaderName
        | HttpError::BadHeaderValue
        | HttpError::ObsFold
        | HttpError::MissingHost
        | HttpError::DuplicateHost
        | HttpError::BadHost
        | HttpError::ContentLengthWithTransferEncoding
        | HttpError::BadContentLength
        | HttpError::DuplicateContentLength
        | HttpError::UnsupportedTransferEncoding
        | HttpError::TransferEncodingInHttp10
        | HttpError::ConnectionNamesFraming
        | HttpError::BadExpect => Found::Headers,
        HttpError::HeadTooLarge | HttpError::BareLf => Found::Either,
    };
    let crlf_line = template
        .iter()
        .position(|&b| b == b'\n')
        .is_some_and(|lf| lf > 0 && template.get(lf - 1) == Some(&b'\r'));
    let after_line = match found {
        Found::RequestLine => false,
        Found::Headers => true,
        Found::Either => crlf_line,
    };
    match (after_line, request) {
        (false, Some(line)) => Err(format!(
            "{error:?} is in the request line, but {line:?} was logged"
        )),
        (true, None) => Err(format!(
            "{error:?} follows a valid request line, which was not logged"
        )),
        (true, Some(line)) if line.path != template_path(template) => Err(format!(
            "{line:?} logged for request {:?}",
            template_path(template)
        )),
        (false, None) | (true, Some(_)) => Ok(()),
    }
}

pub(crate) fn log_conn(log: &Log) -> ConnId {
    match log {
        Log::Response { conn, .. }
        | Log::Rejected { conn, .. }
        | Log::Reset { conn, .. }
        | Log::FileAborted { conn, .. }
        | Log::ProxiedResponse { conn, .. }
        | Log::BackendFailed { conn, .. } => *conn,
    }
}

/// The persistences a response to request `template` may have
/// (FR-HTTP-09, FR-CONN-04): close for HTTP/1.0, for `Connection: close`
/// and for a body the core does not read; keep-alive otherwise. A proxied
/// 502 to a request with a body may be either: the body is read only if
/// it came with the head.
pub(crate) fn expected_persistence(template: &[u8], status: Status) -> &'static [Persistence] {
    let text = String::from_utf8_lossy(template);
    let head = text.split("\r\n\r\n").next().unwrap_or_default();
    let http10 = head.lines().next().is_some_and(|l| l.ends_with("HTTP/1.0"));
    let close = head
        .lines()
        .any(|l| l.eq_ignore_ascii_case("connection: close"));
    let body = head.lines().any(|l| {
        l.to_ascii_lowercase()
            .strip_prefix("content-length: ")
            .is_some_and(|n| n.trim() != "0")
    });
    match (body, status) {
        (true, Status::BadGateway) => &BOTH,
        (true, _) => &[Persistence::Close],
        (false, _) if http10 || close => &[Persistence::Close],
        (false, _) => &[Persistence::KeepAlive],
    }
}

/// A response cut short: which announced response, in which rendering,
/// and how many of its bytes arrived.
pub(crate) struct Cut {
    pub(crate) index: usize,
    pub(crate) head_len: usize,
    pub(crate) received: usize,
}

/// Check one client's received bytes against its requests, the logs and
/// the tree.
pub(crate) fn check_client(
    sim: &Sim,
    tree: &Tree,
    client: &Client,
    marks: &Marks,
    coverage: &mut Coverage,
) {
    let fail = |what: String| -> ! {
        panic!("conn={}: {what}\ntrace:\n{}", client.id.0, sim.trace());
    };
    // Each of the connection's logs with the phase it was written in.
    let phased: Vec<(&Log, Phase)> = sim
        .logs()
        .iter()
        .enumerate()
        .filter(|(_, l)| log_conn(l) == client.id)
        .map(|(i, l)| (l, marks.phase(i)))
        .collect();
    // The connection's first log written during the drain answers the
    // request whose file was being opened then, if one was (M-a).
    let opened_at_drain = marks
        .at_drain
        .get(&client.id)
        .is_some_and(|a| a.opening == Opening::File);
    let first_draining = sim
        .logs()
        .iter()
        .enumerate()
        .filter(|(_, l)| log_conn(l) == client.id)
        .position(|(i, _)| marks.drain.is_some_and(|d| i >= d));
    let logs: Vec<&Log> = phased.iter().map(|(l, _)| *l).collect();
    // A connection refused (over a limit, or while shutting down) has no
    // request: its 503 answers none.
    let refusal = |log: &Log, phase: Phase| match log {
        Log::Rejected {
            reason: RejectReason::Limit(_),
            ..
        } => true,
        Log::Rejected {
            reason: RejectReason::ShuttingDown,
            ..
        } => phase != Phase::AbortCut,
        Log::Response { .. }
        | Log::Rejected { .. }
        | Log::Reset { .. }
        | Log::FileAborted { .. }
        | Log::ProxiedResponse { .. }
        | Log::BackendFailed { .. } => false,
    };
    let refused = phased.iter().any(|&(l, p)| refusal(l, p));
    let mut expected = Vec::new();
    // The phase each announced response was logged in.
    let mut expected_phase = Vec::new();
    // Every announced response but a connection refusal answers the next
    // request template: its persistence follows from that request.
    let mut templates = client.templates.iter();
    let mut consumed = 0;
    for (n, &(log, phase)) in phased.iter().enumerate() {
        if phase == Phase::AbortCut {
            coverage.shutdown.abort_cut += 1;
        }
        let refusable = if opened_at_drain && first_draining == Some(n) {
            if matches!(log, Log::Response { .. }) {
                coverage.shutdown.opened_before_drain += 1;
            }
            Refusable::No
        } else {
            Refusable::Yes
        };
        match announced(log, tree, phase, refusable) {
            Ok(Some(mut a)) => {
                if refusal(log, phase) {
                    if matches!(
                        log,
                        Log::Rejected {
                            reason: RejectReason::ShuttingDown,
                            ..
                        }
                    ) {
                        coverage.shutdown.refused_connection += 1;
                    }
                } else {
                    let Some(template) = templates.next() else {
                        fail(format!("{log:?} answers no request sent"));
                    };
                    consumed += 1;
                    if let Log::Rejected {
                        reason: RejectReason::Http { error, request },
                        ..
                    } = log
                        && let Err(e) = check_request_line(error, request.as_ref(), template)
                    {
                        fail(e);
                    }
                    // Rejections always close (their renderings already do).
                    if let Log::Response { request, .. } = log {
                        // The pairing holds: the logged path is the
                        // template's.
                        let path = template_path(template);
                        if let Some(r) = request.as_ref().filter(|r| r.path != path) {
                            fail(format!(
                                "{log:?} paired with request {path:?}, not {:?}",
                                r.path
                            ));
                        }
                        // A head rendered during the drain says close
                        // (FR-PROC-05 "Core half").
                        let allowed = match phase {
                            Phase::Serving => expected_persistence(template, a.status),
                            Phase::Draining | Phase::AbortCut => &[Persistence::Close],
                        };
                        a.renderings.retain(|r| allowed.contains(&r.persistence));
                    }
                }
                expected.push(a);
                expected_phase.push(phase);
            }
            Ok(None) => {}
            Err(e) => fail(e),
        }
    }
    // What arrived is, in order, a prefix of what the core logged, each
    // response exactly as rendered; the last may be cut short.
    let mut seen: Vec<Seen> = Vec::new();
    // The phase the last complete response was logged in.
    let mut last_seen_phase: Option<Phase> = None;
    let mut cut: Option<Cut> = None;
    let mut rest = client.received.as_slice();
    for (index, a) in expected.iter().enumerate() {
        if rest.is_empty() {
            break;
        }
        let offset = client.received.len() - rest.len();
        if let Some(r) = a.renderings.iter().find(|r| rest.starts_with(&r.bytes)) {
            seen.push(Seen {
                status: a.status,
                persistence: r.persistence,
                body: a.body,
            });
            last_seen_phase = expected_phase.get(index).copied();
            if last_seen_phase == Some(Phase::Draining) {
                if a.status != Status::ServiceUnavailable {
                    coverage.shutdown.closing_head += 1;
                } else if !refused {
                    coverage.shutdown.refused_request += 1;
                }
            }
            rest = rest.get(r.bytes.len()..).unwrap_or_default();
            if let (Some((f, policy)), BodyMode::Full) = (a.file, a.body) {
                if f.expected_abort(policy).is_some() {
                    fail(format!(
                        "complete {} byte body of a file that must abort ({:?})",
                        f.size(),
                        f.fault
                    ));
                }
                if f.size() > CHUNK {
                    coverage.multi_chunk += 1;
                }
                if let (Policy::AllowGrowth, Fault::Change { after, new }) = (policy, f.fault) {
                    if new.size > f.size() && after < f.size().div_ceil(CHUNK) {
                        coverage.grown += 1;
                    }
                }
            }
            if a.status == Status::Ok && a.body == BodyMode::HeadOnly {
                coverage.head_ok += 1;
            }
            if r.persistence == Persistence::Close {
                match a.status {
                    Status::Ok => coverage.close_ok += 1,
                    Status::MovedPermanently => coverage.close_redirect += 1,
                    Status::BadRequest
                    | Status::NotFound
                    | Status::MethodNotAllowed
                    | Status::RequestTimeout
                    | Status::ContentTooLarge
                    | Status::UriTooLong
                    | Status::ExpectationFailed
                    | Status::RequestHeaderFieldsTooLarge
                    | Status::BadGateway
                    | Status::ServiceUnavailable
                    | Status::GatewayTimeout
                    | Status::HttpVersionNotSupported => {}
                }
            }
            if a.status == Status::MovedPermanently && r.with_query {
                coverage.redirect_query += 1;
            }
            continue;
        }
        match a.renderings.iter().find(|r| r.bytes.starts_with(rest)) {
            Some(r) => {
                cut = Some(Cut {
                    index,
                    head_len: r.head_len,
                    received: rest.len(),
                });
                rest = &[];
                break;
            }
            None => fail(format!(
                "unrecognised response bytes at offset {offset} (expected a {})",
                model_code(a.status)
            )),
        }
    }
    if !rest.is_empty() {
        let statuses: Vec<Status> = seen.iter().map(|s| s.status).collect();
        fail(format!(
            "{} bytes after the logged responses {statuses:?}",
            rest.len()
        ));
    }
    let statuses: Vec<Status> = seen.iter().map(|s| s.status).collect();
    let announced_statuses: Vec<Status> = expected.iter().map(|a| a.status).collect();
    let truncated = cut.is_some();
    // Less than announced, or cut short, only when the write was abandoned
    // (send or disk timeout, reset, file abort) or a refusal was closed at
    // once.
    if truncated || statuses.len() < announced_statuses.len() {
        let aborted = matches!(phased.last(), Some((_, Phase::AbortCut)));
        let abandoned = aborted
            || matches!(
                logs.last(),
                Some(Log::Rejected {
                    reason: RejectReason::Timeout(TimeoutKind::Send | TimeoutKind::Disk),
                    ..
                }) | Some(Log::Reset { .. })
                    | Some(Log::FileAborted { .. })
            );
        // A refusal is closed at once, or cut by the abort before it is
        // out (silently: its refusal is logged already, FR-PROC-05c).
        let refused_at_once = statuses.is_empty() && refused && phased.len() == 1;
        if aborted && !refused_at_once {
            coverage.shutdown.abort_truncated += 1;
        }
        if !abandoned && !refused_at_once {
            fail(format!(
                "received {statuses:?} (truncated: {truncated}) of {announced_statuses:?}"
            ));
        }
    }
    // A file abort is the connection's last word, right after the 200 of a
    // GET whose file must abort that way, and the body is cut exactly where
    // the fault strikes.
    if let Some(at) = logs
        .iter()
        .position(|l| matches!(l, Log::FileAborted { .. }))
    {
        let Some(Log::FileAborted { error, .. }) = logs.get(at) else {
            fail("unreachable".to_owned());
        };
        if at + 1 != logs.len() {
            fail(format!("logs after FileAborted: {logs:?}"));
        }
        let last = expected.len().checked_sub(1);
        let Some(a) = last.and_then(|i| expected.get(i)) else {
            fail("FileAborted without a 200".to_owned());
        };
        let Some((f, policy)) = a.file.filter(|_| a.body == BodyMode::Full) else {
            fail(format!(
                "FileAborted after a {} {:?}",
                model_code(a.status),
                a.body
            ));
        };
        let Some((want, body_at)) = f.expected_abort(policy) else {
            fail(format!(
                "FileAborted({error:?}) for a file with {:?}",
                f.fault
            ));
        };
        if *error != want {
            fail(format!(
                "FileAborted({error:?}), expected {want:?} for {:?} under {policy:?}",
                f.fault
            ));
        }
        match (body_at, &cut) {
            (Some(n), Some(c))
                if Some(c.index) == last
                    && u64::try_from(c.received).ok() == Some(c.head_len as u64 + n) => {}
            (None, Some(c)) if Some(c.index) == last && c.received <= c.head_len => {}
            (None, None) if Some(statuses.len()) == last => {}
            _ => fail(format!(
                "FileAborted({error:?}) for {:?} of a {} byte file, but received {:?} bytes \
                 (head {:?}) of response {:?}",
                f.fault,
                f.size(),
                cut.as_ref().map(|c| c.received),
                cut.as_ref().map(|c| c.head_len),
                cut.as_ref().map(|c| c.index),
            )),
        }
        match error {
            FileError::Truncated => coverage.aborted_truncated += 1,
            FileError::Changed => coverage.aborted_changed += 1,
            FileError::NotFound
            | FileError::Symlink
            | FileError::PermissionDenied
            | FileError::Other => {}
        }
        if body_at.is_none() {
            coverage.aborted_first_chunk += 1;
        }
    }
    if refused && statuses.len() > 1 {
        fail(format!(
            "a refused connection's 503 is not its only response: {statuses:?}"
        ));
    }
    let answered = statuses.len() - usize::from(refused && !statuses.is_empty());
    if answered > client.started {
        fail(format!(
            "{answered} responses to {} requests started",
            client.started
        ));
    }
    let head_only = seen.iter().filter(|s| s.body == BodyMode::HeadOnly).count();
    if head_only > client.started_head {
        fail(format!(
            "{head_only} bodiless responses to {} HEAD requests",
            client.started_head
        ));
    }
    if let Some(close) = seen
        .iter()
        .position(|s| s.persistence == Persistence::Close)
    {
        if close + 1 != seen.len() || truncated {
            fail(format!("response after Connection: close: {seen:?}"));
        }
    }
    // The core shuts its sending side only after a response it rendered
    // as closing (FR-HTTP-19, FR-STATIC-11): the last complete response
    // must say `Connection: close`.
    let shut = sim
        .trace()
        .contains(&format!(" conn={} -> ShutdownWrite", client.id.0));
    // During the drain it also does after a keep-alive response queued
    // before the drain began (FR-PROC-05 "Core half", FR-PROC-05b).
    if shut
        && !truncated
        && seen
            .last()
            .is_none_or(|s| s.persistence != Persistence::Close)
    {
        if marks.shut_down() && last_seen_phase == Some(Phase::Serving) {
            coverage.shutdown.shut_after_keep_alive += 1;
        } else {
            fail(format!(
                "the connection was shut down after {seen:?}, whose last response does not close"
            ));
        }
    }
    // No request the spec says must be answered vanished.
    let last = match expected.iter().enumerate().next_back() {
        None => LastAnswer::None,
        Some((i, a)) => {
            let renderings = &a.renderings;
            let closes = renderings
                .iter()
                .all(|r| r.persistence == Persistence::Close);
            match seen.get(i).map(|s| s.persistence) {
                Some(Persistence::Close) => LastAnswer::Closes,
                Some(Persistence::KeepAlive) => LastAnswer::KeepsOpen,
                None if closes => LastAnswer::Closes,
                // Either is allowed, and it was not received whole.
                None if renderings.len() > 1 => LastAnswer::Closes,
                None => LastAnswer::KeepsOpen,
            }
        }
    };
    let answers = Answers {
        consumed,
        last,
        refused: if refused { Refused::Yes } else { Refused::No },
        cut: phased
            .last()
            .map_or(CutOff::No, |&(log, phase)| cuts(log, phase)),
    };
    if let Err(e) = check_unanswered(sim, client, marks, &answers) {
        fail(e);
    }
    if marks.drain.is_some()
        && consumed < client.templates.len()
        && answers.last == LastAnswer::KeepsOpen
        && answers.cut == CutOff::No
    {
        coverage.shutdown.dropped_unread += 1;
    }
    for (slot, status) in coverage.statuses.iter_mut().zip(STATUSES) {
        *slot += statuses.iter().filter(|s| **s == status).count();
    }
    if client.pipelined_sends > 0 && answered >= 2 {
        coverage.pipelined += 1;
    }
}
