//! One random schedule, and the test over all seeds.

use super::*;

/// One random schedule. Panics (sim invariant or check) on failure.
pub(crate) fn run_seed(seed: u64, coverage: &mut Coverage) {
    let mut rng = fastrand::Rng::with_seed(seed);
    // One seed in three allows long request lines (and long paths).
    let long = rng.u8(0..3) == 0;
    let mut sim = dated_sim(if long {
        LONG_RANDOM_CONFIG
    } else {
        RANDOM_CONFIG
    });
    // Here the backend refuses, so every forwarded request is a 502;
    // proxying itself has a randomized net of its own (`proxy/`).
    sim.set_connect("127.0.0.1:3000", ConnectBehaviour::Refuse);
    // One seed in three serves files up to 1 MiB.
    let big = rng.u8(0..3) == 0;
    let tree = Tree::random(&mut sim, &mut rng, big, long);
    sim.set_disk_delay(random_disk_delay(&mut rng));
    let mut clients: Vec<Client> = Vec::new();
    let steps = rng.usize(5..80);
    for step in 0..steps {
        let op = rng.u8(0..100);
        if op < 25 || step < 3 || clients.is_empty() {
            let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, rng.u8(1..4)));
            let id = sim.connect(ip);
            if rng.bool() {
                sim.set_read_chunk(id, rng.usize(1..40));
            }
            if rng.u8(0..3) == 0 {
                sim.set_client_window(id, random_window(&mut rng, big));
            }
            clients.push(Client {
                id,
                queue: VecDeque::new(),
                front_started: false,
                started: 0,
                started_head: 0,
                templates: Vec::new(),
                pipelined_sends: 0,
                eof: false,
                received: Vec::new(),
                ended: false,
            });
        } else {
            let i = rng.usize(0..clients.len());
            let Some(client) = clients.get_mut(i) else {
                continue;
            };
            match op {
                25..60 => {
                    if !client.eof {
                        send_piece(&mut sim, client, &mut rng, tree.long_dir.as_deref());
                    }
                }
                60..68 => sim.set_client_window(client.id, random_window(&mut rng, big)),
                68..72 => sim.reset(client.id),
                72..78 => {
                    sim.send_eof(client.id);
                    client.eof = true;
                }
                78..86 => sim.advance(Duration::from_millis(rng.u64(0..12_000))),
                86..90 => sim.advance(Duration::from_secs(rng.u64(20..=90))),
                90..95 => sim.set_disk_delay(random_disk_delay(&mut rng)),
                _ => sim.set_read_chunk(client.id, rng.usize(1..200)),
            }
        }
        if rng.u8(0..4) != 0 {
            sim.run_until_idle();
        }
        collect(&mut sim, &mut clients);
    }
    // Wind down: every client sends EOF, then 5 minutes pass (longer than
    // any disk delay, so every open and readahead is answered).
    for client in &clients {
        sim.send_eof(client.id);
    }
    sim.run_until_idle();
    collect(&mut sim, &mut clients);
    sim.advance(Duration::from_secs(300));
    collect(&mut sim, &mut clients);
    for id in sim.conn_ids() {
        assert_eq!(
            sim.state(id),
            SimConnState::Closed,
            "conn={} not closed\ntrace:\n{}",
            id.0,
            sim.trace()
        );
        assert!(
            !sim.core_knows(id),
            "core still holds conn={}\ntrace:\n{}",
            id.0,
            sim.trace()
        );
    }
    assert_eq!(
        sim.open_files(),
        0,
        "files left open\ntrace:\n{}",
        sim.trace()
    );
    for client in &clients {
        check_client(&sim, &tree, client, coverage);
    }
    for log in sim.logs() {
        match log {
            Log::Response {
                status, request, ..
            } => {
                if *status == Status::UriTooLong {
                    coverage.redirect_414 += 1;
                }
                if request
                    .as_ref()
                    .is_some_and(|r| r.path.starts_with("/.well-known"))
                {
                    coverage.well_known += 1;
                }
                if *status == Status::Ok
                    && request
                        .as_ref()
                        .is_some_and(|r| is_respelled_route(&r.path))
                {
                    coverage.respelled_route += 1;
                }
            }
            Log::Rejected {
                reason:
                    RejectReason::Http {
                        error: HttpError::BadTarget(TargetError::EncodedUnreserved),
                        ..
                    },
                ..
            } => coverage.encoded_unreserved += 1,
            Log::Rejected { .. }
            | Log::Reset { .. }
            | Log::FileAborted { .. }
            | Log::ProxiedResponse { .. }
            | Log::BackendFailed { .. } => {}
        }
        if let Log::Rejected { reason, .. } = log {
            match reason {
                RejectReason::Http { .. } => coverage.http += 1,
                RejectReason::Timeout(TimeoutKind::Header) => coverage.header_timeout += 1,
                RejectReason::Timeout(TimeoutKind::Send) => coverage.send_timeout += 1,
                RejectReason::Timeout(TimeoutKind::Disk) => coverage.disk_timeout += 1,
                RejectReason::Limit(LimitKind::Connections) => coverage.limit_connections += 1,
                RejectReason::Limit(LimitKind::ConnectionsPerIp) => coverage.limit_per_ip += 1,
                // Proxied bodies are the proxy net's (`proxy/`).
                RejectReason::Route(_)
                | RejectReason::Body(_)
                | RejectReason::Timeout(TimeoutKind::RequestBody) => {}
            }
        }
    }
    // Every connection slot is free again, orphaned opens included: a full
    // house of new connections is accepted.
    let logged = sim.logs().len();
    for i in 0..6 {
        sim.connect(IpAddr::V4(Ipv4Addr::new(198, 51, 100, i)));
    }
    assert_eq!(
        sim.logs().get(logged..).unwrap_or_default(),
        [],
        "connection slots still taken after the run\ntrace:\n{}",
        sim.trace()
    );
}

/// The seed to replay, from `SIM_SEED`, if set (every randomized net of
/// this binary replays it).
pub(crate) fn replay_seed() -> Option<u64> {
    match std::env::var("SIM_SEED") {
        Ok(seed) => match seed.parse::<u64>() {
            Ok(seed) => Some(seed),
            Err(e) => panic!("SIM_SEED={seed:?} is not a u64: {e}"),
        },
        Err(_) => None,
    }
}

/// Run one seed; on failure, panic with `SIM_SEED=<seed>` and the original
/// message (which carries the trace).
pub(crate) fn run_reporting(seed: u64, run: impl FnOnce()) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(run)) {
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default();
        panic!("SIM_SEED={seed} failed: {message}");
    }
}

#[test]
fn random_fault_schedules() {
    let mut coverage = Coverage::default();
    match replay_seed() {
        Some(seed) => run_reporting(seed, || run_seed(seed, &mut coverage)),
        None => {
            for seed in 0..SEEDS {
                run_reporting(seed, || run_seed(seed, &mut coverage));
            }
            eprintln!(
                "COVERAGE statuses={:?} http={} header={} send={} disk={} conns={} per_ip={} \
                 pipelined={} truncated={} changed={} first_chunk={} multi_chunk={} grown={} \
                 head_ok={} close_ok={} close_redirect={} redirect_414={} encoded_unreserved={} well_known={} \
                 redirect_query={} respelled_route={}",
                coverage.statuses,
                coverage.http,
                coverage.header_timeout,
                coverage.send_timeout,
                coverage.disk_timeout,
                coverage.limit_connections,
                coverage.limit_per_ip,
                coverage.pipelined,
                coverage.aborted_truncated,
                coverage.aborted_changed,
                coverage.aborted_first_chunk,
                coverage.multi_chunk,
                coverage.grown,
                coverage.head_ok,
                coverage.close_ok,
                coverage.close_redirect,
                coverage.redirect_414,
                coverage.encoded_unreserved,
                coverage.well_known,
                coverage.redirect_query,
                coverage.respelled_route
            );
            coverage.check();
        }
    }
}
