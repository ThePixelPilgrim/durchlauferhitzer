//! The request-head parser (`parse_head`, `HeadParser`) never panics, a
//! `Complete` head's length is at most the input's, a prefix yields
//! `Incomplete` or the whole input's verdict, and the resumable parser fed
//! in two steps gives exactly the verdict of `parse_head`.

#![no_main]

use durchlauferhitzer_core::http::request::{HeadParser, ParseStatus, parse_head};
use libfuzzer_sys::fuzz_target;

#[allow(dead_code, reason = "the request kind is for the response-head target")]
mod head_input;
use head_input::HeadInput;

fuzz_target!(|data: &[u8]| {
    let Some(input) = HeadInput::read(data) else {
        return;
    };
    let limits = &input.limits;
    let whole = parse_head(input.bytes, limits);
    if let Ok(ParseStatus::Complete { consumed, .. }) = &whole {
        assert!(*consumed <= input.bytes.len());
    }

    let (prefix, _) = input.bytes.split_at(input.cut);
    let early = parse_head(prefix, limits);
    match &early {
        Ok(ParseStatus::Incomplete) => {}
        verdict => assert_eq!(verdict, &whole, "a prefix contradicts the whole input"),
    }

    let mut parser = HeadParser::new();
    let resumed = match parser.advance(prefix, limits) {
        Ok(ParseStatus::Incomplete) => parser.advance(input.bytes, limits),
        verdict => verdict,
    };
    assert_eq!(
        resumed, whole,
        "HeadParser in two steps differs from parse_head"
    );
});
