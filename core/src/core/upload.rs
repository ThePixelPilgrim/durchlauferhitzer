//! The request side of a proxied exchange once connected: writing the
//! request head and body to the backend, reading the body from the client
//! fill, drain, refill (NFR-RES-04), its framing and limit errors
//! (FR-PROXY-19, -23) and a client that stalls it (FR-PROXY-26).

use super::proxy::body_mode;
use super::{Action, ConnId, Core};
use crate::config::Limits;
use crate::conn::{ConnState, UnreadBody};
use crate::http::request::{Method, Persistence};
use crate::http::response::render_fixed;
use crate::http::status::Status;
use crate::log::{Log, RejectReason, RequestBodyError, TimeoutKind};
use crate::request_body::{RequestBody, Stage};
use crate::time::Instant;
use crate::upstream::UpstreamId;

impl Core {
    /// The socket accepted `len` bytes of the request head, or once that
    /// is written, of the request body.
    pub(super) fn upstream_sent(
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
        let ConnState::AwaitingResponse {
            request,
            buffers,
            head_deadline,
            deadline,
            ..
        } = &mut c.state
        else {
            return;
        };
        let sent = if buffers.head.is_empty() {
            request.body.sent(len, &mut c.recv)
        } else {
            let sent = len.min(buffers.head.len());
            buffers.head.drain(..sent);
            sent
        };
        if sent == 0 {
            return;
        }
        if buffers.head.is_empty() {
            *deadline = awaiting_deadline(now, &request.body, *head_deadline, limits);
        }
        self.sync_interest(conn, actions);
    }

    /// The driver read `len` more body bytes into the client's buffer.
    pub(super) fn body_received(
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
        let ConnState::AwaitingResponse {
            request,
            buffers,
            head_deadline,
            deadline,
            ..
        } = &mut c.state
        else {
            return;
        };
        // Only what was offered can have arrived (nothing while the head is
        // still being written).
        if !buffers.head.is_empty() {
            return;
        }
        match request.body.received(len, &mut c.recv) {
            Ok(0) => {}
            Ok(_) => {
                *deadline = awaiting_deadline(now, &request.body, *head_deadline, limits);
                self.sync_interest(conn, actions);
            }
            Err(error) => self.cut_body(conn, error, actions),
        }
    }

    /// The body that came with a request head is invalid or too large,
    /// before anything was forwarded: a framing error is answered 400 and
    /// closed; a body over the limit cuts the connection, as it would later
    /// (FR-PROXY-23).
    pub(super) fn reject_body(
        &mut self,
        now: Instant,
        conn: ConnId,
        method: &Method,
        error: RequestBodyError,
        actions: &mut Vec<Action>,
    ) {
        let send_deadline = now.saturating_add(self.config.limits.send_idle_timeout);
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        match error {
            RequestBodyError::Chunked(_) => {
                let body = body_mode(method);
                render_fixed(Status::BadRequest, Persistence::Close, body, &mut c.send);
                actions.push(Action::Log(Log::Rejected {
                    conn,
                    peer: c.peer,
                    reason: RejectReason::Body(error),
                }));
                c.state = ConnState::Responding {
                    persistence: Persistence::Close,
                    unread_body: UnreadBody::Pending,
                    buffered: 0,
                    deadline: send_deadline,
                };
            }
            RequestBodyError::TooLarge => self.cut_body(conn, error, actions),
        }
    }

    /// The request body broke a rule after its head was forwarded: nothing
    /// can be answered any more, so both connections are cut (logged, then
    /// `CloseUpstream`, then `Close`).
    fn cut_body(&mut self, conn: ConnId, error: RequestBodyError, actions: &mut Vec<Action>) {
        let Some(c) = self.conns.get(&conn) else {
            return;
        };
        actions.push(Action::Log(Log::Rejected {
            conn,
            peer: c.peer,
            reason: RejectReason::Body(error),
        }));
        self.forget(conn, actions);
        actions.push(Action::Close { conn });
    }

    /// The body idle timeout passed while the core waited for the client's
    /// next body bytes, before the response head: the client is at fault,
    /// so it gets a 408 and closes; the upstream is closed (FR-PROXY-26).
    pub(super) fn body_stalled(&mut self, now: Instant, conn: ConnId, actions: &mut Vec<Action>) {
        let send_deadline = now.saturating_add(self.config.limits.send_idle_timeout);
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let ConnState::AwaitingResponse { up, request, .. } = &c.state else {
            return;
        };
        let (up, body) = (*up, body_mode(&request.summary.method));
        render_fixed(
            Status::RequestTimeout,
            Persistence::Close,
            body,
            &mut c.send,
        );
        actions.push(Action::Log(Log::Rejected {
            conn,
            peer: c.peer,
            reason: RejectReason::Timeout(TimeoutKind::RequestBody),
        }));
        super::proxy::close_upstream(&mut self.upstreams, up, actions);
        c.state = ConnState::Responding {
            persistence: Persistence::Close,
            unread_body: UnreadBody::Pending,
            buffered: 0,
            deadline: send_deadline,
        };
        self.sync_interest(conn, actions);
    }
}

/// The deadline of a connection awaiting its response once the request
/// head was written, after progress at `now`: the body idle timeout while
/// the body streams, the response timeout once the whole request is
/// written, but never later than the response head's total deadline
/// `head_deadline`, if its first byte came (FR-PROXY-29).
pub(super) fn awaiting_deadline(
    now: Instant,
    body: &RequestBody,
    head_deadline: Option<Instant>,
    limits: &Limits,
) -> Instant {
    let deadline = match body.stage() {
        Stage::Receiving | Stage::Writing => now.saturating_add(limits.body_idle_timeout),
        Stage::Written => now.saturating_add(limits.backend_response_timeout),
    };
    head_deadline.map_or(deadline, |cap| deadline.min(cap))
}
