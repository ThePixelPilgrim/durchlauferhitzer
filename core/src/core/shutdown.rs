//! Shutdown (FR-PROC-05, stage 4 amendment A4, with the owner decisions
//! of 2026-10-27 and 2026-10-29 in FR-PROC-05b to 05d): the graceful drain
//! for a shutdown or a restart (FR-PROC-07), the fast path, and the 503
//! that answers work arriving during a shutdown's drain.

use super::{Action, ConnId, Core, Mode};
use crate::conn::{Conn, ConnState, Counted, UnreadBody};
use crate::http::request::{BodyFraming, Persistence, RequestHead};
use crate::http::response::{BodyMode, render_fixed};
use crate::http::status::Status;
use crate::log::{Log, RejectReason, RequestSummary};
use crate::time::Instant;

/// What a drain prepares for (FR-PROC-05): the process exits
/// (`Shutdown`, SIGTERM), or a new process has taken over the listening
/// ports (`Restart`, SIGHUP, FR-PROC-07).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainTarget {
    /// Work arriving during the drain is refused with 503.
    Shutdown,
    /// What the process holds is served, with `Connection: close`.
    Restart,
}

impl Core {
    /// Start the graceful drain for `target`. The driver calls it once,
    /// after it closed its listening sockets, between two
    /// [`Core::handle`] calls: on the first SIGTERM for
    /// [`DrainTarget::Shutdown`], on SIGHUP for [`DrainTarget::Restart`].
    ///
    /// For a shutdown (FR-PROC-05), from then on:
    ///
    /// - idle keep-alive connections, and connections that have not
    ///   received a byte of their request yet, are closed at once (in this
    ///   call: `Close`, no log entry, as for a keep-alive or header timeout
    ///   that ends without bytes);
    /// - a request head that is partly received completes as usual (within
    ///   its header deadline) and is then answered `503 Service
    ///   Unavailable` with `Connection: close` (logged as its
    ///   [`Log::Response`]), as is every request completed later; a head
    ///   the parser rejects is no request and keeps its own error, with
    ///   `Connection: close` (owner decision 2026-10-29, FR-PROC-05b);
    /// - every exchange in flight (a fixed response, a static file, a
    ///   proxied request) finishes. A head already queued keeps what it
    ///   said; a head rendered from now on says `Connection: close`. Then
    ///   the connection takes the normal close path (shutdown of the
    ///   sending side, lingering close), unless a request was pipelined
    ///   behind a head queued keep-alive before the drain: once complete,
    ///   that request is answered 503 with `Connection: close` like the
    ///   partly received head above (FR-PROC-05b, owner decision
    ///   2026-10-27). Behind a head rendered during the drain (a file
    ///   being opened, a backend being connected to or awaited), which
    ///   says `Connection: close`, it is not answered (owner decision
    ///   2026-10-29, FR-PROC-05b);
    /// - a connection delivered by `Accepted` anyway is answered 503 and
    ///   closed like one over the limits, logged as
    ///   [`RejectReason::ShuttingDown`].
    ///
    /// For a restart (FR-PROC-07), nothing is refused: every request
    /// completed from then on (a partly received head, one pipelined
    /// behind a queued head, the next one on an idle keep-alive
    /// connection, the first one on a connection delivered by `Accepted`)
    /// is routed and answered as usual, its head saying
    /// `Connection: close`; a head the parser rejects keeps its own error.
    /// Idle connections and those without a byte of their request stay:
    /// each ends with its next request or at its keep-alive or header
    /// deadline, as usual; so does a connection whose head was queued
    /// keep-alive before the drain, once its response is written. The
    /// exchanges in flight finish as for a shutdown.
    ///
    /// It emits only `Close` (none for a restart): the connections it
    /// closes hold no file or upstream. [`Core::handle`] keeps working as
    /// before; the driver keeps running its loop until
    /// [`Core::is_drained`] or its deadline (`shutdown_timeout`,
    /// `restart_timeout`), then calls [`Core::abort_all`]. Deadlines are
    /// unchanged, so `now` is not needed yet; it is taken for symmetry
    /// with `handle`.
    ///
    /// A shutdown during a restart's drain escalates it as if it came
    /// first (idle and fresh connections close, requests and connections
    /// are refused): the driver calls it on a SIGTERM during the restart
    /// drain (owner decision 2026-11-06, FR-PROC-07). Any other second
    /// call is a driver mistake the core tolerates: a restart during a
    /// shutdown's drain is ignored (no change, no action), since a
    /// shutdown is final; a second restart changes nothing.
    pub fn begin_drain(&mut self, _now: Instant, target: DrainTarget, actions: &mut Vec<Action>) {
        // A shutdown is final: a restart cannot reopen what it refuses.
        if (self.mode, target) == (Mode::Draining(DrainTarget::Shutdown), DrainTarget::Restart) {
            return;
        }
        self.mode = Mode::Draining(target);
        let ids: Vec<ConnId> = self.conns.keys().copied().collect();
        for conn in ids {
            let Some(c) = self.conns.get_mut(&conn) else {
                continue;
            };
            match &mut c.state {
                ConnState::Idle { .. } | ConnState::ReadingHead { buffered: 0, .. } => {
                    match target {
                        DrainTarget::Shutdown => {
                            self.forget(conn, actions);
                            actions.push(Action::Close { conn });
                        }
                        // Closing it would race a request the client may
                        // be sending (FR-PROC-07).
                        DrainTarget::Restart => {}
                    }
                }
                // A partly received head completes, then gets 503 (or, in
                // a restart, its answer); a lingering connection is
                // already closing.
                ConnState::ReadingHead { .. } | ConnState::Lingering { .. } => {}
                // The head is queued: it keeps what it said, and `finish`
                // ends the connection after it, or answers a request
                // pipelined behind it 503 (owner decision 2026-10-27; in a
                // restart, with its answer).
                ConnState::Responding { .. }
                | ConnState::SendingFile { .. }
                | ConnState::Relaying { .. } => {}
                // The head is rendered later: it says `Connection: close`.
                ConnState::OpeningFile { persistence, .. } => *persistence = Persistence::Close,
                ConnState::Connecting { request, .. }
                | ConnState::AwaitingResponse { request, .. } => {
                    request.persistence = Persistence::Close;
                }
            }
        }
        self.conns.rekey();
    }

    /// The fast path: close every connection at once, each open upstream
    /// and each open file exactly once (per connection, in `ConnId`
    /// order: its `Log`, if any, then `CloseFile`, `CloseUpstream`,
    /// `Close`). The driver calls it on a second SIGTERM, a SIGINT or when
    /// `shutdown_timeout` expires, with or without a drain before it,
    /// between two [`Core::handle`] calls.
    ///
    /// Each connection with an exchange in flight (a request read past its
    /// first byte, a response being written, a file being opened or sent,
    /// a backend being connected to, awaited or relayed) is logged first,
    /// as [`Log::Rejected`] with [`RejectReason::ShuttingDown`]
    /// (FR-PROC-05c, owner decision 2026-10-27). Idle and lingering
    /// connections, those without a byte of their request, and those
    /// refused with 503 (their refusal is logged already) are cut
    /// silently. An `OpenFile` still unanswered becomes an orphaned open,
    /// as for any connection that closes: its `FileOpened` is answered
    /// with `CloseFile` if it comes, and [`Core::is_drained`] stays false
    /// until then; the driver exits after this call without waiting for
    /// orphaned opens (DRV-14). The core is draining for a shutdown
    /// afterwards, also after a restart drain, and `handle` keeps working
    /// (a stray `Accepted` is answered 503).
    pub fn abort_all(&mut self, actions: &mut Vec<Action>) {
        self.mode = Mode::Draining(DrainTarget::Shutdown);
        let ids: Vec<ConnId> = self.conns.keys().copied().collect();
        for conn in ids {
            let Some(c) = self.conns.get(&conn) else {
                continue;
            };
            if cuts_an_exchange(c) {
                actions.push(Action::Log(Log::Rejected {
                    conn,
                    peer: c.peer,
                    reason: RejectReason::ShuttingDown,
                }));
            }
            if self.forget(conn, actions).is_some() {
                actions.push(Action::Close { conn });
            }
        }
        self.conns.rekey();
    }

    /// Whether the core holds nothing any more: no connection (lingering
    /// ones included), no open upstream, no open file and no orphaned open.
    /// Once draining, nothing new is opened, so it stays true; the driver
    /// may then exit.
    pub fn is_drained(&self) -> bool {
        self.conns.is_empty() && self.upstreams.is_empty() && self.files.is_empty()
    }

    /// While draining for a shutdown, a complete request `head` on `conn`
    /// is answered 503
    /// with `Connection: close` (FR-PROC-05 "target shutdown",
    /// FR-PROC-05b), and logged as its response; a body it announced is
    /// left unread.
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

/// Whether closing `c` now cuts an exchange in flight (FR-PROC-05c): a
/// request read past its first byte, a response being written, a file
/// being opened or sent, a backend being connected to, awaited or relayed.
/// A connection refused with 503 never had one; its refusal is logged.
fn cuts_an_exchange(c: &Conn) -> bool {
    match c.counted {
        Counted::No => return false,
        Counted::Yes => {}
    }
    match c.state {
        ConnState::Idle { .. }
        | ConnState::ReadingHead { buffered: 0, .. }
        | ConnState::Lingering { .. } => false,
        ConnState::ReadingHead { .. }
        | ConnState::Responding { .. }
        | ConnState::OpeningFile { .. }
        | ConnState::SendingFile { .. }
        | ConnState::Connecting { .. }
        | ConnState::AwaitingResponse { .. }
        | ConnState::Relaying { .. } => true,
    }
}

#[cfg(test)]
mod tests;
