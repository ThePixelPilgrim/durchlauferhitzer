//! Connection limits: counting accepted connections globally and per
//! peer group, refusing (503) beyond them, and the slots orphaned opens
//! keep.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv6Addr};

use super::proxy::close_upstream;
use super::{Action, ConnId, Core, Mode};
use crate::conn::{Conn, ConnState, Counted, UnreadBody};
use crate::file::{FileId, OrphanSlots, Owner};
use crate::http::request::{HeadParser, Persistence};
use crate::http::response::{BodyMode, render_fixed};
use crate::http::status::Status;
use crate::log::{LimitKind, Log, RejectReason};
use crate::time::Instant;

/// The group `peer` is counted in for the per-IP limits (FR-CONN-13): an
/// IPv4 (or IPv4-mapped) peer is its own IPv4 address, an IPv6 peer its
/// first `ipv6_prefix` bits with the rest zeroed.
pub(crate) fn peer_group(peer: IpAddr, ipv6_prefix: u8) -> IpAddr {
    match peer.to_canonical() {
        IpAddr::V4(v4) => IpAddr::V4(v4),
        IpAddr::V6(v6) => {
            let host_bits = 128u32.saturating_sub(u32::from(ipv6_prefix));
            let mask = u128::MAX.checked_shl(host_bits).unwrap_or(0);
            IpAddr::V6(Ipv6Addr::from(u128::from(v6) & mask))
        }
    }
}

/// Count one less for `group`; an entry is removed at zero.
fn decrement(counts: &mut BTreeMap<IpAddr, u32>, group: IpAddr) {
    if let Some(n) = counts.get_mut(&group) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            counts.remove(&group);
        }
    }
}

impl Core {
    /// Remove `conn` from the map and release its share of the limits, its
    /// file and its upstream: an open file is closed, an open in flight is
    /// orphaned, an open upstream is closed (`CloseFile` and
    /// `CloseUpstream` come in that order).
    pub(super) fn forget(&mut self, conn: ConnId, actions: &mut Vec<Action>) -> Option<Conn> {
        let c = self.conns.remove(&conn)?;
        let group = peer_group(c.peer, self.config.limits.ipv6_prefix);
        match c.state {
            ConnState::OpeningFile { file, .. } => {
                let slots = match c.counted {
                    Counted::Yes => OrphanSlots::Held { group },
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
                | ConnState::Lingering { .. } => self.release_slots(group),
            },
            Counted::No => {
                self.refused = self.refused.saturating_sub(1);
                decrement(&mut self.refused_per_group, group);
            }
        }
        Some(c)
    }

    /// A counted connection of peer `group` is gone: free its global and
    /// per-IP slots.
    fn release_slots(&mut self, group: IpAddr) {
        self.counted = self.counted.saturating_sub(1);
        decrement(&mut self.counted_per_ip, group);
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
                OrphanSlots::Held { group } => self.release_slots(group),
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
        let group = peer_group(peer, limits.ipv6_prefix);
        let per_ip = self.counted_per_ip.get(&group).copied().unwrap_or(0);
        let refusal = if self.mode == Mode::Draining {
            // The driver closed its listeners before the drain; answer a
            // straggler all the same (FR-PROC-05).
            Some(RejectReason::ShuttingDown)
        } else if self.counted >= limits.max_connections {
            Some(RejectReason::Limit(LimitKind::Connections))
        } else if per_ip >= limits.max_connections_per_ip {
            Some(RejectReason::Limit(LimitKind::ConnectionsPerIp))
        } else {
            None
        };
        let capacity = self.new_recv_capacity();
        let c = match refusal {
            None => {
                self.counted = self.counted.saturating_add(1);
                self.counted_per_ip.insert(group, per_ip.saturating_add(1));
                let state = ConnState::ReadingHead {
                    buffered: 0,
                    parser: HeadParser::new(),
                    deadline: now.saturating_add(limits.header_timeout),
                };
                Conn::new(peer, Counted::Yes, state, capacity)
            }
            Some(reason) => {
                actions.push(Action::Log(Log::Rejected { conn, peer, reason }));
                // Too many refusals in flight, in all (FR-CONN-10) or from
                // this group (FR-CONN-13): do not even answer.
                let group_refused = self.refused_per_group.get(&group).copied().unwrap_or(0);
                if self.refused >= limits.max_connections
                    || group_refused >= limits.max_connections_per_ip
                {
                    actions.push(Action::Close { conn });
                    return;
                }
                self.refused = self.refused.saturating_add(1);
                self.refused_per_group
                    .insert(group, group_refused.saturating_add(1));
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
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::peer_group;
    use crate::conn::{LINGER_TIMEOUT, SEND_CAPACITY};
    use crate::core::test_support::{
        FILES, GET_F, PEER, accept, core, files_core, regular, request, secs,
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
        assert!(core.refused_per_group.is_empty());
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
        released_on_every_close_path(capped(), PEER, refused);
    }

    /// Each closing path, applied to a refused connection from `peer`,
    /// frees the single refused slot `core` allows it for the next round;
    /// `rejected` is the log of a refusal.
    fn released_on_every_close_path(mut core: Core, peer: IpAddr, rejected: fn(ConnId) -> Action) {
        let mut actions = Vec::new();
        accept(&mut core, secs(0), 1, peer);
        let mut id = 2;
        for path in 0..4 {
            let t = secs(100 * path);
            let conn = ConnId(id);
            id += 1;
            let accepted = accept(&mut core, t, conn.0, peer);
            assert!(!accepted.contains(&Action::Close { conn }), "path {path}");
            let over = ConnId(id);
            id += 1;
            assert_eq!(
                accept(&mut core, t, over.0, peer),
                [rejected(over), Action::Close { conn: over }],
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
            assert_eq!(core.refused, 0, "path {path}");
            assert!(core.refused_per_group.is_empty(), "path {path}");
        }
    }

    const V6_A1: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
    const V6_A2: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0xffff, 1));
    const V6_A3: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 1, 2, 3, 4));
    /// Another /64.
    const V6_B: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 1, 0, 0, 0, 1));

    #[test]
    fn peer_group_masks_ipv6_by_prefix() {
        let group_a = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0));
        assert_eq!(peer_group(V6_A1, 64), group_a);
        assert_eq!(peer_group(V6_A2, 64), group_a);
        assert_eq!(peer_group(V6_A3, 64), group_a);
        assert_ne!(peer_group(V6_B, 64), group_a);
        assert_eq!(peer_group(V6_A1, 128), V6_A1);
        assert_eq!(peer_group(V6_A2, 128), V6_A2);
        assert_eq!(peer_group(V6_B, 32), group_a);
        let odd = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0xffff, 0, 0, 0, 0, 0));
        assert_eq!(
            peer_group(odd, 33),
            IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0x8000, 0, 0, 0, 0, 0))
        );
        let v4 = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let mapped = IpAddr::V6(Ipv4Addr::new(192, 0, 2, 1).to_ipv6_mapped());
        assert_eq!(peer_group(v4, 64), v4);
        assert_eq!(peer_group(mapped, 32), v4);
        assert_eq!(peer_group(mapped, 128), v4);
    }

    /// Two counted connections per group, at most four in all.
    const GROUPED: &str = "listen 127.0.0.1:8080; \
        limits { max_connections 4; max_connections_per_ip 2; header_timeout 60m; } \
        host a.test { path / { static /srv; } }";

    fn rejected(conn: ConnId, peer: IpAddr, kind: LimitKind) -> Action {
        Action::Log(Log::Rejected {
            conn,
            peer,
            reason: RejectReason::Limit(kind),
        })
    }

    /// Accepted: nothing logged.
    fn admitted(actions: &[Action]) -> bool {
        !actions.iter().any(|a| matches!(a, Action::Log(_)))
    }

    #[test]
    fn peers_in_one_prefix_share_the_per_ip_limit() {
        let mut core = Core::new(crate::config::load(GROUPED).unwrap());
        let now = secs(0);
        assert!(admitted(&accept(&mut core, now, 1, V6_A1)));
        accept(&mut core, now, 2, V6_A2);
        assert_eq!(core.counted, 2);
        // A third address of the same /64 is over the limit: its 503.
        let third = accept(&mut core, now, 3, V6_A3);
        assert_eq!(
            third.first(),
            Some(&rejected(ConnId(3), V6_A3, LimitKind::ConnectionsPerIp))
        );
        assert!(!third.contains(&Action::Close { conn: ConnId(3) }));
        assert!(!core.send_buffer(ConnId(3)).is_empty(), "a 503 is waiting");
        // Another /64 has its own slots.
        let other = accept(&mut core, now, 4, V6_B);
        assert!(admitted(&other), "{other:?}");
        assert_eq!(core.counted, 3);
    }

    #[test]
    fn a_prefix_of_128_counts_every_address_alone() {
        let config = GROUPED.replace(
            "header_timeout 60m;",
            "header_timeout 60m; ipv6_prefix 128;",
        );
        let mut core = Core::new(crate::config::load(&config).unwrap());
        for (id, peer) in [(1, V6_A1), (2, V6_A1), (3, V6_A2), (4, V6_A2)] {
            let actions = accept(&mut core, secs(0), id, peer);
            assert!(admitted(&actions), "{actions:?}");
        }
        assert_eq!(core.counted, 4);
    }

    #[test]
    fn an_orphan_counts_against_its_group() {
        let config = FILES.replace(
            "listen 127.0.0.1:8080;",
            "listen 127.0.0.1:8080; limits { max_connections_per_ip 1; }",
        );
        let mut core = Core::new(crate::config::load(&config).unwrap());
        let now = secs(0);
        let mut actions = accept(&mut core, now, 1, V6_A1);
        let conn = ConnId(1);
        let spare = core.recv_buffer(conn);
        spare.get_mut(..GET_F.len()).unwrap().copy_from_slice(GET_F);
        let len = GET_F.len();
        core.handle(now, Event::Received { conn, len }, &mut actions);
        assert!(
            actions.iter().any(|a| matches!(a, Action::OpenFile { .. })),
            "{actions:?}"
        );
        core.handle(now, Event::ReadClosed { conn }, &mut actions);
        assert!(core.conns.is_empty());
        // The orphan holds its group's only slot.
        assert_eq!(
            accept(&mut core, now, 2, V6_A2).first(),
            Some(&rejected(ConnId(2), V6_A2, LimitKind::ConnectionsPerIp))
        );
        assert!(admitted(&accept(&mut core, now, 3, V6_B)));
        actions.clear();
        let file = FileId(1);
        core.handle(
            now,
            Event::FileFailed {
                file,
                error: FileError::NotFound,
            },
            &mut actions,
        );
        let fourth = accept(&mut core, now, 4, V6_A3);
        assert!(admitted(&fourth), "{fourth:?}");
    }

    /// Refused connections of one group stop at `max_connections_per_ip`
    /// while the global refused pool has room: the next one from that
    /// group is closed at once, unanswered; another group still gets its
    /// 503.
    #[test]
    fn refused_connections_are_capped_per_group() {
        let mut core = Core::new(crate::config::load(GROUPED).unwrap());
        let now = secs(0);
        accept(&mut core, now, 1, V6_A1);
        accept(&mut core, now, 2, V6_A2);
        for (id, peer) in [(3, V6_A3), (4, V6_A1)] {
            let actions = accept(&mut core, now, id, peer);
            assert!(
                !actions.contains(&Action::Close { conn: ConnId(id) }),
                "{actions:?}"
            );
            assert!(!core.send_buffer(ConnId(id)).is_empty(), "a 503 is waiting");
        }
        let conn = ConnId(5);
        assert_eq!(
            accept(&mut core, now, 5, V6_A2),
            [
                rejected(conn, V6_A2, LimitKind::ConnectionsPerIp),
                Action::Close { conn }
            ]
        );
        assert!(!core.conns.contains_key(&conn));
        assert!(core.send_buffer(conn).is_empty());
        // Another group: two counted, then its own refusals get their 503s.
        accept(&mut core, now, 6, V6_B);
        let mapped = IpAddr::V6(Ipv4Addr::new(192, 0, 2, 1).to_ipv6_mapped());
        accept(&mut core, now, 7, mapped);
        assert_eq!(core.counted, 4);
        for id in [8, 9] {
            let peer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
            let actions = accept(&mut core, now, id, peer);
            assert_eq!(
                actions.first(),
                Some(&rejected(ConnId(id), peer, LimitKind::Connections))
            );
            assert!(
                !actions.contains(&Action::Close { conn: ConnId(id) }),
                "{actions:?}"
            );
        }
        // The mapped peer is in the IPv4 group, at its two refusals now.
        let conn = ConnId(10);
        assert_eq!(
            accept(&mut core, now, 10, mapped),
            [
                rejected(conn, mapped, LimitKind::Connections),
                Action::Close { conn }
            ]
        );
        assert_eq!(core.refused, 4);
    }

    /// One counted connection per group: a group's second connection is
    /// refused, its third closed at once, on every close path alike.
    #[test]
    fn group_refused_cap_is_released_on_every_close_path() {
        let config = GROUPED.replace("max_connections_per_ip 2;", "max_connections_per_ip 1;");
        let core = Core::new(crate::config::load(&config).unwrap());
        released_on_every_close_path(core, PEER, |conn| {
            Action::Log(Log::Rejected {
                conn,
                peer: PEER,
                reason: RejectReason::Limit(LimitKind::ConnectionsPerIp),
            })
        });
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
