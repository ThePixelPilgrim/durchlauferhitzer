//! The `Date` header end to end (FR-HTTP-22a, FR-PROXY-12): every final
//! response the core renders carries the time the driver set, right after
//! its status line; the interim `100 Continue` does not; a relayed
//! response keeps the backend's `Date` and gets the core's only when it
//! has none. The expected bytes come from the spec-written model.

use std::net::{IpAddr, Ipv4Addr};

use durchlauferhitzer_core::http::request::Persistence;
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_sim::Sim;

#[allow(dead_code, reason = "shared by all test binaries; each uses part")]
#[path = "support/model_http.rs"]
mod model_http;

use model_http::{RFC_DATE, model_file_head, model_fixed, model_redirect};

const CONFIG: &str = "listen 127.0.0.1:8080;
host example.test {
  path / { static /srv/www; }
  path /api { proxy 127.0.0.1:3000; strip_prefix; max_request_body 1MiB; max_response_body 1MiB; }
}";

fn peer() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))
}

fn dated() -> Sim {
    let mut sim = Sim::new(CONFIG);
    sim.set_date(RFC_DATE.0);
    sim.add_file("/srv/www", "f.txt", b"abc");
    sim.add_dir("/srv/www", "d");
    sim
}

/// The request `bytes` on a fresh connection; what the client received.
fn exchange(sim: &mut Sim, bytes: &[u8]) -> Vec<u8> {
    let c = sim.connect(peer());
    sim.send(c, bytes);
    sim.run_until_idle();
    sim.take_received(c)
}

const DATE: Option<&str> = Some(RFC_DATE.1);

#[test]
fn file_heads_redirects_and_fixed_responses_carry_date() {
    let mut sim = dated();
    let get = b"GET /f.txt HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n";
    let mut expected = model_file_head("text/plain; charset=utf-8", 3, Persistence::Close, DATE);
    expected.extend_from_slice(b"abc");
    assert_eq!(exchange(&mut sim, get), expected, "{}", sim.trace());

    let head = b"HEAD /d HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n";
    let expected = model_redirect("/d/", Persistence::Close, BodyMode::HeadOnly, DATE);
    assert_eq!(exchange(&mut sim, head), expected, "{}", sim.trace());

    let missing = b"GET /nope HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n";
    let expected = model_fixed(Status::NotFound, Persistence::Close, BodyMode::Full, DATE);
    assert_eq!(exchange(&mut sim, missing), expected, "{}", sim.trace());

    // A parse error, before any request was accepted.
    let bad = b"GET / HTTP/1.1\nHost: example.test\n\n";
    let expected = model_fixed(Status::BadRequest, Persistence::Close, BodyMode::Full, DATE);
    assert_eq!(exchange(&mut sim, bad), expected, "{}", sim.trace());
}

/// The 503 of a refused connection is the core's own response too.
#[test]
fn a_refused_connection_gets_a_dated_503() {
    let mut sim = Sim::new(
        "listen 127.0.0.1:8080; limits { max_connections 1; } \
         host example.test { path / { static /srv/www; } }",
    );
    sim.set_date(RFC_DATE.0);
    let first = sim.connect(peer());
    let refused = sim.connect(peer());
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(refused),
        model_fixed(
            Status::ServiceUnavailable,
            Persistence::Close,
            BodyMode::Full,
            DATE
        ),
        "{}",
        sim.trace()
    );
    assert!(sim.take_received(first).is_empty());
}

/// The date the driver set last is the one sent.
#[test]
fn a_later_date_replaces_the_earlier_one() {
    let mut sim = dated();
    sim.set_date(0);
    let missing = b"GET /nope HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n";
    let expected = model_fixed(
        Status::NotFound,
        Persistence::Close,
        BodyMode::Full,
        Some("Thu, 01 Jan 1970 00:00:00 GMT"),
    );
    assert_eq!(exchange(&mut sim, missing), expected, "{}", sim.trace());
}

#[test]
fn interim_100_continue_has_no_date_and_the_relayed_response_gets_ours() {
    let mut sim = dated();
    let c = sim.connect(peer());
    sim.send(
        c,
        b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nExpect: 100-continue\r\n\
          Content-Length: 2\r\n\r\n",
    );
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 100 Continue\r\n\r\n",
        "{}",
        sim.trace()
    );
    sim.send(c, b"hi");
    sim.run_until_idle();
    let up = *sim.upstreams().first().unwrap();
    sim.backend_send(up, b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        format!(
            "HTTP/1.1 201 Created\r\ncontent-length: 0\r\ndate: {}\r\n\r\n",
            RFC_DATE.1
        )
        .into_bytes(),
        "{}",
        sim.trace()
    );
}

#[test]
fn the_backends_date_passes_through_byte_exact() {
    let mut sim = dated();
    let c = sim.connect(peer());
    sim.send(c, b"GET /api/x HTTP/1.1\r\nHost: example.test\r\n\r\n");
    sim.run_until_idle();
    let up = *sim.upstreams().first().unwrap();
    sim.backend_send(
        up,
        b"HTTP/1.1 200 OK\r\nDate:   Tue, 27 Oct 2026 10:00:00 GMT\r\nContent-Length: 2\r\n\r\nok",
    );
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        b"HTTP/1.1 200 OK\r\ndate: Tue, 27 Oct 2026 10:00:00 GMT\r\ncontent-length: 2\r\n\r\nok",
        "{}",
        sim.trace()
    );
}

/// Without `set_date` (the sim's default) no response carries `Date`.
#[test]
fn no_date_before_the_driver_sets_one() {
    let mut sim = Sim::new(CONFIG);
    let missing = b"GET /nope HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n";
    let expected = model_fixed(Status::NotFound, Persistence::Close, BodyMode::Full, None);
    assert_eq!(exchange(&mut sim, missing), expected, "{}", sim.trace());
}
