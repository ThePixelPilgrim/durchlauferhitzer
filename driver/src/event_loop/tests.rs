//! Unit tests of the loop's own decisions, on a `Driver` that is built but
//! never run.

use std::io;

use durchlauferhitzer_core::config::load;
use durchlauferhitzer_core::{Core, Interest};

use super::Driver;
use super::upstreams::{WriteMode, write_mode};
use crate::conns::{Eof, Writes};
use crate::log::LogSink;

/// A config with a static root: the core may emit `OpenFile`.
const STATIC: &str = "listen 127.0.0.1:8080; host a.test { path / { static /srv; } }";

/// DRV-09: every `OpenFile` gets an answer, so a core with static roots
/// needs the disk pool; without one the driver refuses to start rather
/// than drop file actions.
#[test]
fn a_core_with_static_roots_and_no_pool_is_refused() {
    let core = Core::new(load(STATIC).unwrap());
    let (log, _logs) = LogSink::channel();
    let Err(err) = Driver::new(core, Vec::new(), Vec::new(), None, log) else {
        panic!("a driver without a pool was built for a core with static roots");
    };
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

/// DRV-10: under `ReadWrite` a failed write is left to the reads, which
/// report it after the backend's bytes; once the end of stream was
/// delivered there are no more reads, so the write reports it at once.
#[test]
fn a_failed_write_after_the_end_of_stream_is_reported_at_once() {
    assert_eq!(
        write_mode(Interest::ReadWrite, Writes::Open, Eof::NotYet),
        Some(WriteMode::WithReads)
    );
    assert_eq!(
        write_mode(Interest::ReadWrite, Writes::Open, Eof::Delivered),
        Some(WriteMode::Alone)
    );
    for eof in [Eof::NotYet, Eof::Delivered] {
        assert_eq!(
            write_mode(Interest::Write, Writes::Open, eof),
            Some(WriteMode::Alone)
        );
        assert_eq!(write_mode(Interest::ReadWrite, Writes::Failed, eof), None);
        assert_eq!(write_mode(Interest::Read, Writes::Open, eof), None);
        assert_eq!(write_mode(Interest::None, Writes::Open, eof), None);
    }
}
