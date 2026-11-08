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
    /// Conditional requests (`conditional.rs`).
    pub(crate) conditional: ConditionalCoverage,
    /// Range requests (`range.rs`).
    pub(crate) range: RangeCoverage,
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
            (
                "two /64s in one group (ipv6_prefix 48)",
                self.admission.joined_prefixes,
            ),
            (
                "a connection admitted during a restart's drain",
                self.admission.restart_admitted,
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
        self.conditional.check();
        self.range.check();
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
    /// For a 200 or 206: the file, its route's policy and what of it is
    /// sent.
    pub(crate) file: Option<(&'t ModelFile, Policy, Span)>,
    /// For a 200, 206, 304 or 416: the file's stamp at open.
    pub(crate) stamp: Option<FileStamp>,
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
    /// before, or the drain is a restart's (FR-PROC-07): it gets the
    /// tree's status (with close), not the 503.
    No,
}

/// The response `log` (written in `phase`) announces, if it announces
/// one, with the exact bytes it may be sent as. A logged request status
/// that does not follow from the tree (and, for a file, from the
/// conditional lines of `head`, the request it answers if it answers one:
/// `conditional.rs`) is an error; during a shutdown's
/// drain a request may be answered `503` with close instead (FR-PROC-05),
/// unless it was parsed before the drain (`refusable`). Nothing else in
/// this net answers a request `503` (refusals are `Rejected`), so any
/// other `503` response is an error, whatever the host.
pub(crate) fn announced<'t>(
    log: &Log,
    tree: &'t Tree,
    phase: Phase,
    refusable: Refusable,
    head: Option<&[u8]>,
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
                stamp: None,
            }));
        }
        // The 502 that follows announces the response.
        Log::Reset { .. }
        | Log::FileAborted { .. }
        | Log::BackendFailed { .. }
        | Log::BackendRetried { .. }
        | Log::BackendNotSilent { .. } => {
            return Ok(None);
        }
        Log::ProxiedResponse { .. } | Log::BackendFile { .. } => {
            return Err(format!("{log:?}, but every backend refuses"));
        }
    };
    let body = match request.map(|r| &r.method) {
        Some(Method::Head) => BodyMode::HeadOnly,
        Some(Method::Get | Method::Other(_)) | None => BodyMode::Full,
    };
    if status == Status::ServiceUnavailable {
        if phase != Phase::Draining || refusable == Refusable::No {
            return Err(format!(
                "{log:?}: a 503 answers a request, but no shutdown's drain may refuse it \
                 ({phase:?}, {refusable:?})"
            ));
        }
        if request.is_none() {
            return Err(format!("{log:?}: a drain's 503 without its request"));
        }
        return Ok(Some(Announced {
            status,
            body,
            renderings: fixed_renderings(status, &[Persistence::Close], body),
            file: None,
            stamp: None,
        }));
    }
    let Some(request) = request.filter(|r| r.host.as_deref() == Some("example.test")) else {
        return Ok(Some(Announced {
            status,
            body,
            renderings: fixed_renderings(status, &BOTH, body),
            file: None,
            stamp: None,
        }));
    };
    let path = request.path.as_str();
    if has_encoded_unreserved(path) {
        return Err(format!("{path} answered, but it must be rejected (400)"));
    }
    let mut expected = tree.expected_status(&request.method, path);
    // A 304, 206 or 416 by the request's conditional and range lines.
    if let (Status::Ok, Expect::File(f, _), Some(head)) = (expected, tree.resolve(path), head) {
        expected = model_file_status(head, f);
    }
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
        (
            Status::Ok | Status::PartialContent | Status::RangeNotSatisfiable,
            Expect::File(f, policy),
        ) => file_announced(status, f, policy, body, head),
        // FR-STATIC-18: a 304 is the head alone, the file unread.
        (Status::NotModified, Expect::File(f, _)) => {
            let renderings = BOTH
                .iter()
                .map(|&persistence| {
                    let bytes = model_not_modified(f.stamp, persistence, Some(NET_DATE.1));
                    Rendering {
                        persistence,
                        with_query: false,
                        head_len: bytes.len(),
                        bytes,
                    }
                })
                .collect();
            Announced {
                status,
                body,
                renderings,
                file: None,
                stamp: Some(f.stamp),
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
                stamp: None,
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
                stamp: None,
            }
        }
        _ => Announced {
            status,
            body,
            renderings: fixed_renderings(status, &BOTH, body),
            file: None,
            stamp: None,
        },
    };
    Ok(Some(announced))
}

pub(crate) fn log_conn(log: &Log) -> ConnId {
    match log {
        Log::Response { conn, .. }
        | Log::Rejected { conn, .. }
        | Log::Reset { conn, .. }
        | Log::FileAborted { conn, .. }
        | Log::ProxiedResponse { conn, .. }
        | Log::BackendFile { conn, .. }
        | Log::BackendFailed { conn, .. }
        | Log::BackendRetried { conn, .. }
        | Log::BackendNotSilent { conn, .. } => *conn,
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
    limits: HeadLimits,
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
        | Log::BackendFile { .. }
        | Log::BackendFailed { .. }
        | Log::BackendRetried { .. }
        | Log::BackendNotSilent { .. } => false,
    };
    let refused = phased.iter().any(|&(l, p)| refusal(l, p));
    let mut expected = Vec::new();
    // The phase each announced response was logged in.
    let mut expected_phase = Vec::new();
    // The request head each announced response answers, if any.
    let mut expected_head: Vec<Option<&[u8]>> = Vec::new();
    // Every announced response but a connection refusal answers the next
    // request template: its persistence follows from that request.
    // Two templates may form one head (`heads`); `consumed` counts templates.
    let heads = heads(&client.templates, limits);
    let mut templates = heads.iter();
    let mut consumed = 0;
    for (n, &(log, phase)) in phased.iter().enumerate() {
        if phase == Phase::AbortCut {
            coverage.shutdown.abort_cut += 1;
        }
        let refusable = if marks.restart() {
            Refusable::No
        } else if opened_at_drain && first_draining == Some(n) {
            if matches!(log, Log::Response { .. }) {
                coverage.shutdown.opened_before_drain += 1;
            }
            Refusable::No
        } else {
            Refusable::Yes
        };
        // The head this log answers, unless it is a refusal (checked below).
        let next_head = templates.clone().next().map(|(t, _)| t.as_slice());
        match announced(log, tree, phase, refusable, next_head) {
            Ok(Some(mut a)) => {
                let mut answers = None;
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
                    let Some((template, last)) = templates.next() else {
                        fail(format!("{log:?} answers no request sent"));
                    };
                    answers = Some(template.as_slice());
                    let joined = if *last > consumed {
                        Joined::Joined
                    } else {
                        Joined::Single
                    };
                    consumed = last + 1;
                    if let Log::Rejected {
                        reason: RejectReason::Http { error, request },
                        ..
                    } = log
                        && let Err(e) = check_request_line(error, request.as_ref(), template)
                    {
                        fail(e);
                    }
                    if phase == Phase::Draining {
                        let judged = Drained {
                            log,
                            refusable,
                            head: template,
                            joined,
                            limits,
                        };
                        if let Err(e) = judged.check(&mut coverage.shutdown) {
                            fail(e);
                        }
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
                expected_head.push(answers);
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
                if marks.restart() {
                    if !refused {
                        coverage.shutdown.restart_served += 1;
                    }
                } else if a.status != Status::ServiceUnavailable {
                    coverage.shutdown.closing_head += 1;
                } else if !refused {
                    coverage.shutdown.refused_request += 1;
                }
            }
            rest = rest.get(r.bytes.len()..).unwrap_or_default();
            if let (Some((f, policy, span)), BodyMode::Full) = (a.file, a.body) {
                if f.expected_abort(policy, span).is_some() {
                    fail(format!(
                        "complete {} byte body ({span:?}) of a file that must abort ({:?})",
                        f.size(),
                        f.fault
                    ));
                }
                if span.len() > CHUNK {
                    coverage.multi_chunk += 1;
                }
                if let (Policy::AllowGrowth, Fault::Change { after, new }) = (policy, f.fault) {
                    if new.size > f.size() && after < span.chunks() {
                        coverage.grown += 1;
                    }
                }
            }
            if a.status == Status::Ok && a.body == BodyMode::HeadOnly {
                coverage.head_ok += 1;
            }
            if let (Some(stamp), Some(Some(head))) = (a.stamp, expected_head.get(index)) {
                coverage
                    .conditional
                    .count(head, stamp, a.status, r.persistence);
                coverage.range.count(head, stamp, a.status);
            }
            if r.persistence == Persistence::Close {
                match a.status {
                    Status::Ok | Status::PartialContent => coverage.close_ok += 1,
                    Status::MovedPermanently => coverage.close_redirect += 1,
                    Status::RangeNotSatisfiable
                    | Status::NotModified
                    | Status::BadRequest
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
                // A 206 checked byte for byte past its first chunk.
                if a.status == Status::PartialContent && rest.len() > r.head_len + CHUNK as usize {
                    coverage.range.across_chunk();
                }
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
        let Some((f, policy, span)) = a.file.filter(|_| a.body == BodyMode::Full) else {
            fail(format!(
                "FileAborted after a {} {:?}",
                model_code(a.status),
                a.body
            ));
        };
        let Some((want, body_at)) = f.expected_abort(policy, span) else {
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
        if a.status == Status::PartialContent && body_at.is_some() {
            coverage.range.cut();
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
    // During a shutdown's drain it also does after a keep-alive response
    // queued before the drain began (FR-PROC-05 "Core half",
    // FR-PROC-05b); a restart's drain keeps that connection for its next
    // request (FR-PROC-07).
    if shut
        && !truncated
        && seen
            .last()
            .is_none_or(|s| s.persistence != Persistence::Close)
    {
        if marks.shutdown_drain() && last_seen_phase == Some(Phase::Serving) {
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
    if marks.shutdown_drain()
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
