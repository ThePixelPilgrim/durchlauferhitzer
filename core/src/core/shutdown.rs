//! Shutdown (FR-PROC-05, stage 4 amendment A4): the graceful drain, the
//! fast path, and the 503 that answers work arriving during the drain.

use super::{Action, ConnId, Core, Mode};
use crate::conn::{ConnState, UnreadBody};
use crate::http::request::{BodyFraming, Persistence, RequestHead};
use crate::http::response::{BodyMode, render_fixed};
use crate::http::status::Status;
use crate::log::{Log, RequestSummary};
use crate::time::Instant;

impl Core {
    /// Start the graceful drain. The driver calls it once, on the first
    /// SIGTERM, after it closed its listening sockets, between two
    /// [`Core::handle`] calls. From then on:
    ///
    /// - idle keep-alive connections, and connections that have not
    ///   received a byte of their request yet, are closed at once (in this
    ///   call: `Close`, no log entry, as for a keep-alive or header timeout
    ///   that ends without bytes);
    /// - a request head that is partly received completes as usual (within
    ///   its header deadline) and is then answered `503 Service
    ///   Unavailable` with `Connection: close` (logged as its
    ///   [`Log::Response`]), as is every request completed later;
    /// - every exchange in flight (a fixed response, a static file, a
    ///   proxied request) finishes, with `Connection: close` in any head
    ///   rendered from now on, and then its connection takes the normal
    ///   close path (shutdown of the sending side, lingering close); input
    ///   pipelined behind it is not answered;
    /// - a connection delivered by `Accepted` anyway is answered 503 and
    ///   closed like one over the limits, logged as
    ///   [`RejectReason::ShuttingDown`](crate::log::RejectReason::ShuttingDown).
    ///
    /// It emits only `Close` (with the `CloseFile` or `CloseUpstream` a
    /// closing connection never holds). [`Core::handle`] keeps working as
    /// before; the driver keeps running its loop until
    /// [`Core::is_drained`] or `shutdown_timeout`, then calls
    /// [`Core::abort_all`]. Deadlines are unchanged, so `now` is not
    /// needed yet; it is taken for symmetry with `handle`.
    pub fn begin_drain(&mut self, _now: Instant, actions: &mut Vec<Action>) {
        self.mode = Mode::Draining;
        let ids: Vec<ConnId> = self.conns.keys().copied().collect();
        for conn in ids {
            let Some(c) = self.conns.get_mut(&conn) else {
                continue;
            };
            match &mut c.state {
                ConnState::Idle { .. } | ConnState::ReadingHead { buffered: 0, .. } => {
                    self.forget(conn, actions);
                    actions.push(Action::Close { conn });
                }
                // A partly received head completes, then gets 503; a
                // lingering connection is already closing.
                ConnState::ReadingHead { .. } | ConnState::Lingering { .. } => {}
                ConnState::Responding { persistence, .. }
                | ConnState::OpeningFile { persistence, .. }
                | ConnState::SendingFile { persistence, .. }
                | ConnState::Relaying { persistence, .. } => *persistence = Persistence::Close,
                ConnState::Connecting { request, .. }
                | ConnState::AwaitingResponse { request, .. } => {
                    request.persistence = Persistence::Close;
                }
            }
        }
    }

    /// The fast path: close every connection at once, each open upstream
    /// and each open file exactly once (per connection, in `ConnId` order:
    /// `CloseFile`, `CloseUpstream`, `Close`). The driver calls it on a
    /// second SIGTERM, a SIGINT or when `shutdown_timeout` expires, with
    /// or without a drain before it, between two [`Core::handle`] calls.
    ///
    /// It emits no log entries: the cut exchanges are the shutdown the
    /// operator asked for, not failures, and the driver logs the fast
    /// shutdown once itself. An `OpenFile` still unanswered becomes an
    /// orphaned open, as for any connection that closes: its `FileOpened`
    /// is answered with `CloseFile` when it comes, and
    /// [`Core::is_drained`] stays false until then. The core is draining
    /// afterwards, and `handle` keeps working (a stray `Accepted` is
    /// answered 503).
    pub fn abort_all(&mut self, actions: &mut Vec<Action>) {
        self.mode = Mode::Draining;
        let ids: Vec<ConnId> = self.conns.keys().copied().collect();
        for conn in ids {
            if self.forget(conn, actions).is_some() {
                actions.push(Action::Close { conn });
            }
        }
    }

    /// Whether the core holds nothing any more: no connection (lingering
    /// ones included), no open upstream, no open file and no orphaned open.
    /// Once draining, nothing new is opened, so it stays true; the driver
    /// may then exit.
    pub fn is_drained(&self) -> bool {
        self.conns.is_empty() && self.upstreams.is_empty() && self.files.is_empty()
    }

    /// While draining, a complete request `head` on `conn` is answered 503
    /// with `Connection: close` (FR-PROC-05 "target shutdown"), and logged
    /// as its response; a body it announced is left unread.
    pub(super) fn refuse_during_drain(
        &mut self,
        now: Instant,
        conn: ConnId,
        head: RequestHead,
        actions: &mut Vec<Action>,
    ) {
        let deadline = now.saturating_add(self.config.limits.send_idle_timeout);
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let unread_body = match head.body {
            BodyFraming::None => UnreadBody::None,
            BodyFraming::Length(_) | BodyFraming::Chunked => UnreadBody::Pending,
        };
        let status = Status::ServiceUnavailable;
        let body = BodyMode::of(&head.method);
        render_fixed(
            status,
            Persistence::Close,
            body,
            self.date.as_ref(),
            &mut c.send,
        );
        actions.push(Action::Log(Log::Response {
            conn,
            peer: c.peer,
            status,
            request: Some(RequestSummary {
                method: head.method,
                host: head.host,
                path: head.target.path,
            }),
        }));
        c.state = ConnState::Responding {
            persistence: Persistence::Close,
            unread_body,
            buffered: 0,
            deadline,
        };
    }
}
