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
//! upstream; a story may end silently where the drain closed it.

use super::*;

/// A seed's shutdown: its plan, where it fell, and the interleaving steps
/// counted so far (only in rounds without a timed scenario).
pub(crate) struct Shutdown {
    pub(crate) plan: ShutdownPlan,
    pub(crate) marks: Marks,
    step: usize,
}

/// The steps a proxy seed's shutdown falls among (its rounds' steps).
pub(crate) const PROXY_SPAN: usize = 160;

impl Shutdown {
    pub(crate) fn new(seed: u64) -> Shutdown {
        Shutdown {
            plan: shutdown_plan(seed, PROXY_SPAN),
            marks: Marks::default(),
            step: 0,
        }
    }

    /// One step of a round's interleaving: the calls due before it.
    pub(crate) fn tick(&mut self, sim: &mut Sim, stories: &[Story]) {
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
/// side is unchanged, it was decided before).
fn closing_alts(ex: &Exchange, who: &Who, net: &NetConfig) -> Vec<Alt> {
    let mut req = ex.req.clone();
    req.persistence = Persistence::Close;
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
    };
    let mut out = alts(&closing_ex, who, net);
    for alt in &mut out {
        alt.tags = vec!["drain: closing head"];
    }
    out
}

/// The outcomes `ex` may have once the drain began, given that its first
/// log (if any) was written in `phase`.
pub(crate) fn drain_alts(
    ex: &Exchange,
    who: &Who,
    net: &NetConfig,
    phase: Option<Phase>,
) -> Vec<Alt> {
    match phase {
        Some(Phase::Serving) => return ex.alts.clone(),
        Some(Phase::Draining | Phase::AbortCut) | None => {}
    }
    let mut out = match &ex.req.verdict {
        // Answered with its head: during the drain that head gets the 503.
        Verdict::Answer(..) => Vec::new(),
        // A head the parser rejects is not a request: its 400 (which
        // closes) or the 503 (the spec does not say which).
        Verdict::Parse(_) => ex.alts.clone(),
        Verdict::Forward(_) | Verdict::BadBody { .. } => closing_alts(ex, who, net),
    };
    out.push(refused_alt(ex, who));
    out
}
