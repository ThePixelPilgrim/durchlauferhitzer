//! Unit tests of the loop's own decisions, on a `Driver` that is built but
//! never run.

use std::io;

use durchlauferhitzer_core::Core;
use durchlauferhitzer_core::config::load;

use super::Driver;
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
