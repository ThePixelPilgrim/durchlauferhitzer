//! The response side of a proxied exchange once its head is judged:
//! queueing the rewritten head, then relaying the body to the client
//! straight out of the upstream's receive buffer, fill, drain, refill
//! (FR-PROXY-19…22, -24, -28; NFR-RES-04), and the failures after the head
//! (FR-PROXY-26: the connection is cut).

use std::time::Duration;

use super::proxy::{BODY_IDLE_TIMEOUT, close_upstream};
use super::request::{AfterResponse, finish};
use super::{Action, ConnId, Core};
use crate::conn::{Conn, ConnState, ProxyRequest, UnreadBody};
use crate::http::forward::{client_response_len, render_client_response};
use crate::http::request::{Persistence, Version};
use crate::http::response_head::{ResponseBody, ResponseError, ResponseHead};
use crate::log::{BackendFailure, Log};
use crate::relay_body::{Closing, RelayBody};
use crate::request_body::{Arrived, BodyEnd};
use crate::time::Instant;
use crate::upstream::{ExchangeBuffers, UpstreamId};

impl Core {
    /// Queue the rewritten response `head` (the first `consumed` bytes of
    /// the upstream's receive buffer) for the client, followed by its body,
    /// the first bytes of which may already sit right after the head. Those
    /// are judged first: a framing error or a body over the limit is still
    /// a 502, as is any `Transfer-Encoding` for an HTTP/1.0 client
    /// (FR-PROXY-04).
    pub(super) fn relay(
        &mut self,
        now: Instant,
        conn: ConnId,
        head: ResponseHead,
        consumed: usize,
        actions: &mut Vec<Action>,
    ) {
        let head_capacity = self.head_capacity;
        let send_idle_timeout = self.config.limits.send_idle_timeout;
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let ConnState::AwaitingResponse {
            up,
            request,
            buffers,
            filled,
            ..
        } = &mut c.state
        else {
            return;
        };
        // The persistence of the request, if its body was fully read (else
        // the backend answered early and the body stays unread, FR-PROXY-27);
        // a close-delimited response closes the client (FR-PROXY-28).
        let persistence = match (request.body.all_arrived(), head.body) {
            (Arrived::Partly, _) | (Arrived::All, ResponseBody::UntilClose) => Persistence::Close,
            (
                Arrived::All,
                ResponseBody::None | ResponseBody::Length(_) | ResponseBody::Chunked,
            ) => request.persistence,
        };
        // An HTTP/1.0 client knows no `Transfer-Encoding`: a head naming
        // it is refused whether or not a body follows (owner decision
        // 2026-10-27).
        let checked = match request.version {
            Version::Http10 if head.names_transfer_encoding() => {
                Err(BackendFailure::TransferEncodingToHttp10)
            }
            Version::Http10 | Version::Http11
                if client_response_len(&head, persistence) > head_capacity =>
            {
                Err(BackendFailure::BadResponse(ResponseError::HeadTooLarge))
            }
            Version::Http10 | Version::Http11 => RelayBody::new(
                head.body,
                *up,
                request.backend,
                request.response_limit,
                consumed,
                *filled,
                &mut buffers.recv,
            ),
        };
        let (body, closing) = match checked {
            Ok(relayed) => relayed,
            Err(kind) => {
                self.fail_before_head(now, conn, kind, actions);
                return;
            }
        };
        let Some((mut request, mut buffers)) = take_awaiting(c) else {
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
        match closing {
            // Every body byte is here: anything after it is dropped.
            Closing::Upstream(up) => close_upstream(&mut self.upstreams, up, actions),
            Closing::No => {}
        }
        // The rewritten head is for the client to take now.
        c.state = ConnState::Relaying {
            deadline: relay_deadline(now, true, &body, send_idle_timeout),
            body,
            buffers,
            persistence,
            buffered,
        };
        self.sync_interest(conn, actions);
    }

    /// The driver read `len` bytes of the body from `conn`'s upstream:
    /// judge them, close the upstream once the body is complete, cut both
    /// connections on a framing error or a body over the limit.
    pub(super) fn relay_received(
        &mut self,
        now: Instant,
        conn: ConnId,
        len: usize,
        actions: &mut Vec<Action>,
    ) {
        let send_idle_timeout = self.config.limits.send_idle_timeout;
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let ConnState::Relaying {
            body,
            buffers,
            deadline,
            ..
        } = &mut c.state
        else {
            return;
        };
        if body.spare(&mut buffers.recv).is_empty() {
            return;
        }
        match body.received(len, &mut buffers.recv) {
            Ok(Closing::Upstream(up)) => close_upstream(&mut self.upstreams, up, actions),
            Ok(Closing::No) => {}
            Err(kind) => {
                self.cut(conn, kind, actions);
                return;
            }
        }
        // Progress either way: bytes for the client, or a chunked tail
        // that grew.
        *deadline = relay_deadline(now, body.unsent_len() > 0, body, send_idle_timeout);
        self.sync_interest(conn, actions);
    }

    /// The backend of `up` closed its sending side while a response was
    /// relayed: that ends a close-delimited body (the exchange finishes
    /// once the client has every byte). Whether it did; if not, the caller
    /// treats the end of stream as a failure.
    pub(super) fn relay_read_closed(
        &mut self,
        now: Instant,
        up: UpstreamId,
        actions: &mut Vec<Action>,
    ) -> bool {
        let send_idle_timeout = self.config.limits.send_idle_timeout;
        let Some(conn) = self.upstreams.conn(up) else {
            return false;
        };
        let Some(c) = self.conns.get_mut(&conn) else {
            return false;
        };
        let ConnState::Relaying { body, deadline, .. } = &mut c.state else {
            return false;
        };
        // End of stream is only learnt from a read, offered once the
        // buffer was drained.
        if body.upstream() != Some(up) || body.unsent_len() > 0 {
            return false;
        }
        let Some(up) = body.end_of_stream() else {
            return false;
        };
        close_upstream(&mut self.upstreams, up, actions);
        let pending = !c.send.is_empty();
        *deadline = relay_deadline(now, pending, body, send_idle_timeout);
        self.finish_relay(now, conn, actions);
        self.sync_interest(conn, actions);
        true
    }

    /// The backend failed after the response head was queued: log it and
    /// cut the connection (and close the upstream), so the client sees an
    /// incomplete response, never a short complete-looking one. No
    /// lingering close: the order is `Log`, `CloseUpstream`, `Close`.
    pub(super) fn cut(&mut self, conn: ConnId, kind: BackendFailure, actions: &mut Vec<Action>) {
        let Some(c) = self.conns.get(&conn) else {
            return;
        };
        let ConnState::Relaying { body, .. } = &c.state else {
            return;
        };
        let Some(backend) = body.backend() else {
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
        let send_idle_timeout = self.config.limits.send_idle_timeout;
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let ConnState::Relaying {
            body,
            buffers,
            deadline,
            ..
        } = &mut c.state
        else {
            return;
        };
        let sent = if !c.send.is_empty() {
            let sent = len.min(c.send.len());
            c.send.drain(..sent);
            sent
        } else if buffers.head.is_empty() {
            body.sent(len, &mut buffers.recv)
        } else {
            let sent = len.min(buffers.head.len());
            buffers.head.drain(..sent);
            sent
        };
        if sent == 0 {
            return;
        }
        let pending = !c.send.is_empty() || !buffers.head.is_empty() || body.unsent_len() > 0;
        *deadline = relay_deadline(now, pending, body, send_idle_timeout);
        self.finish_relay(now, conn, actions);
        self.sync_interest(conn, actions);
    }

    /// If every byte of a complete response is out, end the exchange: keep
    /// the client for its next request or close it (FR-PROXY-28).
    fn finish_relay(&mut self, now: Instant, conn: ConnId, actions: &mut Vec<Action>) {
        let limits = &self.config.limits;
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let ConnState::Relaying {
            body,
            buffers,
            persistence,
            buffered,
            ..
        } = &c.state
        else {
            return;
        };
        let done = c.send.is_empty() && buffers.head.is_empty() && body.unsent_len() == 0;
        if !done || body.upstream().is_some() {
            return;
        }
        let after = AfterResponse {
            persistence: *persistence,
            unread_body: UnreadBody::None,
            buffered: *buffered,
        };
        c.state = finish(conn, c, after, now, limits, actions);
        self.try_parse(now, conn, actions);
    }
}

/// The deadline of a relaying connection after progress at `now`: the
/// client's `send_idle_timeout` while it has bytes to take (`pending`) or
/// nothing more is due from the backend, the body idle timeout while it
/// waits for the backend's next bytes (FR-PROXY-29).
fn relay_deadline(
    now: Instant,
    pending: bool,
    body: &RelayBody,
    send_idle_timeout: Duration,
) -> Instant {
    match (pending, body.upstream()) {
        (true, _) | (false, None) => now.saturating_add(send_idle_timeout),
        (false, Some(_)) => now.saturating_add(BODY_IDLE_TIMEOUT),
    }
}

/// Take the parts of `c`'s `AwaitingResponse` state that relaying keeps
/// (request, buffers); the caller sets the next state. Any other state
/// stays.
fn take_awaiting(c: &mut Conn) -> Option<(ProxyRequest, ExchangeBuffers)> {
    match c.take_state() {
        ConnState::AwaitingResponse {
            request, buffers, ..
        } => Some((request, buffers)),
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
