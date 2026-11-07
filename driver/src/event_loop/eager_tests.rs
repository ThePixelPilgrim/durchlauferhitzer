//! Eager I/O at its edges (eager.rs): the per-turn budget, and an eager
//! read that meets the end of stream or an error.

use std::io::Write;
use std::time::Duration;

use durchlauferhitzer_core::{ConnId, Log};

use super::eager::{EAGER_BUDGET, Socket};
use super::tests::{driver, known_client};
use crate::conns::Hint;
use crate::log::LogEntry;
use crate::sys::epoll::EpollEvent;

/// Marks `conn` as likely readable and notes it, as a `SetInterest` would.
/// Its socket is made non-blocking, as the driver's sockets are, so a
/// read ends at `EAGAIN`.
fn note_readable(driver: &mut super::Driver, conn: ConnId) {
    let fd = driver.clients[&conn].fd.try_clone().unwrap();
    std::os::unix::net::UnixStream::from(fd)
        .set_nonblocking(true)
        .unwrap();
    driver.clients.get_mut(&conn).unwrap().hints.read = Hint::Ready;
    driver.note(Socket::Client(conn));
}

/// One turn reads at most `EAGER_BUDGET` sockets eagerly; the rest keep
/// their registration and are read after the wait.
#[test]
fn sockets_beyond_the_budget_are_served_after_the_wait() {
    let (mut driver, _logs) = driver();
    let n = EAGER_BUDGET + 3;
    let conns: Vec<ConnId> = (1..=n as u64).map(ConnId).collect();
    let mut peers: Vec<_> = conns
        .iter()
        .map(|&conn| known_client(&mut driver, conn))
        .collect();
    for (&conn, peer) in conns.iter().zip(&mut peers) {
        // Part of a request line: the core reads it and waits for more.
        peer.write_all(b"GE").unwrap();
        note_readable(&mut driver, conn);
    }
    driver.settle().unwrap();
    let mut events = vec![EpollEvent::EMPTY; 2 * n];
    let ready = driver
        .epoll
        .wait(&mut events, Some(Duration::ZERO))
        .unwrap();
    // The eager reads drained the first EAGER_BUDGET sockets.
    assert_eq!(ready, n - EAGER_BUDGET);
    for event in events.iter().take(ready) {
        driver.handle_ready(event).unwrap();
    }
    driver.settle().unwrap();
    assert_eq!(
        driver
            .epoll
            .wait(&mut events, Some(Duration::ZERO))
            .unwrap(),
        0,
        "every socket was read"
    );
    assert_eq!(driver.clients.len(), n);
}

/// An eager read that meets the end of stream reports it like any read:
/// the idle connection closes.
#[test]
fn an_eager_read_reports_the_end_of_stream() {
    let (mut driver, _logs) = driver();
    let conn = ConnId(1);
    let peer = known_client(&mut driver, conn);
    drop(peer);
    note_readable(&mut driver, conn);
    driver.settle().unwrap();
    assert!(!driver.clients.contains_key(&conn));
}

/// An eager read that meets an error reports `Failed`: the peer closed
/// with our bytes unread, so the read gets `ECONNRESET`.
#[test]
fn an_eager_read_reports_an_error() {
    let (mut driver, logs) = driver();
    let conn = ConnId(1);
    let peer = known_client(&mut driver, conn);
    let ours = driver.clients[&conn].fd.try_clone().unwrap();
    std::fs::File::from(ours).write_all(b"x").unwrap();
    drop(peer);
    note_readable(&mut driver, conn);
    driver.settle().unwrap();
    assert!(!driver.clients.contains_key(&conn));
    let logs: Vec<LogEntry> = logs.try_iter().collect();
    assert!(
        logs.contains(&LogEntry::Core(Log::Reset {
            conn,
            peer: super::tests::peer(),
        })),
        "{logs:?}"
    );
}
