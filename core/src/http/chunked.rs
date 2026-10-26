//! Strict, streaming validator for `Transfer-Encoding: chunked` bodies.
//!
//! Chunked bodies from clients and from backends are validated in flight
//! and forwarded **verbatim**: the caller writes exactly the bytes this
//! validator reports as consumed to the other side, straight out of its
//! receive buffer. The validator therefore decides exactly where a body
//! ends; it is the request-smuggling boundary, so the rule is "when in
//! doubt, reject" (spec amendment 3):
//!
//! - chunk sizes are hex digits (either case), leading zeros allowed, at
//!   most [`MAX_SIZE_CHARS`] characters in all and at most 16 significant
//!   digits (the size must fit `u64`); no sign, no whitespace, no empty
//!   size;
//! - no chunk extensions (`;…`);
//! - no trailers: the last chunk is exactly `0\r\n\r\n` (with any leading
//!   zeros on its size);
//! - every line ends in CRLF; a bare LF or a CR not followed by LF is an
//!   error, and so is data longer than its chunk size.
//!
//! # Calling convention
//!
//! The validator keeps only where it is between chunks (a few machine
//! words); it never stores input bytes. A line it cannot judge yet — a
//! chunk-size line without its CRLF, the CRLF after chunk data when only
//! its CR arrived, or the last chunk before its final CRLF — is **not
//! consumed**: the caller keeps those bytes and passes them again, at the
//! front of the next call, followed by the bytes that arrived since. In
//! short, every call gets *the unconsumed tail of the previous call plus
//! the new bytes*, never bytes that were already consumed.
//!
//! The kept tail is at most [`MAX_UNCONSUMED`] bytes, so re-judging it costs
//! constant work per call, and the result does not depend on how the input
//! is split: any split, down to single bytes, gives the same totals and the
//! same error as the whole input at once.
//!
//! Bytes count as consumed only once they are final: a chunk-size line once
//! its CRLF is complete, data bytes as they arrive (their number is known),
//! the CRLF after data once complete, and the last chunk `0\r\n\r\n` once
//! complete. After [`Done::Yes`] every further call consumes nothing: what
//! follows the body belongs to the next message. An error reports no
//! consumed bytes: nothing from the failing call may be forwarded. Bytes
//! consumed by earlier calls were already forwarded, so an error after the
//! head was forwarded cannot be answered any more: the caller cuts both
//! connections (FR-PROXY-19). After an error every further call returns
//! the same error.

/// The most characters a chunk size may have, leading zeros included.
pub const MAX_SIZE_CHARS: usize = 32;

/// The most bytes [`ChunkedValidator::advance`] ever leaves unconsumed
/// before the body ends: a last chunk with a size of
/// [`MAX_SIZE_CHARS`] zeros, its CRLF and the CR of the final CRLF.
pub const MAX_UNCONSUMED: usize = MAX_SIZE_CHARS + 3;

/// Validates one chunked body as it arrives; constant memory, no
/// allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChunkedValidator {
    state: State,
}

/// What one call to [`ChunkedValidator::advance`] accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkProgress {
    /// Bytes from the front of the input that are valid framing and data
    /// and may be forwarded verbatim.
    pub consumed: usize,
    /// Payload bytes among the consumed ones (for body-size limits).
    pub data: u64,
    pub done: Done,
}

/// Whether the body has ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Done {
    No,
    /// The last chunk `0\r\n\r\n` was consumed.
    Yes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkedError {
    /// A chunk size that is empty, too long, or has a character other than
    /// a hex digit (sign, whitespace, ...).
    BadSize,
    /// A chunk size that does not fit `u64`.
    SizeOverflow,
    /// A chunk extension (`;` after the size).
    Extension,
    /// A trailer field after the last chunk.
    Trailer,
    /// A bare LF, a CR without LF, or data longer than its chunk size.
    BadLineEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    /// Expecting a chunk-size line.
    #[default]
    Size,
    /// Inside chunk data, with this many (more than 0) bytes still to come.
    Data {
        remaining: u64,
    },
    /// Expecting the CRLF after chunk data.
    DataEnd,
    /// The last chunk was consumed.
    Done,
    Failed(ChunkedError),
}

/// The verdict on the input at a line boundary.
enum Line<T> {
    /// More bytes are needed to judge it.
    Partial,
    /// Valid; the line is `len` bytes long, its CRLF included.
    Complete { value: T, len: usize },
}

impl ChunkedValidator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Judges `bytes`, which must start right after the last consumed byte
    /// (see the module docs for the calling convention).
    pub fn advance(&mut self, bytes: &[u8]) -> Result<ChunkProgress, ChunkedError> {
        let mut consumed = 0usize;
        let mut data = 0u64;
        loop {
            let rest = bytes.get(consumed..).unwrap_or_default();
            match self.state {
                State::Failed(e) => return Err(e),
                State::Done => break,
                State::Size => match self.judge(size_line(rest))? {
                    Line::Partial => break,
                    Line::Complete { value: 0, len } => {
                        let after = rest.get(len..).unwrap_or_default();
                        match self.judge(crlf(after, ChunkedError::Trailer))? {
                            Line::Partial => break,
                            Line::Complete {
                                value: (),
                                len: end,
                            } => {
                                consumed = consumed.saturating_add(len).saturating_add(end);
                                self.state = State::Done;
                            }
                        }
                    }
                    Line::Complete { value, len } => {
                        consumed = consumed.saturating_add(len);
                        self.state = State::Data { remaining: value };
                    }
                },
                State::Data { remaining } => {
                    if rest.is_empty() {
                        break;
                    }
                    let available = u64::try_from(rest.len()).unwrap_or(u64::MAX);
                    let take = remaining.min(available);
                    // `take` is at most `rest.len()`, so it fits `usize`.
                    consumed = consumed.saturating_add(usize::try_from(take).unwrap_or(0));
                    data = data.saturating_add(take);
                    let left = remaining.saturating_sub(take);
                    self.state = match left {
                        0 => State::DataEnd,
                        _ => State::Data { remaining: left },
                    };
                }
                State::DataEnd => match self.judge(crlf(rest, ChunkedError::BadLineEnd))? {
                    Line::Partial => break,
                    Line::Complete { value: (), len } => {
                        consumed = consumed.saturating_add(len);
                        self.state = State::Size;
                    }
                },
            }
        }
        let done = match self.state {
            State::Done => Done::Yes,
            State::Size | State::Data { .. } | State::DataEnd | State::Failed(_) => Done::No,
        };
        Ok(ChunkProgress {
            consumed,
            data,
            done,
        })
    }

    /// Makes an error sticky.
    fn judge<T>(
        &mut self,
        verdict: Result<Line<T>, ChunkedError>,
    ) -> Result<Line<T>, ChunkedError> {
        if let Err(e) = verdict {
            self.state = State::Failed(e);
        }
        verdict
    }
}

/// Judges a chunk-size line at the front of `bytes`. Looks at no more than
/// `MAX_SIZE_CHARS + 2` bytes.
fn size_line(bytes: &[u8]) -> Result<Line<u64>, ChunkedError> {
    let mut size = 0u64;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'\r' | b';' if i == 0 => return Err(ChunkedError::BadSize),
            b'\r' => {
                return match bytes.get(i.saturating_add(1)) {
                    None => Ok(Line::Partial),
                    Some(b'\n') => Ok(Line::Complete {
                        value: size,
                        len: i.saturating_add(2),
                    }),
                    Some(_) => Err(ChunkedError::BadLineEnd),
                };
            }
            b'\n' => return Err(ChunkedError::BadLineEnd),
            b';' => return Err(ChunkedError::Extension),
            _ => {
                let digit = hex_value(b).ok_or(ChunkedError::BadSize)?;
                if i >= MAX_SIZE_CHARS {
                    return Err(ChunkedError::BadSize);
                }
                // 16 significant digits always fit, 17 never do.
                size = size
                    .checked_mul(16)
                    .and_then(|s| s.checked_add(digit))
                    .ok_or(ChunkedError::SizeOverflow)?;
            }
        }
    }
    Ok(Line::Partial)
}

/// Judges a CRLF at the front of `bytes`; `other` is the error for any
/// first byte that is neither CR nor LF.
fn crlf(bytes: &[u8], other: ChunkedError) -> Result<Line<()>, ChunkedError> {
    match bytes.first() {
        None => Ok(Line::Partial),
        Some(b'\r') => match bytes.get(1) {
            None => Ok(Line::Partial),
            Some(b'\n') => Ok(Line::Complete { value: (), len: 2 }),
            Some(_) => Err(ChunkedError::BadLineEnd),
        },
        Some(b'\n') => Err(ChunkedError::BadLineEnd),
        Some(_) => Err(other),
    }
}

fn hex_value(b: u8) -> Option<u64> {
    let v = match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => return None,
    };
    Some(u64::from(v))
}

#[cfg(test)]
mod tests;
