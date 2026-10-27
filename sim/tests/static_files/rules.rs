//! Static route rules: methods, bodies, missing files, redirects,
//! index files, HEAD.

use super::*;

// ---------------------------------------------------------------------------
// Static route rules

pub(crate) fn fixed(status: Status, persistence: Persistence, body: BodyMode) -> Vec<u8> {
    model_http::model_fixed(status, persistence, body, None)
}

pub(crate) fn redirect(location: &str, persistence: Persistence, body: BodyMode) -> Vec<u8> {
    model_http::model_redirect(location, persistence, body, None)
}

pub(crate) fn head_req(path: &str) -> Vec<u8> {
    format!("HEAD {path} HTTP/1.1\r\nHost: example.test\r\n\r\n").into_bytes()
}

pub(crate) fn response_log(conn: ConnId, status: Status, method: Method, path: &str) -> Log {
    Log::Response {
        conn,
        peer: peer(),
        status,
        request: Some(RequestSummary {
            method,
            host: Some("example.test".to_owned()),
            path: path.to_owned(),
        }),
    }
}

/// Send `request` on a fresh connection and run until idle.
pub(crate) fn exchange(sim: &mut Sim, request: &[u8]) -> (ConnId, Vec<u8>) {
    let c = sim.connect(peer());
    sim.send(c, request);
    sim.run_until_idle();
    let received = sim.take_received(c);
    (c, received)
}

/// A plain `GET` of `path` gets a keep-alive 404, logged once with the raw
/// path, and leaves no file open.
pub(crate) fn assert_404(sim: &mut Sim, path: &str) {
    let (c, received) = exchange(sim, &get(path));
    assert_eq!(
        received,
        fixed(Status::NotFound, Persistence::KeepAlive, BodyMode::Full),
        "{path}\n{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{path}\n{}", sim.trace());
    let logs: Vec<&Log> = sim.logs().iter().filter(|l| log_conn(l) == c).collect();
    assert_eq!(
        logs,
        [&response_log(c, Status::NotFound, Method::Get, path)],
        "{path}\n{}",
        sim.trace()
    );
    assert_eq!(sim.open_files(), 0, "{path}\n{}", sim.trace());
}

pub(crate) fn log_conn(log: &Log) -> ConnId {
    match log {
        Log::Response { conn, .. }
        | Log::Rejected { conn, .. }
        | Log::Reset { conn, .. }
        | Log::FileAborted { conn, .. }
        | Log::ProxiedResponse { conn, .. }
        | Log::BackendFailed { conn, .. } => *conn,
    }
}

pub(crate) fn opens(trace: &str) -> usize {
    trace.lines().filter(|l| l.contains("-> OpenFile")).count()
}

#[test]
fn post_gets_405_with_allow() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "index.html", b"hello");
    let (c, received) = exchange(
        &mut sim,
        b"POST /index.html HTTP/1.1\r\nHost: example.test\r\n\r\n",
    );
    let expected = fixed(
        Status::MethodNotAllowed,
        Persistence::KeepAlive,
        BodyMode::Full,
    );
    assert_eq!(received, expected, "{}", sim.trace());
    let text = String::from_utf8(received).unwrap();
    assert!(text.contains("\r\nAllow: GET, HEAD\r\n"), "{text}");
    assert_eq!(opens(sim.trace()), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[response_log(
            c,
            Status::MethodNotAllowed,
            Method::Other("POST".to_owned()),
            "/index.html"
        )],
        "{}",
        sim.trace()
    );
}

#[test]
fn body_gets_413_and_closes() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "index.html", b"hello");
    let (c, received) = exchange(
        &mut sim,
        b"GET /index.html HTTP/1.1\r\nHost: example.test\r\nContent-Length: 5\r\n\r\nhello",
    );
    assert_eq!(
        received,
        fixed(Status::ContentTooLarge, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(opens(sim.trace()), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[response_log(
            c,
            Status::ContentTooLarge,
            Method::Get,
            "/index.html"
        )],
        "{}",
        sim.trace()
    );
    // Chunked bodies too.
    let (c, received) = exchange(
        &mut sim,
        b"GET /index.html HTTP/1.1\r\nHost: example.test\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
    );
    assert_eq!(
        received,
        fixed(Status::ContentTooLarge, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    assert_eq!(opens(sim.trace()), 0, "{}", sim.trace());
}

#[test]
fn missing_file_404() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "a/b.txt", b"x");
    assert_404(&mut sim, "/nope.txt");
    assert_404(&mut sim, "/a/nope.txt");
    // A path through a regular file.
    assert_404(&mut sim, "/a/b.txt/c");
    assert_404(&mut sim, "/assets/nope.css");
}

#[test]
fn regular_file_with_trailing_slash_404() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/assets", "a.css", b"body{}");
    assert_404(&mut sim, "/assets/a.css/");
    assert!(sim.trace().contains("-> CloseFile"), "{}", sim.trace());
}

#[test]
fn symlink_404() {
    let mut sim = Sim::new(CONFIG);
    sim.add_symlink("/srv/www", "passwd", "/etc/passwd");
    sim.add_file("/srv/www", "real/x.txt", b"x");
    sim.add_file("/srv/www", "ln/x.txt", b"x");
    sim.add_symlink("/srv/www", "ln", "real");
    assert_404(&mut sim, "/passwd");
    assert_404(&mut sim, "/ln/x.txt");
}

#[test]
fn permission_denied_404() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "secret.txt", b"x");
    sim.add_file("/srv/www", "private/x.txt", b"x");
    sim.deny("/srv/www", "secret.txt");
    sim.deny("/srv/www", "private");
    assert_404(&mut sim, "/secret.txt");
    assert_404(&mut sim, "/private/x.txt");
}

#[test]
fn dotfile_404() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", ".env", b"SECRET=1");
    sim.add_file("/srv/www", "a/.git/config", b"[core]");
    assert_404(&mut sim, "/.env");
    assert_404(&mut sim, "/a/.git/config");
    assert_eq!(opens(sim.trace()), 0, "{}", sim.trace());
}

#[test]
fn fifo_or_device_404() {
    let mut sim = Sim::new(CONFIG);
    sim.add_special("/srv/www", "fifo");
    sim.add_special("/srv/www", "dev/null");
    assert_404(&mut sim, "/fifo");
    assert_404(&mut sim, "/dev/null");
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(close_files(sim.trace(), 2), 1, "{}", sim.trace());
}

#[test]
fn directory_without_slash_redirects() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "docs/index.html", b"docs");
    let (c, received) = exchange(&mut sim, &get("/docs?x=1"));
    assert_eq!(
        received,
        b"HTTP/1.1 301 Moved Permanently\r\nLocation: /docs/?x=1\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 22\r\nX-Content-Type-Options: nosniff\r\n\r\n301 Moved Permanently\n",
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[response_log(
            c,
            Status::MovedPermanently,
            Method::Get,
            "/docs"
        )],
        "{}",
        sim.trace()
    );
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(opens(sim.trace()), 1, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
    // The root of a route without its slash; no query, no `?`.
    let (_, received) = exchange(&mut sim, &get("/assets"));
    assert_eq!(
        received,
        redirect("/assets/", Persistence::KeepAlive, BodyMode::Full),
        "{}",
        sim.trace()
    );
    // An empty query is kept as such.
    let (_, received) = exchange(&mut sim, &get("/docs?"));
    assert_eq!(
        received,
        redirect("/docs/?", Persistence::KeepAlive, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

#[test]
fn directory_with_slash_serves_index() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "docs/index.html", b"docs!");
    sim.add_file("/srv/www", "index.html", b"home");
    let (c, received) = exchange(&mut sim, &get("/docs/"));
    let mut expected = head("text/html; charset=utf-8", 5, Persistence::KeepAlive);
    expected.extend_from_slice(b"docs!");
    assert_eq!(received, expected, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[response_log(c, Status::Ok, Method::Get, "/docs/")],
        "{}",
        sim.trace()
    );
    assert!(
        sim.trace()
            .contains("file=2 -> OpenFile root=0 path=[\"docs\", \"index.html\"]"),
        "{}",
        sim.trace()
    );
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(close_files(sim.trace(), 2), 1, "{}", sim.trace());
    // The directory is closed before its index is opened.
    let close1 = sim.trace().find("file=1 -> CloseFile").unwrap();
    let open2 = sim.trace().find("file=2 -> OpenFile").unwrap();
    assert!(close1 < open2, "{}", sim.trace());
    // The root itself.
    let (_, received) = exchange(&mut sim, &get("/"));
    let mut expected = head("text/html; charset=utf-8", 4, Persistence::KeepAlive);
    expected.extend_from_slice(b"home");
    assert_eq!(received, expected, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

#[test]
fn directory_without_index_404() {
    let mut sim = Sim::new(CONFIG);
    sim.add_dir("/srv/www", "empty");
    sim.add_dir("/srv/www", "odd/index.html");
    sim.add_special("/srv/www", "weird/index.html");
    sim.add_file("/srv/www", "denied/index.html", b"x");
    sim.deny("/srv/www", "denied/index.html");
    assert_404(&mut sim, "/empty/");
    assert_404(&mut sim, "/odd/");
    assert_404(&mut sim, "/weird/");
    assert_404(&mut sim, "/denied/");
    assert_404(&mut sim, "/assets/");
}

/// Review Focus 5: HEAD on a large file sends the head with the full
/// `Content-Length`, reads nothing and closes the file.
#[test]
fn head_on_large_file_sends_head_only() {
    let mut sim = Sim::new(CONFIG);
    let size = 5 * 1024 * 1024;
    sim.add_file("/srv/www", "big.bin", &vec![b'b'; size]);
    let (c, received) = exchange(&mut sim, &head_req("/big.bin"));
    assert_eq!(
        received,
        head(
            "application/octet-stream",
            size as u64,
            Persistence::KeepAlive
        ),
        "{}",
        sim.trace()
    );
    assert!(!sim.trace().contains("Readahead"), "{}", sim.trace());
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.interest(c), Interest::Read, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[response_log(c, Status::Ok, Method::Head, "/big.bin")],
        "{}",
        sim.trace()
    );
    // A pipelined GET after the HEAD gets the body.
    sim.add_file("/srv/www", "s.txt", b"small");
    let mut both = head_req("/s.txt");
    both.extend_from_slice(&get("/s.txt"));
    let (_, received) = exchange(&mut sim, &both);
    let mut expected = head("text/plain; charset=utf-8", 5, Persistence::KeepAlive);
    expected.extend_from_slice(&head(
        "text/plain; charset=utf-8",
        5,
        Persistence::KeepAlive,
    ));
    expected.extend_from_slice(b"small");
    assert_eq!(received, expected, "{}", sim.trace());
    // HEAD of a directory index.
    sim.add_file("/srv/www", "docs/index.html", b"docs!");
    let (_, received) = exchange(&mut sim, &head_req("/docs/"));
    assert_eq!(
        received,
        head("text/html; charset=utf-8", 5, Persistence::KeepAlive),
        "{}",
        sim.trace()
    );
    // Only the pipelined GET read anything.
    assert_eq!(
        sim.trace().matches("-> Readahead").count(),
        1,
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

#[test]
fn head_error_response_has_no_body() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "docs/index.html", b"docs");
    // Unknown host.
    let c = sim.connect(peer());
    sim.send(c, b"HEAD /x HTTP/1.1\r\nHost: other.test\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        fixed(Status::NotFound, Persistence::KeepAlive, BodyMode::HeadOnly),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    // Missing file, dotfile, directory without index.
    for path in ["/missing", "/.env", "/docs/x/"] {
        let (_, received) = exchange(&mut sim, &head_req(path));
        assert_eq!(
            received,
            fixed(Status::NotFound, Persistence::KeepAlive, BodyMode::HeadOnly),
            "{path}\n{}",
            sim.trace()
        );
    }
    // Redirect.
    let (_, received) = exchange(&mut sim, &head_req("/docs"));
    assert_eq!(
        received,
        redirect("/docs/", Persistence::KeepAlive, BodyMode::HeadOnly),
        "{}",
        sim.trace()
    );
    // A body.
    let (c, received) = exchange(
        &mut sim,
        b"HEAD /docs/ HTTP/1.1\r\nHost: example.test\r\nContent-Length: 2\r\n\r\nhi",
    );
    assert_eq!(
        received,
        fixed(
            Status::ContentTooLarge,
            Persistence::Close,
            BodyMode::HeadOnly
        ),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

pub(crate) const WELL_KNOWN: &str = "listen 127.0.0.1:8080;
host example.test {
  path / { static /srv/www; }
  path /.well-known { static /srv/well-known; }
}";

#[test]
fn well_known_route_serves_files() {
    let mut sim = Sim::new(WELL_KNOWN);
    sim.add_file("/srv/well-known", "acme-challenge/abc", b"token");
    sim.add_file("/srv/well-known", ".secret", b"no");
    let (_, received) = exchange(&mut sim, &get("/.well-known/acme-challenge/abc"));
    let mut expected = head("application/octet-stream", 5, Persistence::KeepAlive);
    expected.extend_from_slice(b"token");
    assert_eq!(received, expected, "{}", sim.trace());
    assert_404(&mut sim, "/.well-known/.secret");
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());

    // Without the route the prefix is just a dotfile under `/`.
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", ".well-known/x", b"x");
    assert_404(&mut sim, "/.well-known/x");
    assert_eq!(opens(sim.trace()), 0, "{}", sim.trace());
}
