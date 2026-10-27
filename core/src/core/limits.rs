//! Connection limits: counting accepted connections globally and per
//! peer, refusing (503) beyond them, and the slots orphaned opens keep.

use std::net::IpAddr;

use super::proxy::close_upstream;
use super::{Action, ConnId, Core};
use crate::conn::{Conn, ConnState, Counted, UnreadBody};
use crate::file::{FileId, OrphanSlots, Owner};
use crate::http::request::{HeadParser, Persistence};
use crate::http::response::{BodyMode, render_fixed};
use crate::http::status::Status;
use crate::log::{LimitKind, Log, RejectReason};
use crate::time::Instant;

impl Core {
    /// Remove `conn` from the map and release its share of the limits, its
    /// file and its upstream: an open file is closed, an open in flight is
    /// orphaned, an open upstream is closed (`CloseFile` and
    /// `CloseUpstream` come in that order).
    pub(super) fn forget(&mut self, conn: ConnId, actions: &mut Vec<Action>) -> Option<Conn> {
        let c = self.conns.remove(&conn)?;
        let peer = c.peer.to_canonical();
        match c.state {
            ConnState::OpeningFile { file, .. } => {
                let slots = match c.counted {
                    Counted::Yes => OrphanSlots::Held { peer },
                    Counted::No => OrphanSlots::None,
                };
                self.files.orphan(file, slots);
            }
            ConnState::SendingFile { file, .. } => {
                self.files.release(file);
                actions.push(Action::CloseFile { file });
            }
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::Connecting { .. }
            | ConnState::AwaitingResponse { .. }
            | ConnState::Relaying { .. }
            | ConnState::Lingering { .. } => {}
        }
        if let Some(up) = c.state.upstream() {
            close_upstream(&mut self.upstreams, up, actions);
        }
        match c.counted {
            Counted::Yes => match c.state {
                // An orphaned open keeps both slots until the disk answers.
                ConnState::OpeningFile { .. } => {}
                ConnState::ReadingHead { .. }
                | ConnState::Idle { .. }
                | ConnState::Responding { .. }
                | ConnState::SendingFile { .. }
                | ConnState::Connecting { .. }
                | ConnState::AwaitingResponse { .. }
                | ConnState::Relaying { .. }
                | ConnState::Lingering { .. } => self.release_slots(peer),
            },
            Counted::No => self.refused = self.refused.saturating_sub(1),
        }
        Some(c)
    }

    /// A counted connection from `peer` (canonical) is gone: free its
    /// global and per-IP slots.
    fn release_slots(&mut self, peer: IpAddr) {
        self.counted = self.counted.saturating_sub(1);
        if let Some(n) = self.counted_per_ip.get_mut(&peer) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.counted_per_ip.remove(&peer);
            }
        }
    }

    /// The disk answered an orphaned open: the slots it held are free.
    ///
    /// Invariant (checked in debug builds): every orphan came from a
    /// counted connection and holds its slots. A refused connection only
    /// ever answers 503 and lingers; it never reads a request, so it never
    /// opens a file that could be orphaned.
    pub(super) fn release_orphan(&mut self, file: FileId) {
        if let Owner::Orphan(slots) = self.files.owner(file) {
            debug_assert!(
                matches!(slots, OrphanSlots::Held { .. }),
                "orphaned open {file:?} came from a refused connection"
            );
            self.files.release(file);
            match slots {
                OrphanSlots::Held { peer } => self.release_slots(peer),
                OrphanSlots::None => {}
            }
        }
    }

    pub(super) fn accepted(
        &mut self,
        now: Instant,
        conn: ConnId,
        peer: IpAddr,
        actions: &mut Vec<Action>,
    ) {
        if self.conns.contains_key(&conn) {
            return;
        }
        let limits = &self.config.limits;
        let canonical = peer.to_canonical();
        let per_ip = self.counted_per_ip.get(&canonical).copied().unwrap_or(0);
        let refusal = if self.counted >= limits.max_connections {
            Some(LimitKind::Connections)
        } else if per_ip >= limits.max_connections_per_ip {
            Some(LimitKind::ConnectionsPerIp)
        } else {
            None
        };
        let capacity = self.new_recv_capacity();
        let c = match refusal {
            None => {
                self.counted = self.counted.saturating_add(1);
                self.counted_per_ip
                    .insert(canonical, per_ip.saturating_add(1));
                let state = ConnState::ReadingHead {
                    buffered: 0,
                    parser: HeadParser::new(),
                    deadline: now.saturating_add(limits.header_timeout),
                };
                Conn::new(peer, Counted::Yes, state, capacity)
            }
            Some(kind) => {
                actions.push(Action::Log(Log::Rejected {
                    conn,
                    peer,
                    reason: RejectReason::Limit(kind),
                }));
                // Too many refusals in flight: do not even answer.
                if self.refused >= limits.max_connections {
                    actions.push(Action::Close { conn });
                    return;
                }
                self.refused = self.refused.saturating_add(1);
                let state = ConnState::Responding {
                    persistence: Persistence::Close,
                    unread_body: UnreadBody::None,
                    buffered: 0,
                    deadline: now.saturating_add(limits.send_idle_timeout),
                };
                // Refused connections never read while responding and then
                // linger into the shared scratch buffer: no buffer of their
                // own.
                let mut c = Conn::new(peer, Counted::No, state, 0);
                render_fixed(
                    Status::ServiceUnavailable,
                    Persistence::Close,
                    BodyMode::Full,
                    self.date.as_ref(),
                    &mut c.send,
                );
                c
            }
        };
        self.conns.insert(conn, c);
        self.sync_interest(conn, actions);
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use crate::conn::{LINGER_TIMEOUT, SEND_CAPACITY};
    use crate::core::test_support::{
        GET_F, PEER, accept, core, files_core, regular, request, secs,
    };
    use crate::core::{Action, ConnError, ConnId, Core, Event};
    use crate::file::{FileError, FileId};
    use crate::log::{LimitKind, Log, RejectReason, TimeoutKind};

    #[test]
    fn limit_counts_return_to_zero_on_every_close_path() {
        let mut core = core();
        let mut actions = Vec::new();
        let now = secs(0);
        let other = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 9));
        for (id, peer) in [(1, PEER), (2, PEER), (3, other), (4, other), (5, other)] {
            accept(&mut core, now, id, peer);
        }
        assert_eq!(core.counted, 5);
        assert_eq!(core.counted_per_ip.get(&PEER), Some(&2));
        // conns 4 and 5: a full buffer of zeros is answered 400 at once.
        for id in [4, 5] {
            let conn = ConnId(id);
            core.handle(now, Event::Received { conn, len: 10 }, &mut actions);
        }
        // conn 5's answer is written: it lingers (deadline 2 s); conn 4's is
        // never taken (send deadline 30 s).
        let conn = ConnId(5);
        core.handle(now, Event::Sent { conn, len: 999 }, &mut actions);
        core.handle(now, Event::ReadClosed { conn: ConnId(1) }, &mut actions);
        let error = ConnError::Reset;
        let conn = ConnId(2);
        core.handle(now, Event::Failed { conn, error }, &mut actions);
        assert_eq!(core.counted, 3);
        assert_eq!(core.counted_per_ip.get(&PEER), None);
        // Lingering expiry.
        core.handle(secs(2), Event::TimerFired, &mut actions);
        assert_eq!(core.counted, 2);
        // conn 3 sent nothing: it times out silently at the header deadline.
        core.handle(secs(10), Event::TimerFired, &mut actions);
        assert_eq!(core.counted, 1);
        // Send idle expiry.
        core.handle(secs(30), Event::TimerFired, &mut actions);
        assert_eq!(core.counted, 0);
        assert!(core.counted_per_ip.is_empty());
        assert!(core.conns.is_empty());
        assert_eq!(core.next_deadline(), None);
    }

    /// One counted connection allowed, so at most one refused one. The long
    /// header timeout keeps the counted connection open throughout.
    const CAPPED: &str = "listen 127.0.0.1:8080; \
        limits { max_connections 1; header_timeout 60m; } \
        host a.test { path / { static /srv; } }";

    fn capped() -> Core {
        Core::new(crate::config::load(CAPPED).unwrap())
    }

    fn refused(conn: ConnId) -> Action {
        Action::Log(Log::Rejected {
            conn,
            peer: PEER,
            reason: RejectReason::Limit(LimitKind::Connections),
        })
    }

    #[test]
    fn refused_connection_has_no_receive_buffer() {
        let mut core = capped();
        accept(&mut core, secs(0), 1, PEER);
        accept(&mut core, secs(0), 2, PEER);
        let conn = ConnId(2);
        assert!(!core.send_buffer(conn).is_empty(), "a 503 is waiting");
        assert_eq!(core.recv_capacity(conn), 0);
        assert!(core.recv_buffer(conn).is_empty());
        assert_eq!(core.send_capacity(conn), SEND_CAPACITY);
    }

    #[test]
    fn lingering_connections_share_a_scratch_buffer() {
        let mut core = capped();
        let mut actions = Vec::new();
        let now = secs(0);
        accept(&mut core, now, 1, PEER);
        accept(&mut core, now, 2, PEER);
        // conn 1 (counted): no Host header, answered 400, then lingers.
        let c1 = ConnId(1);
        let request = b"GET / HTTP/1.1\r\n\r\n";
        let spare = core.recv_buffer(c1);
        assert!(spare.len() > 4096);
        if let Some(dst) = spare.get_mut(..request.len()) {
            dst.copy_from_slice(request);
        }
        let len = request.len();
        core.handle(now, Event::Received { conn: c1, len }, &mut actions);
        for id in [1, 2] {
            let conn = ConnId(id);
            core.handle(now, Event::Sent { conn, len: 999 }, &mut actions);
            assert!(actions.contains(&Action::ShutdownWrite { conn }));
            assert_eq!(core.recv_capacity(conn), 4096);
            assert_eq!(core.recv_buffer(conn).len(), 4096);
            assert_eq!(core.conns.get(&conn).map(|c| c.recv.len()), Some(0));
            actions.clear();
            core.handle(now, Event::Received { conn, len: 4096 }, &mut actions);
            assert!(actions.is_empty());
        }
    }

    #[test]
    fn refused_connections_beyond_the_cap_are_closed_at_once() {
        let mut core = capped();
        let now = secs(0);
        accept(&mut core, now, 1, PEER);
        let second = accept(&mut core, now, 2, PEER);
        assert!(!second.contains(&Action::Close { conn: ConnId(2) }));
        let conn = ConnId(3);
        assert_eq!(
            accept(&mut core, now, 3, PEER),
            [refused(conn), Action::Close { conn }]
        );
        assert!(!core.conns.contains_key(&conn));
        assert!(core.send_buffer(conn).is_empty());
        assert_eq!(core.recv_capacity(conn), 0);
    }

    #[test]
    fn refused_cap_is_released_on_every_close_path() {
        let mut core = capped();
        let mut actions = Vec::new();
        accept(&mut core, secs(0), 1, PEER);
        let mut id = 2;
        // Each closing path, applied to a refused connection accepted at `t`,
        // frees the single refused slot for the next round.
        for path in 0..4 {
            let t = secs(100 * path);
            let conn = ConnId(id);
            id += 1;
            let accepted = accept(&mut core, t, conn.0, PEER);
            assert!(!accepted.contains(&Action::Close { conn }), "path {path}");
            let over = ConnId(id);
            id += 1;
            assert_eq!(
                accept(&mut core, t, over.0, PEER),
                [refused(over), Action::Close { conn: over }],
                "path {path}"
            );
            actions.clear();
            match path {
                0 => core.handle(t, Event::ReadClosed { conn }, &mut actions),
                1 => {
                    let error = ConnError::Reset;
                    core.handle(t, Event::Failed { conn, error }, &mut actions);
                }
                2 => {
                    core.handle(t, Event::Sent { conn, len: 999 }, &mut actions);
                    let linger_end = t.saturating_add(LINGER_TIMEOUT);
                    core.handle(linger_end, Event::TimerFired, &mut actions);
                }
                _ => {
                    let stalled = t.saturating_add(std::time::Duration::from_secs(30));
                    core.handle(stalled, Event::TimerFired, &mut actions);
                }
            }
            assert!(actions.contains(&Action::Close { conn }), "path {path}");
            assert_eq!(core.conns.len(), 1, "path {path}");
        }
    }

    /// An orphaned open keeps its connection slot until the disk answers;
    /// the per-IP slot is released at close.
    #[test]
    fn orphaned_opens_count_until_the_disk_answers() {
        let mut core = files_core();
        let now = secs(0);
        let mut actions = Vec::new();
        // conn 1: closed by the peer, then the open succeeds.
        request(&mut core, now, 1, GET_F);
        core.handle(now, Event::ReadClosed { conn: ConnId(1) }, &mut actions);
        // conn 2: reset, then the open fails.
        request(&mut core, now, 2, GET_F);
        let error = ConnError::Reset;
        core.handle(
            now,
            Event::Failed {
                conn: ConnId(2),
                error,
            },
            &mut actions,
        );
        // conn 3: times out waiting on the disk, then the open succeeds.
        request(&mut core, now, 3, GET_F);
        core.handle(secs(30), Event::TimerFired, &mut actions);
        assert!(core.conns.is_empty());
        assert_eq!(core.counted, 3);
        // The per-IP slots are held too, so one peer cannot take every
        // global slot with orphans.
        assert_eq!(core.counted_per_ip.get(&PEER), Some(&3));
        assert_eq!(core.next_deadline(), None);
        let meta = regular(3);
        actions.clear();
        core.handle(
            now,
            Event::FileOpened {
                file: FileId(1),
                meta,
            },
            &mut actions,
        );
        assert_eq!(actions, [Action::CloseFile { file: FileId(1) }]);
        assert_eq!(core.counted, 2);
        assert_eq!(core.counted_per_ip.get(&PEER), Some(&2));
        let error = FileError::NotFound;
        core.handle(
            now,
            Event::FileFailed {
                file: FileId(2),
                error,
            },
            &mut actions,
        );
        assert_eq!(core.counted, 1);
        assert_eq!(core.counted_per_ip.get(&PEER), Some(&1));
        core.handle(
            now,
            Event::FileOpened {
                file: FileId(3),
                meta,
            },
            &mut actions,
        );
        assert_eq!(core.counted, 0);
        assert!(core.counted_per_ip.is_empty());
        assert_eq!(core.files.len(), 0);
        // Repeated answers change nothing.
        core.handle(
            now,
            Event::FileOpened {
                file: FileId(3),
                meta,
            },
            &mut actions,
        );
        assert_eq!(core.counted, 0);
    }

    /// A disk timeout orphans the open; its connection slot comes back
    /// with the late answer, a `FileOpened` (closed at once) or a
    /// `FileFailed` (nothing to close) alike.
    #[test]
    fn disk_timeout_orphan_slot_is_released_by_any_late_answer() {
        let file = FileId(1);
        let conn = ConnId(1);
        let late_answers = [
            Event::FileOpened {
                file,
                meta: regular(3),
            },
            Event::FileFailed {
                file,
                error: FileError::NotFound,
            },
        ];
        for late in late_answers {
            let mut core = files_core();
            request(&mut core, secs(0), 1, GET_F);
            let mut actions = Vec::new();
            core.handle(secs(30), Event::TimerFired, &mut actions);
            assert_eq!(
                actions,
                [
                    Action::Log(Log::Rejected {
                        conn,
                        peer: PEER,
                        reason: RejectReason::Timeout(TimeoutKind::Disk),
                    }),
                    Action::Close { conn }
                ]
            );
            assert!(core.conns.is_empty());
            assert_eq!(core.counted, 1, "{late:?}");
            assert_eq!(core.counted_per_ip.get(&PEER), Some(&1), "{late:?}");
            assert_eq!(core.files.len(), 1);
            actions.clear();
            let opened = matches!(late, Event::FileOpened { .. });
            core.handle(secs(31), late, &mut actions);
            if opened {
                assert_eq!(actions, [Action::CloseFile { file }]);
            } else {
                assert!(actions.is_empty(), "{actions:?}");
            }
            assert_eq!(core.counted, 0);
            assert!(core.counted_per_ip.is_empty(), "opened: {opened}");
            assert_eq!(core.files.len(), 0);
            assert_eq!(core.next_deadline(), None);
        }
    }
}
