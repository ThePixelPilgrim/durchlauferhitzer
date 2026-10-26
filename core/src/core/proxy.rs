//! Proxy routes: the rules decided before connecting, connecting to the
//! backend, writing the rewritten request head, reading and judging the
//! response head, and relaying the rewritten head and a `Content-Length`
//! body to the client straight out of the upstream's receive buffer. The
//! request body's side (reading it from the client and writing it to the
//! backend, its errors and stalls) is in `upload.rs`.
//!
//! Only responses without a body or with a `Content-Length` one are
//! relayed so far; the rest answers 501 (chunked, close-delimited and
//! interim responses: Task 7).

use std::time::Duration;

use super::request::{AfterResponse, HeadEnd, finish};
use super::{Action, ConnId, Core};
use crate::config::{BackendAddr, BodyLimit, ProxyRoute, Route, StripPrefix};
use crate::conn::{Conn, ConnState, ProxyRequest, Source, UnreadBody};
use crate::http::forward::{
    backend_request_len, check_forwardable, client_response_len, render_backend_request,
    render_client_response,
};
use crate::http::request::{BodyFraming, Expect, Method, Persistence, RequestHead};
use crate::http::response::{BodyMode, render_fixed};
use crate::http::response_head::{
    RequestKind, ResponseBody, ResponseError, ResponseHead, ResponseParser, ResponseStatus,
};
use crate::http::status::Status;
use crate::log::{BackendFailure, Log, RejectReason, RequestSummary, TimeoutKind};
use crate::request_body::{Arrived, BodyEnd, RequestBody, Stage};
use crate::routing::strip_segments;
use crate::time::Instant;
use crate::upstream::{BackendId, ExchangeBuffers, UpstreamId, UpstreamTable};

/// How long a connect may take (FR-PROXY-29's default of
/// `backend_connect_timeout`; Task 8 makes it a limit).
pub(super) const BACKEND_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the backend may take from the request head being fully
/// written until its response head is complete (the default of
/// `backend_response_timeout`). It also bounds writing the request head.
pub(super) const BACKEND_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a body may make no progress, in either direction: a relayed
/// response body while the upstream is open, a request body before the
/// response head (the default of `body_idle_timeout`).
pub(super) const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How a request on a proxy route is answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ProxyPlan {
    /// Connect to the backend and forward the request.
    Forward(Forward),
    /// Answer `status` right away.
    Respond(Status, Persistence),
}

/// Forward a request to `backend` with `path` as its (possibly stripped)
/// raw path; a chunked body may carry at most `limit` payload bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Forward {
    backend: BackendId,
    path: String,
    limit: BodyLimit,
}

/// The proxy route rules that need no backend: the request must be
/// forwardable (FR-PROXY-08: at most one `X-Forwarded-For`, else 400 and
/// close), and a `Content-Length` must not exceed the route's
/// `max_request_body` (FR-PROXY-23: else 413 and close, its body unread).
/// Any method is forwarded (FR-PROXY-01).
pub(super) fn plan(
    route: &Route,
    target: &ProxyRoute,
    head: &RequestHead,
    backends: &[BackendAddr],
) -> ProxyPlan {
    if let Err(error) = check_forwardable(head) {
        return ProxyPlan::Respond(error.status(), Persistence::Close);
    }
    let limit = target.max_request_body;
    match (head.body, limit) {
        (BodyFraming::Length(length), BodyLimit::Bytes(max)) if length > max => {
            return ProxyPlan::Respond(Status::ContentTooLarge, Persistence::Close);
        }
        (BodyFraming::None | BodyFraming::Length(_) | BodyFraming::Chunked, _) => {}
    }
    let Some(backend) = backends
        .iter()
        .position(|b| *b == target.backend)
        .and_then(|index| u32::try_from(index).ok())
        .map(BackendId)
    else {
        // Every configured backend is listed; unreachable in practice.
        return ProxyPlan::Respond(Status::BadGateway, head.persistence);
    };
    let path = match target.strip_prefix {
        StripPrefix::Strip => strip_segments(&head.target.path, route.segments.len()),
        StripPrefix::Keep => head.target.path.clone(),
    };
    ProxyPlan::Forward(Forward {
        backend,
        path,
        limit,
    })
}

impl Core {
    /// Start forwarding `head` on `conn`: judge the body bytes that came
    /// with it, render the rewritten request head into a fresh exchange's
    /// head buffer and connect. The bytes after the head (`after`) are the
    /// body, forwarded from where they are, then the next request.
    pub(super) fn forward(
        &mut self,
        now: Instant,
        conn: ConnId,
        head: RequestHead,
        forward: Forward,
        after: HeadEnd,
        actions: &mut Vec<Action>,
    ) {
        let send_deadline = now.saturating_add(self.config.limits.send_idle_timeout);
        let (recv_capacity, head_capacity) = (self.upstream_recv_capacity, self.head_capacity);
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let Forward {
            backend,
            path,
            limit,
        } = forward;
        let summary = RequestSummary {
            method: head.method.clone(),
            host: head.host.clone(),
            path: head.target.path.clone(),
        };
        // Never beyond the head buffer's capacity. A head within the limits
        // always fits (see `head_capacity`); this guards that bound.
        if backend_request_len(&head, &path, c.peer) > head_capacity {
            let status = Status::RequestHeaderFieldsTooLarge;
            render_fixed(
                status,
                Persistence::Close,
                body_mode(&head.method),
                &mut c.send,
            );
            actions.push(Action::Log(Log::Response {
                conn,
                peer: c.peer,
                status,
                request: Some(summary),
            }));
            c.state = ConnState::Responding {
                persistence: Persistence::Close,
                unread_body: UnreadBody::None,
                buffered: 0,
                deadline: send_deadline,
            };
            return;
        }
        // The body bytes that came with the head are judged before anything
        // is forwarded: a framing error is still a 400.
        let body = match RequestBody::new(
            head.body,
            limit,
            after.consumed,
            after.buffered,
            &mut c.recv,
        ) {
            Ok(body) => body,
            Err(error) => {
                self.reject_body(now, conn, &head.method, error, actions);
                return;
            }
        };
        // `100 Continue` only makes sense for a body still to come.
        let expect = match head.body {
            BodyFraming::Length(_) | BodyFraming::Chunked => head.expect,
            BodyFraming::None => Expect::None,
        };
        let mut buffers = ExchangeBuffers::new(recv_capacity, head_capacity);
        render_backend_request(&head, &path, c.peer, &mut buffers.head);
        let up = self.upstreams.allocate(conn);
        actions.push(Action::Connect { up, backend });
        c.state = ConnState::Connecting {
            up,
            request: ProxyRequest {
                summary,
                backend,
                kind: match head.method {
                    Method::Head => RequestKind::Head,
                    Method::Get | Method::Other(_) => RequestKind::Other,
                },
                persistence: head.persistence,
                body,
                expect,
            },
            buffers,
            deadline: now.saturating_add(BACKEND_CONNECT_TIMEOUT),
        };
    }
}

/// Emit `CloseUpstream` for `up` if it is still open: the table entry is
/// removed, so this happens exactly once per upstream.
pub(super) fn close_upstream(
    upstreams: &mut UpstreamTable,
    up: UpstreamId,
    actions: &mut Vec<Action>,
) {
    if upstreams.close(up) {
        actions.push(Action::CloseUpstream { up });
    }
}

fn body_mode(method: &Method) -> BodyMode {
    match method {
        Method::Head => BodyMode::HeadOnly,
        Method::Get | Method::Other(_) => BodyMode::Full,
    }
}

/// How the response head the backend sent is answered.
enum Verdict {
    /// Relay it: a head of `consumed` bytes, with `body` bytes to follow.
    Relay {
        head: ResponseHead,
        consumed: usize,
        body: u64,
    },
    /// Not yet supported in this task: 501 (chunked, close-delimited and
    /// interim responses are Task 7's).
    NotImplemented,
    Bad(ResponseError),
}

impl Core {
    /// The connect for `up` completed: write the request head, and answer
    /// `100 Continue` if the client waits for it (FR-PROXY-25: the head
    /// passed every check and the backend is there to take the body).
    pub(super) fn upstream_connected(
        &mut self,
        now: Instant,
        up: UpstreamId,
        actions: &mut Vec<Action>,
    ) {
        let Some(conn) = self.upstreams.conn(up) else {
            return;
        };
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        c.state = match c.take_state() {
            ConnState::Connecting {
                up: own,
                request,
                buffers,
                ..
            } if own == up => {
                match request.expect {
                    Expect::Continue => c.send.extend_from_slice(CONTINUE),
                    Expect::None => {}
                }
                ConnState::AwaitingResponse {
                    up,
                    request,
                    buffers,
                    filled: 0,
                    parser: ResponseParser::new(),
                    deadline: now.saturating_add(BACKEND_RESPONSE_TIMEOUT),
                }
            }
            other @ (ConnState::Connecting { .. }
            | ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::AwaitingResponse { .. }
            | ConnState::Relaying { .. }
            | ConnState::Lingering { .. }) => other,
        };
        self.sync_interest(conn, actions);
    }

    /// `up`'s connect or socket failed: before the response head that is a
    /// 502, after it the connection is cut.
    pub(super) fn upstream_failed(
        &mut self,
        now: Instant,
        up: UpstreamId,
        actions: &mut Vec<Action>,
    ) {
        let Some(conn) = self.upstreams.conn(up) else {
            return;
        };
        let Some(c) = self.conns.get(&conn) else {
            return;
        };
        match c.state {
            ConnState::Connecting { .. } => {
                self.fail_before_head(now, conn, BackendFailure::Refused, actions);
            }
            ConnState::AwaitingResponse { .. } => {
                self.fail_before_head(now, conn, BackendFailure::Reset, actions);
            }
            ConnState::Relaying { .. } => self.cut(conn, BackendFailure::Reset, actions),
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Lingering { .. } => {}
        }
    }

    /// The backend closed its sending side before its response was
    /// complete: the same as a reset. (After the whole body arrived the
    /// upstream is already closed and no event comes.)
    pub(super) fn upstream_read_closed(
        &mut self,
        now: Instant,
        up: UpstreamId,
        actions: &mut Vec<Action>,
    ) {
        self.upstream_failed(now, up, actions);
    }

    /// The driver read `len` bytes from `up`: more of the response head,
    /// or the next piece of the body.
    pub(super) fn upstream_received(
        &mut self,
        now: Instant,
        up: UpstreamId,
        len: usize,
        actions: &mut Vec<Action>,
    ) {
        let Some(conn) = self.upstreams.conn(up) else {
            return;
        };
        let send_idle_timeout = self.config.limits.send_idle_timeout;
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        // Only what was offered can have arrived: clamp to it.
        if !c.state.reads_upstream(up) {
            return;
        }
        match &mut c.state {
            ConnState::AwaitingResponse {
                buffers, filled, ..
            } => {
                let added = len.min(buffers.recv.len().saturating_sub(*filled));
                if added == 0 {
                    return;
                }
                *filled = filled.saturating_add(added);
                self.judge_head(now, conn, actions);
                return;
            }
            ConnState::Relaying {
                source,
                buffers,
                start,
                end,
                deadline,
                ..
            } => {
                let Source::Open {
                    remaining, backend, ..
                } = *source
                else {
                    return;
                };
                let added = len.min(buffers.recv.len());
                if added == 0 {
                    return;
                }
                // Bytes beyond the body's end are never forwarded.
                let body = usize::try_from(remaining).map_or(added, |r| r.min(added));
                *start = 0;
                *end = body;
                let remaining = remaining.saturating_sub(body as u64);
                if remaining == 0 {
                    *source = Source::Done;
                    close_upstream(&mut self.upstreams, up, actions);
                } else {
                    *source = Source::Open {
                        up,
                        backend,
                        remaining,
                    };
                }
                // The client has these bytes to take now.
                *deadline = relay_deadline(now, true, *source, send_idle_timeout);
            }
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Connecting { .. }
            | ConnState::Lingering { .. } => return,
        }
        self.sync_interest(conn, actions);
    }

    /// Parse the response head bytes of `conn`'s upstream so far and act
    /// on a verdict.
    fn judge_head(&mut self, now: Instant, conn: ConnId, actions: &mut Vec<Action>) {
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let ConnState::AwaitingResponse {
            request,
            buffers,
            filled,
            parser,
            ..
        } = &mut c.state
        else {
            return;
        };
        let bytes = buffers.recv.get(..*filled).unwrap_or_default();
        let full = *filled >= buffers.recv.len();
        let verdict = match parser.advance(bytes, &self.head_limits, request.kind) {
            Ok(ResponseStatus::Incomplete) if full => Verdict::Bad(ResponseError::HeadTooLarge),
            Ok(ResponseStatus::Incomplete) => {
                self.sync_interest(conn, actions);
                return;
            }
            // Interim heads are dropped from Task 7 on.
            Ok(ResponseStatus::Interim { .. }) => Verdict::NotImplemented,
            Ok(ResponseStatus::Complete { head, consumed }) => match head.body {
                ResponseBody::None => Verdict::Relay {
                    head,
                    consumed,
                    body: 0,
                },
                ResponseBody::Length(body) => Verdict::Relay {
                    head,
                    consumed,
                    body,
                },
                ResponseBody::Chunked | ResponseBody::UntilClose => Verdict::NotImplemented,
            },
            Err(error) => Verdict::Bad(error),
        };
        match verdict {
            Verdict::Relay {
                head,
                consumed,
                body,
            } => self.relay(now, conn, head, consumed, body, actions),
            Verdict::NotImplemented => self.not_implemented(now, conn, actions),
            Verdict::Bad(error) => {
                self.fail_before_head(now, conn, BackendFailure::BadResponse(error), actions);
            }
        }
    }

    /// Queue the rewritten response `head` (the first `consumed` bytes of
    /// the upstream's receive buffer) for the client, followed by `body`
    /// bytes, the first of which may already sit right after the head.
    fn relay(
        &mut self,
        now: Instant,
        conn: ConnId,
        head: ResponseHead,
        consumed: usize,
        body: u64,
        actions: &mut Vec<Action>,
    ) {
        let head_capacity = self.head_capacity;
        let send_idle_timeout = self.config.limits.send_idle_timeout;
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        // The persistence of the request, if its body was fully read (else
        // the backend answered early and the body stays unread, FR-PROXY-27);
        // a `Content-Length` or body-less response is not close delimited
        // (FR-PROXY-28).
        let persistence = match &c.state {
            ConnState::AwaitingResponse { request, .. } => match request.body.all_arrived() {
                Arrived::All => request.persistence,
                Arrived::Partly => Persistence::Close,
            },
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Connecting { .. }
            | ConnState::Relaying { .. }
            | ConnState::Lingering { .. } => return,
        };
        if client_response_len(&head, persistence) > head_capacity {
            let error = BackendFailure::BadResponse(ResponseError::HeadTooLarge);
            self.fail_before_head(now, conn, error, actions);
            return;
        }
        let Some((up, mut request, mut buffers, filled)) = take_awaiting(c) else {
            return;
        };
        // Body forwarding stops here; a fully read body leaves the next
        // request at the front.
        let buffered = match request.body.end(&mut c.recv) {
            BodyEnd::Read { buffered } => buffered,
            BodyEnd::Unread => 0,
        };
        actions.push(Action::Log(Log::ProxiedResponse {
            conn,
            peer: c.peer,
            backend: request.backend,
            status: head.status,
            request: request.summary,
        }));
        buffers.head.clear();
        render_client_response(&head, persistence, &mut buffers.head);
        let arrived = filled.saturating_sub(consumed);
        let first = usize::try_from(body).map_or(arrived, |b| b.min(arrived));
        let remaining = body.saturating_sub(first as u64);
        let source = if remaining == 0 {
            // Every body byte is here: anything after it is dropped.
            close_upstream(&mut self.upstreams, up, actions);
            Source::Done
        } else {
            Source::Open {
                up,
                backend: request.backend,
                remaining,
            }
        };
        // The rewritten head is for the client to take now.
        let deadline = relay_deadline(now, true, source, send_idle_timeout);
        c.state = ConnState::Relaying {
            source,
            buffers,
            start: consumed,
            end: consumed.saturating_add(first),
            persistence,
            buffered,
            deadline,
        };
        self.sync_interest(conn, actions);
    }

    /// The response has a form this task does not relay yet: 501 and close,
    /// and the upstream is closed.
    fn not_implemented(&mut self, now: Instant, conn: ConnId, actions: &mut Vec<Action>) {
        let deadline = now.saturating_add(self.config.limits.send_idle_timeout);
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let Some((up, request)) = take_request(c) else {
            return;
        };
        let status = Status::NotImplemented;
        // The exchange ends with `Connection: close` (Task 7 relays these).
        render_fixed(
            status,
            Persistence::Close,
            body_mode(&request.summary.method),
            &mut c.send,
        );
        actions.push(Action::Log(Log::Response {
            conn,
            peer: c.peer,
            status,
            request: Some(request.summary),
        }));
        close_upstream(&mut self.upstreams, up, actions);
        c.state = ConnState::Responding {
            persistence: Persistence::Close,
            unread_body: UnreadBody::None,
            buffered: 0,
            deadline,
        };
        self.sync_interest(conn, actions);
    }

    /// The backend failed before its response head was queued: log why,
    /// close the upstream and answer 502, or 504 for a timeout. The client
    /// stays as the request asked if its body was fully read, else it
    /// closes (FR-PROXY-26, -28).
    pub(super) fn fail_before_head(
        &mut self,
        now: Instant,
        conn: ConnId,
        kind: BackendFailure,
        actions: &mut Vec<Action>,
    ) {
        let deadline = now.saturating_add(self.config.limits.send_idle_timeout);
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let Some((up, mut request)) = take_request(c) else {
            return;
        };
        let (persistence, unread_body, buffered) = match request.body.end(&mut c.recv) {
            BodyEnd::Read { buffered } => (request.persistence, UnreadBody::None, buffered),
            BodyEnd::Unread => (Persistence::Close, UnreadBody::Pending, 0),
        };
        let status = match kind {
            BackendFailure::Refused | BackendFailure::Reset | BackendFailure::BadResponse(_) => {
                Status::BadGateway
            }
            BackendFailure::ConnectTimeout | BackendFailure::ResponseTimeout => {
                Status::GatewayTimeout
            }
        };
        actions.push(Action::Log(Log::BackendFailed {
            conn,
            peer: c.peer,
            backend: request.backend,
            kind,
        }));
        render_fixed(
            status,
            persistence,
            body_mode(&request.summary.method),
            &mut c.send,
        );
        actions.push(Action::Log(Log::Response {
            conn,
            peer: c.peer,
            status,
            request: Some(request.summary),
        }));
        close_upstream(&mut self.upstreams, up, actions);
        c.state = ConnState::Responding {
            persistence,
            unread_body,
            buffered,
            deadline,
        };
        self.sync_interest(conn, actions);
    }

    /// The backend failed after the response head was queued: log it and
    /// cut the connection (and close the upstream), so the client sees an
    /// incomplete response, never a short complete-looking one.
    fn cut(&mut self, conn: ConnId, kind: BackendFailure, actions: &mut Vec<Action>) {
        let Some(c) = self.conns.get(&conn) else {
            return;
        };
        let ConnState::Relaying {
            source: Source::Open { backend, .. },
            ..
        } = c.state
        else {
            return;
        };
        actions.push(Action::Log(Log::BackendFailed {
            conn,
            peer: c.peer,
            backend,
            kind,
        }));
        self.forget(conn, actions);
        actions.push(Action::Close { conn });
    }

    /// A relaying connection's deadline passed. Before the head was queued
    /// that is a 504; after it, the connection is cut: logged as a send
    /// timeout when the client had bytes to take, as a backend timeout when
    /// the body stalled on the backend's side.
    pub(super) fn expire_proxy(&mut self, now: Instant, conn: ConnId, actions: &mut Vec<Action>) {
        let Some(c) = self.conns.get(&conn) else {
            return;
        };
        match &c.state {
            ConnState::Connecting { .. } => {
                self.fail_before_head(now, conn, BackendFailure::ConnectTimeout, actions);
            }
            // A client that stalls its body is at fault (408); anything
            // else waits on the backend (504).
            ConnState::AwaitingResponse {
                request, buffers, ..
            } if buffers.head.is_empty() && request.body.stage() == Stage::Receiving => {
                self.body_stalled(now, conn, actions);
            }
            ConnState::AwaitingResponse { .. } => {
                self.fail_before_head(now, conn, BackendFailure::ResponseTimeout, actions);
            }
            ConnState::Relaying {
                source,
                buffers,
                start,
                end,
                ..
            } => {
                let client_stalled = !c.send.is_empty() || !buffers.head.is_empty() || start < end;
                match source {
                    Source::Open { .. } if !client_stalled => {
                        self.cut(conn, BackendFailure::ResponseTimeout, actions);
                    }
                    Source::Open { .. } | Source::Done => {
                        actions.push(Action::Log(Log::Rejected {
                            conn,
                            peer: c.peer,
                            reason: RejectReason::Timeout(TimeoutKind::Send),
                        }));
                        self.forget(conn, actions);
                        actions.push(Action::Close { conn });
                    }
                }
            }
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Lingering { .. } => {}
        }
    }

    /// The client's socket accepted `len` bytes of the relayed response:
    /// first of the connection's own send buffer (if it holds any), then of
    /// the response's head, then of the body bytes in the upstream's buffer.
    /// Once the buffer is drained the upstream is read again; once the
    /// whole response is out the exchange ends (keep-alive or close).
    pub(super) fn relay_sent(
        &mut self,
        now: Instant,
        conn: ConnId,
        len: usize,
        actions: &mut Vec<Action>,
    ) {
        let limits = &self.config.limits;
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let ConnState::Relaying {
            source,
            buffers,
            start,
            end,
            persistence,
            buffered,
            deadline,
        } = &mut c.state
        else {
            return;
        };
        let sent = if !c.send.is_empty() {
            let sent = len.min(c.send.len());
            c.send.drain(..sent);
            sent
        } else if buffers.head.is_empty() {
            let sent = len.min(end.saturating_sub(*start));
            *start = start.saturating_add(sent);
            sent
        } else {
            let sent = len.min(buffers.head.len());
            buffers.head.drain(..sent);
            sent
        };
        if sent == 0 {
            return;
        }
        let done = c.send.is_empty() && buffers.head.is_empty() && start >= end;
        *deadline = relay_deadline(now, !done, *source, limits.send_idle_timeout);
        match (*source, done) {
            (Source::Done, true) => {
                let (persistence, buffered) = (*persistence, *buffered);
                c.state = finish(
                    conn,
                    c,
                    AfterResponse {
                        persistence,
                        unread_body: UnreadBody::None,
                        buffered,
                    },
                    now,
                    limits,
                    actions,
                );
                self.try_parse(now, conn, actions);
            }
            (Source::Done, false) | (Source::Open { .. }, true | false) => {}
        }
        self.sync_interest(conn, actions);
    }
}

/// What the core answers itself to `Expect: 100-continue`.
const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

/// The deadline of a relaying connection after progress at `now`: the
/// client's `send_idle_timeout` while it has bytes to take (`pending`) or
/// nothing more is due from the backend, the body idle timeout while it
/// waits for the backend's next bytes (FR-PROXY-29).
fn relay_deadline(
    now: Instant,
    pending: bool,
    source: Source,
    send_idle_timeout: Duration,
) -> Instant {
    match (pending, source) {
        (true, Source::Open { .. } | Source::Done) | (false, Source::Done) => {
            now.saturating_add(send_idle_timeout)
        }
        (false, Source::Open { .. }) => now.saturating_add(BODY_IDLE_TIMEOUT),
    }
}

/// Take the parts of `c`'s `AwaitingResponse` state that relaying keeps
/// (upstream, request, buffers, filled bytes); the caller sets the next
/// state. Any other state stays.
fn take_awaiting(c: &mut Conn) -> Option<(UpstreamId, ProxyRequest, ExchangeBuffers, usize)> {
    match c.take_state() {
        ConnState::AwaitingResponse {
            up,
            request,
            buffers,
            filled,
            ..
        } => Some((up, request, buffers, filled)),
        other @ (ConnState::ReadingHead { .. }
        | ConnState::Idle { .. }
        | ConnState::Responding { .. }
        | ConnState::OpeningFile { .. }
        | ConnState::SendingFile { .. }
        | ConnState::Connecting { .. }
        | ConnState::Relaying { .. }
        | ConnState::Lingering { .. }) => {
            c.state = other;
            None
        }
    }
}

/// Take the request out of `c`'s `Connecting` or `AwaitingResponse` state,
/// with the upstream id; the caller sets the next state. Any other state
/// stays.
fn take_request(c: &mut Conn) -> Option<(UpstreamId, ProxyRequest)> {
    match c.take_state() {
        ConnState::Connecting { up, request, .. }
        | ConnState::AwaitingResponse { up, request, .. } => Some((up, request)),
        other @ (ConnState::ReadingHead { .. }
        | ConnState::Idle { .. }
        | ConnState::Responding { .. }
        | ConnState::OpeningFile { .. }
        | ConnState::SendingFile { .. }
        | ConnState::Relaying { .. }
        | ConnState::Lingering { .. }) => {
            c.state = other;
            None
        }
    }
}

#[cfg(test)]
mod tests;
