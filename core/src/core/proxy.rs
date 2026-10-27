//! Proxy routes: the rules decided before connecting, connecting to the
//! backend, reading and judging the response head (interim heads dropped,
//! FR-PROXY-18), failures before it (502/504) and timeouts. The request
//! side once connected (writing the head and body to the backend) is in
//! `upload.rs`; relaying the response head and body to the client is in
//! `relay.rs`.

use super::request::HeadEnd;
use super::{Action, ConnId, Core};
use crate::config::{BackendAddr, BodyLimit, Limits, ProxyRoute, Route, StripPrefix};
use crate::conn::{Conn, ConnState, ProxyRequest, UnreadBody};
use crate::http::forward::{backend_request_len, check_forwardable, render_backend_request};
use crate::http::request::{BodyFraming, Expect, Method, Persistence, RequestHead};
use crate::http::response::{BodyMode, render_fixed};
use crate::http::response_head::{
    MAX_INTERIM_HEADS, RequestKind, ResponseError, ResponseHead, ResponseParser, ResponseStatus,
};
use crate::http::status::Status;
use crate::log::{BackendFailure, Log, RejectReason, RequestSummary, TimeoutKind};
use crate::request_body::{BodyEnd, RequestBody, Stage};
use crate::routing::strip_segments;
use crate::time::Instant;
use crate::upstream::{BackendId, ExchangeBuffers, UpstreamId, UpstreamTable};

/// How a request on a proxy route is answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ProxyPlan {
    /// Connect to the backend and forward the request.
    Forward(Forward),
    /// Answer `status` right away.
    Respond(Status, Persistence),
}

/// Forward a request to `backend` with `path` as its (possibly stripped)
/// raw path; a chunked body may carry at most `limit` payload bytes, the
/// response body at most `response_limit` bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Forward {
    backend: BackendId,
    path: String,
    limit: BodyLimit,
    response_limit: BodyLimit,
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
        response_limit: target.max_response_body,
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
        let limits = &self.config.limits;
        let send_deadline = now.saturating_add(limits.send_idle_timeout);
        let connect_deadline = now.saturating_add(limits.backend_connect_timeout);
        let (recv_capacity, head_capacity) = (self.upstream_recv_capacity, self.head_capacity);
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let Forward {
            backend,
            path,
            limit,
            response_limit,
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
                version: head.version,
                persistence: head.persistence,
                response_limit,
                body,
                expect,
            },
            buffers,
            deadline: connect_deadline,
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

pub(super) fn body_mode(method: &Method) -> BodyMode {
    match method {
        Method::Head => BodyMode::HeadOnly,
        Method::Get | Method::Other(_) => BodyMode::Full,
    }
}

/// How the response head the backend sent is answered.
enum Verdict {
    /// Relay it: a head of `consumed` bytes, its body (if any) after it.
    Relay {
        head: ResponseHead,
        consumed: usize,
    },
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
        let response_deadline = now.saturating_add(self.config.limits.backend_response_timeout);
        let send_deadline = now.saturating_add(self.config.limits.send_idle_timeout);
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
                // The client's send idle timeout runs while it has the
                // `100 Continue` to take (FR-PROXY-29).
                let send_deadline = match request.expect {
                    Expect::Continue => {
                        c.send.extend_from_slice(CONTINUE);
                        Some(send_deadline)
                    }
                    Expect::None => None,
                };
                ConnState::AwaitingResponse {
                    up,
                    request,
                    buffers,
                    filled: 0,
                    parser: ResponseParser::new(),
                    interim: 0,
                    head_deadline: None,
                    send_deadline,
                    deadline: response_deadline,
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
    /// 502 (`Refused` while connecting, `Reset` after), after it the
    /// connection is cut (`Reset`).
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

    /// The backend closed its sending side: the end of a close-delimited
    /// body; otherwise it closed before its response was complete, a 502
    /// before the response head and a cut after it, logged as
    /// [`BackendFailure::ClosedEarly`]. (After any other complete body the
    /// upstream is already closed and no event comes; while connecting no
    /// read is offered, so none can come then either.)
    pub(super) fn upstream_read_closed(
        &mut self,
        now: Instant,
        up: UpstreamId,
        actions: &mut Vec<Action>,
    ) {
        if self.relay_read_closed(now, up, actions) {
            return;
        }
        let Some(conn) = self.upstreams.conn(up) else {
            return;
        };
        let Some(c) = self.conns.get(&conn) else {
            return;
        };
        let kind = BackendFailure::ClosedEarly;
        match c.state {
            ConnState::AwaitingResponse { .. } => self.fail_before_head(now, conn, kind, actions),
            ConnState::Relaying { .. } => self.cut(conn, kind, actions),
            ConnState::Connecting { .. }
            | ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Lingering { .. } => {}
        }
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
        let limits = &self.config.limits;
        let Some(conn) = self.upstreams.conn(up) else {
            return;
        };
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        // Only what was offered can have arrived: clamp to it.
        if !c.state.reads_upstream(up) {
            return;
        }
        match &mut c.state {
            ConnState::AwaitingResponse {
                request,
                buffers,
                filled,
                head_deadline,
                deadline,
                ..
            } => {
                let added = len.min(buffers.recv.len().saturating_sub(*filled));
                if added == 0 {
                    return;
                }
                *filled = filled.saturating_add(added);
                // A head's first byte starts its deadline (FR-PROXY-29);
                // later bytes of the same head do not move it.
                head_deadline
                    .get_or_insert_with(|| now.saturating_add(limits.backend_response_timeout));
                // While the body streams, backend bytes are progress too
                // (FR-PROXY-29: no progress in either direction); once the
                // whole request is written, the response timeout is a total
                // deadline and runs on.
                match request.body.stage() {
                    Stage::Receiving | Stage::Writing => {
                        *deadline = now.saturating_add(limits.body_idle_timeout);
                    }
                    Stage::Written => {}
                }
                self.judge_head(now, conn, actions);
            }
            ConnState::Relaying { .. } => self.relay_received(now, conn, len, actions),
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Connecting { .. }
            | ConnState::Lingering { .. } => {}
        }
    }

    /// Parse the response head bytes of `conn`'s upstream so far and act
    /// on a verdict. An interim (1xx) head is dropped: the bytes after it
    /// move to the front, once, as after a request head, and are parsed
    /// afresh; more than [`MAX_INTERIM_HEADS`] of them are a 502
    /// (FR-PROXY-18). The connection's deadline runs on (an interim head
    /// is not the response head), but a completed interim head ends its
    /// own head deadline; bytes behind it start the next head's
    /// (FR-PROXY-29).
    fn judge_head(&mut self, now: Instant, conn: ConnId, actions: &mut Vec<Action>) {
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let ConnState::AwaitingResponse {
            request,
            buffers,
            filled,
            parser,
            interim,
            head_deadline,
            ..
        } = &mut c.state
        else {
            return;
        };
        let response_timeout = self.config.limits.backend_response_timeout;
        let verdict = loop {
            let bytes = buffers.recv.get(..*filled).unwrap_or_default();
            let full = *filled >= buffers.recv.len();
            match parser.advance(bytes, &self.head_limits, request.kind) {
                Ok(ResponseStatus::Incomplete) if full => {
                    break Verdict::Bad(ResponseError::HeadTooLarge);
                }
                Ok(ResponseStatus::Incomplete) => {
                    self.sync_interest(conn, actions);
                    return;
                }
                Ok(ResponseStatus::Interim { .. }) if *interim >= MAX_INTERIM_HEADS => {
                    break Verdict::Bad(ResponseError::TooManyInterimHeads);
                }
                Ok(ResponseStatus::Interim { consumed }) => {
                    *interim = interim.saturating_add(1);
                    let consumed = consumed.min(*filled);
                    buffers.recv.copy_within(consumed..*filled, 0);
                    *filled = filled.saturating_sub(consumed);
                    *parser = ResponseParser::new();
                    // The interim head is complete: its deadline ends, and
                    // bytes behind it start the next head's (FR-PROXY-29).
                    *head_deadline = (*filled > 0).then(|| now.saturating_add(response_timeout));
                }
                Ok(ResponseStatus::Complete { head, consumed }) => {
                    break Verdict::Relay { head, consumed };
                }
                Err(error) => break Verdict::Bad(error),
            }
        };
        match verdict {
            Verdict::Relay { head, consumed } => self.relay(now, conn, head, consumed, actions),
            Verdict::Bad(error) => {
                self.fail_before_head(now, conn, BackendFailure::BadResponse(error), actions);
            }
        }
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
        let limits = &self.config.limits;
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let deadline = queued_send_deadline(c.state.unread_continue(), now, limits);
        let Some((up, mut request)) = take_request(c) else {
            return;
        };
        let (persistence, unread_body, buffered) = match request.body.end(&mut c.recv) {
            BodyEnd::Read { buffered } => (request.persistence, UnreadBody::None, buffered),
            BodyEnd::Unread => (Persistence::Close, UnreadBody::Pending, 0),
        };
        let status = match kind {
            BackendFailure::Refused
            | BackendFailure::Reset
            | BackendFailure::ClosedEarly
            | BackendFailure::BadResponse(_)
            | BackendFailure::BadChunk(_)
            | BackendFailure::BodyTooLarge
            | BackendFailure::TransferEncodingToHttp10 => Status::BadGateway,
            BackendFailure::ConnectTimeout
            | BackendFailure::ResponseTimeout
            | BackendFailure::BodyTimeout => Status::GatewayTimeout,
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

    /// A relaying connection's deadline passed. Before the head was queued
    /// that is a 504 (a 408 when the client stalled its body, unless the
    /// current response head's deadline passed), or a send timeout when the
    /// client took nothing of its `100 Continue`; after it, the connection
    /// is cut: logged as a send timeout when the client had bytes to take,
    /// as a backend timeout when the body stalled on the backend's side.
    pub(super) fn expire_proxy(&mut self, now: Instant, conn: ConnId, actions: &mut Vec<Action>) {
        let Some(c) = self.conns.get(&conn) else {
            return;
        };
        match &c.state {
            ConnState::Connecting { .. } => {
                self.fail_before_head(now, conn, BackendFailure::ConnectTimeout, actions);
            }
            // The client took nothing of its `100 Continue` for its send
            // idle timeout, which still applies (FR-PROXY-29): it is cut,
            // as a client that stops reading is, and the upstream released.
            // Checked first: a client that is gone cannot be answered.
            ConnState::AwaitingResponse {
                send_deadline: Some(send),
                ..
            } if now >= *send => {
                actions.push(Action::Log(Log::Rejected {
                    conn,
                    peer: c.peer,
                    reason: RejectReason::Timeout(TimeoutKind::Send),
                }));
                self.forget(conn, actions);
                actions.push(Action::Close { conn });
            }
            // The backend did not finish a response head (interim or final)
            // in time after its first byte, even if the body streams
            // (FR-PROXY-29).
            ConnState::AwaitingResponse {
                head_deadline: Some(cap),
                ..
            } if now >= *cap => {
                self.fail_before_head(now, conn, BackendFailure::ResponseTimeout, actions);
            }
            // A client that stalls its body is at fault (408); anything
            // else waits on the backend (504).
            ConnState::AwaitingResponse {
                request, buffers, ..
            } if buffers.head.is_empty() && request.body.stage() == Stage::Receiving => {
                self.body_stalled(now, conn, actions);
            }
            // Body bytes waited for a backend that stopped taking them.
            ConnState::AwaitingResponse {
                request, buffers, ..
            } if buffers.head.is_empty() && request.body.stage() == Stage::Writing => {
                self.fail_before_head(now, conn, BackendFailure::BodyTimeout, actions);
            }
            ConnState::AwaitingResponse { .. } => {
                self.fail_before_head(now, conn, BackendFailure::ResponseTimeout, actions);
            }
            ConnState::Relaying { body, buffers, .. } => {
                let client_stalled =
                    !c.send.is_empty() || !buffers.head.is_empty() || body.unsent_len() > 0;
                match body.upstream() {
                    Some(_) if !client_stalled => {
                        self.cut(conn, BackendFailure::ResponseTimeout, actions);
                    }
                    Some(_) | None => {
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
}

/// The send idle deadline of a response (a 408, 502 or 504, or a proxied
/// response) queued at `now` on a connection that awaited its response.
/// Behind an unread `100 Continue` the deadline the `100` started runs on:
/// it restarts only when the client takes a byte (FR-PROXY-25 carry-over).
/// Otherwise it starts now.
pub(super) fn queued_send_deadline(
    unread_continue: Option<Instant>,
    now: Instant,
    limits: &Limits,
) -> Instant {
    unread_continue.unwrap_or_else(|| now.saturating_add(limits.send_idle_timeout))
}

/// What the core answers itself to `Expect: 100-continue`.
const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

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
