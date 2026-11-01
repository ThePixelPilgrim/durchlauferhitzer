//! The chunked-body validator never panics, never consumes more than it
//! was given nor reports more data than it consumed, leaves at most
//! `MAX_UNCONSUMED` bytes before the body ends, and gives the same totals
//! and the same error whether the body arrives at once or in pieces (fed
//! by its calling convention: the unconsumed tail plus the new bytes).

#![no_main]

use durchlauferhitzer_core::http::chunked::{ChunkedValidator, Done, MAX_UNCONSUMED};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let [step, body @ ..] = data else {
        return;
    };
    let whole = ChunkedValidator::new().advance(body);
    if let Ok(progress) = whole {
        assert!(progress.consumed <= body.len());
        assert!(progress.data <= progress.consumed as u64);
        if progress.done == Done::No {
            assert!(body.len() - progress.consumed <= MAX_UNCONSUMED);
        }
    }

    let mut validator = ChunkedValidator::new();
    let mut kept: Vec<u8> = Vec::new();
    let (mut consumed, mut payload, mut done) = (0usize, 0u64, Done::No);
    let mut error = None;
    for piece in body.chunks(1 + usize::from(*step)) {
        kept.extend_from_slice(piece);
        match validator.advance(&kept) {
            Ok(progress) => {
                assert!(progress.consumed <= kept.len());
                assert!(progress.data <= progress.consumed as u64);
                kept.drain(..progress.consumed);
                consumed += progress.consumed;
                payload += progress.data;
                done = progress.done;
                if done == Done::No {
                    assert!(kept.len() <= MAX_UNCONSUMED);
                }
            }
            Err(e) => {
                error = Some(e);
                break;
            }
        }
    }
    match (whole, error) {
        (Ok(progress), None) => {
            assert_eq!(
                (progress.consumed, progress.data, progress.done),
                (consumed, payload, done),
                "pieces give other totals than the whole body"
            );
        }
        (Err(at_once), Some(in_pieces)) => assert_eq!(at_once, in_pieces),
        (at_once, in_pieces) => {
            panic!("whole body: {at_once:?}, in pieces: error {in_pieces:?}")
        }
    }
});
