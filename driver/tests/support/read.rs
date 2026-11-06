//! A `read` for the tests' sockets that retries `EINTR`.
//!
//! The tests set read timeouts (`SO_RCVTIMEO`), and Linux never restarts a
//! socket call with a timeout after a signal: it fails with `EINTR`. The
//! test process gets `SIGCHLD` from the children its threads spawn (the
//! server, the `sh` behind `signal_pid`); usually the default disposition
//! drops it, but one that arrives while the forking thread has signals
//! blocked (glibc blocks them all inside `posix_spawn`, for one) is
//! queued and then delivered to any thread, interrupting a blocked `read`
//! there. Seen as `Interrupted` panics under parallel load (reproduced
//! with kernel signal tracing, 2026-11-06). `read_exact` and `read_to_end`
//! retry by themselves; a bare `read` does not.

use std::io::{self, ErrorKind, Read};

/// [`Read::read`], retried while it fails with `EINTR`.
pub fn read_some(stream: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match stream.read(buf) {
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}
