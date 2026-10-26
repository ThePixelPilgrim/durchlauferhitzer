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
//! always with its full capacity (fill, drain, refill). Capacities are
//! recomputed from the config by the documented formulas, not taken from
//! the core. Which phase an upstream is in (and how many body bytes are
//! still unsent) the sim cannot tell from the bytes alone, since heads are
//! rewritten: the fill-drain-refill check relies on the core's
//! `upstream_body_pending` report for that, and checks the offered
//! buffer against it. Once `run_until_idle` settles, an upstream the
//! backend failed or ended must have been closed. And every upstream gets
//! exactly one well-framed request: what the sim writes to a backend is
//! judged as it goes (the head by the core's request parser, then the body
//! by its `Content-Length` or chunked framing), and a malformed head or
//! body, or any byte after the body's end, is a violation (FR-PROXY-22).

use std::collections::{BTreeMap, VecDeque};

use durchlauferhitzer_core::config::{BackendAddr, Limits};
use durchlauferhitzer_core::http::chunked::{ChunkedValidator, Done};
use durchlauferhitzer_core::http::request::{BodyFraming, HeadLimits, ParseStatus, parse_head};
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
    /// Every byte the backend has received, judged as one request.
    forwarded: Forwarded,
    window: Window,
}

/// What a backend has been sent, judged as exactly one request: its head
/// (until complete), then its body by its framing, then nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Forwarded {
    Head(Vec<u8>),
    /// This many (more than 0) `Content-Length` bytes are still due.
    Length(u64),
    /// A chunked body; `tail` are bytes the validator has not consumed.
    Chunked {
        validator: ChunkedValidator,
        tail: Vec<u8>,
    },
    Done,
}

impl Forwarded {
    pub(crate) fn new() -> Forwarded {
        Forwarded::Head(Vec::new())
    }

    /// Judge `bytes`, the next ones the backend received; why they cannot
    /// be part of one well-framed request, if they cannot.
    pub(crate) fn feed(&mut self, bytes: &[u8], limits: &HeadLimits) -> Result<(), String> {
        match self {
            Forwarded::Head(head) => {
                head.extend_from_slice(bytes);
                match parse_head(head, limits) {
                    Ok(ParseStatus::Incomplete) => Ok(()),
                    Ok(ParseStatus::Complete {
                        head: parsed,
                        consumed,
                    }) => {
                        let rest = head[consumed..].to_vec();
                        *self = match parsed.body {
                            BodyFraming::None => Forwarded::Done,
                            BodyFraming::Length(n) => Forwarded::Length(n),
                            BodyFraming::Chunked => Forwarded::Chunked {
                                validator: ChunkedValidator::new(),
                                tail: Vec::new(),
                            },
                        };
                        self.feed(&rest, limits)
                    }
                    Err(e) => Err(format!("a request head the parser rejects ({e:?})")),
                }
            }
            Forwarded::Length(remaining) => {
                let n = bytes.len() as u64;
                if n > *remaining {
                    return Err(format!(
                        "{} bytes after the end of its Content-Length body",
                        n - *remaining
                    ));
                }
                *remaining -= n;
                if *remaining == 0 {
                    *self = Forwarded::Done;
                }
                Ok(())
            }
            Forwarded::Chunked { validator, tail } => {
                tail.extend_from_slice(bytes);
                let progress = validator
                    .advance(tail)
                    .map_err(|e| format!("a chunked body the validator rejects ({e:?})"))?;
                tail.drain(..progress.consumed);
                match progress.done {
                    Done::No => Ok(()),
                    Done::Yes if tail.is_empty() => {
                        *self = Forwarded::Done;
                        Ok(())
                    }
                    Done::Yes => Err(format!(
                        "{} bytes after the end of its chunked body",
                        tail.len()
                    )),
                }
            }
            Forwarded::Done if bytes.is_empty() => Ok(()),
            Forwarded::Done => Err(format!(
                "{} bytes after the end of its request",
                bytes.len()
            )),
        }
    }
}

/// What the core hands out for an open upstream, as the checks see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UpstreamView {
    pub(crate) interest: Interest,
    pub(crate) recv: usize,
    pub(crate) send: usize,
    /// The core's report of unsent body bytes (`None` outside a body).
    pub(crate) body_pending: Option<usize>,
}

/// The sim's scripted backends and the upstreams the core opened.
#[derive(Debug)]
pub(crate) struct Backends {
    connect: BTreeMap<String, ConnectBehaviour>,
    default_window: Window,
    upstreams: BTreeMap<UpstreamId, Upstream>,
    /// The upstream receive buffer's capacity for this config, by the
    /// documented formula `max(16 KiB, max_head)`.
    pub(crate) recv_bound: usize,
    /// The head buffer's capacity, `max_head + max_headers + 128`.
    pub(crate) head_bound: usize,
    /// Limits to judge a forwarded request head by: the head buffer's
    /// capacity, and room for the proxy's own header lines.
    pub(crate) forwarded_limits: HeadLimits,
}

impl Backends {
    pub(crate) fn new(limits: &Limits) -> Backends {
        Backends {
            connect: BTreeMap::new(),
            default_window: Window::Unlimited,
            upstreams: BTreeMap::new(),
            recv_bound: limits.max_head.max(16 * 1024),
            head_bound: limits.max_head + limits.max_headers + 128,
            forwarded_limits: HeadLimits {
                max_request_line: limits.max_head + limits.max_headers + 128,
                max_head: limits.max_head + limits.max_headers + 128,
                max_headers: limits.max_headers + 3,
            },
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
                (Phase::Connected, Interest::ReadWrite) => {
                    progress |= self.pump_upstream_read(up);
                    // The read may have changed what the core wants.
                    let u = self.upstream(up);
                    if u.phase == Phase::Connected
                        && matches!(u.interest, Interest::Write | Interest::ReadWrite)
                    {
                        progress |= self.pump_upstream_write(up);
                    }
                }
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
        // Field borrows, not `upstream_mut`: the core's buffer is borrowed
        // alongside.
        let Some(u) = self.backends.upstreams.get_mut(&up) else {
            self.violation(&format!("read from unknown up={}", up.0));
        };
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
        let window = self.upstream(up).window.size();
        let pending = self.core.upstream_send_buffer(up);
        let n = pending.len().min(window);
        if n == 0 {
            return false;
        }
        let bytes = pending[..n].to_vec();
        let limits = self.backends.forwarded_limits;
        let u = self.upstream_mut(up);
        u.window.take(n);
        u.received.extend_from_slice(&bytes);
        if let Err(what) = u.forwarded.feed(&bytes, &limits) {
            self.violation(&format!("up={} received {what}", up.0));
        }
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
                        forwarded: Forwarded::new(),
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
            // The capacities follow from the config alone; the core's own
            // report must agree with them.
            let reported = (
                self.core.upstream_recv_capacity(up),
                self.core.upstream_send_capacity(up),
            );
            let expected = (self.backends.recv_bound, self.backends.head_bound);
            if reported != expected {
                self.violation(&format!(
                    "up={} reports capacities {reported:?} (receive, send), not {expected:?}",
                    up.0
                ));
            }
            let view = UpstreamView {
                interest,
                recv: self.core.upstream_recv_buffer(up).len(),
                send: self.core.upstream_send_buffer(up).len(),
                body_pending: self.core.upstream_body_pending(up),
            };
            self.check_upstream_view(up, &view);
        }
    }

    /// Judge what the core offers for open upstream `up` against the
    /// capacities the config implies.
    pub(crate) fn check_upstream_view(&self, up: UpstreamId, view: &UpstreamView) {
        let (recv_cap, send_cap) = (self.backends.recv_bound, self.backends.head_bound);
        let UpstreamView {
            interest,
            recv,
            send,
            body_pending,
        } = *view;
        if send > send_cap {
            self.violation(&format!(
                "up={} upstream_send_buffer {send} bytes exceeds capacity {send_cap}",
                up.0
            ));
        }
        if recv > recv_cap {
            self.violation(&format!(
                "up={} upstream_recv_buffer {recv} bytes exceeds capacity {recv_cap}",
                up.0
            ));
        }
        if matches!(interest, Interest::Read | Interest::ReadWrite) && recv == 0 {
            self.violation(&format!(
                "up={} wants to read but upstream_recv_buffer is empty",
                up.0
            ));
        }
        if matches!(interest, Interest::Write | Interest::ReadWrite) && send == 0 {
            self.violation(&format!(
                "up={} wants to write but upstream_send_buffer is empty",
                up.0
            ));
        }
        match body_pending {
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

    /// Once nothing moves: no connected upstream keeps bytes to write
    /// without `Write` interest, and none that failed or reached end of
    /// stream is left unclosed by the core.
    pub(crate) fn check_upstreams_settled(&self) {
        for (up, u) in &self.backends.upstreams {
            if u.phase == Phase::Connected
                && !matches!(u.interest, Interest::Write | Interest::ReadWrite)
                && !self.core.upstream_send_buffer(*up).is_empty()
            {
                self.violation(&format!(
                    "up={} has pending send bytes but interest stays {:?}",
                    up.0, u.interest
                ));
            }
            if u.phase == Phase::Failed {
                self.violation(&format!("up={} failed but the core never closed it", up.0));
            }
            if u.eof == Eof::Delivered && u.phase != Phase::Closed {
                self.violation(&format!(
                    "up={} reached end of stream but the core never closed it",
                    up.0
                ));
            }
        }
    }

    /// Test hook: put `up` into the state a driver reaches after the
    /// backend failed (`failed`) or after delivering its end of stream.
    #[cfg(test)]
    pub(crate) fn fake_upstream_end(&mut self, up: UpstreamId, failed: bool) {
        let u = self.upstream_mut(up);
        if failed {
            u.phase = Phase::Failed;
        } else {
            u.eof = Eof::Delivered;
        }
    }

    /// Whether `conn` ever had an upstream.
    pub(crate) fn ever_proxied(&self, conn: ConnId) -> bool {
        self.backends.upstreams.values().any(|u| u.conn == conn)
    }
}
