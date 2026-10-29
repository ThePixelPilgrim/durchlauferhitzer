//! One random schedule, and the test over all seeds.

use super::*;

/// One random schedule. Panics (sim invariant or check) on failure.
pub(crate) fn run_seed(seed: u64, coverage: &mut Coverage) {
    let mut rng = fastrand::Rng::with_seed(seed);
    // One seed in three allows long request lines (and long paths).
    let long = rng.u8(0..3) == 0;
    let (peers, prefix) = peer_pool(seed);
    let mut sim = dated_sim(&prefix.apply(if long {
        LONG_RANDOM_CONFIG
    } else {
        RANDOM_CONFIG
    }));
    let mut admission = Admission::new(prefix);
    // Here the backend refuses, so every forwarded request is a 502;
    // proxying itself has a randomized net of its own (`proxy/`).
    sim.set_timer_order(timer_order(seed));
    sim.set_connect("127.0.0.1:3000", ConnectBehaviour::Refuse);
    // One seed in three serves files up to 1 MiB.
    let big = rng.u8(0..3) == 0;
    let tree = Tree::random(&mut sim, &mut rng, big, long);
    sim.set_disk_delay(random_disk_delay(&mut rng));
    let mut clients: Vec<Client> = Vec::new();
    // Kept apart from `clients`, so no random step touches them.
    let mut witnesses: Vec<Client> = Vec::new();
    let steps = rng.usize(5..80);
    let plan = shutdown_plan(seed, STATIC_SPAN);
    let mut marks = Marks::default();
    for step in 0..steps {
        if plan.drain_due(step, &marks) {
            witnesses.extend(drain_witnesses(&mut sim, &mut admission, &marks, step));
        }
        let ids: Vec<ConnId> = clients.iter().chain(&witnesses).map(|c| c.id).collect();
        shutdown_at(&mut sim, plan, step, &mut marks, &ids);
        let op = rng.u8(0..100);
        if op < 25 || step < 3 || clients.is_empty() {
            let ip = peers
                .get(usize::from(rng.u8(0..3)))
                .copied()
                .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
            let id = admission.connect(&mut sim, &marks, ip);
            if rng.bool() {
                sim.set_read_chunk(id, rng.usize(1..40));
            }
            if rng.u8(0..3) == 0 {
                sim.set_client_window(id, random_window(&mut rng, big));
            }
            clients.push(Client::new(id, Role::Random));
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
                68..72 => {
                    sim.reset(client.id);
                    client.reset = ClientReset::Yes;
                }
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
        collect(&mut sim, &mut witnesses);
    }
    // A shutdown the steps did not reach comes now.
    if plan.drain_due(END, &marks) {
        witnesses.extend(drain_witnesses(&mut sim, &mut admission, &marks, steps));
    }
    // The witnesses are checked like every client, and exactly.
    clients.append(&mut witnesses);
    let ids: Vec<ConnId> = clients.iter().map(|c| c.id).collect();
    shutdown_at(&mut sim, plan, END, &mut marks, &ids);
    // The longest wait left: a disk answer (45 s), then a pipelined head's
    // deadline (10 s); `keepalive_timeout` is 60 s.
    check_drain_finishes(&mut sim, plan, &marks, &ids, Duration::from_secs(59));
    collect(&mut sim, &mut clients);
    // Wind down: every client sends EOF, then 5 minutes pass (longer than
    // any disk delay, so every open and readahead is answered), again
    // while anything is still due: with late timers each wake runs out
    // only the deadlines due by then, and the next ones start from it.
    for client in &clients {
        sim.send_eof(client.id);
    }
    sim.run_until_idle();
    collect(&mut sim, &mut clients);
    wind_down_clock(&mut sim, timer_order(seed));
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
    check_wound_down(&sim, &marks);
    for client in &clients {
        check_client(&sim, &tree, client, &marks, coverage);
        check_witness(&sim, client, &marks, &mut coverage.shutdown);
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
                // Counted by the oracle (`check_client`), which knows
                // a refusal from a cut exchange.
                RejectReason::ShuttingDown => {}
            }
        }
    }
    // Every connection slot is free again, orphaned opens included: a full
    // house of new connections is accepted (refused, once shut down).
    let peers: Vec<IpAddr> = (0..6)
        .map(|i| IpAddr::V4(Ipv4Addr::new(198, 51, 100, i)))
        .collect();
    check_full_house(&mut sim, &marks, &peers);
    coverage.admission.add(admission.coverage);
}

/// The static net's shutdown steps: below its longest schedule (80 steps).
pub(crate) const STATIC_SPAN: usize = 80;

/// Two clients the drain finds in known states (`check_witness` knows
/// their bytes). One has a keep-alive response queued: a `404` it does
/// not read yet (`Window::Budget(0)`), with (on an odd `step`) a second
/// request pipelined behind it in the same read, which must get the
/// drain's `503` (FR-PROC-05b); alone, the connection must close after
/// the `404`. The other is idle keep-alive, its `404` read: the drain
/// must close it at once.
fn drain_witnesses(
    sim: &mut Sim,
    admission: &mut Admission,
    marks: &Marks,
    step: usize,
) -> [Client; 2] {
    let mut witness =
        |sim: &mut Sim, host: u8, window: Window, role: Role, templates: Vec<Vec<u8>>| {
            let id = admission.connect(sim, marks, IpAddr::V4(Ipv4Addr::new(203, 0, 113, host)));
            sim.set_client_window(id, window);
            let bytes = templates.concat();
            sim.send(id, &bytes);
            sim.run_until_idle();
            let mut client = Client::new(id, role);
            client.started = templates.len();
            client.pipelined_sends = usize::from(templates.len() > 1);
            client.sent = bytes.len();
            client.templates = templates;
            client
        };
    let in_flight = if step % 2 == 1 {
        witness(
            sim,
            9,
            Window::Budget(0),
            Role::PipelinedWitness,
            vec![VALID.to_vec(), VALID.to_vec()],
        )
    } else {
        witness(
            sim,
            9,
            Window::Budget(0),
            Role::InFlightWitness,
            vec![VALID.to_vec()],
        )
    };
    [
        in_flight,
        witness(
            sim,
            10,
            Window::Unlimited,
            Role::IdleWitness,
            vec![VALID.to_vec()],
        ),
    ]
}

/// Let 5 minutes pass (longer than any disk delay and any deadline): once
/// on time; with late timers again while anything is still due
/// (`Sim::next_due`), at most 8 wakes, one per deadline of a chain (an
/// open answered late, its response, the next head's timeout, a
/// lingering close). Either way the clock must have nothing left to do.
pub(crate) fn wind_down_clock(sim: &mut Sim, order: TimerOrder) {
    let rounds = match order {
        TimerOrder::OnTime => 1,
        TimerOrder::Late => 8,
    };
    for _ in 0..rounds {
        sim.advance(Duration::from_secs(300));
        if sim.next_due().is_none() {
            break;
        }
    }
    assert_clock_done(sim);
}

/// Nothing is left for the clock: no deadline, no disk completion.
pub(crate) fn assert_clock_done(sim: &Sim) {
    assert!(
        sim.next_due().is_none(),
        "clock still has work after wind-down\ntrace:\n{}",
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

/// Run one seed; on failure, panic with `SIM_SEED=<seed>`, the seed's
/// timer order, its shutdown plan and the original message (which
/// carries the trace).
pub(crate) fn run_reporting(seed: u64, plan: ShutdownPlan, run: impl FnOnce()) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(run)) {
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default();
        panic!(
            "SIM_SEED={seed} failed (timer order {:?}, shutdown {plan:?}): {message}",
            timer_order(seed)
        );
    }
}

#[test]
fn random_fault_schedules() {
    let mut coverage = Coverage::default();
    match replay_seed() {
        Some(seed) => run_reporting(seed, shutdown_plan(seed, STATIC_SPAN), || {
            run_seed(seed, &mut coverage)
        }),
        None => {
            for seed in 0..SEEDS {
                run_reporting(seed, shutdown_plan(seed, STATIC_SPAN), || {
                    run_seed(seed, &mut coverage)
                });
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
            eprintln!("SHUTDOWN COVERAGE {:?}", coverage.shutdown);
            eprintln!("ADMISSION COVERAGE {:?}", coverage.admission);
            coverage.check();
        }
    }
}
