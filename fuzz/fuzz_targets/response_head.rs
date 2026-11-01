//! The response-head parser (`ResponseParser`) never panics, an `Interim`
//! or `Complete` head's length is at most the input's, and a prefix yields
//! `Incomplete` or the whole input's verdict, also when the same parser
//! then resumes on the whole input.

#![no_main]

use durchlauferhitzer_core::http::response_head::{RequestKind, ResponseParser, ResponseStatus};
use libfuzzer_sys::fuzz_target;

mod head_input;
use head_input::HeadInput;

fuzz_target!(|data: &[u8]| {
    let Some(input) = HeadInput::read(data) else {
        return;
    };
    let limits = &input.limits;
    let kind = match input.kind & 1 {
        0 => RequestKind::Head,
        _ => RequestKind::Other,
    };
    let whole = ResponseParser::new().advance(input.bytes, limits, kind);
    match &whole {
        Ok(ResponseStatus::Interim { consumed } | ResponseStatus::Complete { consumed, .. }) => {
            assert!(*consumed <= input.bytes.len());
        }
        Ok(ResponseStatus::Incomplete) | Err(_) => {}
    }

    let (prefix, _) = input.bytes.split_at(input.cut);
    let mut parser = ResponseParser::new();
    let resumed = match parser.advance(prefix, limits, kind) {
        Ok(ResponseStatus::Incomplete) => parser.advance(input.bytes, limits, kind),
        verdict => {
            assert_eq!(verdict, whole, "a prefix contradicts the whole input");
            verdict
        }
    };
    assert_eq!(
        resumed, whole,
        "ResponseParser in two steps differs from one"
    );
});
