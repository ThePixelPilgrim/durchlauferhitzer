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
        // more), so this terminates.
        loop {
            let expired: Vec<ConnId> = self
                .conns
                .iter()
                .filter(|(_, c)| c.state.deadline() <= now)
                .map(|(id, _)| *id)
                .collect();
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
                actions.push(Action::Log(Log::Rejected {
                    conn,
                    peer: c.peer,
                    reason: RejectReason::Timeout(kind),
                }));
                self.forget(conn, actions);
                actions.push(Action::Close { conn });
            }
        }
    }
}
