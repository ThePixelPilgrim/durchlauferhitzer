//! The config parser and validator (`load`) never panic on any text.

#![no_main]

use durchlauferhitzer_core::config::load;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = load(&String::from_utf8_lossy(data));
});
