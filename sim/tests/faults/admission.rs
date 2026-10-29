//! The static net's admission model (FR-CONN-09, -10, -11, -13), written
//! from the spec, not the core: every connection the net opens is checked
//! at once against what the limits allow, counting from what the network
//! and the disk show (an open connection, or a closed one whose file open
//! is unanswered, an orphan) and grouping peers by the configured
//! `ipv6_prefix`.

use std::net::Ipv6Addr;

use super::*;

/// `max_connections` and `max_connections_per_ip` of `RANDOM_CONFIG` and
/// `LONG_RANDOM_CONFIG`.
const MAX_CONNECTIONS: usize = 6;
const MAX_PER_GROUP: usize = 3;

/// The peers a seed's random clients come from, and the `ipv6_prefix` it
/// configures (FR-CONN-13). One seed in three draws plain IPv4 peers, one
/// IPv6 peers (two in one /64, one in another; the prefix 64 by default,
/// or set to 48, which joins the two /64s, or 128, which parts every
/// address), one IPv4 peers and the IPv4-mapped form of one of them. The
/// pool follows from the seed alone, so the schedules stay as they were.
pub(crate) fn peer_pool(seed: u64) -> ([IpAddr; 3], Prefix) {
    let v4 = |last| IpAddr::V4(Ipv4Addr::new(192, 0, 2, last));
    let v6 = |net, host| IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, net, 0, 0, 0, host));
    match seed % 3 {
        0 => ([v4(1), v4(2), v4(3)], Prefix::Default),
        1 => {
            let prefix = match (seed / 3) % 3 {
                0 => Prefix::Default,
                1 => Prefix::Set(48),
                _ => Prefix::Set(128),
            };
            ([v6(1, 1), v6(1, 0xffff), v6(2, 1)], prefix)
        }
        _ => {
            let mapped = IpAddr::V6(Ipv4Addr::new(192, 0, 2, 1).to_ipv6_mapped());
            ([v4(1), mapped, v4(3)], Prefix::Default)
        }
    }
}

/// A seed's `ipv6_prefix`: left out of the config, or set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Prefix {
    /// Not in the config: the spec's default, 64 (FR-CFG-20).
    Default,
    Set(u8),
}

impl Prefix {
    pub(crate) fn bits(self) -> u8 {
        match self {
            Prefix::Default => 64,
            Prefix::Set(bits) => bits,
        }
    }

    /// `config` with this prefix in its `limits` block.
    pub(crate) fn apply(self, config: &str) -> String {
        match self {
            Prefix::Default => config.to_owned(),
            Prefix::Set(bits) => config.replace(
                "max_connections_per_ip 3;",
                &format!("max_connections_per_ip 3;\n  ipv6_prefix {bits};"),
            ),
        }
    }
}

/// The group a peer is counted in, by the spec's rule: an IPv4 peer, and
/// an IPv4-mapped IPv6 peer, by its IPv4 address; an IPv6 peer by its
/// first `prefix` bits.
fn group_of(peer: IpAddr, prefix: u8) -> IpAddr {
    match peer {
        IpAddr::V4(v4) => IpAddr::V4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let mut octets = v6.octets();
                for (i, octet) in octets.iter_mut().enumerate() {
                    let kept = usize::from(prefix).saturating_sub(i * 8).min(8);
                    // The first `kept` bits of this octet stay.
                    let mask = u8::try_from((0xff00_u16 >> kept) & 0xff).unwrap_or(0);
                    *octet &= mask;
                }
                IpAddr::V6(Ipv6Addr::from(octets))
            }
        },
    }
}

/// What the model decided for a connection at accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Counted toward the limits.
    Counted,
    /// Refused with a 503: holds a place in the refused pool.
    Refused,
    /// Refused and closed at once, nothing written.
    Dropped,
}

/// The net's connections as the model admitted them.
pub(crate) struct Admission {
    prefix: u8,
    conns: Vec<(ConnId, IpAddr, Verdict)>,
    pub(crate) coverage: AdmissionCoverage,
}

/// What the model saw, summed over seeds.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct AdmissionCoverage {
    /// A peer admitted, or refused for the per-IP limit, while its group
    /// held a counted connection of another address.
    pub(crate) shared_group: usize,
    /// A refusal closed at once because its group held
    /// `max_connections_per_ip` refused connections while the global
    /// refused pool still had room (FR-CONN-13).
    pub(crate) group_dropped: usize,
}

impl AdmissionCoverage {
    pub(crate) fn add(&mut self, other: AdmissionCoverage) {
        self.shared_group += other.shared_group;
        self.group_dropped += other.group_dropped;
    }
}

impl Admission {
    pub(crate) fn new(prefix: Prefix) -> Admission {
        Admission {
            prefix: prefix.bits(),
            conns: Vec::new(),
            coverage: AdmissionCoverage::default(),
        }
    }

    /// Open a connection from `peer` and check the core's answer: accepted
    /// (nothing logged), or refused with its reason and then answered or,
    /// with the refused pool full in all or for its group, closed at once.
    pub(crate) fn connect(&mut self, sim: &mut Sim, marks: &Marks, peer: IpAddr) -> ConnId {
        let group = group_of(peer, self.prefix);
        let mut counted = 0;
        let mut counted_group = 0;
        let mut other_address = false;
        let mut refused = 0;
        let mut refused_group = 0;
        for &(id, p, verdict) in &self.conns {
            let same = group_of(p, self.prefix) == group;
            let open = sim.state(id) != SimConnState::Closed;
            match verdict {
                // An orphaned open keeps its slots (FR-CONN-11).
                Verdict::Counted if open || sim.opening_file(id) => {
                    counted += 1;
                    if same {
                        counted_group += 1;
                        other_address |= p != peer;
                    }
                }
                Verdict::Refused if open => {
                    refused += 1;
                    refused_group += usize::from(same);
                }
                Verdict::Counted | Verdict::Refused | Verdict::Dropped => {}
            }
        }
        let reason = if marks.shut_down() {
            Some(RejectReason::ShuttingDown)
        } else if counted >= MAX_CONNECTIONS {
            Some(RejectReason::Limit(LimitKind::Connections))
        } else if counted_group >= MAX_PER_GROUP {
            Some(RejectReason::Limit(LimitKind::ConnectionsPerIp))
        } else {
            None
        };
        let shares = match &reason {
            None | Some(RejectReason::Limit(LimitKind::ConnectionsPerIp)) => other_address,
            Some(_) => false,
        };
        self.coverage.shared_group += usize::from(shares);
        let logged = sim.logs().len();
        let conn = sim.connect(peer);
        let logs = sim.logs().get(logged..).unwrap_or_default().to_vec();
        let state = sim.state(conn);
        let fail = |what: &str| -> ! {
            panic!(
                "admission of conn={} from {peer} (group {group}): {what}; model: counted={counted} \
                 in group={counted_group} refused={refused} in group={refused_group}; \
                 logs {logs:?}, state {state:?}\ntrace:\n{}",
                conn.0,
                sim.trace()
            )
        };
        let verdict = match reason {
            None => {
                if !logs.is_empty() || state != SimConnState::Open {
                    fail("must be accepted");
                }
                Verdict::Counted
            }
            Some(reason) => {
                let expected = Log::Rejected { conn, peer, reason };
                if logs != [expected.clone()] {
                    fail(&format!("must be logged {expected:?}"));
                }
                if refused >= MAX_CONNECTIONS || refused_group >= MAX_PER_GROUP {
                    if state != SimConnState::Closed {
                        fail("must be closed at once");
                    }
                    if refused < MAX_CONNECTIONS {
                        self.coverage.group_dropped += 1;
                    }
                    Verdict::Dropped
                } else {
                    if state == SimConnState::Closed {
                        fail("must be answered 503");
                    }
                    Verdict::Refused
                }
            }
        };
        self.conns.push((conn, peer, verdict));
        conn
    }
}

#[test]
fn group_of_follows_the_prefix() {
    let v6 = |a: u16, b: u16| IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, a, b, 9, 9, 9, 9));
    assert_eq!(
        group_of(v6(0x1234, 5), 64),
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0x1234, 5, 0, 0, 0, 0))
    );
    assert_eq!(
        group_of(v6(0x1234, 5), 36),
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0x1000, 0, 0, 0, 0, 0))
    );
    assert_eq!(group_of(v6(1, 2), 128), v6(1, 2));
    let mapped = IpAddr::V6(Ipv4Addr::new(192, 0, 2, 1).to_ipv6_mapped());
    assert_eq!(
        group_of(mapped, 32),
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))
    );
}
