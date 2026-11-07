//! Deadlines: every connection state carries one; `TimerFired` handles
//! each that has passed.

use super::{Action, ConnId, Core};
use crate::conn::{ConnState, UnreadBody};
use crate::file::Chunk;
use crate::http::request::Persistence;
use crate::http::response::{BodyMode, render_fixed};
use crate::http::status::Status;
use crate::log::{Log, RejectReason, TimeoutKind};
use crate::time::Instant;

impl Core {
    /// Handle every connection whose deadline is at or before `now`, in
    /// `ConnId` order, until none is left.
    pub(super) fn timer_fired(&mut self, now: Instant, actions: &mut Vec<Action>) {
        // Each pass closes a connection or moves it from reading a head or
        // waiting on a backend to responding (which can only expire once
        // more), so this terminates. The deadline index yields only the
        // connections due (NFR-PERF-06); each pass first re-keys those the
        // previous one changed.
        loop {
            self.conns.rekey();
            let expired = self.conns.due(now);
            if expired.is_empty() {
                return;
            }
            for conn in expired {
                self.expire(now, conn, actions);
            }
        }
    }

    /// `conn`'s deadline has passed.
    fn expire(&mut self, now: Instant, conn: ConnId, actions: &mut Vec<Action>) {
        let send_idle_timeout = self.config.limits.send_idle_timeout;
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        match c.state {
            // Part of a head arrived, too slowly: answer 408.
            ConnState::ReadingHead { buffered, .. } if buffered > 0 => {
                render_fixed(
                    Status::RequestTimeout,
                    Persistence::Close,
                    BodyMode::Full,
                    self.date.as_ref(),
                    &mut c.send,
                );
                actions.push(Action::Log(Log::Rejected {
                    conn,
                    peer: c.peer,
                    reason: RejectReason::Timeout(TimeoutKind::Header),
                }));
                c.state = ConnState::Responding {
                    persistence: Persistence::Close,
                    unread_body: UnreadBody::None,
                    buffered: 0,
                    deadline: now.saturating_add(send_idle_timeout),
                };
                self.sync_interest(conn, actions);
            }
            // The backend took too long, or the relayed response stalled.
            ConnState::Connecting { .. }
            | ConnState::AwaitingResponse { .. }
            | ConnState::Relaying { .. } => self.expire_proxy(now, conn, actions),
            // Nothing arrived, or nothing is left to say: close silently.
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Lingering { .. } => {
                self.forget(conn, actions);
                actions.push(Action::Close { conn });
            }
            // The client stopped reading, or the disk did not answer in
            // time: close without further writes. An open in flight becomes
            // an orphan; an open file is closed.
            ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. } => {
                let kind = match c.state {
                    ConnState::OpeningFile { .. }
                    | ConnState::SendingFile {
                        chunk: Chunk::Reading { .. },
                        ..
                    } => TimeoutKind::Disk,
                    ConnState::SendingFile {
                        chunk: Chunk::Ready { .. } | Chunk::Done,
                        ..
                    }
                    | ConnState::Responding { .. }
                    | ConnState::ReadingHead { .. }
                    | ConnState::Idle { .. }
                    | ConnState::Connecting { .. }
                    | ConnState::AwaitingResponse { .. }
                    | ConnState::Relaying { .. }
                    | ConnState::Lingering { .. } => TimeoutKind::Send,
                };
                self.close_timed_out(conn, kind, actions);
            }
        }
    }

    /// `conn` timed out where nothing can be answered any more (the client
    /// stopped reading, or the disk did not answer): log the timeout and
    /// close at once, without further writes; `forget` closes any file or
    /// upstream in between (`Log`, `CloseFile` / `CloseUpstream`, `Close`).
    pub(super) fn close_timed_out(
        &mut self,
        conn: ConnId,
        kind: TimeoutKind,
        actions: &mut Vec<Action>,
    ) {
        let Some(c) = self.conns.get(&conn) else {
            return;
        };
        actions.push(Action::Log(Log::Rejected {
            conn,
            peer: c.peer,
            reason: RejectReason::Timeout(kind),
        }));
        self.forget(conn, actions);
        actions.push(Action::Close { conn });
    }
}

#[cfg(test)]
mod tests {
    use crate::core::test_support::{PEER, accept, core, secs};
    use crate::core::{Action, ConnId, Event};

    /// Connections due at one `TimerFired` expire in `ConnId` order,
    /// whatever their deadlines' order and the order they arrived in: two
    /// at the same instant (accepted 7 then 5) and an earlier id (2) with
    /// a later deadline.
    #[test]
    fn due_connections_expire_in_id_order() {
        let mut core = core();
        accept(&mut core, secs(0), 7, PEER);
        accept(&mut core, secs(0), 5, PEER);
        accept(&mut core, secs(1), 2, PEER);
        let mut actions = Vec::new();
        core.handle(secs(20), Event::TimerFired, &mut actions);
        let closed: Vec<ConnId> = actions
            .iter()
            .filter_map(|a| {
                if let Action::Close { conn } = a {
                    Some(*conn)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(closed, vec![ConnId(2), ConnId(5), ConnId(7)], "{actions:?}");
        assert_eq!(core.next_deadline(), None);
    }
}
