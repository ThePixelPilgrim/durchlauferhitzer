use super::*;
use proptest::prelude::*;

/// Outcome of feeding a whole input: the totals over all calls, or the
/// first error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Progress {
        consumed: usize,
        data: u64,
        done: Done,
    },
    Failed(ChunkedError),
}

fn once(input: &[u8]) -> Result<ChunkProgress, ChunkedError> {
    ChunkedValidator::new().advance(input)
}

fn progress(consumed: usize, data: u64, done: Done) -> Result<ChunkProgress, ChunkedError> {
    Ok(ChunkProgress {
        consumed,
        data,
        done,
    })
}

/// Feeds `input` in pieces ending at each of `cuts` (and finally the whole
/// input) under the documented calling convention: the caller keeps a
/// buffer, appends each new piece, passes the whole buffer, then drops the
/// `consumed` bytes from its front. Returns the outcome and the largest
/// number of unconsumed bytes the caller had to keep before the end.
fn feed(input: &[u8], cuts: &[usize]) -> (Outcome, usize) {
    let mut v = ChunkedValidator::new();
    let mut ends: Vec<usize> = cuts.iter().map(|&c| c.min(input.len())).collect();
    ends.sort_unstable();
    ends.push(input.len());
    let mut buf: Vec<u8> = Vec::new();
    let mut start = 0;
    let (mut consumed, mut data, mut done) = (0usize, 0u64, Done::No);
    let mut max_tail = 0;
    for end in ends {
        buf.extend_from_slice(input.get(start..end).unwrap_or_default());
        start = end;
        match v.advance(&buf) {
            Err(e) => return (Outcome::Failed(e), max_tail),
            Ok(p) => {
                assert!(p.consumed <= buf.len());
                assert!(p.data <= p.consumed as u64);
                consumed += p.consumed;
                data += p.data;
                done = p.done;
                buf.drain(..p.consumed);
                if p.done == Done::No {
                    max_tail = max_tail.max(buf.len());
                }
            }
        }
    }
    (
        Outcome::Progress {
            consumed,
            data,
            done,
        },
        max_tail,
    )
}

#[test]
fn simple_body() {
    let body = b"5\r\nhello\r\nA\r\n0123456789\r\n0\r\n\r\n";
    assert_eq!(once(body), progress(body.len(), 15, Done::Yes));
}

#[test]
fn hex_digits_are_case_insensitive_and_may_have_leading_zeros() {
    let body = b"000a\r\n0123456789\r\nA\r\n0123456789\r\n00\r\n\r\n";
    assert_eq!(once(body), progress(body.len(), 20, Done::Yes));
}

#[test]
fn zero_length_body() {
    assert_eq!(once(b"0\r\n\r\n"), progress(5, 0, Done::Yes));
}

#[test]
fn empty_input_consumes_nothing() {
    assert_eq!(once(b""), progress(0, 0, Done::No));
}

#[test]
fn extension_rejected() {
    assert_eq!(
        once(b"5;x=y\r\nhello\r\n0\r\n\r\n"),
        Err(ChunkedError::Extension)
    );
    assert_eq!(once(b"0;x\r\n\r\n"), Err(ChunkedError::Extension));
    assert_eq!(once(b"5;"), Err(ChunkedError::Extension));
}

#[test]
fn trailer_rejected() {
    assert_eq!(once(b"0\r\nX: y\r\n\r\n"), Err(ChunkedError::Trailer));
    assert_eq!(
        once(b"5\r\nhello\r\n0\r\nX: y\r\n\r\n"),
        Err(ChunkedError::Trailer)
    );
}

#[test]
fn leading_plus_and_whitespace_rejected() {
    for bad in [
        &b"+5\r\nhello\r\n0\r\n\r\n"[..],
        b"-5\r\nhello\r\n",
        b" 5\r\nhello\r\n",
        b"5 \r\nhello\r\n",
        b"\t5\r\nhello\r\n",
        b"5\t\r\nhello\r\n",
        b"5 ;x\r\nhello\r\n",
        b"\r\nhello\r\n",
        b";x\r\n",
        b"0x5\r\nhello\r\n",
        b"g\r\n",
    ] {
        assert_eq!(once(bad), Err(ChunkedError::BadSize), "{bad:?}");
    }
}

#[test]
fn overflow_rejected() {
    // 17 significant hex digits never fit u64.
    assert_eq!(
        once(b"10000000000000000\r\n"),
        Err(ChunkedError::SizeOverflow)
    );
    assert_eq!(once(b"10000000000000000"), Err(ChunkedError::SizeOverflow));
    // 16 digits fit, also behind leading zeros.
    assert_eq!(once(b"ffffffffffffffff\r\n"), progress(18, 0, Done::No));
    assert_eq!(once(b"0000ffffffffffffffff\r\n"), progress(22, 0, Done::No));
}

#[test]
fn size_longer_than_32_characters_rejected() {
    let mut line = vec![b'0'; 31];
    line.extend_from_slice(b"5\r\nhello\r\n0\r\n\r\n");
    assert_eq!(once(&line), progress(line.len(), 5, Done::Yes));

    let mut line = vec![b'0'; 32];
    line.extend_from_slice(b"5\r\n");
    assert_eq!(once(&line), Err(ChunkedError::BadSize));
    // Rejected before any line end arrives: the scan is bounded.
    assert_eq!(once(&[b'0'; 33]), Err(ChunkedError::BadSize));
}

#[test]
fn bare_lf_rejected() {
    for bad in [
        &b"5\nhello\r\n0\r\n\r\n"[..],
        b"5\r\nhello\n0\r\n\r\n",
        b"5\r\nhello\r\n0\n\r\n",
        b"5\r\nhello\r\n0\r\n\n",
        b"\n",
    ] {
        assert_eq!(once(bad), Err(ChunkedError::BadLineEnd), "{bad:?}");
    }
}

#[test]
fn cr_without_lf_rejected() {
    for bad in [&b"5\rhello"[..], b"5\r\nhello\rx", b"0\r\n\rx"] {
        assert_eq!(once(bad), Err(ChunkedError::BadLineEnd), "{bad:?}");
    }
}

#[test]
fn data_longer_than_declared_rejected() {
    assert_eq!(
        once(b"5\r\nhello!\r\n0\r\n\r\n"),
        Err(ChunkedError::BadLineEnd)
    );
}

#[test]
fn stops_at_end() {
    // A request smuggled behind the final chunk belongs to the next
    // message: it is never consumed.
    let body = b"5\r\nhello\r\n0\r\n\r\n";
    let mut input = body.to_vec();
    input.extend_from_slice(b"GET /admin HTTP/1.1\r\nHost: x\r\n\r\n");
    let mut v = ChunkedValidator::new();
    assert_eq!(v.advance(&input), progress(body.len(), 5, Done::Yes));
    let rest = input.get(body.len()..).unwrap_or_default();
    assert_eq!(v.advance(rest), progress(0, 0, Done::Yes));
    assert_eq!(v.advance(b"0\r\n\r\n"), progress(0, 0, Done::Yes));
}

#[test]
fn partial_lines_are_not_consumed() {
    let mut v = ChunkedValidator::new();
    assert_eq!(v.advance(b"5"), progress(0, 0, Done::No));
    assert_eq!(v.advance(b"5\r"), progress(0, 0, Done::No));
    // The size line is final once its CRLF is complete; data bytes are
    // consumed as they arrive.
    assert_eq!(v.advance(b"5\r\nhe"), progress(5, 2, Done::No));
    assert_eq!(v.advance(b"llo\r"), progress(3, 3, Done::No));
    assert_eq!(v.advance(b"\r\n0\r\n"), progress(2, 0, Done::No));
    assert_eq!(v.advance(b"0\r\n\r"), progress(0, 0, Done::No));
    assert_eq!(v.advance(b"0\r\n\r\n"), progress(5, 0, Done::Yes));
}

#[test]
fn errors_are_sticky() {
    let mut v = ChunkedValidator::new();
    assert_eq!(v.advance(b"5;x"), Err(ChunkedError::Extension));
    assert_eq!(v.advance(b"0\r\n\r\n"), Err(ChunkedError::Extension));
}

#[test]
fn byte_by_byte_matches_whole() {
    let body = b"3\r\nabc\r\n10\r\n0123456789abcdef\r\n0\r\n\r\n";
    let cuts: Vec<usize> = (0..body.len()).collect();
    assert_eq!(
        feed(body, &cuts).0,
        Outcome::Progress {
            consumed: body.len(),
            data: 19,
            done: Done::Yes
        }
    );
}

/// Building blocks for chunked bodies, valid and subtly broken.
fn piece() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        8 => (1usize..40).prop_map(|n| {
            let mut c = format!("{n:x}\r\n").into_bytes();
            c.extend(std::iter::repeat_n(b'x', n));
            c.extend_from_slice(b"\r\n");
            c
        }),
        2 => (1usize..20).prop_map(|n| {
            let mut c = format!("000{n:X}\r\n").into_bytes();
            c.extend(std::iter::repeat_n(b'\r', n));
            c.extend_from_slice(b"\r\n");
            c
        }),
        1 => Just(b"0\r\n\r\n".to_vec()),
        1 => Just(b"0\r\nX: y\r\n\r\n".to_vec()),
        1 => Just(b"5;x\r\nhello\r\n".to_vec()),
        1 => Just(b"5\nhello\r\n".to_vec()),
        1 => Just(b"5\r\nhello\n".to_vec()),
        1 => Just(b"+5\r\nhello\r\n".to_vec()),
        1 => Just(b"10000000000000000\r\n".to_vec()),
        1 => Just(vec![b'0'; 40]),
        1 => Just(b"GET / HTTP/1.1\r\n\r\n".to_vec()),
        1 => proptest::collection::vec(any::<u8>(), 0..6),
    ]
}

proptest! {
    #[test]
    fn split_independent(
        pieces in proptest::collection::vec(piece(), 0..8),
        cuts in proptest::collection::vec(0..400usize, 0..16),
    ) {
        let input: Vec<u8> = pieces.concat();
        let (whole, _) = feed(&input, &[]);
        let (split, tail) = feed(&input, &cuts);
        let all: Vec<usize> = (0..input.len()).collect();
        let (bytewise, bytewise_tail) = feed(&input, &all);
        prop_assert_eq!(split, whole);
        prop_assert_eq!(bytewise, whole);
        prop_assert!(tail <= MAX_UNCONSUMED, "tail {}", tail);
        prop_assert!(bytewise_tail <= MAX_UNCONSUMED, "tail {}", bytewise_tail);
    }

    #[test]
    fn never_panics_and_never_overreaches(
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        let mut v = ChunkedValidator::new();
        if let Ok(p) = v.advance(&bytes) {
            prop_assert!(p.consumed <= bytes.len());
            prop_assert!(p.data <= p.consumed as u64);
        }
    }
}

#[test]
fn valid_body_split_everywhere_twice() {
    let body = b"1\r\na\r\n02\r\nbc\r\n0\r\n\r\nrest";
    let n = body.len();
    for i in 0..=n {
        for j in i..=n {
            assert_eq!(
                feed(body, &[i, j]).0,
                Outcome::Progress {
                    consumed: n - 4,
                    data: 3,
                    done: Done::Yes
                },
                "cuts {i} {j}"
            );
        }
    }
}
