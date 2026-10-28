//! Unit tests of the loop's own decisions, on a `Driver` that is built but
//! never run.

use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use durchlauferhitzer_core::config::load;
use durchlauferhitzer_core::{ConnError, ConnId, Core, Event, FileId, Interest, Log, UpstreamId};

use super::accept::Accepting;
use super::upstreams::{WriteMode, write_mode};
use super::{Broken, Driver};
use crate::conns::{Answer, Client, Eof, Phase, Token, Upstream, Writes};
use crate::log::{DriverLog, LogEntry, LogSink};
use crate::pool::JobResult;
use crate::sys::epoll::{EpollEvent, Readiness};

/// A config without static roots, so a driver needs no pool.
const STATIC_LESS: &str = "listen 127.0.0.1:8080; host a.test { path / { proxy 127.0.0.1:9; \
    max_request_body 1MiB; max_response_body 1MiB; } }";

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

/// DRV-07: the driver gets one opened root per static root of the core,
/// in order; a list that does not match is refused, not guessed at.
#[test]
fn a_root_list_that_does_not_match_the_core_is_refused() {
    let core = Core::new(load(STATIC).unwrap());
    let (log, _logs) = LogSink::channel();
    let Err(err) = Driver::new(core, Vec::new(), Vec::new(), Some(1), log) else {
        panic!("a driver without its root was built");
    };
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    let core = Core::new(load(STATIC_LESS).unwrap());
    let (log, _logs) = LogSink::channel();
    let Err(err) = Driver::new(core, Vec::new(), vec![unpollable()], None, log) else {
        panic!("a driver with a root the core does not have was built");
    };
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn zero_disk_threads_are_refused() {
    let core = Core::new(load(STATIC).unwrap());
    let (log, _logs) = LogSink::channel();
    let Err(err) = Driver::new(core, Vec::new(), vec![unpollable()], Some(0), log) else {
        panic!("a driver with no disk thread was built");
    };
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

/// A driver for [`STATIC`] (its root stood in for by `/dev/null`: no
/// open reaches it here) with one disk worker, and its log.
fn static_driver() -> (Driver, Receiver<LogEntry>) {
    let core = Core::new(load(STATIC).unwrap());
    let (log, logs) = LogSink::channel();
    let driver = Driver::new(core, Vec::new(), vec![unpollable()], Some(1), log).unwrap();
    (driver, logs)
}

/// R6: the driver shuts its disk pool down when `run` ends.
#[test]
fn run_shuts_the_pool_down_when_it_ends() {
    let (mut driver, _logs) = static_driver();
    assert!(driver.pool.is_some());
    driver.stop_handle().stop().unwrap();
    assert_eq!(driver.run().unwrap(), super::Exit::Stopped);
    assert!(driver.pool.is_none());
}

/// `EMFILE`/`ENFILE` from an open are logged by the driver (the budget
/// is wrong, NFR-RES-05); the core still gets `FileFailed`. Other
/// errors are the core's to log (as a 404).
#[test]
fn an_open_out_of_descriptors_is_logged() {
    let (mut driver, logs) = static_driver();
    for (file, errno) in [(1, libc::EMFILE), (2, libc::ENFILE), (3, libc::ENOENT)] {
        driver.files.opening(FileId(file));
        driver
            .pool_result(JobResult::OpenFailed {
                file: FileId(file),
                error: io::Error::from_raw_os_error(errno),
            })
            .unwrap();
    }
    assert_eq!(
        logs.try_iter().collect::<Vec<_>>(),
        [
            LogEntry::Driver(DriverLog::OpenFailed {
                errno: libc::EMFILE
            }),
            LogEntry::Driver(DriverLog::OpenFailed {
                errno: libc::ENFILE
            }),
        ]
    );
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
        assert_eq!(
            write_mode(Interest::ReadWrite, Writes::Failed(ConnError::Reset), eof),
            None
        );
        assert_eq!(write_mode(Interest::Read, Writes::Open, eof), None);
        assert_eq!(write_mode(Interest::None, Writes::Open, eof), None);
    }
}

/// A driver for [`STATIC_LESS`] with no listeners, and its log.
fn driver() -> (Driver, Receiver<LogEntry>) {
    let core = Core::new(load(STATIC_LESS).unwrap());
    let (log, logs) = LogSink::channel();
    let driver = Driver::new(core, Vec::new(), Vec::new(), None, log).unwrap();
    (driver, logs)
}

/// A descriptor `epoll_ctl` refuses (`EPERM`: `/dev/null` cannot be
/// polled), standing in for a socket whose registration fails.
fn unpollable() -> OwnedFd {
    OwnedFd::from(std::fs::File::open("/dev/null").unwrap())
}

fn peer() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

const REGISTER_FAILED: LogEntry =
    LogEntry::Driver(DriverLog::RegisterFailed { errno: libc::EPERM });

/// DRV-15: a client whose registration fails is that client's failure:
/// the driver logs it, the core gets `Failed` after the actions of the
/// call (here `Accepted`'s `SetInterest`), closes it, and the loop goes on.
#[test]
fn a_client_whose_registration_fails_is_reported_failed() {
    let (mut driver, logs) = driver();
    let conn = ConnId(1);
    driver.clients.insert(conn, Client::new(unpollable()));
    driver.next_conn = 2;
    driver
        .deliver(Event::Accepted { conn, peer: peer() })
        .unwrap();
    assert!(driver.clients.is_empty());
    let logs: Vec<LogEntry> = logs.try_iter().collect();
    assert_eq!(
        logs,
        [
            REGISTER_FAILED,
            LogEntry::Core(Log::Reset { conn, peer: peer() })
        ]
    );
}

/// DRV-15: a new socket whose registration fails is dropped before the
/// core hears of it (its first event must be `Accepted`, DRV-01), and
/// logged; its id is not used.
#[test]
fn an_accepted_socket_whose_registration_fails_is_dropped_and_logged() {
    let (mut driver, logs) = driver();
    driver.accepted(unpollable(), peer()).unwrap();
    assert!(driver.clients.is_empty());
    assert_eq!(driver.next_conn, 1);
    assert_eq!(logs.try_iter().collect::<Vec<_>>(), [REGISTER_FAILED]);
}

/// DRV-15: an open upstream whose registration fails is closed and
/// queued to be reported `UpstreamFailed` after the current call.
#[test]
fn an_open_upstream_whose_registration_fails_is_reported_failed() {
    let (mut driver, logs) = driver();
    let up = UpstreamId(1);
    driver.upstreams.insert(
        up,
        Upstream {
            phase: Phase::Open {
                fd: unpollable(),
                writes: Writes::Open,
                eof: Eof::NotYet,
            },
            interest: Interest::None,
            registered: None,
        },
    );
    driver.accepting.failed(driver.clock.now());
    driver.set_upstream_interest(up, Interest::Read).unwrap();
    assert!(matches!(driver.upstreams[&up].phase, Phase::Failed));
    // The socket was closed: paused listeners resume (DRV-16).
    assert_eq!(driver.accepting.state(), Accepting::Open);
    assert_eq!(driver.upstreams[&up].registered, None);
    assert_eq!(
        driver.broken.iter().copied().collect::<Vec<_>>(),
        [Broken::Upstream(up, ConnError::Other)]
    );
    assert_eq!(logs.try_iter().collect::<Vec<_>>(), [REGISTER_FAILED]);
}

/// DRV-15 with DRV-10: a connecting upstream whose registration fails is
/// a failed connect, answered `UpstreamFailed` as a later event.
#[test]
fn a_connecting_upstream_whose_registration_fails_is_a_failed_connect() {
    let (mut driver, logs) = driver();
    let up = UpstreamId(1);
    driver.upstreams.insert(
        up,
        Upstream {
            phase: Phase::Connecting(unpollable()),
            interest: Interest::None,
            registered: None,
        },
    );
    driver.accepting.failed(driver.clock.now());
    driver.sync_upstream(up).unwrap();
    assert!(matches!(
        driver.upstreams[&up].phase,
        Phase::Answering(Answer::Failed)
    ));
    assert_eq!(driver.accepting.state(), Accepting::Open);
    assert_eq!(driver.answers.iter().copied().collect::<Vec<_>>(), [up]);
    assert_eq!(
        driver.take_answer(up).unwrap(),
        Some(Event::UpstreamFailed {
            up,
            error: ConnError::Other
        })
    );
    assert_eq!(logs.try_iter().collect::<Vec<_>>(), [REGISTER_FAILED]);
}

/// DRV-16: while the listeners are paused, the wait ends at their retry
/// (1 s), also when the core has no deadline.
#[test]
fn paused_listeners_bound_the_wait_by_their_retry() {
    let (mut driver, _logs) = driver();
    assert_eq!(driver.timeout(), None);
    driver.accepting.failed(driver.clock.now());
    let timeout = driver.timeout().unwrap();
    assert!(
        timeout > Duration::from_millis(900) && timeout <= Duration::from_secs(1),
        "{timeout:?}"
    );
}

/// DRV-16: a descriptor the driver closes resumes paused listeners, an
/// upstream's as much as a client's.
#[test]
fn closing_a_client_or_an_upstream_resumes_paused_listeners() {
    let (mut driver, _logs) = driver();
    let conn = ConnId(1);
    driver.clients.insert(conn, Client::new(unpollable()));
    driver.accepting.failed(driver.clock.now());
    driver.close(conn).unwrap();
    assert_eq!(driver.accepting.state(), Accepting::Open);

    let up = UpstreamId(1);
    driver.upstreams.insert(
        up,
        Upstream {
            phase: Phase::Open {
                fd: unpollable(),
                writes: Writes::Open,
                eof: Eof::NotYet,
            },
            interest: Interest::None,
            registered: None,
        },
    );
    driver.accepting.failed(driver.clock.now());
    driver.close_upstream(up).unwrap();
    assert_eq!(driver.accepting.state(), Accepting::Open);
}

/// DRV-10, "bytes, end of stream, then the error": a write that failed
/// under `ReadWrite` is kept for the reads; when they reach the end of
/// stream no read follows, so the kept error is queued right behind the
/// `UpstreamReadClosed` being delivered, and the socket is closed.
#[test]
fn a_write_error_kept_for_the_reads_follows_the_end_of_stream() {
    let (mut driver, _logs) = driver();
    let up = UpstreamId(1);
    driver.upstreams.insert(
        up,
        Upstream {
            phase: Phase::Open {
                fd: unpollable(),
                writes: Writes::Failed(ConnError::Reset),
                eof: Eof::NotYet,
            },
            interest: Interest::ReadWrite,
            registered: None,
        },
    );
    driver.upstream_eof(up).unwrap();
    assert!(matches!(driver.upstreams[&up].phase, Phase::Failed));
    assert_eq!(
        driver.broken.iter().copied().collect::<Vec<_>>(),
        [Broken::Upstream(up, ConnError::Reset)]
    );
}

/// Without a kept write error the end of stream reports nothing more.
#[test]
fn an_end_of_stream_without_a_kept_write_error_queues_nothing() {
    let (mut driver, _logs) = driver();
    let up = UpstreamId(1);
    driver.upstreams.insert(
        up,
        Upstream {
            phase: Phase::Open {
                fd: unpollable(),
                writes: Writes::Open,
                eof: Eof::NotYet,
            },
            interest: Interest::Read,
            registered: None,
        },
    );
    driver.upstream_eof(up).unwrap();
    assert!(matches!(
        driver.upstreams[&up].phase,
        Phase::Open {
            eof: Eof::Delivered,
            ..
        }
    ));
    assert!(driver.broken.is_empty());
}

/// A registered client socket the core knows, with the peer's end.
fn known_client(driver: &mut Driver, conn: ConnId) -> UnixStream {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let fd = OwnedFd::from(ours);
    let raw = Token::Client(conn).encode().unwrap();
    driver.epoll.add(fd.as_fd(), raw, Readiness::None).unwrap();
    driver.clients.insert(conn, Client::new(fd));
    driver.next_conn = conn.0 + 1;
    driver
        .deliver(Event::Accepted { conn, peer: peer() })
        .unwrap();
    theirs
}

/// A failure queued outside any `deliver` (say, by a write's
/// registration change) is delivered once the ready descriptor that
/// queued it is handled, not left for an unrelated later call.
#[test]
fn a_failure_queued_while_handling_a_descriptor_is_delivered_after_it() {
    let (mut driver, logs) = driver();
    let conn = ConnId(1);
    let _peer = known_client(&mut driver, conn);
    driver
        .broken
        .push_back(Broken::Client(conn, ConnError::Reset));
    // Token 0 is listener 0, and this driver has no listeners: the event
    // itself does nothing.
    driver.handle_ready(&EpollEvent::EMPTY).unwrap();
    assert!(driver.broken.is_empty());
    assert!(driver.clients.is_empty());
    assert_eq!(
        logs.try_iter().collect::<Vec<_>>(),
        [LogEntry::Core(Log::Reset { conn, peer: peer() })]
    );
}

/// A broken client leaves the epoll set at once, so a core that does not
/// close it straight away cannot make the loop wake for it again.
#[test]
fn a_broken_client_leaves_the_epoll_set() {
    let (mut driver, _logs) = driver();
    let conn = ConnId(1);
    let mut peer_end = known_client(&mut driver, conn);
    // `Accepted` made the core ask to read; the peer makes it readable.
    std::io::Write::write_all(&mut peer_end, b"x").unwrap();
    let mut events = [EpollEvent::EMPTY; 4];
    assert_eq!(
        driver
            .epoll
            .wait(&mut events, Some(Duration::ZERO))
            .unwrap(),
        1
    );
    driver.break_client(conn, &io::Error::from_raw_os_error(libc::ENOMEM));
    assert_eq!(
        driver
            .epoll
            .wait(&mut events, Some(Duration::ZERO))
            .unwrap(),
        0
    );
}
