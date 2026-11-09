//! Backend-directed files through the built binary (FR-PROXY-32,
//! FR-STATIC-19): a std stub backend names files by `X-Accel-Redirect`
//! or `X-Sendfile` below an `internal_files` root on the real disk, and
//! the server sends them with `sendfile`. Landlock is `required` when
//! `DURCHLAUFERHITZER_REQUIRE_LANDLOCK=1` (CI), else `best_effort`; the
//! seccomp kill switch adds `seccomp required; seccomp_action kill;`
//! (FR-PROC-06). The 600 KiB file arrives exactly with the backend's
//! other headers kept, a range across a chunk boundary gives exactly its
//! bytes, a path outside the root is a 502 that sends none of that file,
//! a symlink in the root a 404, a client's forged file headers never
//! reach the backend (Rack's hints do), and after `Content-Length: 0`
//! the backend connection is pooled. Each head is compared byte for byte
//! with one built from the sim's spec-written model
//! (`sim/tests/support/model_http.rs`, with the file's stamp and the
//! `Date` the response carried), as `file_ranges.rs` does.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

#[path = "support/child.rs"]
mod child;

#[allow(dead_code, reason = "shared with the sim's test binaries; we use part")]
#[path = "../../sim/tests/support/model_http.rs"]
mod model_http;

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::UNIX_EPOCH;

use child::{
    Containment, PATIENCE, Running, Setup, TempDir, free_port, read_some, seccomp_kill, text,
};
use durchlauferhitzer_core::FileStamp;
use durchlauferhitzer_core::http::request::Persistence;
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;
use model_http::{model_file_head, model_fixed, model_partial_head};

/// The size of `big.bin`: more than two 256 KiB chunks (FR-STATIC-17).
const SIZE: usize = 600 * 1024;

/// `big.bin`'s type (FR-STATIC-12): the backend sends no `Content-Type`.
const TYPE: &str = "application/octet-stream";

/// Bytes that tell their positions apart (251 is prime).
fn pattern() -> Vec<u8> {
    (0..SIZE).map(|i| (i % 251) as u8).collect()
}

/// The stub backend's body behind a file header: never sent to a client
/// (FR-PROXY-32 "The backend's body is never read").
const DECOY: &str = "not the file";

/// What the stub backend answers for a request path (the routes do not
/// strip their prefix). `root` is the `internal_files` root.
fn answer(path: &str, root: &Path) -> String {
    let root = root.display();
    let (lines, body) = match path {
        // A body behind the header: never read, the upstream closed.
        "/a/big" => (
            "Content-Disposition: attachment; filename=\"big.bin\"\r\n\
             X-Accel-Redirect: /protected/big.bin\r\n"
                .to_owned(),
            DECOY,
        ),
        "/s/big" => (format!("X-Sendfile: {root}/big.bin\r\n"), ""),
        "/s/passwd" => ("X-Sendfile: /etc/passwd\r\n".to_owned(), DECOY),
        "/s/link" => (format!("X-Sendfile: {root}/link\r\n"), ""),
        "/s/inner" => (format!("X-Sendfile: {root}/inner\r\n"), ""),
        _ => (String::new(), "plain"),
    };
    format!(
        "HTTP/1.1 200 OK\r\n{lines}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// A keep-alive stub backend: records every request head it gets and
/// counts its connections.
struct Backend {
    addr: SocketAddr,
    accepted: Arc<AtomicUsize>,
    heads: Arc<Mutex<Vec<String>>>,
}

impl Backend {
    fn start(root: PathBuf) -> Backend {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let heads = Arc::new(Mutex::new(Vec::new()));
        let (count, record) = (Arc::clone(&accepted), Arc::clone(&heads));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                count.fetch_add(1, Ordering::SeqCst);
                let (record, root) = (Arc::clone(&record), root.clone());
                thread::spawn(move || serve(stream, &record, &root));
            }
        });
        Backend {
            addr,
            accepted,
            heads,
        }
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    fn heads(&self) -> Vec<String> {
        self.heads.lock().unwrap().clone()
    }
}

/// Answers each request head on `stream` (no bodies here) until the
/// proxy closes the connection.
fn serve(mut stream: TcpStream, record: &Mutex<Vec<String>>, root: &Path) {
    stream.set_read_timeout(Some(PATIENCE * 4)).unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = text(&buf.drain(..end + 4).collect::<Vec<u8>>());
            let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
            record.lock().unwrap().push(head);
            // The proxy may have closed it already (a file whose body it
            // never reads).
            if stream.write_all(answer(&path, root).as_bytes()).is_err() {
                return;
            }
        }
        // EINTR is no end: the read has a timeout (`support/read.rs`).
        match read_some(&mut stream, &mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// The site: `files/big.bin` (the pattern) and `files/link`, a symlink
/// to `/etc/passwd`, outside the root.
struct Site {
    dir: TempDir,
    root: PathBuf,
}

impl Site {
    /// The stamp the server reports for `big.bin`: its size and mtime in
    /// nanoseconds (FR-STATIC-18).
    fn stamp(&self) -> FileStamp {
        let meta = fs::metadata(self.root.join("big.bin")).unwrap();
        let mtime = meta.modified().unwrap().duration_since(UNIX_EPOCH).unwrap();
        FileStamp {
            size: meta.len(),
            modified_ns: u64::try_from(mtime.as_nanos()).unwrap(),
        }
    }

    fn new() -> Site {
        let dir = TempDir::new();
        let root = dir.dir("files");
        let big = root.join("big.bin");
        fs::write(&big, pattern()).unwrap();
        fs::set_permissions(&big, fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("link")).unwrap();
        // Inside the root: only the no-symlink rule refuses it (Landlock
        // would allow the file it names).
        std::os::unix::fs::symlink("big.bin", root.join("inner")).unwrap();
        Site { dir, root }
    }

    /// The server: `/a` serves `x-accel-redirect /protected/` and `/s`
    /// `x-sendfile` below the root, both proxied to `backend`.
    fn serve(&self, backend: SocketAddr) -> (Running, SocketAddr) {
        let addr = SocketAddr::from(([127, 0, 0, 1], free_port()));
        let root = self.root.display();
        let route = |prefix: &str, files: &str| {
            format!(
                "  path {prefix} {{ proxy {backend}; max_request_body 1MiB; \
                 max_response_body 1MiB; internal_files {files}; }}\n"
            )
        };
        let conf = format!(
            "listen {addr};\n{}host example.test {{\n{}{}}}\n",
            containment(),
            route("/a", &format!("x-accel-redirect /protected/ {root}")),
            route("/s", &format!("x-sendfile {root}")),
        );
        let path = self
            .dir
            .config("internal.conf", &conf, Containment::AsWritten);
        let running = Running::start(&[path.to_str().unwrap()], Setup::non_root(), addr);
        (running, addr)
    }
}

/// Landlock `required` where it must be there (CI), else `best_effort`;
/// seccomp by the kill switch (`support/conf.rs`).
fn containment() -> String {
    let landlock =
        if std::env::var_os("DURCHLAUFERHITZER_REQUIRE_LANDLOCK").is_some_and(|v| v == "1") {
            "required"
        } else {
            "best_effort"
        };
    let seccomp = if seccomp_kill() {
        " seccomp required; seccomp_action kill;"
    } else {
        ""
    };
    format!("containment {{ landlock {landlock};{seccomp} }}\n")
}

/// Sends `head_lines` (after the request line and `Host`, with
/// `Connection: close`) for `path`; the response head and body.
fn exchange(addr: SocketAddr, path: &str, head_lines: &str) -> (String, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: example.test\r\n{head_lines}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no head end: {}", text(&response)))
        + 4;
    (text(&response[..end]), response[end..].to_vec())
}

/// The `Date` value `head` carries (FR-HTTP-22a).
fn date_of(head: &str) -> String {
    head.split("\r\nDate: ")
        .nth(1)
        .and_then(|rest| rest.get(..29))
        .unwrap_or_else(|| panic!("no Date: {head}"))
        .to_owned()
}

/// A merged head (FR-PROXY-32): the model's file head `model` with the
/// backend's kept lines `kept` right after its `Date`.
fn merged(model: &[u8], date: &str, kept: &str) -> String {
    let date_line = format!("Date: {date}\r\n");
    text(model).replacen(&date_line, &format!("{date_line}{kept}"), 1)
}

/// Whether the server logged a line of `kind` with the field `field`
/// (`key=value`).
fn logged(log: &str, kind: &str, field: &str) -> bool {
    log.lines().any(|l| {
        l.starts_with(&format!("kind={kind} ")) && l.split_whitespace().any(|f| f == field)
    })
}

/// FR-PROXY-32: the backend's `X-Accel-Redirect` names the 600 KiB file,
/// which arrives exactly, behind the backend's kept headers; its body is
/// never sent. Logged once as `backend_file` with the client's status.
#[test]
fn x_accel_redirect_serves_the_file_under_containment() {
    let site = Site::new();
    let backend = Backend::start(site.root.clone());
    let (running, addr) = site.serve(backend.addr);
    let (head, body) = exchange(addr, "/a/big", "");
    let date = date_of(&head);
    let model = model_file_head(TYPE, site.stamp(), Persistence::Close, Some(&date));
    let kept = "content-disposition: attachment; filename=\"big.bin\"\r\n";
    assert_eq!(head, merged(&model, &date, kept));
    assert!(body == pattern(), "{} body bytes", body.len());
    let log = running.kill();
    assert!(logged(&log, "backend_file", "status=200"), "{log}");
    assert!(!log.contains("kind=proxied_response"), "{log}");
}

/// FR-STATIC-19 on a backend's file: a range across the 256 KiB chunk
/// boundary gets exactly its bytes.
#[test]
fn x_sendfile_range_crosses_a_chunk() {
    let site = Site::new();
    let backend = Backend::start(site.root.clone());
    let (running, addr) = site.serve(backend.addr);
    let (head, body) = exchange(addr, "/s/big", "Range: bytes=200000-500000\r\n");
    let date = date_of(&head);
    let model = model_partial_head(
        TYPE,
        site.stamp(),
        (200_000, 500_000),
        Persistence::Close,
        Some(&date),
    );
    assert_eq!(head, text(&model));
    assert!(
        body == pattern()[200_000..=500_000],
        "{} body bytes",
        body.len()
    );
    let log = running.kill();
    assert!(logged(&log, "backend_file", "status=206"), "{log}");
}

/// A path outside the root is the backend misbehaving: a 502, logged
/// `bad_file_path`, and not a byte of that file is sent.
#[test]
fn outside_the_root_is_502() {
    let site = Site::new();
    let backend = Backend::start(site.root.clone());
    let (running, addr) = site.serve(backend.addr);
    let (head, body) = exchange(addr, "/s/passwd", "");
    let model = model_fixed(
        Status::BadGateway,
        Persistence::Close,
        BodyMode::Full,
        Some(&date_of(&head)),
    );
    assert_eq!(format!("{head}{}", text(&body)), text(&model));
    let passwd = fs::read_to_string("/etc/passwd").unwrap();
    let first = passwd.lines().next().unwrap();
    assert!(!head.contains(first) && !text(&body).contains(DECOY));
    let log = running.kill();
    assert!(
        logged(&log, "backend_failed", "failure=bad_file_path"),
        "{log}"
    );
}

/// FR-STATIC-09: a symlink in the root is a 404, as on a static route:
/// one pointing out of it, and one pointing at `big.bin` beside it.
#[test]
fn symlink_in_the_root_is_404() {
    let site = Site::new();
    let backend = Backend::start(site.root.clone());
    let (running, addr) = site.serve(backend.addr);
    for path in ["/s/link", "/s/inner"] {
        let (head, body) = exchange(addr, path, "");
        let model = model_fixed(
            Status::NotFound,
            Persistence::Close,
            BodyMode::Full,
            Some(&date_of(&head)),
        );
        assert_eq!(format!("{head}{}", text(&body)), text(&model), "{path}");
    }
    running.kill();
}

/// FR-PROXY-32 Stripping and Hints: a client's file headers never reach
/// the backend, which gets Rack's hints for its route instead; nor does
/// one spelled with `_` for `-` (`X_Accel_Mapping`, which a CGI server
/// reads as the same key; owner decision 2026-11-09).
#[test]
fn client_cannot_forge_file_headers() {
    let site = Site::new();
    let backend = Backend::start(site.root.clone());
    let (running, addr) = site.serve(backend.addr);
    let forged = "X-Sendfile: /etc/passwd\r\nX-Accel-Redirect: /protected/../x\r\n\
                  X-Sendfile-Type: X-Sendfile\r\nX-Accel-Mapping: /=/protected/\r\n\
                  X_Accel_Mapping: /etc/=/protected/\r\n";
    let (head, body) = exchange(addr, "/a/plain", forged);
    // Relayed (FR-PROXY-12): the backend's lines, then the core's
    // `Date` (the backend sent none), lowercase like every relayed line.
    let date = head
        .split("\r\ndate: ")
        .nth(1)
        .and_then(|rest| rest.get(..29))
        .unwrap_or_else(|| panic!("no date: {head}"));
    assert_eq!(
        head,
        format!(
            "HTTP/1.1 200 OK\r\ncontent-length: 5\r\ndate: {date}\r\nconnection: close\r\n\r\n"
        )
    );
    assert_eq!(text(&body), "plain");
    let heads = backend.heads();
    assert_eq!(heads.len(), 1, "{heads:?}");
    let got = heads[0].to_ascii_lowercase();
    for value in [
        "/etc/passwd",
        "/protected/../x",
        "x-sendfile-type: x-sendfile",
        "x-accel-mapping: /=/",
        "x_accel_mapping",
        "/etc/=/protected/",
    ] {
        assert!(!got.contains(value), "{value} reached the backend: {got}");
    }
    let hints = format!(
        "\r\nx-sendfile-type: X-Accel-Redirect\r\nx-accel-mapping: {}/=/protected/\r\n",
        site.root.display()
    );
    assert!(heads[0].contains(&hints), "{}", heads[0]);
    running.kill();
}

/// FR-PROXY-02: after `Content-Length: 0` the backend connection is
/// pooled, so two sequential requests share one.
#[test]
fn pooled_after_content_length_0() {
    let site = Site::new();
    let backend = Backend::start(site.root.clone());
    let (running, addr) = site.serve(backend.addr);
    for _ in 0..2 {
        let (head, body) = exchange(addr, "/s/big", "");
        let model = model_file_head(
            TYPE,
            site.stamp(),
            Persistence::Close,
            Some(&date_of(&head)),
        );
        assert_eq!(head, text(&model));
        assert!(body == pattern(), "{} body bytes", body.len());
    }
    assert_eq!(backend.accepted(), 1);
    let log = running.kill();
    assert_eq!(
        log.lines()
            .filter(|l| l.starts_with("kind=backend_file "))
            .count(),
        2,
        "{log}"
    );
}
