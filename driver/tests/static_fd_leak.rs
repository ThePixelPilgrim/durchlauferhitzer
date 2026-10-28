//! Review Focus 1 on the file path: a client that resets mid-`sendfile`.
//! No panic, the file is closed (also when a readahead was in flight:
//! DRV-09 keeps its descriptor until the worker returns), and the
//! process's descriptor count returns to its value before the download,
//! while the driver runs.
//!
//! The count (`/proc/self/fd`) is shared by every thread of the process,
//! so this binary holds exactly this one test (R4).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

use std::fs;
use std::io::{Read, Write};

use durchlauferhitzer::{Exit, LogEntry};
use durchlauferhitzer_core::{ConnId, Log};

#[path = "support/server.rs"]
mod server;
#[path = "support/tree.rs"]
mod tree;

use server::{Server, content_length, eventually, read_head};
use tree::{Tree, pattern, static_config};

fn open_fds() -> usize {
    fs::read_dir("/proc/self/fd").unwrap().count()
}

#[test]
fn client_reset_mid_sendfile_closes_the_file_and_leaks_no_fd() {
    let tree = Tree::new();
    let content = pattern(7, 32 * 1024 * 1024);
    tree.file("big.bin", &content);
    let server = Server::start_static(&static_config(tree.path()), 2);
    let baseline = open_fds();

    let mut client = server.connect();
    client
        .write_all(b"GET /big.bin HTTP/1.1\r\nHost: example.test\r\n\r\n")
        .unwrap();
    let head = read_head(&mut client);
    assert_eq!(content_length(&head), Some(content.len()));
    // Take part of the body, then close with sent bytes still unread: the
    // client's kernel resets the connection while `sendfile` goes on.
    let mut part = vec![0u8; 512 * 1024];
    client.read_exact(&mut part).unwrap();
    assert!(part == content[..part.len()]);
    drop(client);

    assert!(
        eventually(|| open_fds() == baseline),
        "descriptors: {} open, {baseline} before the download",
        open_fds()
    );
    let (exit, logs) = server.stop();
    assert_eq!(exit, Exit::Stopped);
    let core: Vec<Log> = logs
        .into_iter()
        .map(|entry| match entry {
            LogEntry::Core(log) => log,
            LogEntry::Driver(other) => panic!("driver log {other:?}"),
        })
        .collect();
    assert_eq!(core.len(), 2, "{core:?}");
    assert!(
        matches!(
            core[0],
            Log::Response {
                conn: ConnId(1),
                ..
            }
        ),
        "{:?}",
        core[0]
    );
    assert!(
        matches!(
            core[1],
            Log::Reset {
                conn: ConnId(1),
                ..
            }
        ),
        "{:?}",
        core[1]
    );
}
