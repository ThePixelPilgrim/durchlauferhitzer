//! Shutdown in both randomized nets (FR-PROC-05, 05b to 05d): one seed in
//! five drains (`begin_drain`), drains and then aborts (`abort_all`), or
//! aborts without a drain (a SIGINT), at seeded steps. The plan follows
//! from the seed alone, by an RNG of its own, so the other seeds keep
//! their schedules and a failure replays from `SIM_SEED`.
//!
//! What the oracles may expect of a drain comes from the spec, not the
//! core: a response head logged before the drain keeps what it said; one
//! logged after it says `Connection: close`, or is the `503` with
//! `Connection: close` that answers a request completed during the drain;
//! a connection accepted during the drain (or after the abort) is refused
//! with `503`, logged `Rejected { ShuttingDown }`; the abort cuts every
//! connection, logging `Rejected { ShuttingDown }` for each exchange in
//! flight; no upstream is opened once the drain began.
//!
//! A request the spec says must be answered may not vanish: what the
//! core had read of each connection when the drain began (`AtDrain`, the
//! sim's count of delivered bytes: FR-PROC-05b's "buffered" input) and
//! which connections the abort found open are recorded, so the oracles
//! can tell a request the drain may drop (none of it read when the drain
//! began) from one it must answer.

use super::*;

/// What a seed does to shut the server down, at which of its net's steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownPlan {
    /// The seed never shuts down.
    None,
    /// `begin_drain` before step `drain`; then every client reads all it
    /// is sent, so the exchanges in flight can finish.
    Drain { drain: usize },
    /// `begin_drain` before step `drain`, then `abort_all` before step
    /// `abort` (later); the clients read as before, so a stalled one
    /// keeps the drain from finishing.
    DrainThenAbort { drain: usize, abort: usize },
    /// `abort_all` before step `abort`, without a drain.
    Abort { abort: usize },
}

/// The shutdown plan of `seed`: one seed in five shuts down, at a step
/// below `span` (a plan whose step the run never reaches is carried out
/// at its end, before the wind-down).
pub(crate) fn shutdown_plan(seed: u64, span: usize) -> ShutdownPlan {
    if seed % 5 != 0 {
        return ShutdownPlan::None;
    }
    let mut rng = fastrand::Rng::with_seed(seed ^ 0x0d4a_1b5e_ed00);
    let drain = rng.usize(0..span / 2);
    match rng.u8(0..6) {
        0..3 => ShutdownPlan::Drain { drain },
        3 | 4 => ShutdownPlan::DrainThenAbort {
            drain,
            abort: drain + rng.usize(1..=span / 2 + 1),
        },
        _ => ShutdownPlan::Abort { abort: drain },
    }
}

/// Where the shutdown calls fell in the log, for the oracles.
#[derive(Debug, Default, Clone)]
pub(crate) struct Marks {
    /// `sim.logs().len()` when `begin_drain` was called.
    pub(crate) drain: Option<usize>,
    /// The logs `abort_all` wrote: `start..end`.
    pub(crate) abort: Option<(usize, usize)>,
    /// `sim.upstreams().len()` when the drain (or the abort) began.
    pub(crate) upstreams: Option<usize>,
    /// Each net connection as `begin_drain` found it.
    pub(crate) at_drain: BTreeMap<ConnId, AtDrain>,
    /// The net connections `abort_all` found not yet closed: it cut them,
    /// so whatever they still had to say is lost (FR-PROC-05 "fast").
    pub(crate) cut_by_abort: BTreeSet<ConnId>,
}

/// One connection when the drain began, as the network and the disk saw
/// it (not the core's state).
#[derive(Debug, Clone, Copy)]
pub(crate) struct AtDrain {
    /// Client bytes the core had read (`Sim::delivered_to_server`): only
    /// these can be buffered input the drain must answer (FR-PROC-05b).
    pub(crate) read: usize,
    /// Whether a file was being opened for it.
    pub(crate) opening: Opening,
}

/// Whether an `OpenFile` of the connection was still unanswered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Opening {
    /// Its request was parsed before the drain: it gets its own status
    /// (with close), never the drain's 503.
    File,
    Nothing,
}

impl Marks {
    /// Bytes of `conn` the core had read when the drain began, if it
    /// began with `conn` among the net's connections.
    pub(crate) fn read_at_drain(&self, conn: ConnId) -> Option<usize> {
        self.at_drain.get(&conn).map(|a| a.read)
    }

    /// Whether a plain drain ran (no abort followed it).
    pub(crate) fn plain_drain(&self) -> bool {
        self.drain.is_some() && self.abort.is_none()
    }
}

/// When a log entry was written, relative to the shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Before any shutdown call: the server serves as usual.
    Serving,
    /// After `begin_drain` (or `abort_all`): heads say close, requests
    /// get 503, connections are refused.
    Draining,
    /// Written by `abort_all` itself: an exchange it cut.
    AbortCut,
}

impl Marks {
    /// The log index from which the server is draining.
    fn draining_from(&self) -> Option<usize> {
        match (self.drain, self.abort) {
            (Some(d), _) => Some(d),
            (None, Some((a, _))) => Some(a),
            (None, None) => None,
        }
    }

    /// Whether the server began to shut down at all.
    pub(crate) fn shut_down(&self) -> bool {
        self.draining_from().is_some()
    }

    /// The phase of the log entry at `index` of `sim.logs()`.
    pub(crate) fn phase(&self, index: usize) -> Phase {
        if self
            .abort
            .is_some_and(|(start, end)| (start..end).contains(&index))
        {
            return Phase::AbortCut;
        }
        match self.draining_from() {
            Some(from) if index >= from => Phase::Draining,
            Some(_) | None => Phase::Serving,
        }
    }
}

/// The step value that carries out every call still due (the end of a
/// run).
pub(crate) const END: usize = usize::MAX;

impl ShutdownPlan {
    /// The steps of its `begin_drain` and its `abort_all`, if any.
    fn steps(self) -> (Option<usize>, Option<usize>) {
        match self {
            ShutdownPlan::None => (None, None),
            ShutdownPlan::Drain { drain } => (Some(drain), None),
            ShutdownPlan::DrainThenAbort { drain, abort } => (Some(drain), Some(abort)),
            ShutdownPlan::Abort { abort } => (None, Some(abort)),
        }
    }

    /// Whether `begin_drain` is due before `step` and not called yet.
    pub(crate) fn drain_due(self, step: usize, marks: &Marks) -> bool {
        marks.drain.is_none() && self.steps().0.is_some_and(|d| step >= d)
    }
}

/// Carries out the plan's calls due before `step` (with `step` [`END`]:
/// every call still due); `clients` are the net's connections, which read
/// all they are sent once a plain drain (one not followed by an abort)
/// began, so the exchanges in flight can finish.
pub(crate) fn shutdown_at(
    sim: &mut Sim,
    plan: ShutdownPlan,
    step: usize,
    marks: &mut Marks,
    clients: &[ConnId],
) {
    let (_, abort) = plan.steps();
    if plan.drain_due(step, marks) {
        marks.drain = Some(sim.logs().len());
        marks.upstreams.get_or_insert(sim.upstreams().len());
        for &conn in clients {
            let opening = if sim.opening_file(conn) {
                Opening::File
            } else {
                Opening::Nothing
            };
            let read = sim.delivered_to_server(conn);
            marks.at_drain.insert(conn, AtDrain { read, opening });
        }
        sim.begin_drain();
        if abort.is_none() {
            for &conn in clients {
                if sim.state(conn) != SimConnState::Closed {
                    sim.set_client_window(conn, Window::Unlimited);
                }
            }
            // They read at once, before any clock step can expire a send
            // deadline the stalled window ran down.
            sim.run_until_idle();
        }
    }
    if abort.is_some_and(|a| step >= a) && marks.abort.is_none() {
        let start = sim.logs().len();
        marks.upstreams.get_or_insert(sim.upstreams().len());
        marks.cut_by_abort.extend(
            clients
                .iter()
                .copied()
                .filter(|&conn| sim.state(conn) != SimConnState::Closed),
        );
        sim.abort_all();
        marks.abort = Some((start, sim.logs().len()));
    }
}

/// A plain drain that began finishes (FR-PROC-05: in-flight requests finish, then
/// every connection closes): after `within`, which the net chooses as the
/// longest chain of deadlines an exchange may still wait on and shorter
/// than `keepalive_timeout`, no connection is `Open` any more (each is
/// closed, or shut down for writing and lingering), although no client
/// ended its stream. The clock moves in 1 s steps, so late timers fire
/// near their deadlines too.
pub(crate) fn check_drain_finishes(
    sim: &mut Sim,
    plan: ShutdownPlan,
    marks: &Marks,
    clients: &[ConnId],
    within: Duration,
) {
    match plan {
        ShutdownPlan::Drain { .. } if marks.drain.is_some() => {}
        ShutdownPlan::None
        | ShutdownPlan::Drain { .. }
        | ShutdownPlan::DrainThenAbort { .. }
        | ShutdownPlan::Abort { .. } => return,
    }
    for _ in 0..within.as_secs() {
        sim.advance(Duration::from_secs(1));
    }
    for &conn in clients {
        assert_ne!(
            sim.state(conn),
            SimConnState::Open,
            "conn={} still open {within:?} into the drain\ntrace:\n{}",
            conn.0,
            sim.trace()
        );
    }
}

/// After the wind-down: the core holds nothing (`is_drained`), and, once
/// the server shut down, no upstream was opened after the drain began.
pub(crate) fn check_wound_down(sim: &Sim, marks: &Marks) {
    assert!(
        sim.is_drained(),
        "the core is not drained after the wind-down\ntrace:\n{}",
        sim.trace()
    );
    if let Some(n) = marks.upstreams {
        assert_eq!(
            sim.upstreams().len(),
            n,
            "an upstream was opened during the drain\ntrace:\n{}",
            sim.trace()
        );
    }
}

/// `count` new connections from `peers`: accepted (nothing logged) while
/// the server serves; refused, each logged `Rejected { ShuttingDown }`,
/// once it shut down (FR-PROC-05d).
pub(crate) fn check_full_house(sim: &mut Sim, marks: &Marks, peers: &[IpAddr]) {
    let logged = sim.logs().len();
    let ids: Vec<ConnId> = peers.iter().map(|&peer| sim.connect(peer)).collect();
    let expected: Vec<Log> = if marks.shut_down() {
        ids.iter()
            .zip(peers)
            .map(|(&conn, &peer)| Log::Rejected {
                conn,
                peer,
                reason: RejectReason::ShuttingDown,
            })
            .collect()
    } else {
        Vec::new()
    };
    assert_eq!(
        sim.logs().get(logged..).unwrap_or_default(),
        expected.as_slice(),
        "connection slots still taken after the run (shut down: {})\ntrace:\n{}",
        marks.shut_down(),
        sim.trace()
    );
}

/// What the static net's drains and aborts reached, summed over all seeds.
#[derive(Debug, Default)]
pub(crate) struct ShutdownCoverage {
    /// Requests completed during the drain, answered `503` with close.
    pub(crate) refused_request: usize,
    /// Other heads logged during the drain, rendered with close.
    pub(crate) closing_head: usize,
    /// A keep-alive response queued before the drain, then the close.
    pub(crate) shut_after_keep_alive: usize,
    /// Connections accepted during the drain or after the abort, refused.
    pub(crate) refused_connection: usize,
    /// Exchanges the abort cut (each logged).
    pub(crate) abort_cut: usize,
    /// Responses the abort cut short.
    pub(crate) abort_truncated: usize,
    /// Responses to a request whose file was being opened when the drain
    /// began: its own status, with close.
    pub(crate) opened_before_drain: usize,
    /// Connections whose request went unanswered behind a keep-alive
    /// answer during the drain, as allowed: none of it was read when the
    /// drain began.
    pub(crate) dropped_unread: usize,
    /// Witnesses whose bytes were checked exactly.
    pub(crate) witnesses: usize,
}

impl ShutdownCoverage {
    pub(crate) fn check(&self) {
        let named = [
            ("503 to a request during the drain", self.refused_request),
            ("closing head during the drain", self.closing_head),
            (
                "close after a keep-alive response during the drain",
                self.shut_after_keep_alive,
            ),
            (
                "connection refused while shutting down",
                self.refused_connection,
            ),
            ("exchange cut by the abort", self.abort_cut),
            ("response cut short by the abort", self.abort_truncated),
            (
                "own status for an open in flight at the drain",
                self.opened_before_drain,
            ),
            (
                "unanswered request unread at the drain",
                self.dropped_unread,
            ),
            ("witness checked exactly", self.witnesses),
        ];
        for (what, n) in named {
            assert!(n > 0, "no {what} in any seed");
        }
    }
}

/// A witness of the static net receives exactly what the spec says
/// (`run.rs` sets them up just before the drain): after a plain drain
/// the pipelined one its `404` keep-alive and then the `503` with close
/// for the request behind it (FR-PROC-05b), the other in-flight one its
/// `404` and then the shutdown of the sending side, the idle one its
/// `404` and then the close at once (no shutdown, nothing more). With an
/// abort after the drain the in-flight ones, whose windows stay shut,
/// receive nothing. A witness over the connection limits is refused
/// instead, and `check_client` judges it like any refused connection.
pub(crate) fn check_witness(
    sim: &Sim,
    client: &Client,
    marks: &Marks,
    coverage: &mut ShutdownCoverage,
) {
    let over_limit = sim.logs().iter().any(|l| {
        matches!(
            l,
            Log::Rejected { conn, reason: RejectReason::Limit(_), .. } if *conn == client.id
        )
    });
    if over_limit {
        return;
    }
    let not_found = not_found_keep_alive();
    let unavailable = fixed(
        Status::ServiceUnavailable,
        Persistence::Close,
        BodyMode::Full,
    );
    let shut = sim
        .trace()
        .contains(&format!(" conn={} -> ShutdownWrite", client.id.0));
    let (want, want_shut): (Vec<u8>, Option<bool>) = match (client.role, marks.plain_drain()) {
        (Role::Random, _) => return,
        (Role::PipelinedWitness, true) => ([not_found, unavailable].concat(), Some(true)),
        (Role::InFlightWitness, true) => (not_found, Some(true)),
        (Role::PipelinedWitness | Role::InFlightWitness, false) => (Vec::new(), None),
        (Role::IdleWitness, true | false) => (not_found, Some(false)),
    };
    if client.received != want || want_shut.is_some_and(|w| w != shut) {
        panic!(
            "witness conn={} ({:?}) received {:?} (shut down: {shut}), want {:?} (shut down: \
             {want_shut:?})\ntrace:\n{}",
            client.id.0,
            client.role,
            String::from_utf8_lossy(&client.received),
            String::from_utf8_lossy(&want),
            sim.trace()
        );
    }
    coverage.witnesses += 1;
}
