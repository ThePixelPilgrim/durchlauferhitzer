//! Backend connection pooling and the safe retry (FR-PROXY-02,
//! FR-PROXY-26).
//!
//! - **Reuse.** A request on a pooled route from an HTTP/1.1 client asks
//!   the backend to keep its connection (no `Connection: close`) and takes
//!   the backend's newest idle connection, if any (`forward` in
//!   `proxy.rs`). Its upstream goes back to the pool once the response is
//!   complete and reusable on both sides: the whole request was written,
//!   the response is HTTP/1.1 without `close`, framed by
//!   `Content-Length` or chunked (or `Content-Length: 0`; never a
//!   response to HEAD, a 204 or a 304, owner decision 2026-11-07), and
//!   nothing came after its end in the read that completed it, which did
//!   not fill the buffer (`relay.rs`, `relay_body.rs`).
//! - **Silence.** Before a request is written on a reused upstream, the
//!   driver checks it has nothing to read (`Action::CheckSilent`, one
//!   non-blocking peek; owner decision 2026-11-07): stray bytes a buggy
//!   backend sent after a response never become the next response. A
//!   failed check closes it and the request connects afresh
//!   ([`Core::reconnect`]).
//! - **Idle.** An idle upstream holds no buffer; it is read into the
//!   shared scratch buffer, and any byte, end of stream or error closes it
//!   ([`Core::idle_event`], Review Focus 1). At most `backend_idle_max`
//!   per backend (the oldest goes first), each closed after
//!   `backend_idle_timeout` ([`Core::expire_idle`]). Every drain closes
//!   the pool at once and nothing is parked or taken while draining
//!   ([`Core::close_pool`]).
//! - **Budget.** Every upstream, active or idle, counts against
//!   `max_connections`: a fresh connect at the limit first closes the
//!   oldest idle one ([`make_room`]), so at most one upstream per counted
//!   connection is open and the descriptor budget is unchanged.
//! - **Retry.** A GET or HEAD without a body whose reused connection fails
//!   (reset or end of stream) before any response byte is replayed once on
//!   a fresh connection, logged as [`Log::BackendRetried`]
//!   ([`Core::retry`]). Fresh connections are never retried, nor is
//!   anything once a drain began.

use super::proxy::{close_upstream, take_exchange};
use super::{Action, ConnId, Core, Event, Interest, Mode};
use crate::config::Limits;
use crate::conn::{ConnState, Release, Replay, Reuse};
use crate::log::{BackendFailure, Log};
use crate::relay_body::Rest;
use crate::time::Instant;
use crate::upstream::{UpstreamId, UpstreamTable};

/// Before a fresh connect: with `max_connections` upstreams open, close
/// the oldest idle one, so active and idle upstreams together never
/// exceed it (NFR-RES-05's bound of one upstream per counted connection).
pub(super) fn make_room(upstreams: &mut UpstreamTable, limits: &Limits, actions: &mut Vec<Action>) {
    let max = usize::try_from(limits.max_connections).unwrap_or(usize::MAX);
    if upstreams.len() >= max {
        if let Some(oldest) = upstreams.oldest_idle() {
            close_upstream(upstreams, oldest, actions);
        }
    }
}

/// The response on `up` is complete: park it in the idle pool if the
/// exchange was reusable (`to`), nothing unread came with the body's end
/// (`rest`) and the core is serving (`mode`); close it otherwise. A park
/// over the backend's cap closes the oldest idle connection of that
/// backend.
#[allow(clippy::too_many_arguments, reason = "disjoint borrows of Core")]
pub(super) fn release(
    upstreams: &mut UpstreamTable,
    mode: Mode,
    limits: &Limits,
    now: Instant,
    up: UpstreamId,
    to: Release,
    rest: Rest,
    actions: &mut Vec<Action>,
) {
    match (to, rest, mode) {
        (Release::Park(backend), Rest::Clean, Mode::Serving) => {
            let deadline = now.saturating_add(limits.backend_idle_timeout);
            let cap = usize::try_from(limits.backend_idle_max).unwrap_or(usize::MAX);
            if let Some(oldest) = upstreams.park(up, backend, deadline, cap) {
                close_upstream(upstreams, oldest, actions);
            }
            // Watched for any byte, end of stream or error (it was read
            // already: the body's last bytes came from a read).
            if upstreams.set_interest(up, Interest::Read) {
                actions.push(Action::SetUpstreamInterest {
                    up,
                    interest: Interest::Read,
                });
            }
        }
        (Release::Park(_), Rest::Clean, Mode::Draining(_))
        | (Release::Park(_), Rest::Unread, _)
        | (Release::Close, _, _) => close_upstream(upstreams, up, actions),
    }
}

impl Core {
    /// An event for an idle upstream: a byte, the end of stream or an
    /// error closes it (Review Focus 1); anything else is stray. Whether
    /// the event was for an idle upstream.
    pub(super) fn idle_event(&mut self, event: &Event, actions: &mut Vec<Action>) -> bool {
        match *event {
            Event::UpstreamReceived { up, .. }
            | Event::UpstreamReadClosed { up }
            | Event::UpstreamFailed { up, .. }
                if self.upstreams.is_idle(up) =>
            {
                close_upstream(&mut self.upstreams, up, actions);
                true
            }
            Event::UpstreamConnected { up } | Event::UpstreamSent { up, .. } => {
                self.upstreams.is_idle(up)
            }
            Event::UpstreamReceived { .. }
            | Event::UpstreamReadClosed { .. }
            | Event::UpstreamFailed { .. }
            | Event::Accepted { .. }
            | Event::Received { .. }
            | Event::ReadClosed { .. }
            | Event::Sent { .. }
            | Event::Failed { .. }
            | Event::TimerFired
            | Event::FileOpened { .. }
            | Event::FileFailed { .. }
            | Event::ChunkReady { .. }
            | Event::FileSent { .. } => false,
        }
    }

    /// Close every idle upstream whose idle deadline is at or before `now`.
    pub(super) fn expire_idle(&mut self, now: Instant, actions: &mut Vec<Action>) {
        for up in self.upstreams.idle_due(now) {
            close_upstream(&mut self.upstreams, up, actions);
        }
    }

    /// Close the whole idle pool (both drains, FR-PROC-05, -07).
    pub(super) fn close_pool(&mut self, actions: &mut Vec<Action>) {
        for up in self.upstreams.idle() {
            close_upstream(&mut self.upstreams, up, actions);
        }
    }

    /// The pooled upstream `conn` was to reuse is not silent (its
    /// `CheckSilent` failed): close it and connect afresh with the whole
    /// request, nothing sent, nothing logged (FR-PROXY-02).
    pub(super) fn reconnect(&mut self, now: Instant, conn: ConnId, actions: &mut Vec<Action>) {
        let limits = &self.config.limits;
        let connect_deadline = now.saturating_add(limits.backend_connect_timeout);
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let Some((dead, mut request, buffers)) = take_exchange(c) else {
            return;
        };
        close_upstream(&mut self.upstreams, dead, actions);
        make_room(&mut self.upstreams, limits, actions);
        request.reuse = Reuse::Fresh;
        let backend = request.backend;
        let up = self.upstreams.allocate(conn);
        actions.push(Action::Connect { up, backend });
        c.state = ConnState::Connecting {
            up,
            request,
            buffers,
            deadline: connect_deadline,
        };
        self.sync_interest(conn, actions);
    }

    /// The upstream of `conn` failed (`kind`) while it awaited the response
    /// head. If the request went out on a reused connection, may be
    /// replayed (a GET or HEAD without a body) and no response byte came,
    /// replay it once on a fresh connection while serving: log it, close
    /// the dead upstream and connect anew with the whole request head
    /// (FR-PROXY-26). Whether it did; if not, the caller answers 502.
    pub(super) fn retry(
        &mut self,
        now: Instant,
        conn: ConnId,
        kind: BackendFailure,
        actions: &mut Vec<Action>,
    ) -> bool {
        let limits = &self.config.limits;
        let connect_deadline = now.saturating_add(limits.backend_connect_timeout);
        if self.mode != Mode::Serving {
            return false;
        }
        let Some(c) = self.conns.get_mut(&conn) else {
            return false;
        };
        let ConnState::AwaitingResponse {
            request,
            filled,
            interim,
            ..
        } = &c.state
        else {
            return false;
        };
        let eligible = (request.reuse, request.replay, *filled, *interim)
            == (Reuse::Pooled, Replay::Allowed, 0, 0);
        if !eligible {
            return false;
        }
        let Some((dead, mut request, mut buffers)) = take_exchange(c) else {
            return false;
        };
        actions.push(Action::Log(Log::BackendRetried {
            conn,
            peer: c.peer,
            backend: request.backend,
            kind,
        }));
        close_upstream(&mut self.upstreams, dead, actions);
        make_room(&mut self.upstreams, limits, actions);
        buffers.rewind_head();
        // The replay goes out on a fresh connection, never retried.
        request.reuse = Reuse::Fresh;
        let backend = request.backend;
        let up = self.upstreams.allocate(conn);
        actions.push(Action::Connect { up, backend });
        c.state = ConnState::Connecting {
            up,
            request,
            buffers,
            deadline: connect_deadline,
        };
        self.sync_interest(conn, actions);
        true
    }
}

#[cfg(test)]
mod tests;
