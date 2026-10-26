//! Strict, incremental validator for a chunked message body.
//!
//! The rules are deliberately narrower than RFC 9112, because lenient chunk
//! parsing is a request-smuggling surface: a chunk size is 1 to 16 hex digits
//! in either case, chunk extensions (`;`) are refused, trailer fields are
//! refused (the body ends with exactly `0\r\n\r\n`), and only CRLF is a line
//! ending.
//!
//! [`ChunkedBody`] judges every byte exactly once, never allocates, and never
//! looks at chunk data. The caller passes bytes through unchanged and uses the
//! verdict only to know where the body ends or that it is malformed. Bytes of
//! chunk data are skipped in bulk, so the cost is linear in the number of
//! framing bytes.

/// Incremental validator of a chunked body. Each byte is judged exactly once;
/// it never allocates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkedBody {
    state: State,
    data_len: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Reading the hex digits of a chunk size.
    Size {
        digits: u8,
        value: u64,
    },
    /// The size line's CR has been read; its LF must follow.
    SizeCr {
        value: u64,
    },
    /// Inside chunk data, `left` bytes to go.
    Data {
        left: u64,
    },
    /// After the data: CR expected.
    DataCr,
    /// After the data: LF expected.
    DataLf,
    /// After the last chunk's `0\r\n`: the closing CR (no trailer fields).
    LastCr,
    /// The closing LF.
    LastLf,
    Done,
    Failed(ChunkError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chunked {
    /// All of `input` is a valid prefix of a chunked body; more is needed.
    More,
    /// The body ended after the first `used` bytes of `input` (the final
    /// `0\r\n\r\n` included); the rest belongs to whatever follows.
    Done { used: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkError {
    /// A byte that is not a hex digit where a chunk size is expected.
    BadSize,
    /// More than 16 hex digits in a chunk size.
    SizeTooLong,
    /// A chunk extension (`;`).
    Extension,
    /// A trailer field after the last chunk.
    Trailer,
    /// A line ending that is not CRLF, or data not followed by CRLF.
    BadLineEnding,
}

/// Most hex digits a chunk size may have (it must fit a `u64`).
const MAX_SIZE_DIGITS: u8 = 16;

impl Default for ChunkedBody {
    fn default() -> ChunkedBody {
        ChunkedBody::new()
    }
}

impl ChunkedBody {
    pub const fn new() -> ChunkedBody {
        ChunkedBody {
            state: State::Size {
                digits: 0,
                value: 0,
            },
            data_len: 0,
        }
    }

    /// Judge `input`, the bytes that follow those already judged.
    ///
    /// Once the body is `Done`, later calls return `Done { used: 0 }` (no
    /// byte of the new input belongs to the body); after an error they return
    /// that same error. A spent validator is never reused.
    pub fn advance(&mut self, input: &[u8]) -> Result<Chunked, ChunkError> {
        let mut i = 0;
        loop {
            match self.state {
                State::Done => {
                    return Ok(Chunked::Done { used: i });
                }
                State::Failed(e) => return Err(e),
                State::Data { left } => {
                    let rest = input.len().saturating_sub(i);
                    let take = left.min(u64::try_from(rest).unwrap_or(u64::MAX));
                    let skip = usize::try_from(take).unwrap_or(rest);
                    i += skip;
                    self.data_len = self.data_len.saturating_add(take);
                    let left = left.saturating_sub(take);
                    self.state = if left == 0 {
                        State::DataCr
                    } else {
                        State::Data { left }
                    };
                    if left != 0 {
                        return Ok(Chunked::More);
                    }
                }
                State::Size { .. }
                | State::SizeCr { .. }
                | State::DataCr
                | State::DataLf
                | State::LastCr
                | State::LastLf => {
                    let Some(&byte) = input.get(i) else {
                        return Ok(Chunked::More);
                    };
                    i += 1;
                    self.state = self.step(byte);
                }
            }
        }
    }

    /// Payload bytes (chunk data, without framing) judged so far.
    pub fn data_len(&self) -> u64 {
        self.data_len
    }

    /// The next state after one framing byte (never called in `Data`,
    /// `Done` or `Failed`).
    fn step(&self, byte: u8) -> State {
        match self.state {
            State::Size { digits, value } => match byte {
                b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F' => {
                    if digits >= MAX_SIZE_DIGITS {
                        State::Failed(ChunkError::SizeTooLong)
                    } else {
                        State::Size {
                            digits: digits + 1,
                            value: (value << 4) | u64::from(hex_value(byte)),
                        }
                    }
                }
                b';' => State::Failed(ChunkError::Extension),
                b'\r' if digits > 0 => State::SizeCr { value },
                b'\n' if digits > 0 => State::Failed(ChunkError::BadLineEnding),
                _ => State::Failed(ChunkError::BadSize),
            },
            State::SizeCr { value } => match byte {
                b'\n' if value == 0 => State::LastCr,
                b'\n' => State::Data { left: value },
                _ => State::Failed(ChunkError::BadLineEnding),
            },
            State::DataCr => match byte {
                b'\r' => State::DataLf,
                _ => State::Failed(ChunkError::BadLineEnding),
            },
            State::DataLf => match byte {
                b'\n' => State::Size {
                    digits: 0,
                    value: 0,
                },
                _ => State::Failed(ChunkError::BadLineEnding),
            },
            State::LastCr => match byte {
                b'\r' => State::LastLf,
                b'\n' => State::Failed(ChunkError::BadLineEnding),
                _ => State::Failed(ChunkError::Trailer),
            },
            State::LastLf => match byte {
                b'\n' => State::Done,
                _ => State::Failed(ChunkError::BadLineEnding),
            },
            State::Data { .. } | State::Done | State::Failed(_) => self.state,
        }
    }
}

/// Value of an ASCII hex digit; the caller has checked the byte is one.
fn hex_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => byte - b'A' + 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const SEVENTEEN_DIGITS: &[u8] = b"10000000000000000\r\n";

    fn valid_rows() -> Vec<(&'static [u8], Result<Chunked, ChunkError>, u64)> {
        vec![
            (b"0\r\n\r\n", Ok(Chunked::Done { used: 5 }), 0),
            (
                b"5\r\nhello\r\n0\r\n\r\nGET",
                Ok(Chunked::Done { used: 15 }),
                5,
            ),
            (
                b"A\r\n0123456789\r\n0\r\n\r\n",
                Ok(Chunked::Done { used: 20 }),
                10,
            ),
            (b"a\r\n", Ok(Chunked::More), 0),
            (b"000000000000000F\r\n", Ok(Chunked::More), 0),
            (
                b"0005\r\nhello\r\n0\r\n\r\n",
                Ok(Chunked::Done { used: 18 }),
                5,
            ),
        ]
    }

    fn invalid_rows() -> Vec<(&'static [u8], ChunkError)> {
        vec![
            (b"\r\n", ChunkError::BadSize),
            (b"g\r\n", ChunkError::BadSize),
            (b" 5\r\n", ChunkError::BadSize),
            (b"5 \r\n", ChunkError::BadSize),
            (SEVENTEEN_DIGITS, ChunkError::SizeTooLong),
            (b"5;x=y\r\n", ChunkError::Extension),
            (b"0\r\nX-T: 1\r\n\r\n", ChunkError::Trailer),
            (b"5\nhello\r\n", ChunkError::BadLineEnding),
            (b"5\r\nhelloX\r\n", ChunkError::BadLineEnding),
            (b"0\r\n\n", ChunkError::BadLineEnding),
            (b"0\r\nX", ChunkError::Trailer),
        ]
    }

    #[test]
    fn valid_bodies_table() {
        for (input, expected, data_len) in valid_rows() {
            let mut body = ChunkedBody::new();
            assert_eq!(body.advance(input), expected, "{input:?}");
            assert_eq!(body.data_len(), data_len, "{input:?}");
        }
    }

    #[test]
    fn invalid_bodies_table() {
        for (input, error) in invalid_rows() {
            let mut body = ChunkedBody::new();
            assert_eq!(body.advance(input), Err(error), "{input:?}");
        }
    }

    #[test]
    fn size_overflow_is_too_long() {
        let mut body = ChunkedBody::new();
        assert_eq!(body.advance(b"FFFFFFFFFFFFFFFF\r\n"), Ok(Chunked::More));
        // The validator now wants u64::MAX data bytes; it counts them without
        // overflowing and does not finish.
        assert_eq!(body.advance(b"abc"), Ok(Chunked::More));
        assert_eq!(body.data_len(), 3);
        // A digit past the 16th is refused outright.
        let mut body = ChunkedBody::new();
        assert_eq!(
            body.advance(b"FFFFFFFFFFFFFFFFF"),
            Err(ChunkError::SizeTooLong)
        );
    }

    #[test]
    fn data_len_saturates_instead_of_overflowing() {
        let mut body = ChunkedBody::new();
        body.data_len = u64::MAX - 1;
        assert_eq!(body.advance(b"5\r\nhello"), Ok(Chunked::More));
        assert_eq!(body.data_len(), u64::MAX);
    }

    #[test]
    fn spent_validator_repeats_its_verdict() {
        let mut body = ChunkedBody::new();
        assert_eq!(body.advance(b"0\r\n\r\n"), Ok(Chunked::Done { used: 5 }));
        assert_eq!(
            body.advance(b"GET / HTTP/1.1"),
            Ok(Chunked::Done { used: 0 })
        );
        assert_eq!(body.advance(b""), Ok(Chunked::Done { used: 0 }));

        let mut body = ChunkedBody::new();
        assert_eq!(body.advance(b"g"), Err(ChunkError::BadSize));
        assert_eq!(body.advance(b"0\r\n\r\n"), Err(ChunkError::BadSize));
        assert_eq!(body.advance(b""), Err(ChunkError::BadSize));
    }

    /// Feed `input` in pieces cut at `cuts`; return the first non-`More`
    /// verdict with `used` made relative to the whole input, or `More`.
    fn feed_split(input: &[u8], cuts: &[usize]) -> (Result<Chunked, ChunkError>, u64) {
        let mut points: Vec<usize> = cuts.iter().map(|c| c % (input.len() + 1)).collect();
        points.push(input.len());
        points.sort_unstable();
        let mut body = ChunkedBody::new();
        let mut start = 0;
        for end in points {
            let piece = input.get(start..end).unwrap();
            match body.advance(piece) {
                Ok(Chunked::More) => {}
                Ok(Chunked::Done { used }) => {
                    return (Ok(Chunked::Done { used: start + used }), body.data_len());
                }
                Err(e) => return (Err(e), body.data_len()),
            }
            start = end;
        }
        (Ok(Chunked::More), body.data_len())
    }

    proptest! {
        #[test]
        fn split_anywhere_gives_the_same_verdict(
            cuts in proptest::collection::vec(0..64usize, 0..8),
        ) {
            for (input, expected, data_len) in valid_rows() {
                let (verdict, len) = feed_split(input, &cuts);
                prop_assert_eq!(verdict, expected);
                prop_assert_eq!(len, data_len);
            }
            for (input, error) in invalid_rows() {
                let (verdict, _) = feed_split(input, &cuts);
                prop_assert_eq!(verdict, Err(error));
            }
        }

        #[test]
        fn never_panics_on_arbitrary_bytes(
            bytes in proptest::collection::vec(any::<u8>(), 0..512),
            cuts in proptest::collection::vec(0..512usize, 0..8),
        ) {
            let whole = ChunkedBody::new().advance(&bytes);
            let (split, _) = feed_split(&bytes, &cuts);
            prop_assert_eq!(split, whole);
        }
    }
}
