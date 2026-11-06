//! Conditional requests end to end (FR-STATIC-18): `ETag` and
//! `Last-Modified` on every 200, `If-None-Match` and `If-Modified-Since`
//! giving 304, and the decision taken on the stamp of the opened file. The
//! dates are written out here, the heads come from the spec-written model.

use super::*;

/// 2023-11-14 22:13:20 UTC plus 5 ns, the mtime the tests give `f.txt`.
const MTIME_NS: u64 = 1_700_000_000_000_000_005;
/// `f.txt`'s tag: 3 bytes, `MTIME_NS` in hex.
const TAG: &str = "\"3-17979cfe362a0005\"";
const LAST_MODIFIED: &str = "Tue, 14 Nov 2023 22:13:20 GMT";
/// The sim's clock: an hour after the mtime.
const NOW: (u64, &str) = (1_700_003_600, "Tue, 14 Nov 2023 23:13:20 GMT");

const STAMP: FileStamp = FileStamp {
    size: 3,
    modified_ns: MTIME_NS,
};

fn dated_sim() -> Sim {
    let mut sim = Sim::new(CONFIG);
    sim.set_date(NOW.0);
    sim.add_file("/srv/www", "f.txt", b"abc");
    sim.set_modified("/srv/www", "f.txt", MTIME_NS);
    sim
}

fn request(method: &str, headers: &str) -> Vec<u8> {
    format!("{method} /f.txt HTTP/1.1\r\nHost: example.test\r\n{headers}\r\n").into_bytes()
}

fn ok(stamp: FileStamp, body: &[u8]) -> Vec<u8> {
    let mut out = model_http::model_file_head(
        "text/plain; charset=utf-8",
        stamp,
        Persistence::KeepAlive,
        Some(NOW.1),
    );
    out.extend_from_slice(body);
    out
}

fn not_modified(persistence: Persistence) -> Vec<u8> {
    model_http::model_not_modified(STAMP, persistence, Some(NOW.1))
}

/// `request` on a fresh connection of `sim` is answered 304, logged as
/// such, with the file closed unread.
fn assert_304(sim: &mut Sim, method: Method, headers: &str) {
    let name = match method {
        Method::Head => "HEAD",
        Method::Get | Method::Other(_) => "GET",
    };
    let reads = sim.trace().matches("Readahead").count();
    let (c, received) = exchange(sim, &request(name, headers));
    assert_eq!(
        String::from_utf8_lossy(&received),
        String::from_utf8_lossy(&not_modified(Persistence::KeepAlive)),
        "{headers}\n{}",
        sim.trace()
    );
    let logs: Vec<&Log> = sim.logs().iter().filter(|l| log_conn(l) == c).collect();
    assert_eq!(
        logs,
        [&response_log(c, Status::NotModified, method, "/f.txt")],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
    let read = sim.trace().matches("Readahead").count() > reads;
    assert!(!read, "{}", sim.trace());
}

fn assert_200(sim: &mut Sim, headers: &str) {
    let (c, received) = exchange(sim, &request("GET", headers));
    assert_eq!(
        String::from_utf8_lossy(&received),
        String::from_utf8_lossy(&ok(STAMP, b"abc")),
        "{headers}\n{}",
        sim.trace()
    );
    let logs: Vec<&Log> = sim.logs().iter().filter(|l| log_conn(l) == c).collect();
    assert_eq!(
        logs,
        [&response_log(c, Status::Ok, Method::Get, "/f.txt")],
        "{}",
        sim.trace()
    );
}

/// FR-STATIC-11: the 200 head carries the validators after
/// `Content-Length`, written out in full once.
#[test]
fn a_200_carries_etag_and_last_modified() {
    let mut sim = dated_sim();
    let (_, received) = exchange(&mut sim, &request("GET", ""));
    let expected = format!(
        "HTTP/1.1 200 OK\r\nDate: {}\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: 3\r\nETag: {TAG}\r\nLast-Modified: {LAST_MODIFIED}\r\n\
         X-Content-Type-Options: nosniff\r\n\r\nabc",
        NOW.1
    );
    assert_eq!(String::from_utf8_lossy(&received), expected);
    assert_eq!(received, ok(STAMP, b"abc"));
}

#[test]
fn if_none_match_that_matches_is_a_304() {
    let mut sim = dated_sim();
    for headers in [
        format!("If-None-Match: {TAG}\r\n"),
        format!("If-None-Match: W/{TAG}\r\n"),
        format!("If-None-Match: \"x\", W/\"y\" ,{TAG}\r\n"),
        format!("If-None-Match: \"x\"\r\nIf-None-Match: {TAG}\r\n"),
        "If-None-Match: *\r\n".to_owned(),
    ] {
        assert_304(&mut sim, Method::Get, &headers);
        assert_304(&mut sim, Method::Head, &headers);
    }
}

#[test]
fn if_none_match_that_does_not_match_or_is_malformed_is_a_200() {
    let mut sim = dated_sim();
    for headers in [
        "If-None-Match: \"3-17979cfe362a0004\"\r\n",
        "If-None-Match: \"3-17979CFE362A0005\"\r\n",
        "If-None-Match: \"x\"\r\n",
        "If-None-Match: 3-17979cfe362a0005\r\n",
        "If-None-Match: *, \"x\"\r\n",
        "If-None-Match: *\r\nIf-None-Match: *\r\n",
        "If-Match: \"x\"\r\nIf-Range: \"x\"\r\n",
    ] {
        assert_200(&mut sim, headers);
    }
}

#[test]
fn if_modified_since_in_each_date_form() {
    let mut sim = dated_sim();
    for date in [
        LAST_MODIFIED,
        "Tuesday, 14-Nov-23 22:13:20 GMT",
        "Tue Nov 14 22:13:20 2023",
        "Tue, 14 Nov 2023 23:13:20 GMT",
    ] {
        let headers = format!("If-Modified-Since: {date}\r\n");
        assert_304(&mut sim, Method::Get, &headers);
        assert_304(&mut sim, Method::Head, &headers);
    }
    for date in [
        // Older than the file.
        "Tue, 14 Nov 2023 22:13:19 GMT",
        "Tuesday, 14-Nov-23 22:13:19 GMT",
        // Later than the server's clock: invalid.
        "Tue, 14 Nov 2023 23:13:21 GMT",
        // Malformed.
        "Tue, 14 Nov 2023 22:13:20",
        "2023-11-14T22:13:20Z",
    ] {
        assert_200(&mut sim, &format!("If-Modified-Since: {date}\r\n"));
    }
    let twice =
        format!("If-Modified-Since: {LAST_MODIFIED}\r\nIf-Modified-Since: {LAST_MODIFIED}\r\n");
    assert_200(&mut sim, &twice);
}

#[test]
fn if_none_match_decides_over_if_modified_since() {
    let mut sim = dated_sim();
    assert_200(
        &mut sim,
        &format!("If-None-Match: \"x\"\r\nIf-Modified-Since: {LAST_MODIFIED}\r\n"),
    );
    assert_304(
        &mut sim,
        Method::Get,
        &format!("If-Modified-Since: Thu, 01 Jan 1970 00:00:00 GMT\r\nIf-None-Match: {TAG}\r\n"),
    );
}

/// Review focus 2: the file changes after the client got its tag, and
/// while the open is on its way: the opened version is judged and sent,
/// with its own `ETag`.
#[test]
fn a_file_changed_before_the_open_is_a_200_with_the_new_etag() {
    let mut sim = dated_sim();
    sim.set_disk_delay(Duration::from_millis(5));
    let c = sim.connect(peer());
    sim.send(c, &request("GET", &format!("If-None-Match: {TAG}\r\n")));
    sim.run_until_idle();
    assert!(sim.opening_file(c), "{}", sim.trace());
    sim.add_file("/srv/www", "f.txt", b"xyz");
    let changed = sim.file_stamp("/srv/www", "f.txt");
    assert_ne!(changed, STAMP);
    sim.advance(Duration::from_millis(10));
    sim.run_until_idle();
    assert_eq!(
        String::from_utf8_lossy(&sim.take_received(c)),
        String::from_utf8_lossy(&ok(changed, b"xyz")),
        "{}",
        sim.trace()
    );
}

#[test]
fn a_closing_304_closes() {
    let mut sim = dated_sim();
    let headers = format!("If-None-Match: {TAG}\r\nConnection: close\r\n");
    let (c, received) = exchange(&mut sim, &request("GET", &headers));
    assert_eq!(
        received,
        not_modified(Persistence::Close),
        "{}",
        sim.trace()
    );
    assert_ne!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}

/// A 304 keeps the connection: a pipelined GET after it gets the file.
#[test]
fn a_pipelined_get_follows_a_304() {
    let mut sim = dated_sim();
    let mut both = request("GET", &format!("If-None-Match: {TAG}\r\n"));
    both.extend_from_slice(&request("GET", ""));
    let (_, received) = exchange(&mut sim, &both);
    let mut expected = not_modified(Persistence::KeepAlive);
    expected.extend_from_slice(&ok(STAMP, b"abc"));
    assert_eq!(received, expected, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

/// Errors are not conditional: `*` on a missing file is a 404.
#[test]
fn a_missing_file_is_a_404_whatever_the_conditionals() {
    let mut sim = Sim::new(CONFIG);
    let get = b"GET /nope HTTP/1.1\r\nHost: example.test\r\nIf-None-Match: *\r\n\r\n";
    let (_, received) = exchange(&mut sim, get);
    assert_eq!(
        received,
        fixed(Status::NotFound, Persistence::KeepAlive, BodyMode::Full)
    );
}
