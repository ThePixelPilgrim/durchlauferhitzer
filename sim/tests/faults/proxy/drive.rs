//! Driving the proxy net: stories (one client connection and its
//! exchanges), interleaved at random within a round, their backends
//! acting on what they received, and the timed scenarios.

use super::*;

/// What happened in one exchange, as the driver saw it.
#[derive(Debug, Default)]
pub(crate) struct Run {
    pub(crate) up: Option<UpstreamId>,
    /// Everything the backend received on `up` for this exchange.
    pub(crate) got: Vec<u8>,
    /// Script bytes sent so far.
    pub(crate) sent: usize,
    /// A slow connect was let through.
    pub(crate) accepted: bool,
    /// The script's ending was applied.
    pub(crate) ended: bool,
    /// `up` was reused (FR-PROXY-02): the client and exchange it served
    /// before.
    pub(crate) reused: Option<(ConnId, usize)>,
    /// The reuse's silence check failed: the request went out on a fresh
    /// connection instead (`up`).
    pub(crate) busy: bool,
    /// The reused `up` turned out dead this way (`Stale`), and whether
    /// the backend did it yet.
    pub(crate) stale: Option<Stale>,
    pub(crate) stale_done: bool,
    /// The core replayed the request after this failure (FR-PROXY-26):
    /// `up` is the replay's fresh upstream, `stale_got` what the dead one
    /// received.
    pub(crate) retried: Option<BackendFailure>,
    pub(crate) stale_got: Vec<u8>,
    /// Its backend wrote to `up` while it was idle (`poke`).
    pub(crate) poked: bool,
    /// The number of its request on `up` (`BackendChunk::request`).
    pub(crate) seq: usize,
}

#[derive(Debug)]
pub(crate) struct Story {
    pub(crate) conn: ConnId,
    pub(crate) peer: IpAddr,
    pub(crate) exchanges: Vec<Exchange>,
    pub(crate) runs: Vec<Run>,
    pub(crate) received: Vec<u8>,
    /// The sim state when the client reset, if it did.
    pub(crate) reset: Option<SimConnState>,
    /// The client closed its sending side after its last request.
    eof: bool,
    /// The client closed its sending side at a random point: the story
    /// may end anywhere, silently (FR-CONN-12) or, in the middle of a
    /// forwarded request body, with `Rejected { Body(ClosedEarly) }`
    /// (FR-PROXY-26).
    pub(crate) eof_anywhere: bool,
    /// Routes whose backend accepts slowly in this round.
    slow: [bool; 2],
    /// The story waits while the others of its round run.
    pub(crate) paused: bool,
    /// The exchange whose request is being sent, and how far.
    send_ex: usize,
    send_off: usize,
    /// Windows scaled up for large bodies.
    large: bool,
}

/// One thing the driver may do next.
#[derive(Debug, Clone, Copy)]
enum Step {
    Send(usize),
    Backend(usize, usize),
    Timed(usize),
    Accept(usize),
    /// A `Late` stale connection dies, its backend having the request
    /// head.
    StaleKill(usize),
    Eof,
    EofAnywhere,
    ClientWindow,
    BackendWindow(usize),
    Reset,
}

/// Peers: IPv4, IPv6 and IPv4-mapped IPv6 (logged and forwarded as IPv4).
fn random_peer(rng: &mut fastrand::Rng, n: u8) -> (IpAddr, String) {
    match rng.u8(0..4) {
        0 => {
            let ip: IpAddr = format!("2001:db8::{n:x}")
                .parse()
                .unwrap_or(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST));
            (ip, format!("2001:db8::{n:x}"))
        }
        1 => {
            let ip: IpAddr = format!("::ffff:192.0.2.{n}")
                .parse()
                .unwrap_or(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST));
            (ip, format!("192.0.2.{n}"))
        }
        _ => (
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, n)),
            format!("192.0.2.{n}"),
        ),
    }
}

impl Story {
    pub(crate) fn new(
        sim: &mut Sim,
        rng: &mut fastrand::Rng,
        net: &NetConfig,
        n: u8,
        draw: &Draw,
        slow: [bool; 2],
    ) -> Story {
        let (peer, peer_text) = random_peer(rng, n);
        let conn = sim.connect(peer);
        let who = Who { conn, peer };
        let mut exchanges = Vec::new();
        let count = rng.usize(1..6);
        // A timed exchange sits anywhere among untimed ones.
        let timed_at = draw.timed.then(|| rng.usize(0..count));
        let pooly = rng.u8(0..5) < 2;
        for i in 0..count {
            let d = Draw {
                connect: draw.connect,
                timed: timed_at == Some(i),
                pooly,
            };
            let mut ex = random_exchange(rng, net, &who, &peer_text, &d);
            // A dead pooled connection may close a client whose body it
            // left unread (`pool.rs`); a timed scenario behind it needs
            // the connection.
            let body = match &ex.req.verdict {
                Verdict::Forward(fwd) | Verdict::BadBody { fwd, .. } => {
                    fwd.bytes.len() > fwd.head_len
                }
                Verdict::Parse(_) | Verdict::Answer(..) => false,
            };
            if body && timed_at.is_some_and(|t| t > i) {
                ex.stale = None;
            }
            let keeps = ex.keeps_alive();
            exchanges.push(ex);
            if !keeps {
                break;
            }
        }
        let large = exchanges.iter().any(|e| {
            e.req.wire.len() > 20_000 || e.script.as_ref().is_some_and(|s| s.wire.len() > 20_000)
        });
        if rng.bool() {
            sim.set_read_chunk(
                conn,
                if large {
                    rng.usize(512..20_000)
                } else {
                    rng.usize(1..64)
                },
            );
        }
        let runs = exchanges.iter().map(|_| Run::default()).collect();
        Story {
            conn,
            peer,
            exchanges,
            runs,
            received: Vec::new(),
            reset: None,
            eof: false,
            eof_anywhere: false,
            slow,
            paused: false,
            send_ex: 0,
            send_off: 0,
            large,
        }
    }

    /// Bytes and logs the client must have once exchanges `..=i` are
    /// done (all of them certain to keep the connection).
    fn resolved(&self, sim: &Sim, net: &NetConfig, i: usize) -> bool {
        let who = Who {
            conn: self.conn,
            peer: self.peer,
        };
        // On a reused connection the outcome may be the retry's or a
        // dead connection's (`pool.rs`).
        let (mut bytes, mut logs) = (0, 0);
        for (ex, run) in self.exchanges[..=i].iter().zip(&self.runs) {
            let first = match run.reused {
                // Not yet what the spec says (a retry still to come): not
                // resolved.
                Some(_) => match reuse_alts(ex.alts.clone(), ex, run, &who, net, false) {
                    Ok(alts) => alts.into_iter().next(),
                    Err(_) => return false,
                },
                None => ex.alts.first().cloned(),
            };
            if let Some(alt) = first {
                bytes += alt.bytes.len();
                logs += alt.logs.len();
            }
        }
        let logged = sim
            .logs()
            .iter()
            .filter(|l| conn_of(l) == self.conn)
            .count();
        self.received.len() >= bytes && logged >= logs
    }

    /// Whether the client may send (more of) exchange `send_ex` now.
    fn next_send(&mut self, sim: &Sim, net: &NetConfig) -> Option<Step> {
        if self.reset.is_some() || self.eof || self.paused {
            return None;
        }
        loop {
            let i = self.send_ex;
            let ex = self.exchanges.get(i)?;
            if i > 0 {
                let prev = &self.exchanges[i - 1];
                let pipelined =
                    prev.keeps_alive() && prev.timing == Timing::Normal && !ex.timing.timed();
                if !pipelined && !self.resolved(sim, net, i - 1) {
                    return None;
                }
            }
            if ex.timing.timed() && self.send_off == 0 {
                return Some(Step::Timed(i));
            }
            if self.send_off < ex.send_len() {
                return Some(Step::Send(i));
            }
            if !ex.keeps_alive() {
                return None;
            }
            self.send_ex += 1;
            self.send_off = 0;
        }
    }

    /// Whether the client may end its stream now: its last request is
    /// fully sent and needs no more bytes (a half-close, FR-PROXY-26).
    fn eof_step(&self) -> Option<Step> {
        let last = self.exchanges.len().checked_sub(1)?;
        let ex = &self.exchanges[last];
        (self.reset.is_none()
            && !self.eof
            && self.send_ex == last
            && self.send_off == ex.send_len()
            && ex.timing == Timing::Normal)
            .then_some(Step::Eof)
    }

    /// Whether the backend of exchange `i` has something to do.
    fn backend_step(&self, i: usize) -> Option<Step> {
        let ex = &self.exchanges[i];
        let run = &self.runs[i];
        if let (Some(_), Some(stale), false) = (run.up, run.stale, run.stale_done) {
            if stale.late() {
                let head_len = match &ex.req.verdict {
                    Verdict::Forward(fwd) | Verdict::BadBody { fwd, .. } => fwd.head_len,
                    Verdict::Parse(_) | Verdict::Answer(..) => 0,
                };
                return (run.got.len() >= head_len).then_some(Step::StaleKill(i));
            }
        }
        if let (Some(_), Some(route)) = (run.up, ex.route) {
            if self.slow[route] && !run.accepted {
                return Some(Step::Accept(i));
            }
        }
        let (script, Some(_)) = (ex.script.as_ref()?, run.up) else {
            return None;
        };
        if ex.timing.timed() || run.ended {
            return None;
        }
        let Verdict::Forward(fwd) = &ex.req.verdict else {
            return None;
        };
        let trigger = match ex.timing {
            Timing::Early { .. } => fwd.head_len,
            Timing::Eager => 0,
            Timing::Normal
            | Timing::ConnectHang
            | Timing::Silent
            | Timing::PartialHead(_)
            | Timing::ClientBodyStall(_)
            | Timing::BackendBodyStall(_)
            | Timing::RelayStall(_)
            | Timing::ClientStall
            | Timing::HeadDuringUpload { .. } => fwd.bytes.len(),
        };
        (run.got.len() >= trigger).then_some(Step::Backend(i, script_target(script)))
    }
}

/// How many script bytes are sent before its ending.
fn script_target(script: &Script) -> usize {
    match script.ending {
        Ending::Done { .. } => script.wire.len(),
        Ending::Reset(k) | Ending::ResetAfter(k) | Ending::Eof(k) => k,
    }
}

fn window(rng: &mut fastrand::Rng, large: bool) -> Window {
    match (rng.u8(0..3), large) {
        (0, _) => Window::Unlimited,
        (_, true) => Window::Bytes(rng.usize(4096..65_536)),
        (_, false) => Window::Bytes(rng.usize(1..200)),
    }
}

/// Whether a round runs a timed scenario: then the clock must run out only
/// deadlines the model knows, so no shutdown falls among its steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoundKind {
    Plain,
    Timed,
}

/// Run one round: its stories interleaved until none can move; in a plain
/// round the seed's shutdown may fall among the steps.
pub(crate) fn run_round(
    sim: &mut Sim,
    rng: &mut fastrand::Rng,
    net: &NetConfig,
    stories: &mut [Story],
    assign: &mut Assign,
    kind: RoundKind,
    shutdown: &mut Shutdown,
) {
    let mut steps = 0;
    loop {
        sync(sim, stories, assign);
        check_pool(sim, net);
        match kind {
            RoundKind::Plain => shutdown.tick(sim, stories),
            RoundKind::Timed => {}
        }
        let mut options = Vec::new();
        for (s, story) in stories.iter_mut().enumerate() {
            if let Some(step) = story.next_send(sim, net) {
                options.push((s, step));
            }
            for i in 0..story.exchanges.len() {
                if let Some(step) = story.backend_step(i) {
                    options.push((s, step));
                }
            }
            if let Some(step) = story.eof_step() {
                options.push((s, step));
            }
        }
        if options.is_empty() {
            sim.run_until_idle();
            sync(sim, stories, assign);
            let more = stories.iter_mut().any(|s| {
                s.next_send(sim, net).is_some()
                    || (0..s.exchanges.len()).any(|i| s.backend_step(i).is_some())
            });
            if !more {
                break;
            }
            continue;
        }
        // Now and then a window change or a client reset instead.
        let (s, step) = match rng.u8(0..20) {
            0 | 1 => (rng.usize(0..stories.len()), Step::ClientWindow),
            2 => {
                let s = rng.usize(0..stories.len());
                (s, Step::BackendWindow(rng.usize(0..stories[s].runs.len())))
            }
            3 if rng.u8(0..8) == 0 => (rng.usize(0..stories.len()), Step::Reset),
            4 if rng.u8(0..8) == 0 => (rng.usize(0..stories.len()), Step::EofAnywhere),
            _ => options[rng.usize(0..options.len())],
        };
        do_step(sim, rng, net, &mut stories[s], step, assign);
        if rng.u8(0..4) != 0 {
            sim.run_until_idle();
        }
        steps += 1;
        assert!(
            steps < 200_000,
            "the round does not settle\ntrace:\n{}",
            sim.trace()
        );
    }
}

fn do_step(
    sim: &mut Sim,
    rng: &mut fastrand::Rng,
    net: &NetConfig,
    story: &mut Story,
    step: Step,
    assign: &mut Assign,
) {
    match step {
        Step::Send(i) => {
            // The next request right behind a chunked body still in flight.
            if story.send_off == 0
                && i > 0
                && story.exchanges[i - 1].req.chunked
                && !story.resolved(sim, net, i - 1)
            {
                story.exchanges[i - 1].alts[0]
                    .tags
                    .push("pipelined after a chunked body");
            }
            let ex = &story.exchanges[i];
            let end = ex.send_len();
            let n = rng.usize(1..=end - story.send_off);
            sim.send(story.conn, &ex.req.wire[story.send_off..story.send_off + n]);
            story.send_off += n;
        }
        Step::Backend(i, target) => {
            let run = &mut story.runs[i];
            let (Some(up), Some(script)) = (run.up, story.exchanges[i].script.as_ref()) else {
                return;
            };
            if run.sent < target {
                // An `exact` script goes in one piece (`Ctx::exact`).
                let mut n = if script.exact {
                    target - run.sent
                } else {
                    rng.usize(1..=target - run.sent)
                };
                // What a backend sends after a response's end (bytes, its
                // end of stream, a reset) goes with the response's last
                // byte, before anything else happens: a real backend writes
                // it at once, and whether the connection may be reused
                // (FR-PROXY-02) is then settled before another request
                // could take it.
                let finishing = run.sent + n >= script.body_end;
                if finishing {
                    n = target - run.sent;
                }
                let piece = &script.wire[run.sent..run.sent + n];
                run.sent += n;
                match script.ending {
                    // The last piece goes with the reset behind it.
                    Ending::ResetAfter(_) if run.sent == target => {
                        run.ended = true;
                        sim.backend_reset_after(up, piece);
                    }
                    Ending::ResetAfter(_)
                    | Ending::Done { .. }
                    | Ending::Reset(_)
                    | Ending::Eof(_) => sim.backend_send(up, piece),
                }
                if finishing && !run.ended {
                    end_script(sim, run, up, script.ending);
                }
                if finishing {
                    sim.settle_upstreams();
                }
            } else {
                end_script(sim, run, up, script.ending);
            }
        }
        Step::ClientWindow => {
            if story.reset.is_none() && sim.state(story.conn) != SimConnState::Closed {
                sim.set_client_window(story.conn, window(rng, story.large));
            }
        }
        Step::BackendWindow(i) => {
            if let Some(up) = story.runs.get(i).and_then(|r| r.up) {
                sim.set_backend_window(up, window(rng, story.large));
            }
        }
        Step::Reset => {
            if story.reset.is_none() {
                story.reset = Some(sim.state(story.conn));
                sim.reset(story.conn);
            }
        }
        Step::Timed(i) => run_timed(sim, rng, net, story, i, assign),
        Step::StaleKill(i) => {
            let run = &mut story.runs[i];
            run.stale_done = true;
            match (run.up, run.stale) {
                (Some(up), Some(stale)) if sim.upstream_holder(up).is_some() => {
                    stale.kill(sim, up);
                    sim.run_until_idle();
                }
                // The core let go of it first (the client went): it never
                // died under the request.
                _ => run.stale = None,
            }
        }
        Step::Accept(i) => {
            if let Some(up) = story.runs[i].up {
                sim.accept_upstream(up);
                story.runs[i].accepted = true;
            }
        }
        Step::Eof => {
            story.eof = true;
            sim.send_eof(story.conn);
        }
        Step::EofAnywhere => {
            if story.reset.is_none() && !story.eof {
                story.eof = true;
                story.eof_anywhere = true;
                sim.send_eof(story.conn);
            }
        }
    }
}

/// Apply the script's ending once its bytes are sent.
fn end_script(sim: &mut Sim, run: &mut Run, up: UpstreamId, ending: Ending) {
    run.ended = true;
    match ending {
        Ending::Done { eof: true } | Ending::Eof(_) => sim.backend_eof(up),
        Ending::Done { eof: false } => {}
        Ending::Reset(_) => {
            sim.run_until_idle();
            sim.backend_reset(up);
        }
        Ending::ResetAfter(_) => sim.backend_reset_after(up, &[]),
    }
}

/// Advance by `d` but 1 ms, check that nothing happened on the
/// connection, then by the last millisecond.
fn wait_for(sim: &mut Sim, story: &mut Story, d: Duration, assign: &mut Assign) {
    sync(sim, std::slice::from_mut(story), assign);
    let before_logs = sim.logs().len();
    let before = story.received.len();
    sim.advance(d - Duration::from_millis(1));
    sync(sim, std::slice::from_mut(story), assign);
    assert!(
        sim.logs().len() == before_logs && story.received.len() == before,
        "conn={} acted {}ms before its deadline\ntrace:\n{}",
        story.conn.0,
        1,
        sim.trace()
    );
    sim.advance(Duration::from_millis(1));
    sync(sim, std::slice::from_mut(story), assign);
}

/// A timed scenario, alone in its round: set the stall up, then let the
/// clock run to the deadline it must hit.
fn run_timed(
    sim: &mut Sim,
    rng: &mut fastrand::Rng,
    net: &NetConfig,
    story: &mut Story,
    i: usize,
    assign: &mut Assign,
) {
    let conn = story.conn;
    let ex = &story.exchanges[i];
    let route = ex.route.map(|r| &net.routes[r]);
    let Some(route) = route else {
        return;
    };
    let wire = ex.req.wire.clone();
    let send_len = ex.send_len();
    let timing = ex.timing;
    let script_wire = ex
        .script
        .as_ref()
        .map(|s| s.wire.clone())
        .unwrap_or_default();
    let interim_ends = ex
        .script
        .as_ref()
        .map(|s| s.interim_ends.clone())
        .unwrap_or_default();
    let head_len = match &ex.req.verdict {
        Verdict::Forward(f) => f.head_len,
        Verdict::Parse(_) | Verdict::Answer(..) | Verdict::BadBody { .. } => 0,
    };
    // Alone with the clock: every idle connection runs out first, so the
    // scenario's exchange connects afresh, by the connect and window it
    // sets up (FR-PROXY-02).
    clear_pool(sim, net);
    story.send_off = send_len;
    story.runs[i].ended = true;
    // Half the time in one piece, so a whole request can arrive with its
    // head.
    let whole = rng.bool();
    let mut send = |sim: &mut Sim| {
        let mut at = 0;
        while at < send_len {
            let n = if whole {
                send_len
            } else {
                rng.usize(1..=send_len - at)
            };
            sim.send(conn, &wire[at..at + n]);
            at += n;
            if rng.bool() {
                sim.run_until_idle();
            }
        }
        sim.run_until_idle();
    };
    match timing {
        Timing::ConnectHang => {
            sim.set_connect(route.addr, ConnectBehaviour::Hang);
            send(sim);
            sim.set_connect(route.addr, ConnectBehaviour::Accept);
            wait_for(sim, story, net.connect, assign);
        }
        Timing::Silent | Timing::ClientBodyStall(_) => {
            send(sim);
            let d = if timing == Timing::Silent {
                net.response
            } else {
                net.body_idle
            };
            wait_for(sim, story, d, assign);
        }
        Timing::PartialHead(k) | Timing::RelayStall(k) => {
            send(sim);
            sync(sim, std::slice::from_mut(story), assign);
            let Some(up) = story.runs[i].up else {
                panic!("conn={} never connected\ntrace:\n{}", conn.0, sim.trace());
            };
            sim.backend_send(up, &script_wire[..k]);
            sim.run_until_idle();
            let d = if matches!(timing, Timing::PartialHead(_)) {
                net.response
            } else {
                net.body_idle
            };
            wait_for(sim, story, d, assign);
        }
        Timing::BackendBodyStall(j) => {
            sim.set_default_backend_window(Window::Budget(head_len + j));
            send(sim);
            sim.set_default_backend_window(Window::Unlimited);
            wait_for(sim, story, net.body_idle, assign);
        }
        Timing::ClientStall => {
            sim.set_client_window(conn, Window::Stalled);
            send(sim);
            sync(sim, std::slice::from_mut(story), assign);
            let Some(up) = story.runs[i].up else {
                panic!("conn={} never connected\ntrace:\n{}", conn.0, sim.trace());
            };
            sim.backend_send(up, &script_wire);
            sim.backend_eof(up);
            sim.run_until_idle();
            wait_for(sim, story, net.send_idle, assign);
            sim.set_client_window(conn, Window::Unlimited);
        }
        Timing::HeadDuringUpload { k, .. } => {
            send(sim);
            sync(sim, std::slice::from_mut(story), assign);
            let Some(up) = story.runs[i].up else {
                panic!("conn={} never connected\ntrace:\n{}", conn.0, sim.trace());
            };
            sim.backend_send(up, &script_wire[..k]);
            sim.run_until_idle();
            let capped = !interim_ends.contains(&k) && net.response <= net.body_idle;
            let d = if capped { net.response } else { net.body_idle };
            wait_for(sim, story, d, assign);
        }
        Timing::Normal | Timing::Early { .. } | Timing::Eager => {}
    }
    sim.run_until_idle();
}

/// End a round: windows open, every client ends its stream.
pub(crate) fn wind_down(sim: &mut Sim, stories: &mut [Story], assign: &mut Assign) {
    for story in stories.iter() {
        if story.reset.is_none() && sim.state(story.conn) != SimConnState::Closed {
            sim.set_client_window(story.conn, Window::Unlimited);
        }
        for run in &story.runs {
            if let Some(up) = run.up {
                sim.set_backend_window(up, Window::Unlimited);
            }
        }
    }
    sim.run_until_idle();
    sync(sim, stories, assign);
    for story in stories.iter() {
        if story.reset.is_none() {
            sim.send_eof(story.conn);
        }
    }
    sim.run_until_idle();
    sync(sim, stories, assign);
}

/// Every connection and upstream of a round is closed.
pub(crate) fn assert_closed(sim: &Sim, stories: &[Story]) {
    for story in stories {
        assert_eq!(
            sim.state(story.conn),
            SimConnState::Closed,
            "conn={} still open at the end of its round\ntrace:\n{}",
            story.conn.0,
            sim.trace()
        );
    }
    assert_eq!(
        sim.open_upstreams(),
        0,
        "upstreams left open\ntrace:\n{}",
        sim.trace()
    );
}

pub(crate) fn conn_of(log: &Log) -> ConnId {
    match log {
        Log::Response { conn, .. }
        | Log::Rejected { conn, .. }
        | Log::Reset { conn, .. }
        | Log::FileAborted { conn, .. }
        | Log::ProxiedResponse { conn, .. }
        | Log::BackendFailed { conn, .. }
        | Log::BackendRetried { conn, .. } => *conn,
    }
}

/// One seed of the proxy net.
pub(crate) fn run_proxy_seed(seed: u64, coverage: &mut ProxyCoverage) {
    let mut rng = fastrand::Rng::with_seed(seed ^ 0x005e_ed0f_9e7a);
    let net = NetConfig::random(&mut rng);
    let mut sim = dated_sim(&net.text);
    sim.set_timer_order(timer_order(seed));
    let mut assign = Assign::default();
    let mut n = 1u8;
    let mut used_peer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
    let mut shutdown = Shutdown::new(seed);
    let rounds = rng.usize(2..6);
    for round in 0..rounds {
        shutdown.last_round = round + 1 == rounds;
        let solo = rng.u8(0..3) == 0;
        let kind = if solo {
            RoundKind::Timed
        } else {
            RoundKind::Plain
        };
        // Per backend: accept, refuse, or accept slowly (the model treats
        // a slow accept as an accept).
        let mut connect = [ConnectBehaviour::Accept; 2];
        let mut slow = [false; 2];
        if !solo {
            for (c, s) in connect.iter_mut().zip(slow.iter_mut()) {
                match rng.u8(0..8) {
                    0 => *c = ConnectBehaviour::Refuse,
                    1 => *s = true,
                    _ => {}
                }
            }
        }
        for ((route, c), s) in net.routes.iter().zip(connect).zip(slow) {
            sim.set_connect(route.addr, if s { ConnectBehaviour::Hang } else { c });
        }
        // A timed round: the timed story waits until the others are done,
        // so the clock only runs out deadlines the model knows (the others
        // sit idle in keep-alive, or end their lingering close silently).
        let count = if solo {
            rng.usize(1..4)
        } else {
            rng.usize(1..5)
        };
        let mut stories: Vec<Story> = (0..count)
            .map(|i| {
                n = n.wrapping_add(1).max(1);
                let draw = Draw {
                    connect,
                    timed: solo && i == 0,
                    pooly: false,
                };
                let mut story = Story::new(&mut sim, &mut rng, &net, n, &draw, slow);
                story.paused = solo && i == 0;
                story
            })
            .collect();
        for story in &stories {
            if rng.u8(0..3) == 0 {
                sim.set_client_window(story.conn, window(&mut rng, story.large));
            }
        }
        if rng.bool() {
            sim.set_default_backend_window(window(&mut rng, stories.iter().any(|s| s.large)));
        } else {
            sim.set_default_backend_window(Window::Unlimited);
        }
        // Now and then the idle watch lags: only the silence check before
        // a reuse sees what a backend sent to an idle connection.
        assign.hold = rng.bool();
        sim.hold_idle_reads(assign.hold);
        used_peer = stories.first().map_or(used_peer, |s| s.peer);
        run_round(
            &mut sim,
            &mut rng,
            &net,
            &mut stories,
            &mut assign,
            kind,
            &mut shutdown,
        );
        if let Some(first) = stories.first_mut().filter(|s| s.paused) {
            first.paused = false;
            run_round(
                &mut sim,
                &mut rng,
                &net,
                &mut stories,
                &mut assign,
                kind,
                &mut shutdown,
            );
        }
        if round + 1 == rounds {
            shutdown.finish(&mut sim, &stories);
        }
        // The longest wait left once the backends are done: a head's
        // deadline (20 s); `keepalive_timeout` is 120 s.
        let conns: Vec<ConnId> = stories.iter().map(|s| s.conn).collect();
        check_drain_finishes(
            &mut sim,
            shutdown.plan,
            &shutdown.marks,
            &conns,
            Duration::from_secs(30),
            net.send_idle,
            Duration::from_secs(120),
        );
        wind_down(&mut sim, &mut stories, &mut assign);
        // The pool outlives its clients; its idle timeout ends it.
        clear_pool(&mut sim, &net);
        sync(&mut sim, &mut stories, &mut assign);
        assign.round();
        for story in &stories {
            check_story(&sim, story, &stories, &net, &shutdown.marks, coverage);
        }
        assert_closed(&sim, &stories);
        // A server that shut down takes no new round.
        if shutdown.marks.shut_down() {
            break;
        }
    }
    // Nothing is held once every round closed its connections, before
    // and after every timer has run out.
    for id in sim.conn_ids() {
        assert!(
            !sim.core_knows(id),
            "core still holds conn={} after its round\ntrace:\n{}",
            id.0,
            sim.trace()
        );
    }
    sim.advance(Duration::from_secs(600));
    assert_clock_done(&sim);
    for id in sim.conn_ids() {
        assert!(
            !sim.core_knows(id),
            "core still holds conn={}\ntrace:\n{}",
            id.0,
            sim.trace()
        );
    }
    assert_eq!(
        sim.open_upstreams(),
        0,
        "upstreams left open\ntrace:\n{}",
        sim.trace()
    );
    assert_eq!(
        sim.open_files(),
        0,
        "files left open\ntrace:\n{}",
        sim.trace()
    );
    check_wound_down(&sim, &shutdown.marks);
    // Every slot is free again: a full house of new clients is accepted
    // (refused, once shut down), the per-IP share of it from a peer the
    // run already used.
    let peers: Vec<IpAddr> = (0..net.max_connections)
        .map(|i| {
            if i < net.max_connections_per_ip {
                used_peer
            } else {
                IpAddr::V4(Ipv4Addr::new(198, 51, 100, i))
            }
        })
        .collect();
    check_full_house(&mut sim, &shutdown.marks, &peers);
    for (cause, n) in sim.idle_closes() {
        let what = match cause {
            IdleClose::Byte => "pool: idle close by byte",
            IdleClose::Eof => "pool: idle close by eof",
            IdleClose::Error => "pool: idle close by error",
            IdleClose::Timeout => "pool: idle close by timeout",
            IdleClose::Room => "pool: idle close for room",
            IdleClose::Drain => "pool: idle close by a drain",
            IdleClose::Busy => "pool: not silent at reuse",
        };
        for _ in 0..*n {
            coverage.add(what);
        }
    }
    if shutdown.marks.restart() {
        coverage.add(match shutdown.marks.abort {
            None => "restart: plain drain",
            Some(_) => "restart: drain cut by the abort",
        });
    }
}
