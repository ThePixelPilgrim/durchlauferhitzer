//! Shutdown in the proxy net (`../shutdown.rs` has the plans and the
//! checks both nets share): the drain or abort happens at a step of a
//! round's interleaving (never while a timed scenario waits on the
//! clock), the rest of that round runs, and the seed ends with it.
//!
//! The outcomes a drain allows, from the spec (FR-PROC-05, 05b): an
//! exchange whose first log came before the drain is the model's as
//! usual; one whose head is rendered during the drain is the model's for
//! a client that closes (`Connection: close` in the head), or, for a
//! request completed during the drain, the `503` with close, with no
//! upstream; a head the parser rejects keeps its own error (owner
//! decision 2026-10-29, FR-PROC-05b). In a restart's drain (FR-PROC-07)
//! there is no `503`: a request completed during it is the model's for a
//! client that closes, forwarded as usual. A story may end silently only
//! where the drain may close it (`check.rs`).

use super::*;

/// A seed's shutdown: its plan, where it fell, and the interleaving steps
/// counted so far (only in rounds without a timed scenario).
pub(crate) struct Shutdown {
    pub(crate) plan: ShutdownPlan,
    pub(crate) marks: Marks,
    step: usize,
    /// A seed whose plan has no shutdown drains (even seeds) or restarts
    /// (odd ones) once a `Late` stale connection waits: in its last round,
    /// or, for two seeds in five, in any round (`tick`). So a retry the
    /// drain forbids is common (FR-PROXY-26), and mostly no round is lost.
    on_late: ShutdownPlan,
    eager: bool,
    /// The seed's last round runs (`run_proxy_seed`).
    pub(crate) last_round: bool,
}

/// The steps a proxy seed's shutdown falls among (its rounds' steps).
pub(crate) const PROXY_SPAN: usize = 160;

impl Shutdown {
    pub(crate) fn new(seed: u64) -> Shutdown {
        Shutdown {
            plan: shutdown_plan(seed, PROXY_SPAN),
            marks: Marks::default(),
            step: 0,
            on_late: if seed % 2 == 0 {
                ShutdownPlan::Drain { drain: 0 }
            } else {
                ShutdownPlan::Restart { drain: 0 }
            },
            eager: matches!(seed % 5, 1 | 3),
            last_round: false,
        }
    }

    /// One step of a round's interleaving: the calls due before it. While
    /// a `Late` stale connection waits to die, a drain not begun yet
    /// begins at once (one the plan has, or `on_late`'s), so its failure
    /// falls in the drain: no retry then (FR-PROXY-26).
    pub(crate) fn tick(&mut self, sim: &mut Sim, stories: &[Story]) {
        let late = stories
            .iter()
            .flat_map(|s| &s.runs)
            .any(|r| r.up.is_some() && !r.stale_done && r.stale.is_some_and(Stale::late));
        if late && self.marks.drain.is_none() {
            let none = self.plan == ShutdownPlan::None;
            let plan = if none && (self.last_round || self.eager) {
                self.on_late
            } else {
                self.plan
            };
            self.plan = plan.with_drain_at(self.step);
        }
        shutdown_at(sim, self.plan, self.step, &mut self.marks, &conns(stories));
        self.step += 1;
    }

    /// The end of the seed's last round: every call still due.
    pub(crate) fn finish(&mut self, sim: &mut Sim, stories: &[Story]) {
        shutdown_at(sim, self.plan, END, &mut self.marks, &conns(stories));
    }
}

fn conns(stories: &[Story]) -> Vec<ConnId> {
    stories.iter().map(|s| s.conn).collect()
}

/// The `503` with `Connection: close` that answers `ex`'s request when it
/// completes during the drain (FR-PROC-05 "target shutdown"); no upstream
/// is opened for it.
fn refused_alt(ex: &Exchange, who: &Who) -> Alt {
    let mode = if ex.req.head_only {
        BodyMode::HeadOnly
    } else {
        BodyMode::Full
    };
    Alt {
        bytes: fixed(Status::ServiceUnavailable, Persistence::Close, mode),
        cut: false,
        logs: vec![Log::Response {
            conn: who.conn,
            peer: who.peer,
            status: Status::ServiceUnavailable,
            request: Some(ex.req.summary.clone()),
        }],
        closes: true,
        upstream: None,
        tags: vec!["drain: 503 to a request"],
    }
}

/// The model's outcomes of `ex` for a client that closes after it: its
/// head rendered during the drain says `Connection: close` (the backend
/// side is unchanged, it was decided before), tagged `tag`.
fn closing_alts(ex: &Exchange, who: &Who, net: &NetConfig, tag: &'static str) -> Vec<Alt> {
    let mut req = ex.req.clone();
    req.persistence = Persistence::Close;
    if let Verdict::Answer(status, _) = req.verdict {
        req.verdict = Verdict::Answer(status, Persistence::Close);
    }
    let script = ex.script.clone().map(|mut s| {
        if let Reply::Relay(r) = &s.reply {
            if r.persistence == Persistence::KeepAlive {
                s.reply = Reply::Relay(closing(r));
            }
        }
        s
    });
    let closing_ex = Exchange {
        req,
        route: ex.route,
        connect: ex.connect,
        script,
        timing: ex.timing,
        alts: Vec::new(),
        stale: ex.stale,
        poke: ex.poke,
    };
    let mut out = alts(&closing_ex, who, net);
    for alt in &mut out {
        alt.tags = vec![tag];
    }
    out
}

/// The outcomes `ex` may have once the drain (of `marks`) began, given
/// that its first log (if any) was written in `phase`.
pub(crate) fn drain_alts(
    ex: &Exchange,
    who: &Who,
    net: &NetConfig,
    marks: &Marks,
    phase: Option<Phase>,
) -> Vec<Alt> {
    match phase {
        Some(Phase::Serving) => return ex.alts.clone(),
        Some(Phase::Draining | Phase::AbortCut) | None => {}
    }
    if marks.restart() {
        return match &ex.req.verdict {
            // Its own error, which closes (FR-PROC-05b).
            Verdict::Parse(_) => ex.alts.clone(),
            // Served as usual, its head saying close (FR-PROC-07).
            Verdict::Answer(..) | Verdict::Forward(_) | Verdict::BadBody { .. } => {
                closing_alts(ex, who, net, "restart: served with close")
            }
        };
    }
    match &ex.req.verdict {
        // Answered with its head: during the drain that head gets the 503.
        Verdict::Answer(..) => vec![refused_alt(ex, who)],
        // A head the parser rejects is no request: it keeps its own error,
        // which closes (owner decision 2026-10-29, FR-PROC-05b), never
        // the 503.
        Verdict::Parse(_) => ex.alts.clone(),
        Verdict::Forward(_) | Verdict::BadBody { .. } => {
            let mut out = closing_alts(ex, who, net, "drain: closing head");
            out.push(refused_alt(ex, who));
            out
        }
    }
}

/// Whether the shutdown may have dropped exchange `i` of `story` without
/// a word (its outcome otherwise matched nothing, and nothing more was
/// received or logged): the client cut the story itself (`truncated`: a
/// reset, an end of stream anywhere, the abort's log), the abort found
/// the connection open, or none of the request's bytes had been read by
/// the core when a shutdown's drain began (FR-PROC-05b "buffered": an
/// idle connection is closed at once, and the lingering close after an
/// in-flight response discards what the kernel holds). A restart's drain
/// closes nothing early (FR-PROC-07): there, only a request of which the
/// core never read a byte (its keep-alive deadline came first) may go. A request the core
/// had read part of must be answered: the drain's 503 with close, or its
/// own error; behind a head rendered during the drain nothing is, but
/// that head closes, which the outcome of the exchange before matched.
pub(crate) fn silent_stop(
    sim: &Sim,
    story: &Story,
    i: usize,
    marks: &Marks,
    truncated: bool,
) -> Result<&'static str, String> {
    if truncated {
        return Ok("drain: the client cut the story");
    }
    if marks.cut_by_abort.contains(&story.conn) {
        return Ok("abort: closed before a request");
    }
    let start: usize = story.exchanges[..i].iter().map(Exchange::send_len).sum();
    if marks.restart() {
        let read = sim.delivered_to_server(story.conn);
        return if read <= start {
            Ok("restart: closed before a request")
        } else {
            Err(format!(
                "the request vanished in a restart's drain: the core read {read} bytes of \
                 the connection, the request starts at byte {start}"
            ))
        };
    }
    match marks.read_at_drain(story.conn) {
        Some(read) if read <= start => Ok("drain: closed before a request"),
        read => Err(format!(
            "the request vanished: the core had read {read:?} bytes of the connection when \
             the drain began, the request starts at byte {start} (the core read {} in all)",
            sim.delivered_to_server(story.conn)
        )),
    }
}
