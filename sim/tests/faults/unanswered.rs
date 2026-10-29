//! The static net's check that no request vanishes: a request the spec
//! says must be answered gets a logged answer, before and during the
//! drain alike. Every exemption is a reason from the spec, judged from
//! what the client sent, what the network delivered to the core
//! (`Sim::delivered_to_server`) and the logs, never from the core's state.

use super::*;

/// How the connection's last answered request left it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LastAnswer {
    /// No request was answered.
    None,
    /// Its response kept the connection: the next request is read.
    KeepsOpen,
    /// Its response closes, or may close (a `502` to a request with a
    /// body, not received whole): nothing after it is answered.
    Closes,
}

/// What `check_client` found on one connection, for `check_unanswered`.
pub(crate) struct Answers {
    /// Request templates the logged answers consumed, in order.
    pub(crate) consumed: usize,
    pub(crate) last: LastAnswer,
    /// The connection was refused (over a limit, or while shutting down).
    pub(crate) refused: Refused,
    /// The connection's last log cut its exchange without an answer: a
    /// send or disk timeout, a reset, a file abort, a request body cut,
    /// or the abort.
    pub(crate) cut: CutOff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    Yes,
    No,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CutOff {
    Yes,
    No,
}

/// Whether `log` (written in `phase`) ends its exchange unanswered.
pub(crate) fn cuts(log: &Log, phase: Phase) -> CutOff {
    let cut = match log {
        Log::Rejected { reason, .. } => match reason {
            RejectReason::Timeout(TimeoutKind::Send | TimeoutKind::Disk)
            | RejectReason::Body(RequestBodyError::TooLarge | RequestBodyError::ClosedEarly) => {
                true
            }
            RejectReason::ShuttingDown => phase == Phase::AbortCut,
            RejectReason::Http { .. }
            | RejectReason::Timeout(TimeoutKind::Header | TimeoutKind::RequestBody)
            | RejectReason::Body(RequestBodyError::Chunked(_))
            | RejectReason::Limit(_)
            | RejectReason::Route(_) => false,
        },
        Log::Reset { .. } | Log::FileAborted { .. } => true,
        // A 502 follows it; a proxied response is not expected here.
        Log::Response { .. } | Log::BackendFailed { .. } | Log::ProxiedResponse { .. } => false,
    };
    if cut { CutOff::Yes } else { CutOff::No }
}

/// Whether `template` ends its own head (CRLF CRLF, or the bare LF pair
/// the parser rejects). One that does not runs on into the next template,
/// so one answer may consume both.
fn ends_head(template: &[u8]) -> bool {
    let has = |pat: &[u8]| template.windows(pat.len()).any(|w| w == pat);
    has(b"\r\n\r\n") || has(b"\n\n")
}

/// The first request of `client` that no log answers, if any, must be
/// one the spec lets go unanswered:
///
/// - the connection was refused, or its last log cut the exchange, or
///   the client reset it;
/// - the answer before it closes (FR-HTTP-09; during the drain every
///   head rendered says close, so a request behind it is not answered,
///   owner decision 2026-10-29, FR-PROC-05b);
/// - the abort found the connection open (it cuts whatever is left);
/// - none of its bytes had been read by the core when the drain began
///   (FR-PROC-05b "buffered": the drain closes an idle connection, and an
///   in-flight response's lingering close discards what the kernel holds);
/// - none of its bytes were ever read by the core (a keep-alive or header
///   timeout closed the connection first);
/// - it was not sent whole, so it never completed: the client's end of
///   stream closes it silently (FR-CONN-12), unless a plain drain began
///   with part of it read and the client sent no end of stream, for then
///   the header deadline expires within the net's wait (408).
///
/// Anything else is a request that vanished.
pub(crate) fn check_unanswered(
    sim: &Sim,
    client: &Client,
    marks: &Marks,
    answers: &Answers,
) -> Result<(), String> {
    let Some(template) = client.templates.get(answers.consumed) else {
        return Ok(());
    };
    let exempt = answers.refused == Refused::Yes
        || answers.cut == CutOff::Yes
        || client.reset == ClientReset::Yes
        || answers.last == LastAnswer::Closes
        || marks.cut_by_abort.contains(&client.id);
    // The pairing of answers and templates holds only up to a template
    // that ends its head.
    let paired = client
        .templates
        .iter()
        .take(answers.consumed + 1)
        .all(|t| ends_head(t));
    if exempt || !paired {
        return Ok(());
    }
    let start: usize = client
        .templates
        .iter()
        .take(answers.consumed)
        .map(Vec::len)
        .sum();
    let end = start + template.len();
    let read_at_drain = marks.read_at_drain(client.id);
    let read = sim.delivered_to_server(client.id);
    if read_at_drain.is_some_and(|r| r <= start) || read <= start {
        return Ok(());
    }
    let whole = client.sent >= end;
    let times_out = marks.plain_drain() && read_at_drain.is_some_and(|r| r > start) && !client.eof;
    if !whole && !times_out {
        return Ok(());
    }
    Err(format!(
        "request {} {:?} was never answered: {} of its {} bytes sent, the core read {} \
         of the connection's {} ({:?} when the drain began), after {:?}",
        answers.consumed,
        String::from_utf8_lossy(template),
        client.sent.saturating_sub(start).min(template.len()),
        template.len(),
        read,
        client.sent,
        read_at_drain,
        answers.last
    ))
}
