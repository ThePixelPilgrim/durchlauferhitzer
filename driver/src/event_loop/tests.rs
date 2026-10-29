//! Unit tests of the loop's own decisions, on a `Driver` that is built but
//! never run.

use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use durchlauferhitzer_core::config::load;
use durchlauferhitzer_core::{
    BackendFailure, BackendId, ConnError, ConnId, Core, Event, FileError, FileId, Interest, Log,
    UpstreamId,
};

use super::accept::Accepting;
use super::upstreams::{WriteMode, write_mode};
use super::{Broken, Driver, FileFailure};
use crate::conns::{Answer, Client, Eof, Phase, Token, Upstream, Writes};
use crate::log::{Buffered, DriverLog, LogEntry, LogSink};
use crate::pool::JobResult;
use crate::sys::epoll::{EpollEvent, Readiness};

/// A config without static roots, so a driver needs no pool.
const STATIC_LESS: &str = "listen 127.0.0.1:8080; host a.test { path / { proxy 127.0.0.1:9; \
    max_request_body 1MiB; max_response_body 1MiB; } }";

/// A config with a static root: the core may emit `OpenFile`.
const STATIC: &str = "listen 127.0.0.1:8080; host a.test { path / { static /srv; } }";

/// DRV-09 (R2, M1): the pool exists exactly when the core has static
/// roots, so "roots without a pool" cannot be built and a static-less
/// core costs no idle threads and no extra eventfd.
#[test]
fn the_pool_starts_exactly_when_the_core_has_static_roots() {
    let (without, _logs) = driver();
    assert!(without.pool.is_none());
    let (with, _logs) = static_driver();
    assert!(with.pool.is_some());
}

/// Without static roots the thread count is not used, not even 0.
#[test]
fn a_static_less_core_ignores_the_thread_count() {
    let core = Core::new(load(STATIC_LESS).unwrap());
    let (log, _logs) = LogSink::channel();
    let driver = Driver::new(core, Vec::new(), Vec::new(), 0, log).unwrap();
    assert!(driver.pool.is_none());
}

/// DRV-07: the driver gets one opened root per static root of the core,
/// in order; a list that does not match is refused, not guessed at.
#[test]
fn a_root_list_that_does_not_match_the_core_is_refused() {
    let core = Core::new(load(STATIC).unwrap());
    let (log, _logs) = LogSink::channel();
    let Err(err) = Driver::new(core, Vec::new(), Vec::new(), 1, log) else {
        panic!("a driver without its root was built");
    };
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    let core = Core::new(load(STATIC_LESS).unwrap());
    let (log, _logs) = LogSink::channel();
    let Err(err) = Driver::new(core, Vec::new(), vec![unpollable()], 1, log) else {
        panic!("a driver with a root the core does not have was built");
    };
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn zero_disk_threads_are_refused() {
    let core = Core::new(load(STATIC).unwrap());
    let (log, _logs) = LogSink::channel();
    let Err(err) = Driver::new(core, Vec::new(), vec![unpollable()], 0, log) else {
        panic!("a driver with no disk thread was built");
    };
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

/// A driver for [`STATIC`] (its root stood in for by `/dev/null`: no
/// open reaches it here) with one disk worker, and its log.
fn static_driver() -> (Driver, Receiver<LogEntry>) {
    let core = Core::new(load(STATIC).unwrap());
    let (log, logs) = LogSink::channel();
    let driver = Driver::new(core, Vec::new(), vec![unpollable()], 1, log).unwrap();
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

/// M2: the core asks for one readahead at a time; a second one while the
/// first is in flight is not dropped in silence (the connection would wait
/// for an answer that never comes) but answered `FileFailed { Other }`
/// after the current call, through the queue of failed file requests.
/// I1 (round 2): a `CloseFile` performed before that answer is delivered
/// (later in the same call, or in an earlier queued event's call) wins:
/// after `CloseFile` nothing is delivered for the file.
#[test]
fn a_second_readahead_in_flight_is_answered_failed() {
    let (mut driver, _logs) = static_driver();
    let file = FileId(1);
    driver.files.opening(file);
    let null = OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
    driver.files.opened(file, null);
    driver.readahead(file, 0, 5);
    assert!(driver.file_failures.is_empty());
    driver.readahead(file, 5, 5);
    assert_eq!(
        driver.file_failures.iter().copied().collect::<Vec<_>>(),
        [FileFailure::Read(file)]
    );
    let failed = Event::FileFailed {
        file,
        error: FileError::Other,
    };
    assert_eq!(driver.failure_event(FileFailure::Read(file)), Some(failed));
    driver.close_file(file).unwrap();
    assert_eq!(driver.failure_event(FileFailure::Read(file)), None);
    // An open's answer is due whatever happened since: the core never
    // closes a file whose open is unanswered.
    let other = FileId(2);
    assert_eq!(
        driver.failure_event(FileFailure::Open(other)),
        Some(Event::FileFailed {
            file: other,
            error: FileError::Other
        })
    );
}

/// m1 (round 2): without a pool (unreachable past `new`) a `Readahead`
/// marks nothing in flight, so a later `CloseFile` closes the file at once
/// instead of parking it in `Closing` forever.
#[test]
fn a_readahead_without_a_pool_leaves_the_file_idle() {
    let (mut driver, _logs) = driver();
    let file = FileId(1);
    driver.files.opening(file);
    let null = OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
    driver.files.opened(file, null);
    driver.readahead(file, 0, 5);
    assert_eq!(
        driver.file_failures.iter().copied().collect::<Vec<_>>(),
        [FileFailure::Read(file)]
    );
    assert_eq!(
        driver.files.close(file, |_| crate::pool::Cancel::NotQueued),
        super::Freed::Descriptor
    );
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
    let driver = Driver::new(core, Vec::new(), Vec::new(), 4, log).unwrap();
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

/// R4 (FR-LOG-06): the loop waits for the log descriptor to become
/// writable exactly while the sink holds bytes, so a backed-up log is
/// written as soon as the reader catches up, and an idle loop never wakes
/// for an always-writable descriptor (no spin).
#[test]
fn the_log_descriptor_is_waited_for_only_while_bytes_are_buffered() {
    let (reader, writer) = crate::sys::fd::pipe().unwrap();
    let flags = crate::sys::fd::status_flags(reader.as_fd()).unwrap();
    crate::sys::fd::set_status_flags(
        reader.as_fd(),
        flags.with(crate::sys::fd::Blocking::NonBlocking),
    )
    .unwrap();
    let mut log = LogSink::fd(writer).unwrap();
    let mut conn = 0;
    while log.buffered() == Buffered::Nothing {
        conn += 1;
        log.write(LogEntry::Core(Log::Reset {
            conn: ConnId(conn),
            peer: peer(),
        }));
    }
    let core = Core::new(load(STATIC_LESS).unwrap());
    let mut driver = Driver::new(core, Vec::new(), Vec::new(), 0, log).unwrap();
    let mut events = [EpollEvent::EMPTY; 4];

    // Bytes wait, the pipe is full: registered, not ready.
    driver.sync_log();
    assert_eq!(
        driver
            .epoll
            .wait(&mut events, Some(Duration::ZERO))
            .unwrap(),
        0
    );

    // The reader catches up: the loop is woken for the log and writes it,
    // until nothing is left.
    let mut buf = vec![0u8; 1 << 16];
    let drain = |buf: &mut [u8]| {
        while let crate::sys::fd::Io::Done(_) = crate::sys::socket::read(reader.as_fd(), buf) {}
    };
    while driver.log.buffered() == Buffered::Bytes {
        drain(&mut buf);
        let n = driver
            .epoll
            .wait(&mut events, Some(Duration::ZERO))
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(Token::decode(events[0].token()), Some(Token::Log));
        driver.handle_ready(&events[0]).unwrap();
        driver.sync_log();
    }
    drain(&mut buf);

    // Nothing buffered: the always-writable pipe is not waited for.
    assert_eq!(
        driver
            .epoll
            .wait(&mut events, Some(Duration::ZERO))
            .unwrap(),
        0
    );
}

/// FR-LOG-05 (owner decision O1): the driver hands the sink the core's
/// backends, so a proxy line names the backend by its address.
#[test]
fn proxy_lines_name_the_backend_by_its_address() {
    let (reader, writer) = crate::sys::fd::pipe().unwrap();
    let log = LogSink::fd(writer).unwrap();
    let core = Core::new(load(STATIC_LESS).unwrap());
    let mut driver = Driver::new(core, Vec::new(), Vec::new(), 0, log).unwrap();
    driver.log.write(LogEntry::Core(Log::BackendFailed {
        conn: ConnId(1),
        peer: peer(),
        backend: BackendId(0),
        kind: BackendFailure::Refused,
    }));
    let mut buf = [0u8; 256];
    let crate::sys::fd::Io::Done(n) = crate::sys::socket::read(reader.as_fd(), &mut buf) else {
        panic!("no line");
    };
    assert_eq!(
        std::str::from_utf8(&buf[..n]).unwrap(),
        format!(
            "kind=backend_failed conn=1 peer={} backend=127.0.0.1:9 failure=refused\n",
            peer()
        )
    );
}

/// NFR-PERF-04, DRV-11: every client socket the loop accepts and every
/// upstream socket it connects gets `TCP_NODELAY` (off by default), and
/// keeps the kernel's `SO_SNDBUF`/`SO_RCVBUF` (owner decision 2026-10-29,
/// O1: no buffer size is ever set).
#[test]
fn clients_and_upstreams_get_tcp_nodelay() {
    use std::net::{TcpListener, TcpStream};

    use crate::sys::socket::buffer_sizes;

    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = format!(
        "listen 127.0.0.1:8080; host a.test {{ path / {{ proxy {}; \
         max_request_body 1MiB; max_response_body 1MiB; }} }}",
        backend.local_addr().unwrap()
    );
    let core = Core::new(load(&config).unwrap());
    let (log, _logs) = LogSink::channel();
    let mut driver = Driver::new(core, Vec::new(), Vec::new(), 1, log).unwrap();
    let nodelay = |fd: &OwnedFd| TcpStream::from(fd.try_clone().unwrap()).nodelay().unwrap();
    let sizes = |fd: &OwnedFd| buffer_sizes(fd.as_fd()).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let _peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (accepted, _) = listener.accept().unwrap();
    assert!(!accepted.nodelay().unwrap(), "the kernel's default");
    // The kernel's defaults for an established socket (it grows the send
    // buffer once a connection is set up, so an unconnected socket's
    // sizes differ): the same socket before the driver has it.
    let accepted = OwnedFd::from(accepted);
    let kernel = sizes(&accepted);
    driver.accepted(accepted, peer()).unwrap();
    let client = &driver.clients[&ConnId(1)].fd;
    assert!(nodelay(client));
    // NFR-PERF-04: no buffer size is set, so autotuning stays on.
    assert_eq!(sizes(client), kernel, "an accepted socket's buffer sizes");

    let up = UpstreamId(1);
    driver.connect(up, BackendId(0)).unwrap();
    let fd = match &driver.upstreams[&up].phase {
        Phase::Connecting(fd) | Phase::Answering(Answer::Connected(fd)) => fd,
        Phase::Answering(Answer::Failed) | Phase::Open { .. } | Phase::Failed => {
            panic!("the connect to a listening backend did not start")
        }
    };
    assert!(nodelay(fd));
    // A loopback connect is set up at once; compare with a plain socket
    // connected to the same backend, once both are connected.
    let upstream = TcpStream::from(fd.try_clone().unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while upstream.peer_addr().is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "the upstream never connected"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let plain = OwnedFd::from(TcpStream::connect(backend.local_addr().unwrap()).unwrap());
    assert_eq!(
        sizes(fd),
        sizes(&plain),
        "an upstream socket's buffer sizes"
    );
}

/// A3 (review A, Minor 3): one readiness of a listener accepts at most
/// [`ACCEPT_BATCH`] (64) connections, so a flood of connects cannot hold
/// the loop from its other descriptors and timers; the rest wait in the
/// backlog, and the level-triggered listener is reported again.
///
/// [`ACCEPT_BATCH`]: super::clients::ACCEPT_BATCH
#[test]
fn one_readiness_accepts_at_most_64_connections() {
    use crate::sys::socket::{local_addr, tcp_listen};
    let listener = tcp_listen("127.0.0.1:0".parse().unwrap(), 1024).unwrap();
    let addr = local_addr(listener.as_fd()).unwrap();
    let config = "listen 127.0.0.1:8080; limits { max_connections 1000; \
        max_connections_per_ip 1000; } host a.test { path / { proxy 127.0.0.1:9; \
        max_request_body 1MiB; max_response_body 1MiB; } }";
    let core = Core::new(load(config).unwrap());
    let (log, _logs) = LogSink::channel();
    let mut driver = Driver::new(core, vec![listener], Vec::new(), 4, log).unwrap();
    // `connect` returns once the handshake is done: all 100 are queued.
    let _streams: Vec<std::net::TcpStream> = (0..100)
        .map(|_| std::net::TcpStream::connect(addr).unwrap())
        .collect();
    assert_eq!(super::clients::ACCEPT_BATCH, 64);
    driver.accept_all(0).unwrap();
    assert_eq!(driver.clients.len(), 64);
    driver.accept_all(0).unwrap();
    assert_eq!(driver.clients.len(), 100);
}
