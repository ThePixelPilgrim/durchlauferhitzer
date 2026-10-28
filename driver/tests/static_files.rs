//! The file contract on the real kernel (stage 4 Task 6, DRV-07…09): the
//! event loop with static roots in a temp dir, the disk worker pool and
//! `sendfile`, on loopback. The descriptor count after a client reset
//! mid-`sendfile` is in `tests/static_fd_leak.rs`, alone in its binary
//! (R4).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

use std::fs::OpenOptions;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

use durchlauferhitzer::{Exit, LogEntry};
use durchlauferhitzer_core::{FileError, Log};

#[path = "support/server.rs"]
mod server;
#[path = "support/tree.rs"]
mod tree;

use server::{Server, content_length, read_head, read_response, read_to_eof, text};
use tree::{Tree, pattern, static_config};

const MIB: usize = 1024 * 1024;

fn get(client: &mut TcpStream, method: &str, path: &str) {
    write!(
        client,
        "{method} {path} HTTP/1.1\r\nHost: example.test\r\n\r\n"
    )
    .unwrap();
}

/// The status line of a response.
fn status_line(response: &[u8]) -> String {
    text(response).split("\r\n").next().unwrap().to_owned()
}

/// The body of a response read by `read_response`.
fn body(response: &[u8]) -> &[u8] {
    let at = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    &response[at + 4..]
}

/// The core's log entries, without anything the driver logged itself.
fn core_logs(entries: Vec<LogEntry>) -> Vec<Log> {
    entries
        .into_iter()
        .filter_map(|entry| match entry {
            LogEntry::Core(log) => Some(log),
            LogEntry::Driver(_) => None,
        })
        .collect()
}

#[test]
fn a_10_mib_file_reaches_a_slow_reader_byte_exact() {
    let tree = Tree::new();
    let content = pattern(1, 10 * MIB);
    tree.file("big.bin", &content);
    let server = Server::start_static(&static_config(tree.path()), 2);
    let mut client = server.connect();
    get(&mut client, "GET", "/big.bin");
    // Let the socket fill, so `sendfile` meets `EAGAIN` and the loop must
    // wait for writability, then read in small pieces with a pause every
    // 64 KiB, so the loop waits for writability again many times.
    thread::sleep(Duration::from_millis(200));
    let head = read_head(&mut client);
    assert_eq!(status_line(&head), "HTTP/1.1 200 OK", "{}", text(&head));
    assert_eq!(content_length(&head), Some(content.len()));
    let mut received = Vec::with_capacity(content.len());
    let mut piece = vec![0u8; 16 * 1024];
    let mut reads = 0u32;
    while received.len() < content.len() {
        let want = piece.len().min(content.len() - received.len());
        let n = client.read(&mut piece[..want]).unwrap();
        assert_ne!(n, 0, "cut after {} bytes", received.len());
        received.extend_from_slice(&piece[..n]);
        reads += 1;
        if reads % 4 == 0 {
            thread::sleep(Duration::from_millis(1));
        }
    }
    assert!(received == content, "the body differs from the file");
    // Keep-alive: the connection serves the next request.
    get(&mut client, "HEAD", "/big.bin");
    let head = read_head(&mut client);
    assert_eq!(status_line(&head), "HTTP/1.1 200 OK");
    let (exit, logs) = server.stop();
    assert_eq!(exit, Exit::Stopped);
    let statuses: Vec<_> = core_logs(logs)
        .into_iter()
        .map(|log| match log {
            Log::Response { status, .. } => status.code(),
            other @ (Log::Rejected { .. }
            | Log::Reset { .. }
            | Log::FileAborted { .. }
            | Log::ProxiedResponse { .. }
            | Log::BackendFailed { .. }) => panic!("unexpected log {other:?}"),
        })
        .collect();
    assert_eq!(statuses, [200, 200]);
}

#[test]
fn a_directory_redirects_without_its_slash_and_serves_its_index_with_it() {
    let tree = Tree::new();
    tree.file("docs/index.html", b"<p>index</p>");
    let server = Server::start_static(&static_config(tree.path()), 2);
    let mut client = server.connect();

    get(&mut client, "GET", "/docs?x=1");
    let response = read_response(&mut client);
    assert_eq!(status_line(&response), "HTTP/1.1 301 Moved Permanently");
    let lower = text(&response).to_ascii_lowercase();
    assert!(lower.contains("\r\nlocation: /docs/?x=1\r\n"), "{lower}");

    get(&mut client, "GET", "/docs/");
    let response = read_response(&mut client);
    assert_eq!(status_line(&response), "HTTP/1.1 200 OK");
    assert_eq!(body(&response), b"<p>index</p>");
    server.stop();
}

#[test]
fn a_symlink_a_dotfile_a_fifo_and_a_missing_file_are_404() {
    let tree = Tree::new();
    tree.file("real.txt", b"real");
    tree.file(".secret", b"hidden");
    tree.symlink("link.txt", "real.txt");
    tree.dir("sub");
    tree.symlink("sub/up", "..");
    tree.fifo("pipe");
    let server = Server::start_static(&static_config(tree.path()), 2);
    let mut client = server.connect();
    for path in [
        "/link.txt",
        "/sub/up/real.txt",
        "/.secret",
        "/pipe",
        "/missing",
    ] {
        get(&mut client, "GET", path);
        let response = read_response(&mut client);
        assert_eq!(
            status_line(&response),
            "HTTP/1.1 404 Not Found",
            "{path}: {}",
            text(&response)
        );
    }
    get(&mut client, "GET", "/real.txt");
    assert_eq!(body(&read_response(&mut client)), b"real");
    server.stop();
}

/// Review Focus 2: with a single worker, a FIFO request must not hold it
/// (`O_NONBLOCK`): a request on another connection right behind it is
/// still answered within 1 s.
#[test]
fn a_fifo_does_not_hang_the_only_worker() {
    let tree = Tree::new();
    tree.file("small.txt", b"small");
    tree.fifo("pipe");
    let server = Server::start_static(&static_config(tree.path()), 1);
    let mut first = server.connect();
    let mut second = server.connect();
    second
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    // The FIFO first, with time for the only worker to take its open;
    // only then the second request, which that worker must also serve.
    get(&mut first, "GET", "/pipe");
    thread::sleep(Duration::from_millis(50));
    let start = Instant::now();
    get(&mut second, "GET", "/small.txt");
    let response = read_response(&mut second);
    let waited = start.elapsed();
    assert_eq!(body(&response), b"small");
    assert!(waited < Duration::from_secs(1), "answered after {waited:?}");
    let response = read_response(&mut first);
    assert_eq!(status_line(&response), "HTTP/1.1 404 Not Found");
    server.stop();
}

/// FR-STATIC-13: HEAD gets the head with the full `Content-Length` and no
/// body: the next response on the connection follows the head at once.
#[test]
fn head_gets_the_full_content_length_and_no_body() {
    let tree = Tree::new();
    tree.file("big.bin", &pattern(2, 3 * MIB));
    tree.file("small.txt", b"small");
    let server = Server::start_static(&static_config(tree.path()), 2);
    let mut client = server.connect();
    get(&mut client, "HEAD", "/big.bin");
    let head = read_head(&mut client);
    assert_eq!(status_line(&head), "HTTP/1.1 200 OK");
    assert_eq!(content_length(&head), Some(3 * MIB));
    get(&mut client, "GET", "/small.txt");
    let response = read_response(&mut client);
    assert_eq!(
        status_line(&response),
        "HTTP/1.1 200 OK",
        "{}",
        text(&response)
    );
    assert_eq!(body(&response), b"small");
    server.stop();
}

/// FR-STATIC-15, R7: a file that shrinks during the transfer cuts the
/// connection; the client sees fewer bytes than `Content-Length`, all of
/// them the file's own, never a complete-looking short body.
#[test]
fn a_file_truncated_mid_transfer_cuts_the_connection() {
    let tree = Tree::new();
    let content = pattern(3, 32 * MIB);
    let path = tree.file("big.bin", &content);
    let server = Server::start_static(&static_config(tree.path()), 2);
    let mut client = server.connect();
    get(&mut client, "GET", "/big.bin");
    let head = read_head(&mut client);
    assert_eq!(content_length(&head), Some(content.len()));
    let mut part = vec![0u8; MIB];
    client.read_exact(&mut part).unwrap();
    // Stop reading until the server's `sendfile` stalls on the full
    // socket, then shrink the file below what was sent already.
    thread::sleep(Duration::from_millis(100));
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(2 * MIB as u64)
        .unwrap();
    match read_to_eof(&mut client) {
        Ok(rest) => part.extend_from_slice(&rest),
        // A reset is a cut too; a read timeout is not.
        Err(err) if err.kind() == ErrorKind::ConnectionReset => {}
        Err(err) => panic!("the connection was not cut: {err}"),
    }
    assert!(
        part.len() < content.len(),
        "{} of {} bytes: a complete-looking body",
        part.len(),
        content.len()
    );
    assert!(
        part == content[..part.len()],
        "bytes that are not the file's"
    );
    let (_, logs) = server.stop();
    let logs = core_logs(logs);
    assert_eq!(logs.len(), 2, "{logs:?}");
    assert!(matches!(logs[0], Log::Response { .. }), "{:?}", logs[0]);
    assert!(
        matches!(
            logs[1],
            Log::FileAborted {
                error: FileError::Truncated | FileError::Changed,
                ..
            }
        ),
        "{:?}",
        logs[1]
    );
}

/// 64 downloads at once (the per-IP default allows 64) through two disk
/// workers: every body is byte-exact, each file spanning several chunks.
#[test]
fn sixty_four_concurrent_downloads_through_two_workers_are_byte_exact() {
    let tree = Tree::new();
    let contents: Vec<Vec<u8>> = (0..64u64)
        .map(|i| pattern(100 + i, 600 * 1024 + 4099 * usize::try_from(i).unwrap()))
        .collect();
    for (i, content) in contents.iter().enumerate() {
        tree.file(&format!("f{i}.bin"), content);
    }
    let server = Server::start_static(&static_config(tree.path()), 2);
    let clients: Vec<_> = (0..64)
        .map(|i| {
            let mut client = server.connect();
            get(&mut client, "GET", &format!("/f{i}.bin"));
            client
        })
        .collect();
    let readers: Vec<_> = clients
        .into_iter()
        .enumerate()
        .map(|(i, mut client)| thread::spawn(move || (i, read_response(&mut client))))
        .collect();
    for reader in readers {
        let (i, response) = reader.join().unwrap();
        assert_eq!(status_line(&response), "HTTP/1.1 200 OK", "f{i}");
        assert!(body(&response) == contents[i], "f{i}.bin differs");
    }
    server.stop();
}

/// M5: an empty file is a 200 with `Content-Length: 0` and no body (no
/// readahead, no `sendfile`): the next response follows at once.
#[test]
fn an_empty_file_is_200_with_no_body() {
    let tree = Tree::new();
    tree.file("empty", b"");
    tree.file("small.txt", b"small");
    let server = Server::start_static(&static_config(tree.path()), 1);
    let mut client = server.connect();
    get(&mut client, "GET", "/empty");
    let head = read_head(&mut client);
    assert_eq!(status_line(&head), "HTTP/1.1 200 OK");
    assert_eq!(content_length(&head), Some(0));
    get(&mut client, "GET", "/small.txt");
    let response = read_response(&mut client);
    assert_eq!(
        status_line(&response),
        "HTTP/1.1 200 OK",
        "{}",
        text(&response)
    );
    assert_eq!(body(&response), b"small");
    server.stop();
}

/// M5: a sparse file over 4 GiB: HEAD reports its full 64-bit size, and a
/// GET starts sending it (the first MiB arrives; the rest is not read:
/// the core has no ranges, and offsets past 4 GiB are covered by the
/// `sendfile` test at 5 GiB in `tests/sys.rs`).
#[test]
fn a_file_over_4_gib_reports_its_size_and_starts_sending() {
    // Assumes the temp filesystem supports sparse files (ext4, tmpfs).
    let tree = Tree::new();
    let path = tree.file("huge.bin", b"start");
    let size: u64 = 5 * 1024 * 1024 * 1024 + 7;
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(size)
        .unwrap();
    let server = Server::start_static(&static_config(tree.path()), 1);
    let mut client = server.connect();
    get(&mut client, "HEAD", "/huge.bin");
    let head = read_head(&mut client);
    assert_eq!(status_line(&head), "HTTP/1.1 200 OK");
    assert_eq!(content_length(&head), Some(usize::try_from(size).unwrap()));
    get(&mut client, "GET", "/huge.bin");
    let head = read_head(&mut client);
    assert_eq!(content_length(&head), Some(usize::try_from(size).unwrap()));
    let mut first = vec![0u8; MIB];
    client.read_exact(&mut first).unwrap();
    assert_eq!(&first[..5], b"start");
    assert!(first[5..].iter().all(|&b| b == 0));
    drop(client);
    server.stop();
}
