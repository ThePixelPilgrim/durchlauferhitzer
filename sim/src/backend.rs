//! Scripted backends: the driver's side of the upstream interface.
//!
//! The core asks for a connect to a backend by id; the sim answers it by
//! the behaviour set for that backend's address ([`ConnectBehaviour`],
//! `Accept` by default) on the next pump. A connected upstream is then
//! pumped like a client connection, in the other direction: while its
//! interest is `Write` the sim writes from `upstream_send_buffer` as far as
//! the backend's window allows and records the bytes as what the backend
//! received; while it is `Read` it delivers the bytes the test queued with
//! [`Sim::backend_send`] straight into `upstream_recv_buffer`, then a
//! queued end of stream.
//!
//! Invariants checked after every step, on top of the client ones: no
//! action for an unknown or closed upstream and no second `CloseUpstream`;
//! upstream ids allocated sequentially from 1, each `Connect` during an
//! event of its client and for a listed backend; at most one open upstream
//! per client; no upstream left open after its client closed; upstream
//! buffers within their capacities; no `Read` interest with nothing to read
//! into; and during a body, a read offered only into a drained buffer and
//! always with its full capacity (fill, drain, refill).

use std::collections::{BTreeMap, VecDeque};

use durchlauferhitzer_core::config::BackendAddr;
use durchlauferhitzer_core::{Action, BackendId, ConnError, ConnId, Event, Interest, UpstreamId};

use crate::{Eof, Pending, Sim, SimConnState, Window};

/// How a backend answers a connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectBehaviour {
    /// `UpstreamConnected` on the next pump.
    Accept,
    /// `UpstreamFailed { error: Other }` (`ECONNREFUSED`) on the next pump.
    Refuse,
    /// No answer at all (a connect that never completes).
    Hang,
}

/// Where an upstream is, as the driver sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// `Connect` not yet answered; answered by `behaviour` on the next pump.
    Connecting(ConnectBehaviour),
    Connected,
    /// `UpstreamFailed` delivered; only `CloseUpstream` is still due.
    Failed,
    Closed,
}

#[derive(Debug)]
struct Upstream {
    /// The client whose event led to the `Connect`.
    conn: ConnId,
    backend: BackendId,
    phase: Phase,
    interest: Interest,
    /// Bytes the backend has sent, not yet read by the driver.
    to_core: VecDeque<u8>,
    eof: Eof,
    /// Bytes the backend has received (taken by `take_backend_request`).
    received: Vec<u8>,
    window: Window,
}

/// The sim's scripted backends and the upstreams the core opened.
#[derive(Debug)]
pub(crate) struct Backends {
    connect: BTreeMap<String, ConnectBehaviour>,
    default_window: Window,
    upstreams: BTreeMap<UpstreamId, Upstream>,
}

impl Default for Backends {
    fn default() -> Self {
        Backends {
            connect: BTreeMap::new(),
            default_window: Window::Unlimited,
            upstreams: BTreeMap::new(),
        }
    }
}

impl Backends {
    /// The client an upstream serves, if the sim knows it.
    pub(crate) fn conn_of(&self, up: UpstreamId) -> Option<ConnId> {
        self.upstreams.get(&up).map(|u| u.conn)
    }
}

/// `host:port`, with an IPv6 literal in brackets, as in the config.
fn address(backend: &BackendAddr) -> String {
    if backend.host.contains(':') {
        format!("[{}]:{}", backend.host, backend.port)
    } else {
        format!("{}:{}", backend.host, backend.port)
    }
}

impl Sim {
    /// Connects to backend `addr` (`host:port` as written in the config)
    /// are answered by `behaviour` from now on.
    pub fn set_connect(&mut self, addr: &str, behaviour: ConnectBehaviour) {
        self.backends.connect.insert(addr.to_owned(), behaviour);
    }

    /// The window of upstreams opened from now on (`Unlimited` by default).
    pub fn set_default_backend_window(&mut self, window: Window) {
        self.backends.default_window = window;
    }

    /// How many bytes backend `up` accepts per `UpstreamSent`.
    pub fn set_backend_window(&mut self, up: UpstreamId, window: Window) {
        self.upstream_mut(up).window = window;
    }

    /// Take the bytes backend `up` has received so far.
    pub fn take_backend_request(&mut self, up: UpstreamId) -> Vec<u8> {
        std::mem::take(&mut self.upstream_mut(up).received)
    }

    /// Queue `bytes` from backend `up` to the core.
    pub fn backend_send(&mut self, up: UpstreamId, bytes: &[u8]) {
        let u = self.upstream_mut(up);
        assert_eq!(
            u.eof,
            Eof::NotSent,
            "backend_send after backend_eof on {up:?}"
        );
        u.to_core.extend(bytes);
    }

    /// Queue end of stream from backend `up`, after any queued bytes.
    pub fn backend_eof(&mut self, up: UpstreamId) {
        let u = self.upstream_mut(up);
        if u.eof == Eof::NotSent {
            u.eof = Eof::Queued;
        }
    }

    /// Backend `up` resets the connection; delivers `UpstreamFailed` at
    /// once (unless the upstream is not connected, or already failed or
    /// closed).
    pub fn backend_reset(&mut self, up: UpstreamId) {
        let u = self.upstream_mut(up);
        if u.phase != Phase::Connected {
            return;
        }
        u.phase = Phase::Failed;
        u.to_core.clear();
        self.deliver(Event::UpstreamFailed {
            up,
            error: ConnError::Reset,
        });
    }

    /// Every upstream id the core has opened, in order.
    pub fn upstreams(&self) -> Vec<UpstreamId> {
        self.backends.upstreams.keys().copied().collect()
    }

    /// Upstreams the core has not closed yet (connecting ones included).
    pub fn open_upstreams(&self) -> usize {
        self.backends
            .upstreams
            .values()
            .filter(|u| u.phase != Phase::Closed)
            .count()
    }

    /// The backend upstream `up` was connected to.
    pub fn upstream_backend(&self, up: UpstreamId) -> BackendId {
        self.upstream(up).backend
    }

    /// The core's backends.
    pub fn backends(&self) -> &[BackendAddr] {
        self.core.backends()
    }

    fn upstream(&self, up: UpstreamId) -> &Upstream {
        match self.backends.upstreams.get(&up) {
            Some(u) => u,
            None => panic!("unknown sim upstream {up:?}"),
        }
    }

    fn upstream_mut(&mut self, up: UpstreamId) -> &mut Upstream {
        match self.backends.upstreams.get_mut(&up) {
            Some(u) => u,
            None => panic!("unknown sim upstream {up:?}"),
        }
    }

    /// One pass over all upstreams: answer pending connects, then read or
    /// write by interest. Returns whether anything moved.
    pub(crate) fn pump_upstreams(&mut self) -> bool {
        let mut progress = false;
        let ids: Vec<UpstreamId> = self.backends.upstreams.keys().copied().collect();
        for up in ids {
            let u = self.upstream(up);
            match (u.phase, u.interest) {
                (Phase::Connecting(ConnectBehaviour::Accept), _) => {
                    self.upstream_mut(up).phase = Phase::Connected;
                    self.deliver(Event::UpstreamConnected { up });
                    progress = true;
                }
                (Phase::Connecting(ConnectBehaviour::Refuse), _) => {
                    self.upstream_mut(up).phase = Phase::Failed;
                    self.deliver(Event::UpstreamFailed {
                        up,
                        error: ConnError::Other,
                    });
                    progress = true;
                }
                (Phase::Connected, Interest::Read) => progress |= self.pump_upstream_read(up),
                (Phase::Connected, Interest::Write) => progress |= self.pump_upstream_write(up),
                (Phase::Connecting(ConnectBehaviour::Hang), _)
                | (Phase::Connected, Interest::None)
                | (Phase::Failed | Phase::Closed, _) => {}
            }
        }
        progress
    }

    /// Deliver queued backend bytes into `upstream_recv_buffer`, reported
    /// with `UpstreamReceived` right away; or the queued end of stream.
    fn pump_upstream_read(&mut self, up: UpstreamId) -> bool {
        let u = self.backends.upstreams.get_mut(&up).unwrap();
        if u.to_core.is_empty() {
            if u.eof == Eof::Queued {
                u.eof = Eof::Delivered;
                self.deliver(Event::UpstreamReadClosed { up });
                return true;
            }
            return false;
        }
        let spare = self.core.upstream_recv_buffer(up);
        let n = spare.len().min(u.to_core.len());
        if n == 0 {
            return false;
        }
        for (dst, src) in spare.iter_mut().zip(u.to_core.drain(..n)) {
            *dst = src;
        }
        self.pending_read = Some(Pending::Upstream(up));
        self.deliver(Event::UpstreamReceived { up, len: n });
        true
    }

    /// Write from `upstream_send_buffer` as far as the backend's window
    /// allows.
    fn pump_upstream_write(&mut self, up: UpstreamId) -> bool {
        let pending = self.core.upstream_send_buffer(up);
        let u = self.backends.upstreams.get_mut(&up).unwrap();
        let n = pending.len().min(u.window.size());
        if n == 0 {
            return false;
        }
        u.window.take(n);
        u.received.extend_from_slice(&pending[..n]);
        self.deliver(Event::UpstreamSent { up, len: n });
        true
    }

    /// Perform an upstream action.
    pub(crate) fn apply_upstream(&mut self, action: Action) {
        match action {
            Action::Connect { up, backend } => {
                let Some(conn) = self.context else {
                    self.violation(&format!("Connect up={} outside a connection's event", up.0));
                };
                let expected = self
                    .backends
                    .upstreams
                    .keys()
                    .next_back()
                    .map_or(1, |u| u.0 + 1);
                if up.0 != expected {
                    self.violation(&format!(
                        "Connect up={} but the next id is {expected}",
                        up.0
                    ));
                }
                let Some(addr) = self.core.backends().get(backend.0 as usize).map(address) else {
                    self.violation(&format!(
                        "Connect up={} for unknown backend={}",
                        up.0, backend.0
                    ));
                };
                if let Some((other, _)) = self
                    .backends
                    .upstreams
                    .iter()
                    .find(|(_, u)| u.conn == conn && u.phase != Phase::Closed)
                {
                    self.violation(&format!(
                        "Connect up={} while conn={} still has up={} open",
                        up.0, conn.0, other.0
                    ));
                }
                let behaviour = self
                    .backends
                    .connect
                    .get(&addr)
                    .copied()
                    .unwrap_or(ConnectBehaviour::Accept);
                let window = self.backends.default_window;
                self.backends.upstreams.insert(
                    up,
                    Upstream {
                        conn,
                        backend,
                        phase: Phase::Connecting(behaviour),
                        interest: Interest::None,
                        to_core: VecDeque::new(),
                        eof: Eof::NotSent,
                        received: Vec::new(),
                        window,
                    },
                );
            }
            Action::SetUpstreamInterest { up, interest } => {
                match self.backends.upstreams.get_mut(&up) {
                    Some(u) if u.phase != Phase::Closed => u.interest = interest,
                    Some(_) | None => self.violation(&format!(
                        "SetUpstreamInterest for unknown or closed up={}",
                        up.0
                    )),
                }
            }
            Action::CloseUpstream { up } => match self.backends.upstreams.get_mut(&up) {
                Some(u) if u.phase == Phase::Closed => {
                    self.violation(&format!("second CloseUpstream for up={}", up.0))
                }
                Some(u) => {
                    // Nothing more is delivered for a closed upstream,
                    // including a connect answer still pending.
                    u.phase = Phase::Closed;
                    u.interest = Interest::None;
                }
                None => self.violation(&format!("CloseUpstream for unknown up={}", up.0)),
            },
            Action::SetInterest { .. }
            | Action::ShutdownWrite { .. }
            | Action::Close { .. }
            | Action::OpenFile { .. }
            | Action::Readahead { .. }
            | Action::CloseFile { .. }
            | Action::Log(_) => unreachable!("not an upstream action"),
        }
    }

    /// The upstream invariants that hold after every step.
    pub(crate) fn check_upstreams(&mut self) {
        let open: Vec<(UpstreamId, ConnId, Interest)> = self
            .backends
            .upstreams
            .iter()
            .filter(|(_, u)| u.phase != Phase::Closed)
            .map(|(id, u)| (*id, u.conn, u.interest))
            .collect();
        for (up, conn, interest) in open {
            if self.conn(conn).state == SimConnState::Closed {
                self.violation(&format!(
                    "up={} left open after conn={} closed",
                    up.0, conn.0
                ));
            }
            let send = self.core.upstream_send_buffer(up).len();
            let send_cap = self.core.upstream_send_capacity(up);
            if send > send_cap {
                self.violation(&format!(
                    "up={} upstream_send_buffer {send} bytes exceeds capacity {send_cap}",
                    up.0
                ));
            }
            let recv_cap = self.core.upstream_recv_capacity(up);
            let pending = self.core.upstream_body_pending(up);
            let recv = self.core.upstream_recv_buffer(up).len();
            if recv > recv_cap {
                self.violation(&format!(
                    "up={} upstream_recv_buffer {recv} bytes exceeds capacity {recv_cap}",
                    up.0
                ));
            }
            if interest == Interest::Read && recv == 0 {
                self.violation(&format!(
                    "up={} wants to read but upstream_recv_buffer is empty",
                    up.0
                ));
            }
            match pending {
                Some(unsent) if recv > 0 && unsent > 0 => self.violation(&format!(
                    "up={} offers a read while {unsent} body bytes are unsent",
                    up.0
                )),
                Some(_) if recv > 0 && recv != recv_cap => self.violation(&format!(
                    "up={} offers a body read of {recv} bytes, not its capacity {recv_cap}",
                    up.0
                )),
                Some(_) | None => {}
            }
        }
    }

    /// No connected upstream keeps bytes to write without `Write` interest.
    pub(crate) fn check_upstreams_not_stuck(&self) {
        let stuck = self.backends.upstreams.iter().find(|(id, u)| {
            u.phase == Phase::Connected
                && u.interest != Interest::Write
                && !self.core.upstream_send_buffer(**id).is_empty()
        });
        if let Some((up, u)) = stuck {
            self.violation(&format!(
                "up={} has pending send bytes but interest stays {:?}",
                up.0, u.interest
            ));
        }
    }
}
