//! The `sys` wrappers on the real kernel, unprivileged: epoll, sockets,
//! `openat2`, `fstat`, `sendfile`, eventfd, the user database and
//! `RLIMIT_NOFILE`. (The signalfd test is a unit test in
//! `src/sys/signal.rs`, because it needs a thread-directed signal; `drop_to`
//! is tested in a subprocess by the CLI tests, never in this process.)

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use durchlauferhitzer::sys::epoll::{Epoll, EpollEvent, Readiness, Trouble};
use durchlauferhitzer::sys::eventfd::EventFd;
use durchlauferhitzer::sys::fd::Io;
use durchlauferhitzer::sys::file::{
    Openat2, Stat, file_error, fstat, open_dir_path, openat2_beneath, openat2_supported, readahead,
    sendfile,
};
use durchlauferhitzer::sys::rlimit::{nofile, raise_nofile};
use durchlauferhitzer::sys::socket::{
    ConnectStart, Listening, accept_nonblocking, connect_nonblocking, is_listening_stream,
    local_addr, read, set_nodelay, shutdown_write, take_error, tcp_listen, write,
};
use durchlauferhitzer::sys::user::{euid, lookup_user};
use durchlauferhitzer_core::{FileError, FileKind};

const WAIT: Option<Duration> = Some(Duration::from_secs(5));

/// A directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("dlh-sys-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn loopback_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    (client, server)
}

fn wait_one(epoll: &Epoll) -> EpollEvent {
    let mut events = [EpollEvent::EMPTY; 4];
    let n = epoll.wait(&mut events, WAIT).unwrap();
    assert_eq!(n, 1, "expected exactly one event");
    events[0]
}

// ---- epoll -------------------------------------------------------------

#[test]
fn epoll_reports_readable_writable_both_and_hangup() {
    let (mut client, server) = loopback_pair();
    server.set_nonblocking(true).unwrap();
    let epoll = Epoll::new().unwrap();
    epoll.add(server.as_fd(), 7, Readiness::Read).unwrap();

    // Nothing to read yet: a zero timeout returns no event.
    let mut events = [EpollEvent::EMPTY; 4];
    assert_eq!(epoll.wait(&mut events, Some(Duration::ZERO)).unwrap(), 0);

    client.write_all(b"x").unwrap();
    let ev = wait_one(&epoll);
    assert_eq!(ev.token(), 7);
    assert_eq!(ev.readiness(), Readiness::Read);
    assert_eq!(ev.trouble(), Trouble::None);

    epoll.modify(server.as_fd(), 8, Readiness::Write).unwrap();
    let ev = wait_one(&epoll);
    assert_eq!(ev.token(), 8);
    assert_eq!(ev.readiness(), Readiness::Write);

    epoll
        .modify(server.as_fd(), 9, Readiness::ReadWrite)
        .unwrap();
    let ev = wait_one(&epoll);
    assert_eq!(ev.token(), 9);
    assert_eq!(ev.readiness(), Readiness::ReadWrite);

    let mut buf = [0u8; 16];
    assert!(matches!(read(server.as_fd(), &mut buf), Io::Done(1)));
    assert_eq!(buf[0], b'x');
    assert!(matches!(read(server.as_fd(), &mut buf), Io::WouldBlock));

    // The peer closes: readable (end of stream), then our own shutdown
    // completes the close in both directions and epoll reports a hangup.
    epoll.modify(server.as_fd(), 10, Readiness::Read).unwrap();
    drop(client);
    let ev = wait_one(&epoll);
    assert_eq!(ev.readiness(), Readiness::Read);
    assert!(matches!(read(server.as_fd(), &mut buf), Io::Closed));
    shutdown_write(server.as_fd()).unwrap();
    let ev = wait_one(&epoll);
    assert_eq!(ev.token(), 10);
    assert_eq!(ev.trouble(), Trouble::Hangup);

    epoll.delete(server.as_fd()).unwrap();
    assert_eq!(epoll.wait(&mut events, Some(Duration::ZERO)).unwrap(), 0);
}

#[test]
fn epoll_with_no_interest_still_reports_hangup() {
    let (client, server) = loopback_pair();
    let epoll = Epoll::new().unwrap();
    epoll.add(server.as_fd(), 1, Readiness::None).unwrap();
    let mut events = [EpollEvent::EMPTY; 4];
    assert_eq!(epoll.wait(&mut events, Some(Duration::ZERO)).unwrap(), 0);
    drop(client);
    shutdown_write(server.as_fd()).unwrap();
    let ev = wait_one(&epoll);
    assert_eq!(ev.readiness(), Readiness::None);
    assert_eq!(ev.trouble(), Trouble::Hangup);
}

// ---- sockets -----------------------------------------------------------

#[test]
fn accept_would_block_then_accepts_a_pending_connection() {
    let listener = tcp_listen("127.0.0.1:0".parse().unwrap(), 16).unwrap();
    let addr = local_addr(listener.as_fd()).unwrap();
    assert!(addr.ip().is_loopback());
    assert_ne!(addr.port(), 0);
    assert!(matches!(accept_nonblocking(listener.as_fd()), Ok(None)));

    let client = TcpStream::connect(addr).unwrap();
    let epoll = Epoll::new().unwrap();
    epoll.add(listener.as_fd(), 3, Readiness::Read).unwrap();
    assert_eq!(wait_one(&epoll).readiness(), Readiness::Read);

    let (fd, peer) = accept_nonblocking(listener.as_fd()).unwrap().unwrap();
    assert_eq!(peer, client.local_addr().unwrap());
    assert_eq!(local_addr(fd.as_fd()).unwrap(), addr);
    // The accepted socket is non-blocking.
    let mut buf = [0u8; 4];
    assert!(matches!(read(fd.as_fd(), &mut buf), Io::WouldBlock));
    assert!(matches!(accept_nonblocking(listener.as_fd()), Ok(None)));
}

#[test]
fn listening_stream_is_recognised() {
    let listener = tcp_listen("127.0.0.1:0".parse().unwrap(), 16).unwrap();
    assert_eq!(
        is_listening_stream(listener.as_fd()).unwrap(),
        Listening::Yes
    );
    let (client, _server) = loopback_pair();
    assert_eq!(is_listening_stream(client.as_fd()).unwrap(), Listening::No);
    let file = fs::File::open("/proc/self/status").unwrap();
    assert_eq!(is_listening_stream(file.as_fd()).unwrap(), Listening::No);
}

#[test]
fn listen_on_an_address_in_use_fails() {
    let first = tcp_listen("127.0.0.1:0".parse().unwrap(), 16).unwrap();
    let addr = local_addr(first.as_fd()).unwrap();
    let err = tcp_listen(addr, 16).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AddrInUse);
}

#[test]
fn ipv6_listen_and_local_addr_when_available() {
    match tcp_listen("[::1]:0".parse().unwrap(), 16) {
        Ok(listener) => {
            let addr = local_addr(listener.as_fd()).unwrap();
            assert!(matches!(addr, SocketAddr::V6(_)));
            assert!(addr.ip().is_loopback());
            let client = TcpStream::connect(addr).unwrap();
            let epoll = Epoll::new().unwrap();
            epoll.add(listener.as_fd(), 1, Readiness::Read).unwrap();
            wait_one(&epoll);
            let (_, peer) = accept_nonblocking(listener.as_fd()).unwrap().unwrap();
            assert_eq!(peer, client.local_addr().unwrap());
        }
        Err(err) => eprintln!("skipped: no IPv6 loopback here ({err})"),
    }
}

#[test]
fn ipv6_and_ipv4_wildcards_bind_the_same_port_when_available() {
    // An IPv6 listener is IPv6-only (FR-CFG-10), so `[::]:p` leaves
    // `0.0.0.0:p` free whatever the host's `bindv6only` default.
    let v6 = match tcp_listen("[::]:0".parse().unwrap(), 16) {
        Ok(v6) => v6,
        Err(err) => {
            eprintln!("skipped: no IPv6 here ({err})");
            return;
        }
    };
    let port = local_addr(v6.as_fd()).unwrap().port();
    let v4 = tcp_listen(format!("0.0.0.0:{port}").parse().unwrap(), 16).unwrap();
    assert_eq!(local_addr(v4.as_fd()).unwrap().port(), port);
}

/// Waits until a connecting socket is writable, then reads `SO_ERROR`.
fn finish_connect(fd: &OwnedFd) -> Option<std::io::Error> {
    let epoll = Epoll::new().unwrap();
    epoll.add(fd.as_fd(), 1, Readiness::Write).unwrap();
    let ev = wait_one(&epoll);
    assert_eq!(ev.token(), 1);
    take_error(fd.as_fd()).unwrap()
}

#[test]
fn connect_to_a_closed_port_fails_via_so_error() {
    // A port held by a socket that is bound but not listening (the client
    // end of a connection) refuses connects, and no other test can take it
    // while it is held.
    let (held, _server) = loopback_pair();
    let addr = held.local_addr().unwrap();
    let connecting = connect_nonblocking(addr).unwrap();
    match connecting.immediate {
        Ok(ConnectStart::InProgress) => {
            let err = finish_connect(&connecting.fd).expect("a refused connect");
            assert_eq!(err.kind(), ErrorKind::ConnectionRefused);
        }
        Ok(ConnectStart::Connected) => panic!("connected to a closed port"),
        Err(err) => assert_eq!(err.kind(), ErrorKind::ConnectionRefused),
    }
}

#[test]
fn connect_to_an_open_listener_completes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let connecting = connect_nonblocking(addr).unwrap();
    match connecting.immediate {
        Ok(ConnectStart::InProgress) => assert!(finish_connect(&connecting.fd).is_none()),
        Ok(ConnectStart::Connected) => {}
        Err(err) => panic!("connect failed: {err}"),
    }
    assert!(take_error(connecting.fd.as_fd()).unwrap().is_none());
    set_nodelay(connecting.fd.as_fd()).unwrap();

    let (mut server, _) = listener.accept().unwrap();
    assert!(matches!(write(connecting.fd.as_fd(), b"ping"), Io::Done(4)));
    let mut got = [0u8; 4];
    server.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"ping");

    let stream = TcpStream::from(connecting.fd);
    assert!(stream.nodelay().unwrap());
}

#[test]
fn write_to_a_closed_peer_is_an_error() {
    let (client, server) = loopback_pair();
    server.set_nonblocking(true).unwrap();
    let fd: OwnedFd = server.into();
    drop(client);
    // The first writes may still be accepted; the peer's reset follows.
    let mut outcome = None;
    for _ in 0..1000 {
        match write(fd.as_fd(), &[0u8; 1024]) {
            Io::Done(_) | Io::WouldBlock => thread::sleep(Duration::from_millis(1)),
            Io::Err(err) => {
                outcome = Some(err);
                break;
            }
            Io::Closed => panic!("a write never reports Closed"),
        }
    }
    let err = outcome.expect("a write error after the peer closed");
    assert!(
        matches!(
            err.kind(),
            ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
        ),
        "{err}"
    );
}

#[test]
fn empty_buffers_do_not_touch_the_socket() {
    let (_client, server) = loopback_pair();
    assert!(matches!(read(server.as_fd(), &mut []), Io::Done(0)));
    assert!(matches!(write(server.as_fd(), &[]), Io::Done(0)));
}

// ---- files -------------------------------------------------------------

/// root/ with file.txt, sub/inner.txt, link -> file.txt, fifo; and a file
/// outside the root.
fn file_tree() -> (TempDir, OwnedFd) {
    let tmp = TempDir::new();
    let root = tmp.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("file.txt"), b"hello, world").unwrap();
    fs::create_dir(root.join("sub")).unwrap();
    fs::write(root.join("sub/inner.txt"), b"inner").unwrap();
    symlink("file.txt", root.join("link")).unwrap();
    fs::write(tmp.path().join("outside.txt"), b"secret").unwrap();
    let status = Command::new("mkfifo")
        .arg(root.join("fifo"))
        .status()
        .unwrap();
    assert!(status.success());
    let fd = open_dir_path(&root).unwrap();
    (tmp, fd)
}

fn open(root: &OwnedFd, segments: &[&[u8]]) -> std::io::Result<OwnedFd> {
    openat2_beneath(root.as_fd(), segments)
}

#[test]
fn openat2_is_supported_here() {
    let (_tmp, root) = file_tree();
    assert_eq!(openat2_supported(root.as_fd()).unwrap(), Openat2::Supported);
}

#[test]
fn openat2_opens_files_beneath_the_root_and_the_root_itself() {
    let (_tmp, root) = file_tree();

    let fd = open(&root, &[b"file.txt"]).unwrap();
    let stat: Stat = fstat(fd.as_fd()).unwrap();
    assert_eq!(stat.kind, FileKind::Regular);
    assert_eq!(stat.size, 12);
    assert!(stat.mtime_ns > 0);

    let fd = open(&root, &[b"sub", b"inner.txt"]).unwrap();
    assert_eq!(fstat(fd.as_fd()).unwrap().size, 5);

    let fd = open(&root, &[]).unwrap();
    assert_eq!(fstat(fd.as_fd()).unwrap().kind, FileKind::Directory);
    let fd = open(&root, &[b"sub"]).unwrap();
    assert_eq!(fstat(fd.as_fd()).unwrap().kind, FileKind::Directory);
}

#[test]
fn openat2_refuses_dotdot_symlinks_and_absolute_paths() {
    let (_tmp, root) = file_tree();
    let cases: [&[&[u8]]; 4] = [
        &[b"..", b"outside.txt"],
        &[b"sub", b"..", b"..", b"outside.txt"],
        &[b"link"],
        &[b"/etc/passwd"],
    ];
    for segments in cases {
        let err = open(&root, segments).unwrap_err();
        assert_eq!(file_error(&err), FileError::Symlink, "{segments:?}: {err}");
    }
    let err = open(&root, &[b"missing"]).unwrap_err();
    assert_eq!(file_error(&err), FileError::NotFound);
    let err = open(&root, &[b"file.txt", b"x"]).unwrap_err();
    assert_eq!(file_error(&err), FileError::NotFound);
    let err = open(&root, &[b"nul\0byte"]).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn a_fifo_opens_at_once_and_is_not_a_regular_file() {
    let (_tmp, root) = file_tree();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = open(&root, &[b"fifo"]).map(|fd| fstat(fd.as_fd()).map(|s| s.kind));
        let _ = tx.send(result);
    });
    let kind = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("opening a FIFO must not block")
        .unwrap()
        .unwrap();
    assert_eq!(kind, FileKind::Other);
}

#[test]
fn errno_maps_to_file_error_per_the_contract_table() {
    let table = [
        (libc::ENOENT, FileError::NotFound),
        (libc::ENOTDIR, FileError::NotFound),
        (libc::ELOOP, FileError::Symlink),
        (libc::EXDEV, FileError::Symlink),
        (libc::EACCES, FileError::PermissionDenied),
        (libc::EPERM, FileError::PermissionDenied),
        (libc::EIO, FileError::Other),
        (libc::ENAMETOOLONG, FileError::Other),
        (libc::EMFILE, FileError::Other),
        (libc::ENFILE, FileError::Other),
    ];
    for (errno, expected) in table {
        let err = std::io::Error::from_raw_os_error(errno);
        assert_eq!(file_error(&err), expected, "errno {errno}");
    }
    assert_eq!(
        file_error(&std::io::Error::other("no errno")),
        FileError::Other
    );
}

#[test]
fn sendfile_delivers_the_exact_bytes_then_reports_end_of_file() {
    let tmp = TempDir::new();
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    fs::write(tmp.path().join("data"), &data).unwrap();
    let root = open_dir_path(tmp.path()).unwrap();
    let file = openat2_beneath(root.as_fd(), &[b"data"]).unwrap();
    readahead(file.as_fd(), 0, data.len() as u64).unwrap();

    let (client, mut server) = loopback_pair();
    let (offset, len) = (1000u64, 250_000u64);
    let reader = thread::spawn(move || {
        let mut got = vec![0u8; len as usize];
        server.read_exact(&mut got).unwrap();
        got
    });

    // The client socket is blocking, so sendfile blocks rather than
    // returning WouldBlock; loop over partial sends.
    let mut sent = 0u64;
    while sent < len {
        match sendfile(client.as_fd(), file.as_fd(), offset + sent, len - sent) {
            Io::Done(n) => sent += n as u64,
            Io::WouldBlock => thread::sleep(Duration::from_millis(1)),
            Io::Closed => panic!("unexpected end of file"),
            Io::Err(err) => panic!("sendfile: {err}"),
        }
    }
    let got = reader.join().unwrap();
    assert_eq!(got, &data[offset as usize..(offset + len) as usize]);

    // At the end of the file sendfile returns 0: reported as Closed.
    assert!(matches!(
        sendfile(client.as_fd(), file.as_fd(), data.len() as u64, 10),
        Io::Closed
    ));
    client.shutdown(Shutdown::Both).unwrap();
}

// ---- eventfd -----------------------------------------------------------

#[test]
fn eventfd_notify_wakes_epoll_and_drain_resets() {
    let wake = EventFd::new().unwrap();
    assert_eq!(wake.drain().unwrap(), 0);
    let epoll = Epoll::new().unwrap();
    epoll.add(wake.as_fd(), 42, Readiness::Read).unwrap();

    wake.notify().unwrap();
    wake.notify().unwrap();
    let ev = wait_one(&epoll);
    assert_eq!(ev.token(), 42);
    assert_eq!(ev.readiness(), Readiness::Read);
    assert_eq!(wake.drain().unwrap(), 2);
    assert_eq!(wake.drain().unwrap(), 0);
    let mut events = [EpollEvent::EMPTY; 4];
    assert_eq!(epoll.wait(&mut events, Some(Duration::ZERO)).unwrap(), 0);
}

#[test]
fn eventfd_notify_from_another_thread() {
    let wake = std::sync::Arc::new(EventFd::new().unwrap());
    let epoll = Epoll::new().unwrap();
    epoll.add(wake.as_fd(), 1, Readiness::Read).unwrap();
    let other = std::sync::Arc::clone(&wake);
    thread::spawn(move || other.notify().unwrap())
        .join()
        .unwrap();
    assert_eq!(wait_one(&epoll).token(), 1);
    assert_eq!(wake.drain().unwrap(), 1);
}

// ---- user and limits ---------------------------------------------------

#[test]
fn lookup_user_finds_root_and_not_a_missing_name() {
    assert_eq!(lookup_user("root").unwrap(), Some((0, 0)));
    assert_eq!(lookup_user("no-such-user-dlh-test").unwrap(), None);
    assert_eq!(
        lookup_user("nul\0name").unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
}

#[test]
fn euid_matches_proc_self_status() {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|l| l.starts_with("Uid:")).unwrap();
    let effective: u32 = line.split_whitespace().nth(2).unwrap().parse().unwrap();
    assert_eq!(euid(), effective);
}

#[test]
fn nofile_reads_soft_and_hard_limits() {
    let (soft, hard) = nofile().unwrap();
    assert!(soft > 0);
    assert!(soft <= hard);
    // An fd number in use is below the soft limit.
    let file = fs::File::open("/proc/self/status").unwrap();
    assert!((file.as_raw_fd() as u64) < soft);
}

#[test]
fn raise_nofile_lifts_the_soft_limit_to_the_hard_limit() {
    let (soft, hard) = nofile().unwrap();
    // Asking for less than the current soft limit never lowers it.
    raise_nofile(soft.saturating_sub(1).max(1)).unwrap();
    assert_eq!(nofile().unwrap(), (soft, hard));
    // Raising the soft limit up to the hard limit needs no privilege.
    raise_nofile(hard).unwrap();
    assert_eq!(nofile().unwrap(), (hard, hard));
}
