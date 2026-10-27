//! A request body on its way from the client to the backend: where its
//! bytes are in the client's receive buffer, how much of it is still due,
//! and its framing check (FR-PROXY-19, -20, -22, -23; NFR-RES-04).
//!
//! The body is never copied: the bytes the backend gets are the region of
//! `Conn::recv` the client's bytes landed in. That buffer is read into only
//! once every body byte of the previous read was written (fill, drain,
//! refill), and then with all its capacity but an incomplete chunked-framing
//! tail of at most [`MAX_UNCONSUMED`] bytes, which is moved to the front
//! first. Bytes after the body's end are the next pipelined request: they
//! are never written to the backend, and once every body byte is written
//! they are moved to the front, once, as after a head.
//!
//! The type works on a buffer the caller passes in (the connection owns
//! it); every method keeps `start <= end <= filled <= recv.len()`.

use crate::config::BodyLimit;
#[cfg(doc)]
use crate::http::chunked::MAX_UNCONSUMED;
use crate::http::chunked::{ChunkedValidator, Done};
use crate::http::request::BodyFraming;
use crate::log::RequestBodyError;

/// A request body in flight. `recv[start..end]` are validated body bytes
/// not yet written to the backend; `recv[end..filled]` are bytes after
/// them: while the body is open an unconsumed chunked-framing tail, once it
/// is done the start of the next request.
/// Not `Clone`: it holds the chunked validator's state, which must never
/// fork (like `RelayBody`).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RequestBody {
    due: Due,
    start: usize,
    end: usize,
    filled: usize,
}

/// What of the body is still to arrive from the client.
#[derive(Debug, PartialEq, Eq)]
enum Due {
    /// `remaining` (more than 0) bytes of a `Content-Length` body.
    Length { remaining: u64 },
    /// A chunked body; `data` payload bytes so far, at most `limit`.
    Chunked {
        validator: ChunkedValidator,
        data: u64,
        limit: BodyLimit,
    },
    /// Every body byte arrived (or there is no body).
    Nothing,
}

/// Where the body stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    /// More bytes are due from the client, and none is waiting for the
    /// backend: the client may be read.
    Receiving,
    /// Body bytes wait to be written to the backend.
    Writing,
    /// Every body byte arrived and was written.
    Written,
}

/// How a body ends when the exchange no longer forwards it (a response
/// came, or the backend failed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyEnd {
    /// The whole body was read from the client; `buffered` bytes of the
    /// next request now wait at the front of the buffer.
    Read { buffered: usize },
    /// Part of the body is still unread: the connection cannot be reused.
    Unread,
}

/// For drivers' invariant checks: the body bytes of a client's buffer not
/// yet written to the backend, and the incomplete chunked-framing tail kept
/// at its front (0 unless the body is open and chunked).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyPending {
    pub unsent: usize,
    pub tail: usize,
}

impl RequestBody {
    /// The body of a request whose head ended at `start`, with `filled`
    /// bytes of `recv` received so far. A body-less request has nothing
    /// due; either way the bytes already there are judged at once, and the
    /// error, if any, comes before anything was forwarded. `limit` applies
    /// to a chunked body (a `Content-Length` is checked before this).
    pub(crate) fn new(
        framing: BodyFraming,
        limit: BodyLimit,
        start: usize,
        filled: usize,
        recv: &mut [u8],
    ) -> Result<RequestBody, RequestBodyError> {
        let due = match framing {
            BodyFraming::None | BodyFraming::Length(0) => Due::Nothing,
            BodyFraming::Length(remaining) => Due::Length { remaining },
            BodyFraming::Chunked => Due::Chunked {
                validator: ChunkedValidator::new(),
                data: 0,
                limit,
            },
        };
        let filled = filled.min(recv.len());
        let start = start.min(filled);
        let mut body = RequestBody {
            due,
            start,
            end: start,
            filled,
        };
        body.judge(recv)?;
        Ok(body)
    }

    pub(crate) fn stage(&self) -> Stage {
        if self.start < self.end {
            return Stage::Writing;
        }
        match self.due {
            Due::Length { .. } | Due::Chunked { .. } => Stage::Receiving,
            Due::Nothing => Stage::Written,
        }
    }

    /// Whether every body byte arrived from the client.
    pub(crate) fn all_arrived(&self) -> Arrived {
        match self.due {
            Due::Length { .. } | Due::Chunked { .. } => Arrived::Partly,
            Due::Nothing => Arrived::All,
        }
    }

    /// Where the next client read goes: `recv[filled..]`, while the body is
    /// receiving (nothing otherwise).
    pub(crate) fn spare<'a>(&self, recv: &'a mut [u8]) -> &'a mut [u8] {
        match self.stage() {
            Stage::Receiving => recv.get_mut(self.filled..).unwrap_or_default(),
            Stage::Writing | Stage::Written => &mut [],
        }
    }

    /// How many bytes [`RequestBody::spare`] would offer of a buffer of
    /// `capacity` bytes.
    pub(crate) fn spare_len(&self, capacity: usize) -> usize {
        match self.stage() {
            Stage::Receiving => capacity.saturating_sub(self.filled),
            Stage::Writing | Stage::Written => 0,
        }
    }

    /// The body bytes to write to the backend now.
    pub(crate) fn unsent<'a>(&self, recv: &'a [u8]) -> &'a [u8] {
        recv.get(self.start..self.end).unwrap_or_default()
    }

    pub(crate) fn unsent_len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// See [`BodyPending`]; `None` once the body is written.
    pub(crate) fn pending(&self) -> Option<BodyPending> {
        let tail = match self.due {
            Due::Chunked { .. } => self.filled.saturating_sub(self.end),
            Due::Length { .. } | Due::Nothing => 0,
        };
        match self.stage() {
            Stage::Receiving | Stage::Writing => Some(BodyPending {
                unsent: self.unsent_len(),
                tail,
            }),
            Stage::Written => None,
        }
    }

    /// The driver read `len` bytes into [`RequestBody::spare`]; returns how
    /// many it took (clamped to what was offered). Judges them.
    pub(crate) fn received(
        &mut self,
        len: usize,
        recv: &mut [u8],
    ) -> Result<usize, RequestBodyError> {
        let offered = self.spare(recv).len();
        let added = len.min(offered);
        self.filled = self.filled.saturating_add(added);
        self.judge(recv)?;
        Ok(added)
    }

    /// The backend took `len` of the [`RequestBody::unsent`] bytes; returns
    /// how many (clamped).
    pub(crate) fn sent(&mut self, len: usize, recv: &mut [u8]) -> usize {
        let sent = len.min(self.unsent_len());
        self.start = self.start.saturating_add(sent);
        self.settle(recv);
        sent
    }

    /// Body forwarding stops (a response came, or the backend failed):
    /// what the connection holds afterwards.
    pub(crate) fn end(&mut self, recv: &mut [u8]) -> BodyEnd {
        match self.due {
            Due::Length { .. } | Due::Chunked { .. } => BodyEnd::Unread,
            Due::Nothing => {
                // Unsent body bytes are dropped; the next request moves to
                // the front.
                self.start = self.end;
                self.settle(recv);
                BodyEnd::Read {
                    buffered: self.filled,
                }
            }
        }
    }

    /// Judge `recv[end..filled]`, the bytes not yet judged (for a chunked
    /// body: the kept tail, then the new bytes), then settle. Nothing of a
    /// failing call becomes forwardable.
    fn judge(&mut self, recv: &mut [u8]) -> Result<(), RequestBodyError> {
        let fresh = recv.get(self.end..self.filled).unwrap_or_default();
        match &mut self.due {
            Due::Nothing => {}
            Due::Length { remaining } => {
                let available = u64::try_from(fresh.len()).unwrap_or(u64::MAX);
                let take = (*remaining).min(available);
                // `take` is at most `fresh.len()`, so it fits `usize`.
                self.end = self.end.saturating_add(usize::try_from(take).unwrap_or(0));
                *remaining = remaining.saturating_sub(take);
                if *remaining == 0 {
                    self.due = Due::Nothing;
                }
            }
            Due::Chunked {
                validator,
                data,
                limit,
            } => {
                let progress = validator
                    .advance(fresh)
                    .map_err(RequestBodyError::Chunked)?;
                let total = data.saturating_add(progress.data);
                match limit {
                    BodyLimit::Bytes(max) if total > *max => {
                        return Err(RequestBodyError::TooLarge);
                    }
                    BodyLimit::Bytes(_) | BodyLimit::Unlimited => {}
                }
                *data = total;
                self.end = self.end.saturating_add(progress.consumed);
                match progress.done {
                    Done::Yes => self.due = Due::Nothing,
                    Done::No => {}
                }
            }
        }
        self.settle(recv);
        Ok(())
    }

    /// Once no body byte is left to write, move what follows (a chunked
    /// tail of at most 35 bytes, or the next request) to the front, so the
    /// next read starts at the buffer's front.
    fn settle(&mut self, recv: &mut [u8]) {
        if self.start == self.end && self.end > 0 {
            if self.end < self.filled {
                recv.copy_within(self.end..self.filled, 0);
            }
            self.filled = self.filled.saturating_sub(self.end);
            self.start = 0;
            self.end = 0;
        }
    }
}

/// Whether every body byte arrived from the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arrived {
    All,
    Partly,
}

#[cfg(test)]
mod tests;
