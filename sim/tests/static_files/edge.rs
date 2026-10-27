//! Edge cases: index opens in flight, the proxy route next door, long
//! redirects, encodings and per-IP slots.

use super::*;

// ---------------------------------------------------------------------------
// Deferred targeted tests

/// Two connection slots; a `docs/` directory with an index.
pub(crate) fn index_sim() -> Sim {
    let mut sim = Sim::new(CAPPED);
    sim.add_file("/srv/www", "docs/index.html", b"docs!");
    sim
}

/// The directory open (file 1) is answered after 10 s; the index open
/// (file 2) then takes `index_delay`. Returns the connection once the
/// index open is in flight.
pub(crate) fn index_open_in_flight(sim: &mut Sim, index_delay: Duration) -> ConnId {
    sim.set_disk_delay(Duration::from_secs(10));
    let c = sim.connect(peer());
    sim.send(c, &get("/docs/"));
    sim.run_until_idle();
    sim.set_disk_delay(index_delay);
    sim.advance(Duration::from_secs(10));
    assert!(
        sim.trace().contains("file=2 -> OpenFile"),
        "{}",
        sim.trace()
    );
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(close_files(sim.trace(), 2), 0, "{}", sim.trace());
    c
}

/// Whether a new connection is accepted (not refused with 503); it is
/// closed again either way.
pub(crate) fn slot_free(sim: &mut Sim) -> bool {
    let probe = sim.connect(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)));
    sim.run_until_idle();
    let refused = sim.take_received(probe).starts_with(b"HTTP/1.1 503 ");
    sim.reset(probe);
    sim.run_until_idle();
    !refused
}

/// A reset while the `index.html` open is in flight orphans that open: it
/// keeps its connection slot until the late `FileOpened`, which closes
/// the file exactly once.
#[test]
fn reset_during_index_open_orphans_it() {
    let mut sim = index_sim();
    let c = index_open_in_flight(&mut sim, Duration::from_secs(20));
    sim.reset(c);
    sim.run_until_idle();
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    // One slot is the orphan's, one the probe's: a second probe is refused.
    let holder = sim.connect(peer());
    sim.run_until_idle();
    assert!(!slot_free(&mut sim), "{}", sim.trace());
    sim.reset(holder);
    sim.advance(Duration::from_secs(20));
    assert_eq!(close_files(sim.trace(), 2), 1, "{}", sim.trace());
    // The orphan's slot is free: a holder and a probe both fit.
    let _holder = sim.connect(peer());
    assert!(slot_free(&mut sim), "{}", sim.trace());
    sim.advance(Duration::from_secs(60));
    assert_eq!(close_files(sim.trace(), 2), 1, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
}

/// `send_idle_timeout` passing while the `index.html` open is in flight is
/// a disk timeout; the orphan keeps its slot until the late `FileOpened`,
/// which closes the file exactly once.
#[test]
fn disk_timeout_during_index_open_orphans_it() {
    let mut sim = index_sim();
    let c = index_open_in_flight(&mut sim, Duration::from_secs(45));
    // The deadline was reset when the directory open completed (t=10 s).
    sim.advance(Duration::from_secs(30));
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[Log::Rejected {
            conn: c,
            peer: peer(),
            reason: RejectReason::Timeout(TimeoutKind::Disk),
        }],
        "{}",
        sim.trace()
    );
    let holder = sim.connect(peer());
    sim.run_until_idle();
    assert!(!slot_free(&mut sim), "{}", sim.trace());
    assert_eq!(close_files(sim.trace(), 2), 0, "{}", sim.trace());
    sim.reset(holder);
    // The index open was issued at t=10 s and completes at t=55 s.
    sim.advance(Duration::from_secs(15));
    assert_eq!(close_files(sim.trace(), 2), 1, "{}", sim.trace());
    let _holder = sim.connect(peer());
    assert!(slot_free(&mut sim), "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
}

pub(crate) const WITH_PROXY: &str = "listen 127.0.0.1:8080;
host example.test {
  path / { static /srv/www; }
  path /api/ { proxy 127.0.0.1:3000; max_request_body 1MiB; max_response_body unlimited; }
}";

/// Every response to a HEAD is head only, also the 502 of a proxy route
/// whose backend refuses. A HEAD itself is never answered 405 (GET and HEAD
/// are the allowed methods), so the 405 keeps its body and a HEAD of the
/// same path gets the head of that path's answer.
#[test]
fn head_on_405_path_and_proxy_502_is_head_only() {
    let mut sim = Sim::new(WITH_PROXY);
    sim.set_connect("127.0.0.1:3000", ConnectBehaviour::Refuse);
    sim.add_file("/srv/www", "a.txt", b"hello");
    let (c, received) = exchange(&mut sim, &head_req("/api/x"));
    assert_eq!(
        received,
        fixed(
            Status::BadGateway,
            Persistence::KeepAlive,
            BodyMode::HeadOnly
        ),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[
            Log::BackendFailed {
                conn: c,
                peer: peer(),
                backend: BackendId(0),
                kind: BackendFailure::Refused,
            },
            response_log(c, Status::BadGateway, Method::Head, "/api/x")
        ],
        "{}",
        sim.trace()
    );
    // POST then HEAD of the same path on one connection.
    let mut both = b"POST /a.txt HTTP/1.1\r\nHost: example.test\r\n\r\n".to_vec();
    both.extend_from_slice(&head_req("/a.txt"));
    let (c, received) = exchange(&mut sim, &both);
    let mut expected = fixed(
        Status::MethodNotAllowed,
        Persistence::KeepAlive,
        BodyMode::Full,
    );
    expected.extend_from_slice(&head(
        "text/plain; charset=utf-8",
        5,
        Persistence::KeepAlive,
    ));
    assert_eq!(received, expected, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(opens(sim.trace()), 1, "{}", sim.trace());
    assert!(!sim.trace().contains("Readahead"), "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

/// Under `abort`, a change already reported with the first chunk cuts the
/// connection before any body byte: the client sees at most the head.
#[test]
fn change_on_first_chunk_aborts_before_the_body() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "c.txt", b"hello");
    let new = FileStamp {
        size: 5,
        modified_ns: 99,
    };
    sim.change_during_send("/srv/www", "c.txt", 0, new);
    let (c, received) = exchange(&mut sim, &get("/c.txt"));
    let full_head = head("text/plain; charset=utf-8", 5, Persistence::KeepAlive);
    assert!(full_head.starts_with(&received), "{}", sim.trace());
    assert!(!sim.trace().contains("FileSent"), "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[
            response_log(c, Status::Ok, Method::Get, "/c.txt"),
            aborted(c, FileError::Changed),
        ],
        "{}",
        sim.trace()
    );
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());

    // With a slow disk the head is out before the first chunk is read.
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "c.txt", b"hello");
    sim.change_during_send("/srv/www", "c.txt", 0, new);
    sim.set_disk_delay(Duration::from_millis(10));
    let c = sim.connect(peer());
    sim.send(c, &get("/c.txt"));
    sim.run_until_idle();
    sim.advance(Duration::from_secs(1));
    assert_eq!(sim.take_received(c), full_head, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(sim.logs().last(), Some(&aborted(c, FileError::Changed)));
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

// ---------------------------------------------------------------------------
// Final-review fixes

/// Request lines long enough for a `301` that does not fit the 1 KiB send
/// buffer.
pub(crate) const LONG_LINES: &str = "listen 127.0.0.1:8080;
limits { max_request_line 16KiB; max_head 32KiB; }
host example.test { path / { static /srv/www; } }";

/// C1: a redirect whose `Location` would overflow the send buffer is
/// answered `414` instead, with the same persistence and body rules.
#[test]
fn redirect_too_large_for_the_send_buffer_is_414() {
    let mut sim = Sim::new(LONG_LINES);
    let dir = "a".repeat(600);
    sim.add_dir("/srv/www", &dir);
    let path = format!("/{dir}");
    let target = format!("{path}?{}", "q".repeat(3000));
    let (c, received) = exchange(&mut sim, &get(&target));
    assert_eq!(
        received,
        fixed(Status::UriTooLong, Persistence::KeepAlive, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[response_log(c, Status::UriTooLong, Method::Get, &path)],
        "{}",
        sim.trace()
    );
    let (_, received) = exchange(&mut sim, &head_req(&target));
    assert_eq!(
        received,
        fixed(
            Status::UriTooLong,
            Persistence::KeepAlive,
            BodyMode::HeadOnly
        ),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

/// C1 boundary: a redirect of exactly the send buffer's capacity is sent;
/// one byte more is a `414`.
#[test]
fn redirect_that_just_fits_is_301() {
    let mut sim = Sim::new(LONG_LINES);
    let dir = "a".repeat(600);
    sim.add_dir("/srv/www", &dir);
    let path = format!("/{dir}");
    let base = redirect(&format!("{path}/?"), Persistence::KeepAlive, BodyMode::Full).len();
    let fits = 1024 - base;
    let location = format!("{path}/?{}", "q".repeat(fits));
    let (c, received) = exchange(&mut sim, &get(&format!("{path}?{}", "q".repeat(fits))));
    let expected = redirect(&location, Persistence::KeepAlive, BodyMode::Full);
    assert_eq!(expected.len(), 1024);
    assert_eq!(received, expected, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    let (_, received) = exchange(&mut sim, &get(&format!("{path}?{}", "q".repeat(fits + 1))));
    assert_eq!(
        received,
        fixed(Status::UriTooLong, Persistence::KeepAlive, BodyMode::Full),
        "{}",
        sim.trace()
    );
}

/// C2: a percent-encoded unreserved character in the path is rejected, so
/// `/%61ssets/a.css` cannot slip past the `/assets` route into `/`.
#[test]
fn encoded_unreserved_character_in_the_path_is_400() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/assets", "a.css", b"body{}");
    sim.add_file("/srv/www", "assets/a.css", b"shadowed");
    for target in ["/%61ssets/a.css", "/assets/%61.css", "/assets/a.c%73s"] {
        let (c, received) = exchange(&mut sim, &get(target));
        assert_eq!(
            received,
            fixed(Status::BadRequest, Persistence::Close, BodyMode::Full),
            "{target}\n{}",
            sim.trace()
        );
        let logs: Vec<&Log> = sim.logs().iter().filter(|l| log_conn(l) == c).collect();
        assert_eq!(
            logs,
            [&Log::Rejected {
                conn: c,
                peer: peer(),
                reason: RejectReason::Http {
                    error: HttpError::BadTarget(TargetError::EncodedUnreserved),
                    request: None,
                },
            }],
            "{target}\n{}",
            sim.trace()
        );
    }
    assert_eq!(opens(sim.trace()), 0, "{}", sim.trace());
}

pub(crate) const PER_IP_CAPPED: &str = "listen 127.0.0.1:8080;
limits { max_connections 10; max_connections_per_ip 2; }
host example.test { path / { static /srv/www; } }";

/// I2: an orphaned open keeps its peer's per-IP slot as well as its global
/// one until the disk answers, so one IP looping GET+reset against a hung
/// disk is capped at `max_connections_per_ip` orphans and cannot take every
/// global slot.
#[test]
fn orphaned_opens_hold_their_per_ip_slots() {
    let mut sim = Sim::new(PER_IP_CAPPED);
    sim.add_file("/srv/www", "index.html", b"hello");
    sim.set_disk_delay(Duration::from_secs(3600));
    for _ in 0..2 {
        let c = sim.connect(peer());
        sim.send(c, &get("/index.html"));
        sim.run_until_idle();
        sim.reset(c);
    }
    sim.set_disk_delay(Duration::ZERO);
    let refused = sim.connect(peer());
    sim.run_until_idle();
    assert!(
        sim.take_received(refused).starts_with(b"HTTP/1.1 503 "),
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.logs().last(),
        Some(&Log::Rejected {
            conn: refused,
            peer: peer(),
            reason: RejectReason::Limit(LimitKind::ConnectionsPerIp),
        }),
        "{}",
        sim.trace()
    );
    // Another peer is still served.
    let other = sim.connect(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)));
    sim.send(other, &get("/index.html"));
    sim.run_until_idle();
    let mut expected = head("text/html; charset=utf-8", 5, Persistence::KeepAlive);
    expected.extend_from_slice(b"hello");
    assert_eq!(sim.take_received(other), expected, "{}", sim.trace());
    // Once the disk answers both orphans, both slots are back.
    sim.advance(Duration::from_secs(3600));
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(close_files(sim.trace(), 2), 1, "{}", sim.trace());
    for _ in 0..2 {
        let c = sim.connect(peer());
        sim.send(c, &get("/index.html"));
        sim.run_until_idle();
        assert_eq!(sim.take_received(c), expected, "{}", sim.trace());
    }
}

/// Amendment 5 for late parse errors: an error found after the request
/// line (the method is known) answers a HEAD request head-only; an error
/// in the request line itself keeps its body.
#[test]
fn head_with_a_late_parse_error_is_head_only() {
    let mut sim = Sim::new(CONFIG);
    let late: [(&[u8], Status); 4] = [
        (b"HEAD / HTTP/1.1\r\n\r\n", Status::BadRequest),
        (
            b"HEAD / HTTP/1.1\r\nHost: example.test\r\nContent-Length: x\r\n\r\n",
            Status::BadRequest,
        ),
        (
            b"HEAD / HTTP/1.1\r\nHost: example.test\r\nHost: example.test\r\n\r\n",
            Status::BadRequest,
        ),
        (
            b"HEAD / HTTP/1.1\r\nBad Name: 1\r\n\r\n",
            Status::BadRequest,
        ),
    ];
    for (request, status) in late {
        let (c, received) = exchange(&mut sim, request);
        assert_eq!(
            received,
            fixed(status, Persistence::Close, BodyMode::HeadOnly),
            "{:?}\n{}",
            String::from_utf8_lossy(request),
            sim.trace()
        );
        assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    }
    // A GET with the same error keeps its body.
    let (_, received) = exchange(&mut sim, b"GET / HTTP/1.1\r\n\r\n");
    assert_eq!(
        received,
        fixed(Status::BadRequest, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    let early: [(&[u8], Status); 3] = [
        (b"HEAD /a b HTTP/1.1\r\n\r\n", Status::BadRequest),
        (
            b"HEAD /%2e HTTP/1.1\r\nHost: example.test\r\n\r\n",
            Status::BadRequest,
        ),
        (
            b"HEAD / HTTP/1.2\r\nHost: example.test\r\n\r\n",
            Status::HttpVersionNotSupported,
        ),
    ];
    for (request, status) in early {
        let (_, received) = exchange(&mut sim, request);
        assert_eq!(
            received,
            fixed(status, Persistence::Close, BodyMode::Full),
            "{:?}\n{}",
            String::from_utf8_lossy(request),
            sim.trace()
        );
    }
}

pub(crate) const SPELLINGS: &str = "listen 127.0.0.1:8080;
host example.test {
  path / { static /srv/www; }
  path /@a { static /srv/at; }
  path /%C3%A4 { static /srv/ae; }
}";

/// Routing compares decoded segments: an escaped reserved character
/// (`%40` for `@`) or another hex case (`%c3%a4` for `%C3%A4`) reaches the
/// specific route, not the same name beneath `/`. The raw path is logged.
#[test]
fn escaped_spellings_reach_the_specific_route() {
    let mut sim = Sim::new(SPELLINGS);
    sim.add_file("/srv/at", "x.txt", b"at");
    sim.add_file("/srv/ae", "x.txt", b"ae");
    sim.add_file("/srv/www", "@a/x.txt", b"www-at");
    sim.add_file("/srv/www", "\u{e4}/x.txt", b"www-ae");
    for (path, body) in [
        ("/@a/x.txt", b"at"),
        ("/%40a/x.txt", b"at"),
        ("/%C3%A4/x.txt", b"ae"),
        ("/%c3%a4/x.txt", b"ae"),
        ("/%C3%a4/x.txt", b"ae"),
    ] {
        let (c, received) = exchange(&mut sim, &get(path));
        let mut expected = head("text/plain; charset=utf-8", 2, Persistence::KeepAlive);
        expected.extend_from_slice(body);
        assert_eq!(received, expected, "{path}\n{}", sim.trace());
        let logs: Vec<&Log> = sim.logs().iter().filter(|l| log_conn(l) == c).collect();
        assert_eq!(
            logs,
            [&response_log(c, Status::Ok, Method::Get, path)],
            "{path}\n{}",
            sim.trace()
        );
    }
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

/// An encoded `;` in the path is rejected like a raw one; in the query it
/// is fine.
#[test]
fn encoded_semicolon_in_the_path_is_400() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "a;b", b"semi");
    for target in ["/a%3Bb", "/a%3bb", "/assets/..%3B/x"] {
        let (c, received) = exchange(&mut sim, &get(target));
        assert_eq!(
            received,
            fixed(Status::BadRequest, Persistence::Close, BodyMode::Full),
            "{target}\n{}",
            sim.trace()
        );
        let logs: Vec<&Log> = sim.logs().iter().filter(|l| log_conn(l) == c).collect();
        assert_eq!(
            logs,
            [&Log::Rejected {
                conn: c,
                peer: peer(),
                reason: RejectReason::Http {
                    error: HttpError::BadTarget(TargetError::Semicolon),
                    request: None,
                },
            }],
            "{target}\n{}",
            sim.trace()
        );
    }
    assert_eq!(opens(sim.trace()), 0, "{}", sim.trace());
    sim.add_file("/srv/www", "q.txt", b"q");
    let (_, received) = exchange(&mut sim, &get("/q.txt?x=%3B"));
    let mut expected = head("text/plain; charset=utf-8", 1, Persistence::KeepAlive);
    expected.extend_from_slice(b"q");
    assert_eq!(received, expected, "{}", sim.trace());
}
