//! Driving the proxy net: stories (one client connection and its
//! exchanges), interleaved at random within a round, their backends
//! acting on what they received, and the timed scenarios.

use super::*;

/// What happened in one exchange, as the driver saw it.
#[derive(Debug, Default)]
pub(crate) struct Run {
    pub(crate) up: Option<UpstreamId>,
    /// Everything the backend received.
    pub(crate) got: Vec<u8>,
    /// Script bytes sent so far.
    sent: usize,
    /// A slow connect was let through.
    accepted: bool,
    /// The script's ending was applied.
    ended: bool,
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
        let count = rng.usize(1..5);
        // A timed exchange sits anywhere among untimed ones.
        let timed_at = draw.timed.then(|| rng.usize(0..count));
        for i in 0..count {
            let d = Draw {
                connect: draw.connect,
                timed: timed_at == Some(i),
            };
            let ex = random_exchange(rng, net, &who, &peer_text, &d);
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
    fn resolved(&self, sim: &Sim, i: usize) -> bool {
        let bytes: usize = self.exchanges[..=i]
            .iter()
            .filter_map(|e| e.alts.first())
            .map(|a| a.bytes.len())
            .sum();
        let logs: usize = self.exchanges[..=i]
            .iter()
            .filter_map(|e| e.alts.first())
            .map(|a| a.logs.len())
            .sum();
        let logged = sim
            .logs()
            .iter()
            .filter(|l| conn_of(l) == self.conn)
            .count();
        self.received.len() >= bytes && logged >= logs
    }

    /// Whether the client may send (more of) exchange `send_ex` now.
    fn next_send(&mut self, sim: &Sim) -> Option<Step> {
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
                if !pipelined && !self.resolved(sim, i - 1) {
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
        Ending::Reset(k) | Ending::Eof(k) => k,
    }
}

/// Assign new upstreams to their exchanges; collect what backends and
/// clients received.
pub(crate) fn sync(sim: &mut Sim, stories: &mut [Story], assigned: &mut usize) {
    let ups = sim.upstreams();
    for &up in ups.iter().skip(*assigned) {
        let conn = sim.upstream_conn(up);
        let Some(story) = stories.iter_mut().find(|s| s.conn == conn) else {
            panic!(
                "up={} for a connection of an earlier round\ntrace:\n{}",
                up.0,
                sim.trace()
            );
        };
        let next = story
            .exchanges
            .iter()
            .zip(story.runs.iter())
            .position(|(e, r)| e.may_connect() && r.up.is_none());
        let last = story.runs.iter().rposition(|r| r.up.is_some());
        match next {
            Some(i) if last.is_none_or(|l| l < i) => story.runs[i].up = Some(up),
            Some(_) | None => panic!(
                "conn={} opened up={} with no exchange left to forward\ntrace:\n{}",
                conn.0,
                up.0,
                sim.trace()
            ),
        }
    }
    *assigned = ups.len();
    for story in stories.iter_mut() {
        for run in &mut story.runs {
            if let Some(up) = run.up {
                run.got.extend(sim.take_backend_request(up));
            }
        }
        story.received.extend(sim.take_received(story.conn));
    }
}

fn window(rng: &mut fastrand::Rng, large: bool) -> Window {
    match (rng.u8(0..3), large) {
        (0, _) => Window::Unlimited,
        (_, true) => Window::Bytes(rng.usize(4096..65_536)),
        (_, false) => Window::Bytes(rng.usize(1..200)),
    }
}

/// Run one round: its stories interleaved until none can move.
pub(crate) fn run_round(
    sim: &mut Sim,
    rng: &mut fastrand::Rng,
    net: &NetConfig,
    stories: &mut [Story],
    assigned: &mut usize,
) {
    let mut steps = 0;
    loop {
        sync(sim, stories, assigned);
        let mut options = Vec::new();
        for (s, story) in stories.iter_mut().enumerate() {
            if let Some(step) = story.next_send(sim) {
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
            sync(sim, stories, assigned);
            let more = stories.iter_mut().any(|s| {
                s.next_send(sim).is_some()
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
        do_step(sim, rng, net, &mut stories[s], step, assigned);
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
    assigned: &mut usize,
) {
    match step {
        Step::Send(i) => {
            // The next request right behind a chunked body still in flight.
            if story.send_off == 0
                && i > 0
                && story.exchanges[i - 1].req.chunked
                && !story.resolved(sim, i - 1)
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
                let n = rng.usize(1..=target - run.sent);
                sim.backend_send(up, &script.wire[run.sent..run.sent + n]);
                run.sent += n;
            } else {
                run.ended = true;
                match script.ending {
                    Ending::Done { eof: true } | Ending::Eof(_) => sim.backend_eof(up),
                    Ending::Done { eof: false } => {}
                    Ending::Reset(_) => {
                        sim.run_until_idle();
                        sim.backend_reset(up);
                    }
                }
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
        Step::Timed(i) => run_timed(sim, rng, net, story, i, assigned),
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

/// Advance by `d` but 1 ms, check that nothing happened on the
/// connection, then by the last millisecond.
fn wait_for(sim: &mut Sim, story: &mut Story, d: Duration, assigned: &mut usize) {
    sync(sim, std::slice::from_mut(story), assigned);
    let before_logs = sim.logs().len();
    let before = story.received.len();
    sim.advance(d - Duration::from_millis(1));
    sync(sim, std::slice::from_mut(story), assigned);
    assert!(
        sim.logs().len() == before_logs && story.received.len() == before,
        "conn={} acted {}ms before its deadline\ntrace:\n{}",
        story.conn.0,
        1,
        sim.trace()
    );
    sim.advance(Duration::from_millis(1));
    sync(sim, std::slice::from_mut(story), assigned);
}

/// A timed scenario, alone in its round: set the stall up, then let the
/// clock run to the deadline it must hit.
fn run_timed(
    sim: &mut Sim,
    rng: &mut fastrand::Rng,
    net: &NetConfig,
    story: &mut Story,
    i: usize,
    assigned: &mut usize,
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
            wait_for(sim, story, net.connect, assigned);
        }
        Timing::Silent | Timing::ClientBodyStall(_) => {
            send(sim);
            let d = if timing == Timing::Silent {
                net.response
            } else {
                net.body_idle
            };
            wait_for(sim, story, d, assigned);
        }
        Timing::PartialHead(k) | Timing::RelayStall(k) => {
            send(sim);
            sync(sim, std::slice::from_mut(story), assigned);
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
            wait_for(sim, story, d, assigned);
        }
        Timing::BackendBodyStall(j) => {
            sim.set_default_backend_window(Window::Budget(head_len + j));
            send(sim);
            sim.set_default_backend_window(Window::Unlimited);
            wait_for(sim, story, net.body_idle, assigned);
        }
        Timing::ClientStall => {
            sim.set_client_window(conn, Window::Stalled);
            send(sim);
            sync(sim, std::slice::from_mut(story), assigned);
            let Some(up) = story.runs[i].up else {
                panic!("conn={} never connected\ntrace:\n{}", conn.0, sim.trace());
            };
            sim.backend_send(up, &script_wire);
            sim.backend_eof(up);
            sim.run_until_idle();
            wait_for(sim, story, net.send_idle, assigned);
            sim.set_client_window(conn, Window::Unlimited);
        }
        Timing::HeadDuringUpload { k, .. } => {
            send(sim);
            sync(sim, std::slice::from_mut(story), assigned);
            let Some(up) = story.runs[i].up else {
                panic!("conn={} never connected\ntrace:\n{}", conn.0, sim.trace());
            };
            sim.backend_send(up, &script_wire[..k]);
            sim.run_until_idle();
            let capped = !interim_ends.contains(&k) && net.response <= net.body_idle;
            let d = if capped { net.response } else { net.body_idle };
            wait_for(sim, story, d, assigned);
        }
        Timing::Normal | Timing::Early { .. } | Timing::Eager => {}
    }
    sim.run_until_idle();
}

/// End a round: windows open, every client ends its stream.
pub(crate) fn wind_down(sim: &mut Sim, stories: &mut [Story], assigned: &mut usize) {
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
    sync(sim, stories, assigned);
    for story in stories.iter() {
        if story.reset.is_none() {
            sim.send_eof(story.conn);
        }
    }
    sim.run_until_idle();
    sync(sim, stories, assigned);
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
        | Log::BackendFailed { conn, .. } => *conn,
    }
}

/// One seed of the proxy net.
pub(crate) fn run_proxy_seed(seed: u64, coverage: &mut ProxyCoverage) {
    let mut rng = fastrand::Rng::with_seed(seed ^ 0x005e_ed0f_9e7a);
    let net = NetConfig::random(&mut rng);
    let mut sim = dated_sim(&net.text);
    sim.set_timer_order(timer_order(seed));
    let mut assigned = 0;
    let mut n = 1u8;
    let mut used_peer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
    for _ in 0..rng.usize(2..6) {
        let solo = rng.u8(0..3) == 0;
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
        used_peer = stories.first().map_or(used_peer, |s| s.peer);
        run_round(&mut sim, &mut rng, &net, &mut stories, &mut assigned);
        if let Some(first) = stories.first_mut().filter(|s| s.paused) {
            first.paused = false;
            run_round(&mut sim, &mut rng, &net, &mut stories, &mut assigned);
        }
        wind_down(&mut sim, &mut stories, &mut assigned);
        for story in &stories {
            check_story(&sim, story, coverage);
        }
        assert_closed(&sim, &stories);
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
    // Every slot is free again: a full house of new clients is accepted,
    // the per-IP share of it from a peer the run already used.
    let logged = sim.logs().len();
    for _ in 0..net.max_connections_per_ip {
        sim.connect(used_peer);
    }
    for i in net.max_connections_per_ip..net.max_connections {
        sim.connect(IpAddr::V4(Ipv4Addr::new(198, 51, 100, i)));
    }
    assert_eq!(
        sim.logs().get(logged..).unwrap_or_default(),
        [],
        "connection slots still taken after the run\ntrace:\n{}",
        sim.trace()
    );
}
