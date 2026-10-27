//! A response body on its way from the backend to the client: where its
//! bytes are in the upstream's receive buffer, what of it is still due
//! from the backend, its framing check and its size limit (FR-PROXY-19…22,
//! -24; NFR-RES-04).
//!
//! The body is never copied: the bytes the client gets are the region of
//! the upstream's receive buffer the backend's bytes landed in. That
//! buffer is read into only once every body byte of the previous read was
//! sent (fill, drain, refill), and then with all its capacity but an
//! incomplete chunked-framing tail of at most [`MAX_UNCONSUMED`] bytes,
//! which is moved to the front first. Bytes after the body's end (a second
//! response, junk) are never sent: the upstream is closed as soon as the
//! body is complete, and they are dropped with the buffer's next drain.
//!
//! The type works on a buffer the caller passes in (the exchange owns it);
//! every method keeps `start <= end <= filled <= recv.len()`.

use crate::config::BodyLimit;
#[cfg(doc)]
use crate::http::chunked::MAX_UNCONSUMED;
use crate::http::chunked::{ChunkedValidator, Done};
use crate::http::response_head::ResponseBody;
use crate::log::BackendFailure;
use crate::request_body::BodyPending;
use crate::upstream::{BackendId, UpstreamId};

/// A response body in flight. `recv[start..end]` are validated body bytes
/// not yet sent to the client; `recv[end..filled]` are bytes after them:
/// while the body is open an unconsumed chunked-framing tail, once it is
/// done whatever the backend sent after the body (never sent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RelayBody {
    source: Source,
    limit: BodyLimit,
    start: usize,
    end: usize,
    filled: usize,
}

/// Where the rest of the body comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// More body bytes are due from upstream `up` of `backend`, which is
    /// open.
    Open {
        up: UpstreamId,
        backend: BackendId,
        due: Due,
    },
    /// Every body byte arrived (or there is no body); the upstream is
    /// closed.
    Done,
}

/// What of an open body is still to arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Due {
    /// `remaining` (more than 0) bytes of a `Content-Length` body.
    Length { remaining: u64 },
    /// A chunked body; `data` payload bytes so far, at most the limit.
    Chunked {
        validator: ChunkedValidator,
        data: u64,
    },
    /// A close-delimited body; `data` bytes so far, at most the limit.
    UntilClose { data: u64 },
}

/// Whether the upstream must be closed now: the body just became complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Closing {
    No,
    Upstream(UpstreamId),
}

impl RelayBody {
    /// The body of a response from upstream `up` of `backend`, delimited by
    /// `framing`, whose head ended at `start` of `recv`, with `filled`
    /// bytes received so far. The bytes already there are judged at once,
    /// so a framing error or a body over `limit` (a `Content-Length` over
    /// it, too) comes before anything is sent.
    pub(crate) fn new(
        framing: ResponseBody,
        up: UpstreamId,
        backend: BackendId,
        limit: BodyLimit,
        start: usize,
        filled: usize,
        recv: &mut [u8],
    ) -> Result<(RelayBody, Closing), BackendFailure> {
        let due = match framing {
            ResponseBody::None => None,
            ResponseBody::Length(length) => {
                if exceeds(length, limit) {
                    return Err(BackendFailure::BodyTooLarge);
                }
                Some(Due::Length { remaining: length })
            }
            ResponseBody::Chunked => Some(Due::Chunked {
                validator: ChunkedValidator::new(),
                data: 0,
            }),
            ResponseBody::UntilClose => Some(Due::UntilClose { data: 0 }),
        };
        let filled = filled.min(recv.len());
        let start = start.min(filled);
        let (source, closing) = match due {
            Some(due) => (Source::Open { up, backend, due }, Closing::No),
            // Nothing is due: anything after the head is dropped.
            None => (Source::Done, Closing::Upstream(up)),
        };
        let mut body = RelayBody {
            source,
            limit,
            start,
            end: start,
            filled,
        };
        let judged = body.judge(recv)?;
        Ok((body, either(closing, judged)))
    }

    /// The open upstream the body still comes from.
    pub(crate) fn upstream(&self) -> Option<UpstreamId> {
        match self.source {
            Source::Open { up, .. } => Some(up),
            Source::Done => None,
        }
    }

    /// The backend of the open upstream.
    pub(crate) fn backend(&self) -> Option<BackendId> {
        match self.source {
            Source::Open { backend, .. } => Some(backend),
            Source::Done => None,
        }
    }

    /// The body bytes to send to the client now.
    pub(crate) fn unsent<'a>(&self, recv: &'a [u8]) -> &'a [u8] {
        recv.get(self.start..self.end).unwrap_or_default()
    }

    pub(crate) fn unsent_len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// Where the next upstream read goes: `recv[filled..]`, behind a
    /// chunked tail at the front, once every body byte was sent and while
    /// more are due (nothing otherwise).
    pub(crate) fn spare<'a>(&self, recv: &'a mut [u8]) -> &'a mut [u8] {
        match self.source {
            Source::Open { .. } if self.start >= self.end => {
                recv.get_mut(self.filled..).unwrap_or_default()
            }
            Source::Open { .. } | Source::Done => &mut [],
        }
    }

    /// See [`BodyPending`]; `None` once nothing more is due.
    pub(crate) fn pending(&self) -> Option<BodyPending> {
        match self.source {
            Source::Open { .. } => Some(BodyPending {
                unsent: self.unsent_len(),
                tail: self.filled.saturating_sub(self.end),
            }),
            Source::Done => None,
        }
    }

    /// The driver read `len` bytes into [`RelayBody::spare`] (clamped to
    /// what was offered). Judges them; nothing of a failing read becomes
    /// sendable.
    pub(crate) fn received(
        &mut self,
        len: usize,
        recv: &mut [u8],
    ) -> Result<Closing, BackendFailure> {
        let offered = self.spare(recv).len();
        self.filled = self.filled.saturating_add(len.min(offered));
        self.judge(recv)
    }

    /// The client took `len` of the [`RelayBody::unsent`] bytes; returns
    /// how many (clamped).
    pub(crate) fn sent(&mut self, len: usize, recv: &mut [u8]) -> usize {
        let sent = len.min(self.unsent_len());
        self.start = self.start.saturating_add(sent);
        self.settle(recv);
        sent
    }

    /// The backend closed its sending side: that completes a
    /// close-delimited body (the upstream is to be closed); for any other
    /// body it is a failure (`None`: the body is incomplete).
    pub(crate) fn end_of_stream(&mut self) -> Option<UpstreamId> {
        match self.source {
            Source::Open {
                up,
                due: Due::UntilClose { .. },
                ..
            } => {
                self.source = Source::Done;
                Some(up)
            }
            Source::Open {
                due: Due::Length { .. } | Due::Chunked { .. },
                ..
            }
            | Source::Done => None,
        }
    }

    /// Judge `recv[end..filled]`, the bytes not yet judged (for a chunked
    /// body: the kept tail, then the new bytes), then settle.
    fn judge(&mut self, recv: &mut [u8]) -> Result<Closing, BackendFailure> {
        let fresh = recv.get(self.end..self.filled).unwrap_or_default();
        let available = u64::try_from(fresh.len()).unwrap_or(u64::MAX);
        let limit = self.limit;
        let Source::Open { up, due, .. } = &mut self.source else {
            self.settle(recv);
            return Ok(Closing::No);
        };
        let (accepted, complete) = match due {
            Due::Length { remaining } => {
                let take = (*remaining).min(available);
                *remaining = remaining.saturating_sub(take);
                // `take` is at most `fresh.len()`, so it fits `usize`.
                (usize::try_from(take).unwrap_or(0), *remaining == 0)
            }
            Due::Chunked { validator, data } => {
                let progress = validator.advance(fresh).map_err(BackendFailure::BadChunk)?;
                let total = data.saturating_add(progress.data);
                if exceeds(total, limit) {
                    return Err(BackendFailure::BodyTooLarge);
                }
                *data = total;
                (progress.consumed, progress.done == Done::Yes)
            }
            Due::UntilClose { data } => {
                let total = data.saturating_add(available);
                if exceeds(total, limit) {
                    return Err(BackendFailure::BodyTooLarge);
                }
                *data = total;
                (fresh.len(), false)
            }
        };
        let closing = if complete {
            let up = *up;
            self.source = Source::Done;
            Closing::Upstream(up)
        } else {
            Closing::No
        };
        self.end = self.end.saturating_add(accepted);
        self.settle(recv);
        Ok(closing)
    }

    /// Once no body byte is left to send: while more is due, move a chunked
    /// tail (at most 35 bytes) to the front so the next read starts behind
    /// it; once the body is done, drop whatever came after it.
    fn settle(&mut self, recv: &mut [u8]) {
        if self.start < self.end {
            return;
        }
        match self.source {
            Source::Open { .. } => {
                if self.end > 0 && self.end < self.filled {
                    recv.copy_within(self.end..self.filled, 0);
                }
                self.filled = self.filled.saturating_sub(self.end);
            }
            Source::Done => self.filled = 0,
        }
        self.start = 0;
        self.end = 0;
    }
}

/// Whether `length` payload bytes pass `limit` (exactly the limit is fine).
fn exceeds(length: u64, limit: BodyLimit) -> bool {
    match limit {
        BodyLimit::Bytes(max) => length > max,
        BodyLimit::Unlimited => false,
    }
}

/// The upstream to close, if either says so.
fn either(a: Closing, b: Closing) -> Closing {
    match (a, b) {
        (Closing::Upstream(up), _) | (Closing::No, Closing::Upstream(up)) => Closing::Upstream(up),
        (Closing::No, Closing::No) => Closing::No,
    }
}

#[cfg(test)]
mod tests;
