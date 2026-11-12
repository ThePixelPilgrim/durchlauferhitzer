//! A story driven by hand: a client that resets before its reused
//! backend connection fails (FR-PROXY-26; nightly fix 2026-11-12, seed
//! 175664).

use super::*;

/// A story of one drawn exchange: a `GET` or `HEAD` without a body on a
/// pooled route without `internal_files`, whose backend resets before
/// any response byte (`Reset(0)`), with no planned stale kill. Returns
/// the net, its sim and the story.
fn get_reset_before_any_byte() -> (NetConfig, Sim, Story) {
    let mut rng = fastrand::Rng::with_seed(175_664);
    let net = loop {
        let net = NetConfig::random(&mut rng);
        if net.routes.iter().all(|r| r.pool && r.files.is_none()) {
            break net;
        }
    };
    let mut sim = net.sim();
    let peer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
    let conn = sim.connect(peer);
    let who = Who { conn, peer };
    let draw = Draw {
        connect: [ConnectBehaviour::Accept; 2],
        timed: false,
        pooly: false,
    };
    for _ in 0..1_000_000 {
        let ex = random_exchange(&mut rng, &net, &who, "192.0.2.1", &draw);
        let Verdict::Forward(fwd) = &ex.req.verdict else {
            continue;
        };
        let wanted = matches!(ex.req.summary.method, Method::Get | Method::Head)
            && fwd.bytes.len() == fwd.head_len
            && ex.timing == Timing::Normal
            && ex.stale.is_none()
            && ex.script.as_ref().map(|s| s.ending) == Some(Ending::Reset(0));
        if wanted {
            let story = Story {
                conn,
                peer,
                exchanges: vec![ex],
                runs: vec![Run::default()],
                received: Vec::new(),
                reset: None,
                eof: false,
                eof_anywhere: false,
                slow: [false; 2],
                paused: false,
                send_ex: 0,
                send_off: 0,
                large: false,
            };
            return (net, sim, story);
        }
    }
    panic!("no exchange of the wanted shape in 1000000 draws");
}

/// The client resets once its request went out on a reused connection;
/// only then does its backend's scripted reset come. The core closed the
/// upstream at once, so nothing failed under the request: the model must
/// accept the core's outcome, with no retry and no 502.
#[test]
fn a_client_reset_before_a_scripted_reset_needs_no_retry() {
    let (net, mut sim, mut story) = get_reset_before_any_byte();
    let route = match &story.exchanges[0].req.verdict {
        Verdict::Forward(fwd) => fwd.route,
        Verdict::Parse(_) | Verdict::Answer(..) | Verdict::BadBody { .. } => {
            unreachable!("drawn as forwarded")
        }
    };
    // Another client leaves an idle connection to the route's backend.
    let warm = sim.connect(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 200)));
    let get = format!(
        "GET {}/w HTTP/1.1\r\nHost: example.test\r\n\r\n",
        net.routes[route].prefix
    );
    sim.send(warm, get.as_bytes());
    sim.run_until_idle();
    let [up] = sim.upstreams()[..] else {
        panic!("one upstream\n{}", sim.trace());
    };
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.upstream_holder(up),
        Some(UpstreamHolder::Pool),
        "{}",
        sim.trace()
    );
    // The story's request takes it.
    let wire = story.exchanges[0].req.wire.clone();
    sim.send(story.conn, &wire);
    sim.run_until_idle();
    assert_eq!(sim.upstreams(), [up], "reused\n{}", sim.trace());
    story.runs[0].up = Some(up);
    story.runs[0].reused = Some((warm, 0));
    // The client goes first; the core lets go of the upstream.
    story.reset = Some(sim.state(story.conn));
    sim.reset(story.conn);
    sim.run_until_idle();
    assert_eq!(sim.upstream_holder(up), None, "{}", sim.trace());
    // Only now does the backend's script end.
    let mut rng = fastrand::Rng::with_seed(0);
    let mut assign = Assign::default();
    do_step(
        &mut sim,
        &mut rng,
        &net,
        &mut story,
        Step::Backend(0, 0),
        &mut assign,
    );
    let who = Who {
        conn: story.conn,
        peer: story.peer,
    };
    let ex = &story.exchanges[0];
    let judged = reuse_alts(ex.alts.clone(), ex, &story.runs[0], &who, &net, false);
    assert!(judged.is_ok(), "{judged:?}\n{}", sim.trace());
    assert!(story.runs[0].outrun, "{}", sim.trace());
    assert!(
        sim.logs()
            .iter()
            .all(|l| !matches!(l, Log::BackendRetried { .. }))
    );
}
