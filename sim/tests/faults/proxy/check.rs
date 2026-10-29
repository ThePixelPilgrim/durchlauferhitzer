//! The proxy net's oracle: each story's received bytes, logs and backend
//! receipts must match, exchange by exchange, one outcome the model
//! allows.

use super::*;

/// Where matching a story stands.
struct Cursor<'a> {
    received: &'a [u8],
    logs: &'a [Log],
    pos: usize,
    lpos: usize,
}

/// Whether backend run `run` matches `exp` (`None`: no upstream).
fn backend_ok(run: &Run, exp: Option<&BackendExp>, truncated: bool) -> Result<(), String> {
    match (run.up, exp) {
        (None, None) => Ok(()),
        (None, Some(_)) if truncated => Ok(()),
        (Some(up), None) => Err(format!("up={} opened, but none may be", up.0)),
        (None, Some(_)) => Err("no upstream opened, but one must be".to_owned()),
        (Some(up), Some(e)) => {
            let n = run.got.len();
            let min = if truncated { 0 } else { e.min };
            if n < min || n > e.max || e.bytes.get(..n) != Some(run.got.as_slice()) {
                Err(format!(
                    "up={} received {n} bytes, want a prefix of {}..={} of the model's {}\n\
                     received: {:?}\nmodel:    {:?}",
                    up.0,
                    min,
                    e.max,
                    e.bytes.len(),
                    String::from_utf8_lossy(&run.got[..n.min(2000)]),
                    String::from_utf8_lossy(&e.bytes[..e.bytes.len().min(2000)]),
                ))
            } else {
                Ok(())
            }
        }
    }
}

/// Match `alt` at the cursor: on success, the bytes and logs it takes.
fn matches(c: &Cursor, alt: &Alt, run: &Run, truncated: bool) -> Result<(usize, usize), String> {
    // The backend side first: a wrong forward is the root cause of what
    // follows from it.
    backend_ok(run, alt.upstream.as_ref(), truncated)?;
    let rest = &c.received[c.pos..];
    let logs = &c.logs[c.lpos..];
    let (bytes, nlogs) = if truncated {
        let n = rest.len().min(alt.bytes.len());
        if rest.len() > alt.bytes.len() || alt.bytes[..n] != *rest {
            return Err("received bytes are not a prefix".to_owned());
        }
        let k = logs.len().min(alt.logs.len());
        if logs.len() > alt.logs.len() || alt.logs[..k] != *logs {
            return Err(format!("logs {logs:?} are not a prefix of {:?}", alt.logs));
        }
        (n, k)
    } else {
        if logs.get(..alt.logs.len()) != Some(alt.logs.as_slice()) {
            return Err(format!("logs {:?}\n want {:?}", logs, alt.logs));
        }
        let n = if alt.cut {
            if rest.len() > alt.bytes.len() || alt.bytes[..rest.len()] != *rest {
                return Err(format!(
                    "a cut response, but the {} bytes received are not a prefix of the model's {}",
                    rest.len(),
                    alt.bytes.len()
                ));
            }
            rest.len()
        } else {
            if rest.get(..alt.bytes.len()) != Some(alt.bytes.as_slice()) {
                return Err(format!(
                    "received {:?}\n want {:?}",
                    String::from_utf8_lossy(
                        &rest[..rest.len().min(alt.bytes.len() + 64).min(3000)]
                    ),
                    String::from_utf8_lossy(&alt.bytes[..alt.bytes.len().min(3000)])
                ));
            }
            alt.bytes.len()
        };
        (n, alt.logs.len())
    };
    Ok((bytes, nlogs))
}

fn lossy(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(&bytes[..bytes.len().min(600)]).into_owned();
    if bytes.len() > 600 {
        format!("{s}... ({} bytes)", bytes.len())
    } else {
        s
    }
}

/// A short description of an exchange, for failure messages.
fn describe(ex: &Exchange) -> String {
    let verdict = match &ex.req.verdict {
        Verdict::Parse(e) => format!("Parse({e:?})"),
        Verdict::Answer(s, p) => format!("Answer({}, {p:?})", model_code(*s)),
        Verdict::BadBody { errors, fwd, bound } => {
            format!(
                "BadBody({errors:?}, bound {bound}, forwarded {:?})",
                lossy(&fwd.bytes)
            )
        }
        Verdict::Forward(fwd) => format!(
            "Forward(expect100 {}, {:?})",
            fwd.expect100,
            lossy(&fwd.bytes)
        ),
    };
    let script = ex.script.as_ref().map(|s| {
        let reply = match &s.reply {
            Reply::Fail(k) => format!("Fail({k:?})"),
            Reply::Relay(r) => format!(
                "Relay({} {:?} {:?} fails {:?} cut {:?} head {:?})",
                r.status,
                r.kind,
                r.persistence,
                r.fails,
                r.cut,
                lossy(&r.head)
            ),
        };
        format!(
            "{:?} head_end {} body_end {} {reply} wire {:?}",
            s.ending,
            s.head_end,
            s.body_end,
            lossy(&s.wire)
        )
    });
    format!(
        "request {:?}
  {verdict}
  {:?} {:?}
  script {script:?}",
        lossy(&ex.req.wire),
        ex.connect,
        ex.timing
    )
}

/// Judge one story once its round is over; `marks` say where a drain or
/// abort fell (`drain.rs`).
pub(crate) fn check_story(
    sim: &Sim,
    story: &Story,
    net: &NetConfig,
    marks: &Marks,
    coverage: &mut ProxyCoverage,
) {
    // The story's logs, and the phase each was written in.
    let (mut logs, mut phases): (Vec<Log>, Vec<Phase>) = sim
        .logs()
        .iter()
        .enumerate()
        .filter(|(_, l)| conn_of(l) == story.conn)
        .map(|(i, l)| (l.clone(), marks.phase(i)))
        .unzip();
    let fail = |what: String| -> ! {
        panic!(
            "conn={} {what}\nexchanges:\n{}\nreceived: {:?}\ntrace:\n{}",
            story.conn.0,
            story
                .exchanges
                .iter()
                .map(describe)
                .collect::<Vec<_>>()
                .join("\n"),
            String::from_utf8_lossy(&story.received[..story.received.len().min(4000)]),
            sim.trace()
        )
    };
    // The abort cut the story: its last log says so (FR-PROC-05c), and
    // what came before it may stop anywhere.
    let aborted = phases.last() == Some(&Phase::AbortCut);
    if aborted {
        let cut = Log::Rejected {
            conn: story.conn,
            peer: story.peer,
            reason: RejectReason::ShuttingDown,
        };
        if logs.last() != Some(&cut) {
            fail(format!("the abort logged {:?}", logs.last()));
        }
        logs.pop();
        phases.pop();
        coverage.add("abort: exchange cut");
    }
    let truncated = story.reset.is_some() || story.eof_anywhere || aborted;
    if story.reset == Some(SimConnState::Open) {
        let reset = Log::Reset {
            conn: story.conn,
            peer: story.peer,
        };
        if logs.last() != Some(&reset) {
            fail(format!(
                "reset while open, but the last log is {:?}",
                logs.last()
            ));
        }
        logs.pop();
        phases.pop();
        coverage.add("client reset");
    }
    if story.eof_anywhere {
        coverage.add("client end of stream");
        // FR-PROXY-26 (owner decision 2026-10-27): an end of stream in
        // the middle of a forwarded request body is logged, and nothing
        // follows it. Only a story with such a body may end with it.
        let closed_early = Log::Rejected {
            conn: story.conn,
            peer: story.peer,
            reason: RejectReason::Body(RequestBodyError::ClosedEarly),
        };
        if logs.last() == Some(&closed_early) {
            let body_forwarded = story.exchanges.iter().any(|ex| match &ex.req.verdict {
                Verdict::Forward(fwd) | Verdict::BadBody { fwd, .. } => {
                    fwd.bytes.len() > fwd.head_len
                }
                Verdict::Parse(_) | Verdict::Answer(..) => false,
            });
            if !body_forwarded {
                fail("logged an end of stream mid-body, but no request has a body".to_owned());
            }
            logs.pop();
            phases.pop();
            coverage.add("client end of stream mid-body");
        }
    }
    let mut c = Cursor {
        received: &story.received,
        logs: &logs,
        pos: 0,
        lpos: 0,
    };
    let mut ended = false;
    let who = Who {
        conn: story.conn,
        peer: story.peer,
    };
    for (i, ex) in story.exchanges.iter().enumerate() {
        let run = &story.runs[i];
        if ended {
            if run.up.is_some() {
                fail(format!(
                    "exchange {i} after the connection ended opened an upstream"
                ));
            }
            continue;
        }
        // Once the server shut down, what the exchange may show depends on
        // when its first log was written.
        let drained;
        let candidates: &[Alt] = if marks.shut_down() {
            drained = drain_alts(ex, &who, net, phases.get(c.lpos).copied());
            &drained
        } else {
            &ex.alts
        };
        let full = candidates
            .iter()
            .find_map(|alt| matches(&c, alt, run, false).ok().map(|taken| (alt, taken)));
        if let Some((alt, (n, k))) = full {
            c.pos += n;
            c.lpos += k;
            for tag in &alt.tags {
                coverage.add(tag);
            }
            if alt.closes {
                ended = true;
            }
            continue;
        }
        if truncated
            && candidates
                .iter()
                .any(|alt| matches(&c, alt, run, true).is_ok())
        {
            c.pos = c.received.len();
            c.lpos = c.logs.len();
            ended = true;
            if aborted {
                coverage.add("abort: response cut short");
            }
            continue;
        }
        // The story stops here without a word: nothing more arrived, and
        // no upstream was opened for it. Only where the spec lets the
        // shutdown drop the request (`silent_stop`).
        if marks.shut_down()
            && c.pos == c.received.len()
            && c.lpos == c.logs.len()
            && run.up.is_none()
        {
            match silent_stop(sim, story, i, marks, truncated) {
                Ok(what) => {
                    ended = true;
                    coverage.add(what);
                    continue;
                }
                Err(e) => fail(format!("exchange {i}: {e}")),
            }
        }
        let why: Vec<String> = candidates
            .iter()
            .map(|alt| matches(&c, alt, run, false).err().unwrap_or_default())
            .collect();
        fail(format!(
            "exchange {i} matches no outcome the model allows:\n{}",
            why.join("\n--\n")
        ));
    }
    if c.pos != c.received.len() {
        fail(format!(
            "{} bytes received beyond the model",
            c.received.len() - c.pos
        ));
    }
    if c.lpos != c.logs.len() {
        fail(format!("logs beyond the model: {:?}", &c.logs[c.lpos..]));
    }
    for log in &logs {
        let status = match log {
            Log::Response { status, .. } => Some(model_code(*status)),
            Log::Rejected { reason, .. } => match reason {
                RejectReason::Http { error, .. } => Some(model_code(model_http_status(error))),
                RejectReason::Body(RequestBodyError::Chunked(_)) => Some(400),
                RejectReason::Timeout(TimeoutKind::RequestBody) => Some(408),
                RejectReason::Body(RequestBodyError::TooLarge | RequestBodyError::ClosedEarly)
                | RejectReason::Route(_)
                | RejectReason::Timeout(_)
                | RejectReason::Limit(_)
                | RejectReason::ShuttingDown => None,
            },
            Log::Reset { .. }
            | Log::FileAborted { .. }
            | Log::ProxiedResponse { .. }
            | Log::BackendFailed { .. } => None,
        };
        if let Log::Rejected {
            reason:
                RejectReason::Http {
                    error: HttpError::ConnectionNamesFraming,
                    ..
                },
            ..
        } = log
        {
            coverage.add("400 Connection names framing");
        }
        match status {
            Some(400) => coverage.add("status 400"),
            Some(404) => coverage.add("status 404"),
            Some(408) => coverage.add("status 408"),
            Some(413) => coverage.add("status 413"),
            Some(417) => coverage.add("status 417"),
            Some(431) => coverage.add("status 431"),
            Some(502) => coverage.add("status 502"),
            Some(504) => coverage.add("status 504"),
            Some(_) | None => {}
        }
    }
}
