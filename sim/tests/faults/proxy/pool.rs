//! Backend connection pooling in the proxy net (FR-PROXY-02, FR-PROXY-26).
//!
//! Which exchange an upstream serves is not taken from the order of the
//! connects any more: a reused upstream serves the next exchange of the
//! client the core hands it to, and the replay of a retried request goes
//! out on a fresh upstream of the same exchange. `sync` follows the core's
//! report of who holds each upstream (`Sim::upstream_holder`) and the
//! client each byte the backend received was written for, so a response
//! paired with the wrong request shows up as a mismatch of the oracle.
//!
//! The model, written from the spec, then judges every reuse and every
//! retry (`check.rs`):
//!
//! - a reused upstream must have been left reusable by the exchange that
//!   used it last ([`left_reusable`]): a pooled route, an HTTP/1.1
//!   client, the whole request written, a relayed HTTP/1.1 response
//!   without `close`, framed by `Content-Length` or chunked (or without
//!   a body), nothing sent after it, no end of stream or reset, no byte
//!   while idle;
//! - a request is retried only if it went out on a reused connection,
//!   is a GET or HEAD without a body, and that connection failed before
//!   any response byte ([`reuse_alts`]); then it must be (unless a drain
//!   began), once, logged `BackendRetried`, and the replay's outcome is
//!   the model's for a fresh connection.
//!
//! Backends act on reuse as real ones might: an exchange may find its
//! pooled connection dead (`Stale`: the backend reset or closed it just
//! as the request went out), and a backend may write to an idle
//! connection (`poke`), which must close it at once and never be reused.

use std::collections::BTreeSet;

use super::*;

/// How a pooled connection turns out dead when a request reuses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stale {
    /// The backend reset it, or closed it, just as the request went out:
    /// before any response byte.
    Reset,
    Eof,
    /// The backend took the request head, sent the first bytes of a
    /// response head, then reset or closed: never replayed.
    HalfReset,
    HalfEof,
}

impl Stale {
    pub(crate) fn kind(self) -> BackendFailure {
        match self {
            Stale::Reset | Stale::HalfReset => BackendFailure::Reset,
            Stale::Eof | Stale::HalfEof => BackendFailure::ClosedEarly,
        }
    }

    fn half(self) -> bool {
        matches!(self, Stale::HalfReset | Stale::HalfEof)
    }
}

/// The first bytes of a response head a `Half` stale backend sends.
const HALF_HEAD: &[u8] = b"HTTP/1.1 2";

/// What a backend does to its idle connection (`poke`): it writes, ends
/// its stream or resets; each must close it at once (FR-PROXY-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Poke {
    Byte,
    Eof,
    Reset,
}

/// What the net knows of the round's upstreams.
#[derive(Debug, Default)]
pub(crate) struct Assign {
    /// Upstream ids seen so far (`Sim::upstreams`).
    seen: usize,
    /// The exchange each upstream served last: its client and index, and
    /// the number of its request on the connection.
    last: BTreeMap<UpstreamId, (ConnId, usize, usize)>,
    /// Upstreams their backend wrote to while idle (`poke`).
    dirty: BTreeSet<UpstreamId>,
}

impl Assign {
    /// A new round: the pool was emptied (`clear_pool`), ids go on.
    pub(crate) fn round(&mut self) {
        self.last.clear();
        self.dirty.clear();
    }
}

fn story_of(stories: &[Story], conn: ConnId, sim: &Sim) -> usize {
    match stories.iter().position(|s| s.conn == conn) {
        Some(s) => s,
        None => panic!(
            "conn={} of an earlier round holds an upstream\ntrace:\n{}",
            conn.0,
            sim.trace()
        ),
    }
}

/// The story's next exchange that may get an upstream.
fn next_exchange(story: &Story, sim: &Sim, up: UpstreamId) -> usize {
    let last = story.runs.iter().rposition(|r| r.up.is_some());
    let next = story
        .exchanges
        .iter()
        .zip(story.runs.iter())
        .position(|(e, r)| e.may_connect() && r.up.is_none());
    match next {
        Some(i) if last.is_none_or(|l| l < i) => i,
        Some(_) | None => panic!(
            "conn={} got up={} with no exchange left to forward\ntrace:\n{}",
            story.conn.0,
            up.0,
            sim.trace()
        ),
    }
}

/// The exchange of `story` whose request the core is replaying, if a
/// `BackendRetried` it logged has no replay yet.
fn replay_of(sim: &Sim, story: &Story) -> Option<(usize, BackendFailure)> {
    let logged: Vec<BackendFailure> = sim
        .logs()
        .iter()
        .filter_map(|l| match l {
            Log::BackendRetried { conn, kind, .. } if *conn == story.conn => Some(*kind),
            Log::BackendRetried { .. }
            | Log::Response { .. }
            | Log::Rejected { .. }
            | Log::Reset { .. }
            | Log::FileAborted { .. }
            | Log::ProxiedResponse { .. }
            | Log::BackendFailed { .. } => None,
        })
        .collect();
    let replayed = story.runs.iter().filter(|r| r.retried.is_some()).count();
    let kind = logged.get(replayed).copied()?;
    let last = story.runs.iter().rposition(|r| r.up.is_some())?;
    Some((last, kind))
}

/// `up`, held by `conn`, carries request `seq` on its connection (`None`:
/// whichever it carries now): the latest exchange of `conn`'s story
/// already has it, or a reuse begins and the next exchange takes it. A
/// `Stale` exchange finds it dead (the backend resets or closes it now).
/// Whether the sim was pumped (and the caller must look again).
fn reuse(
    sim: &mut Sim,
    stories: &mut [Story],
    assign: &mut Assign,
    up: UpstreamId,
    conn: ConnId,
    seq: Option<usize>,
) -> bool {
    let s = story_of(stories, conn, sim);
    let story = &mut stories[s];
    let Some(&(prev_conn, prev_i, prev_seq)) = assign.last.get(&up) else {
        panic!(
            "conn={} holds up={}, which no exchange opened\ntrace:\n{}",
            conn.0,
            up.0,
            sim.trace()
        );
    };
    let same = match seq {
        Some(seq) => seq == prev_seq,
        None => prev_conn == conn,
    };
    if same {
        return false;
    }
    let i = next_exchange(story, sim, up);
    // The backend may have ended the connection right after the last
    // response, before the core saw it: then this request finds it dead.
    let ended = stories
        .iter()
        .find(|s| s.conn == prev_conn)
        .and_then(|s| s.exchanges.get(prev_i))
        .and_then(closed_after);
    let story = &mut stories[s];
    // Review Focus 1: a connection that got a byte while idle is never
    // handed out again.
    assert!(
        !assign.dirty.contains(&up),
        "conn={} reuses up={}, which got a byte while idle\ntrace:\n{}",
        conn.0,
        up.0,
        sim.trace()
    );
    let run = &mut story.runs[i];
    run.up = Some(up);
    run.reused = Some((prev_conn, prev_i));
    run.seq = prev_seq + 1;
    assign.last.insert(up, (conn, i, run.seq));
    if let Some(stale) = ended {
        run.ended = true;
        run.stale = Some(stale);
        run.stale_done = true;
        return false;
    }
    let Some(stale) = story.exchanges[i].stale else {
        return false;
    };
    run.ended = true;
    run.stale = Some(stale);
    if stale.half() {
        // Once the backend has the request head (`half_stale`).
        return false;
    }
    run.stale_done = true;
    match stale {
        Stale::Reset | Stale::HalfReset => sim.backend_reset(up),
        Stale::Eof | Stale::HalfEof => sim.backend_eof(up),
    }
    sim.run_until_idle();
    true
}

/// A `Half` stale backend that has the request head: it sends the first
/// bytes of a response head, then resets or closes.
fn half_stale(sim: &mut Sim, stories: &mut [Story]) -> bool {
    let mut acted = false;
    for story in stories.iter_mut() {
        for (ex, run) in story.exchanges.iter().zip(story.runs.iter_mut()) {
            let (Some(up), Some(stale), false) = (run.up, run.stale, run.stale_done) else {
                continue;
            };
            let head_len = match &ex.req.verdict {
                Verdict::Forward(fwd) | Verdict::BadBody { fwd, .. } => fwd.head_len,
                Verdict::Parse(_) | Verdict::Answer(..) => 0,
            };
            if run.got.len() < head_len || sim.upstream_holder(up).is_none() {
                continue;
            }
            run.stale_done = true;
            sim.backend_send(up, HALF_HEAD);
            sim.run_until_idle();
            match stale {
                Stale::Reset | Stale::HalfReset => sim.backend_reset(up),
                Stale::Eof | Stale::HalfEof => sim.backend_eof(up),
            }
            sim.run_until_idle();
            acted = true;
        }
    }
    acted
}

/// Assign upstreams to exchanges (fresh connects, replays and reuses);
/// collect what backends and clients received; let poking backends
/// write to their idle connections.
pub(crate) fn sync(sim: &mut Sim, stories: &mut [Story], assign: &mut Assign) {
    loop {
        let mut again = false;
        let ups = sim.upstreams();
        for &up in ups.iter().skip(assign.seen) {
            let conn = sim.upstream_conn(up);
            let s = story_of(stories, conn, sim);
            let story = &mut stories[s];
            let i = match replay_of(sim, story) {
                Some((i, kind)) => {
                    let run = &mut story.runs[i];
                    let stale = std::mem::take(&mut run.got);
                    run.stale_got.extend(stale);
                    if let Some(dead) = run.up {
                        for chunk in sim.take_backend_chunks(dead) {
                            run.stale_got.extend(chunk.bytes);
                        }
                    }
                    run.retried = Some(kind);
                    run.sent = 0;
                    run.ended = false;
                    run.accepted = false;
                    i
                }
                None => next_exchange(story, sim, up),
            };
            story.runs[i].up = Some(up);
            story.runs[i].seq = 1;
            assign.last.insert(up, (conn, i, 1));
        }
        assign.seen = ups.len();
        for &up in &ups {
            if let Some(UpstreamHolder::Conn(conn)) = sim.upstream_holder(up) {
                again |= reuse(sim, stories, assign, up, conn, None);
            }
        }
        for &up in &ups {
            for chunk in sim.take_backend_chunks(up) {
                let Some(conn) = chunk.holder else {
                    panic!(
                        "up={} received bytes while idle\ntrace:\n{}",
                        up.0,
                        sim.trace()
                    );
                };
                again |= reuse(sim, stories, assign, up, conn, Some(chunk.request));
                let s = story_of(stories, conn, sim);
                let Some(run) = stories[s]
                    .runs
                    .iter_mut()
                    .find(|r| r.up == Some(up) && r.seq == chunk.request)
                else {
                    panic!(
                        "up={} request {} has no exchange of conn={}\ntrace:\n{}",
                        up.0,
                        chunk.request,
                        conn.0,
                        sim.trace()
                    );
                };
                run.got.extend(chunk.bytes);
            }
        }
        again |= half_stale(sim, stories);
        again |= poke(sim, stories, assign);
        if !again {
            break;
        }
    }
    for story in stories.iter_mut() {
        story.received.extend(sim.take_received(story.conn));
    }
}

/// A backend writes a byte to its idle connection (the exchange's
/// `poke`): the core must close it at once (FR-PROXY-02, Review Focus 1).
fn poke(sim: &mut Sim, stories: &mut [Story], assign: &mut Assign) -> bool {
    let mut poked = false;
    for story in stories.iter_mut() {
        let conn = story.conn;
        for (i, (ex, run)) in story
            .exchanges
            .iter()
            .zip(story.runs.iter_mut())
            .enumerate()
        {
            let Some(up) = run.up else { continue };
            // Only the exchange that left it idle.
            let last = assign.last.get(&up) == Some(&(conn, i, run.seq));
            let Some(what) = ex.poke else { continue };
            if run.poked || !last || sim.upstream_holder(up) != Some(UpstreamHolder::Pool) {
                continue;
            }
            run.poked = true;
            assign.dirty.insert(up);
            match what {
                Poke::Byte => sim.backend_send(up, b"x"),
                Poke::Eof => sim.backend_eof(up),
                Poke::Reset => sim.backend_reset(up),
            }
            sim.run_until_idle();
            assert_eq!(
                sim.upstream_holder(up),
                None,
                "up={} got a byte while idle but stays open\ntrace:\n{}",
                up.0,
                sim.trace()
            );
            poked = true;
        }
    }
    poked
}

/// The pool's bounds after every step (FR-PROXY-02): at most
/// `backend_idle_max` idle per backend, and every upstream, active or
/// idle, within `max_connections`.
pub(crate) fn check_pool(sim: &Sim, net: &NetConfig) {
    let open: Vec<UpstreamId> = sim
        .upstreams()
        .into_iter()
        .filter(|up| sim.upstream_holder(*up).is_some())
        .collect();
    assert!(
        open.len() <= usize::from(net.max_connections),
        "{} upstreams open, max_connections is {}\ntrace:\n{}",
        open.len(),
        net.max_connections,
        sim.trace()
    );
    for route in &net.routes {
        let idle = open
            .iter()
            .filter(|up| {
                sim.upstream_holder(**up) == Some(UpstreamHolder::Pool)
                    && sim.upstream_backend(**up) == route.backend
            })
            .count();
        assert!(
            idle <= usize::from(net.idle_max),
            "{idle} idle connections to backend {}, backend_idle_max is {}\ntrace:\n{}",
            route.backend.0,
            net.idle_max,
            sim.trace()
        );
    }
}

/// Let every idle connection run out its `backend_idle_timeout` (no
/// client is waiting on a deadline then: the round is over, or a timed
/// scenario is about to start alone), so the next exchange connects
/// afresh.
pub(crate) fn clear_pool(sim: &mut Sim, net: &NetConfig) {
    let idle = sim
        .upstreams()
        .into_iter()
        .any(|up| sim.upstream_holder(up) == Some(UpstreamHolder::Pool));
    if idle {
        sim.advance(net.idle);
    }
    for up in sim.upstreams() {
        assert_ne!(
            sim.upstream_holder(up),
            Some(UpstreamHolder::Pool),
            "up={} outlived backend_idle_timeout\ntrace:\n{}",
            up.0,
            sim.trace()
        );
    }
}

/// Whether exchange `prev` (as run in `run`) left its connection
/// reusable by the spec (FR-PROXY-02); why not, if not.
pub(crate) fn left_reusable(
    prev: &Exchange,
    run: &Run,
    net: &NetConfig,
    reuser: &Run,
) -> Result<(), String> {
    let Verdict::Forward(fwd) = &prev.req.verdict else {
        return Err("its request was not forwarded".to_owned());
    };
    if !net.routes[fwd.route].pool {
        return Err("its route says `pool off`".to_owned());
    }
    if prev.req.http10 {
        return Err("its client spoke HTTP/1.0".to_owned());
    }
    if run.got != fwd.bytes {
        return Err("its request was not forwarded whole".to_owned());
    }
    let Some(script) = &prev.script else {
        return Err("it had no response".to_owned());
    };
    match &script.reply {
        Reply::Relay(r) if r.fails.is_none() && r.cut.is_none() => {
            if r.kind == BodyKind::UntilClose {
                return Err("its response ended with the connection".to_owned());
            }
        }
        Reply::Relay(_) | Reply::Fail(_) => return Err("its response failed".to_owned()),
    }
    if !script.keeps {
        return Err("its backend said close or spoke HTTP/1.0".to_owned());
    }
    if script.body_end != script.wire.len() {
        return Err("its backend sent bytes after the response".to_owned());
    }
    // An end of stream or reset right behind the response may still be
    // on its way when the core reuses the connection: the reusing request
    // finds it dead (the retry rule decides, FR-PROXY-26).
    match closed_after(prev) {
        None if script.ending == (Ending::Done { eof: false }) => {}
        Some(stale) if reuser.stale == Some(stale) => {}
        None | Some(_) => return Err(format!("its backend ended with {:?}", script.ending)),
    }
    if run.poked {
        return Err("its backend wrote to it while idle".to_owned());
    }
    Ok(())
}

/// How `ex`'s backend ended its connection right after a complete
/// response, if it did: its end of stream, or a reset.
fn closed_after(ex: &Exchange) -> Option<Stale> {
    let script = ex.script.as_ref()?;
    match script.ending {
        Ending::Done { eof: true } => Some(Stale::Eof),
        Ending::Eof(k) if k >= script.body_end => Some(Stale::Eof),
        Ending::Reset(k) | Ending::ResetAfter(k) if k >= script.body_end => Some(Stale::Reset),
        Ending::Done { eof: false } | Ending::Eof(_) | Ending::Reset(_) | Ending::ResetAfter(_) => {
            None
        }
    }
}

/// Whether `ex` may be replayed: a GET or HEAD without a body.
fn replayable(ex: &Exchange) -> bool {
    match &ex.req.verdict {
        Verdict::Forward(fwd) => {
            matches!(ex.req.summary.method, Method::Get | Method::Head)
                && fwd.bytes.len() == fwd.head_len
        }
        Verdict::Parse(_) | Verdict::Answer(..) | Verdict::BadBody { .. } => false,
    }
}

/// How a reused connection fails before any response byte in `ex`, if it
/// does: it was stale, or the script resets or ends before its first
/// byte.
fn fails_first(ex: &Exchange, run: &Run) -> Option<BackendFailure> {
    if let Some(stale) = run.stale {
        return (!stale.half()).then(|| stale.kind());
    }
    if ex.timing.timed() {
        return None;
    }
    match ex.script.as_ref()?.ending {
        Ending::Reset(0) | Ending::ResetAfter(0) => Some(BackendFailure::Reset),
        Ending::Eof(0) => Some(BackendFailure::ClosedEarly),
        Ending::Reset(_) | Ending::ResetAfter(_) | Ending::Eof(_) | Ending::Done { .. } => None,
    }
}

/// The 502 a stale connection gives a request that may not be replayed:
/// whatever the backend got of it, the body maybe unread (FR-PROXY-26).
fn stale_fail(ex: &Exchange, who: &Who, net: &NetConfig, kind: BackendFailure) -> Vec<Alt> {
    let fwd = match &ex.req.verdict {
        Verdict::Forward(fwd) | Verdict::BadBody { fwd, .. } => fwd,
        Verdict::Parse(_) | Verdict::Answer(..) => return Vec::new(),
    };
    let backend = net.routes[fwd.route].backend;
    let mode = if ex.req.head_only {
        BodyMode::HeadOnly
    } else {
        BodyMode::Full
    };
    let mut persistences = vec![ex.req.persistence];
    if fwd.bytes.len() > fwd.head_len {
        persistences.push(Persistence::Close);
    }
    persistences.dedup();
    persistences
        .into_iter()
        .map(|p| {
            let mut bytes = if fwd.expect100 {
                CONTINUE.to_vec()
            } else {
                Vec::new()
            };
            bytes.extend(fixed(Status::BadGateway, p, mode));
            Alt {
                bytes,
                cut: false,
                logs: vec![
                    Log::BackendFailed {
                        conn: who.conn,
                        peer: who.peer,
                        backend,
                        kind,
                    },
                    Log::Response {
                        conn: who.conn,
                        peer: who.peer,
                        status: Status::BadGateway,
                        request: Some(ex.req.summary.clone()),
                    },
                ],
                closes: p == Persistence::Close,
                upstream: Some(BackendExp {
                    bytes: fwd.bytes.clone(),
                    min: 0,
                    max: fwd.bytes.len(),
                }),
                tags: vec!["retry: refused, 502"],
            }
        })
        .collect()
}

/// The outcomes of `ex` given how its upstream came (`run`): `base` as
/// it is on a fresh connection; on a reused one, the retry rule of
/// FR-PROXY-26. An error says the core retried (or did not) against it.
pub(crate) fn reuse_alts(
    base: Vec<Alt>,
    ex: &Exchange,
    run: &Run,
    who: &Who,
    net: &NetConfig,
    draining: bool,
) -> Result<Vec<Alt>, String> {
    if run.reused.is_none() {
        return match run.retried {
            Some(kind) => Err(format!(
                "retried after {kind:?}, but its request went out on a fresh connection"
            )),
            None => Ok(base),
        };
    }
    // A response byte came first: never replayed, a 502 (FR-PROXY-26).
    if let Some(stale) = run.stale.filter(|s| s.half()) {
        if let Some(k) = run.retried {
            return Err(format!(
                "retried after {k:?}, but a response byte came first ({stale:?})"
            ));
        }
        let mut alts = stale_fail(ex, who, net, stale.kind());
        for alt in &mut alts {
            alt.tags = vec!["retry: none after a response byte"];
        }
        if matches!(ex.req.verdict, Verdict::BadBody { .. }) || !run.stale_done {
            alts.extend(base);
        }
        return Ok(alts);
    }
    let first = fails_first(ex, run);
    match (first, replayable(ex), run.retried) {
        (Some(kind), true, Some(k)) if k == kind => Ok(base
            .into_iter()
            .map(|mut alt| {
                alt.logs.insert(
                    0,
                    Log::BackendRetried {
                        conn: who.conn,
                        peer: who.peer,
                        backend: route_backend(ex, net),
                        kind,
                    },
                );
                alt.tags.push(match &ex.script.as_ref().map(|s| &s.reply) {
                    Some(Reply::Relay(_)) => "retry: replayed, relayed",
                    Some(Reply::Fail(_)) | None => "retry: replayed, failed again",
                });
                alt
            })
            .collect()),
        (Some(kind), true, Some(k)) => {
            Err(format!("retried after {k:?}, the backend's was {kind:?}"))
        }
        // Only a drain may have stopped the retry (FR-PROXY-26).
        (Some(kind), true, None) if draining => Ok(match run.stale {
            Some(_) => stale_fail(ex, who, net, kind),
            None => base,
        }),
        (Some(kind), true, None) => Err(format!(
            "a GET or HEAD without a body failed ({kind:?}) on a reused connection \
             before any response byte, but was not retried"
        )),
        (Some(kind), false, None) => Ok(match (run.stale, &ex.req.verdict) {
            // A body defect may be judged first (`BadBody`'s outcomes).
            (Some(_), Verdict::BadBody { .. }) => {
                let mut alts = stale_fail(ex, who, net, kind);
                alts.extend(base);
                alts
            }
            (Some(_), Verdict::Forward(_) | Verdict::Parse(_) | Verdict::Answer(..)) => {
                stale_fail(ex, who, net, kind)
            }
            (None, _) => base,
        }),
        (_, _, Some(k)) => Err(format!(
            "retried after {k:?}, but the spec allows no retry here (replayable: {}, \
             failed before any response byte: {first:?})",
            replayable(ex)
        )),
        (None, _, None) => Ok(base),
    }
}

fn route_backend(ex: &Exchange, net: &NetConfig) -> BackendId {
    ex.route.map_or(BackendId(0), |r| net.routes[r].backend)
}
