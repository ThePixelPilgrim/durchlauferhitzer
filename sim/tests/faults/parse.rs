//! A parser model for the static net's request templates, written from
//! the template table and the spec, not from the core's parser: whether a
//! template is a request or a head the parser rejects. The drain answers a
//! request `503` with close, but a head the parser rejects is no request
//! and keeps its own error (FR-PROC-05b, owner decision 2026-10-29), so a
//! drain's `503` paired with such a template fails the oracle.

use super::*;

/// What the parser makes of a template (`REQUESTS` says it for each of
/// its own; the generated static requests follow `template_parse`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Parse {
    /// A request.
    Request,
    /// Rejected with its own error under both limit sets.
    Rejected,
    /// Rejected under the short limits (`RANDOM_CONFIG`); under the long
    /// ones (`LONG_RANDOM_CONFIG`) its head stays open and the template
    /// sent after it continues it, so what is answered depends on that one.
    RejectedUnderShortLimits,
}

/// The limit set a seed of the static net runs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadLimits {
    /// `RANDOM_CONFIG`: `max_request_line 64`, `max_head 160`.
    Short,
    /// `LONG_RANDOM_CONFIG`: `max_request_line 2KiB`, `max_head 2200`.
    Long,
}

/// What the parser does with a template under `limits`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadVerdict {
    Request,
    Rejected,
    /// Its head is open; the next template decides.
    Open,
}

/// The parser's verdict on `template` under `limits`: the table's word for
/// its own templates; a generated static request (a well-formed head whose
/// request line fits both limit sets) is rejected exactly when its path
/// escapes an unreserved character (FR-HTTP target rules, 400).
pub(crate) fn template_parse(template: &[u8], limits: HeadLimits) -> HeadVerdict {
    let parse = REQUESTS.iter().find(|(t, _)| *t == template).map_or_else(
        || {
            if has_encoded_unreserved(&template_path(template)) {
                Parse::Rejected
            } else {
                Parse::Request
            }
        },
        |&(_, p)| p,
    );
    match (parse, limits) {
        (Parse::Request, _) => HeadVerdict::Request,
        (Parse::Rejected, _) | (Parse::RejectedUnderShortLimits, HeadLimits::Short) => {
            HeadVerdict::Rejected
        }
        (Parse::RejectedUnderShortLimits, HeadLimits::Long) => HeadVerdict::Open,
    }
}

/// The heads a connection's templates form, in order, each with the
/// index of its last template: under the long limits an open head
/// (`HeadVerdict::Open`) is continued by the template sent after it, and
/// so on while the last one joined is open, so the parser sees one head
/// with the first template's request line and every template's lines.
pub(crate) fn heads(templates: &[Vec<u8>], limits: HeadLimits) -> Vec<(Vec<u8>, usize)> {
    let mut out = Vec::new();
    let mut next = 0;
    while let Some(first) = templates.get(next) {
        let mut head = first.clone();
        let mut last = next;
        let mut joined = first;
        while template_parse(joined, limits) == HeadVerdict::Open {
            let Some(more) = templates.get(last + 1) else {
                break;
            };
            head.extend_from_slice(more);
            last += 1;
            joined = more;
        }
        out.push((head, last));
        next = last + 1;
    }
    out
}

/// A log the drain wrote for the `k`th template of a connection.
pub(crate) struct Drained<'l> {
    pub(crate) log: &'l Log,
    pub(crate) refusable: Refusable,
    pub(crate) k: usize,
    pub(crate) limits: HeadLimits,
}

impl Drained<'_> {
    /// A drain's `503` (a [`Log::Response`]) for a head the model says
    /// the parser rejects is an error; such a head answered with its own
    /// error is counted. Once an open head was started at or before the
    /// template, the pairing of logs to templates is no longer certain
    /// (two templates may have formed one head), so nothing is judged.
    pub(crate) fn check(
        &self,
        templates: &[Vec<u8>],
        coverage: &mut ShutdownCoverage,
    ) -> Result<(), String> {
        let Some(upto) = templates.get(..=self.k) else {
            return Ok(());
        };
        if upto
            .iter()
            .any(|t| template_parse(t, self.limits) == HeadVerdict::Open)
        {
            return Ok(());
        }
        let Some(template) = upto.last() else {
            return Ok(());
        };
        let rejected = template_parse(template, self.limits) == HeadVerdict::Rejected;
        match self.log {
            Log::Response {
                status: Status::ServiceUnavailable,
                ..
            } if rejected && self.refusable == Refusable::Yes => Err(format!(
                "a drain's 503 for {:?}, a head the parser rejects: it keeps its own error \
                 (FR-PROC-05b)",
                String::from_utf8_lossy(template)
            )),
            Log::Rejected {
                reason: RejectReason::Http { .. },
                ..
            } if rejected => {
                coverage.own_error += 1;
                Ok(())
            }
            Log::Response { .. }
            | Log::Rejected { .. }
            | Log::Reset { .. }
            | Log::FileAborted { .. }
            | Log::ProxiedResponse { .. }
            | Log::BackendFailed { .. } => Ok(()),
        }
    }
}

#[test]
fn the_table_marks_each_malformed_template() {
    let rejected: Vec<&[u8]> = REQUESTS
        .iter()
        .filter(|(t, _)| template_parse(t, HeadLimits::Short) == HeadVerdict::Rejected)
        .map(|(t, _)| *t)
        .collect();
    assert_eq!(rejected.len(), 5);
    let open = REQUESTS
        .last()
        .map(|(t, _)| template_parse(t, HeadLimits::Long));
    assert_eq!(open, Some(HeadVerdict::Open));
    assert_eq!(
        template_parse(
            b"GET /%61.txt HTTP/1.1\r\nHost: example.test\r\n\r\n",
            HeadLimits::Short
        ),
        HeadVerdict::Rejected
    );
    assert_eq!(
        template_parse(
            b"GET /a.txt?x=%61 HTTP/1.1\r\nHost: example.test\r\n\r\n",
            HeadLimits::Long
        ),
        HeadVerdict::Request
    );
}

#[test]
fn an_open_head_is_continued_by_the_next_template() {
    let open = REQUESTS.last().map(|(t, _)| t.to_vec()).unwrap_or_default();
    let old = b"GET /old HTTP/1.0\r\n\r\n".to_vec();
    let next = b"GET /aaa HTTP/1.1\r\n\r\n".to_vec();
    let templates = vec![open.clone(), old.clone(), next.clone()];
    let joined = [open.clone(), old.clone()].concat();
    assert_eq!(
        heads(&templates, HeadLimits::Long),
        vec![(joined, 1), (next.clone(), 2)]
    );
    // Under the short limits the open template is rejected on its own.
    assert_eq!(
        heads(&templates, HeadLimits::Short),
        vec![(open.clone(), 0), (old, 1), (next, 2)]
    );
    // Open to the end: one head, still open.
    assert_eq!(
        heads(&[open.clone(), open.clone()], HeadLimits::Long),
        vec![([open.clone(), open].concat(), 1)]
    );
}
