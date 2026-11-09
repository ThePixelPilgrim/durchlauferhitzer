//! Backend connection pooling end to end (FR-PROXY-02, FR-PROXY-26):
//! reuse across clients on one backend connection, idle hygiene, the
//! idle timeout and the safe retry, with the sim's contract checks on.

use super::*;
use durchlauferhitzer_core::DrainTarget;

/// One pooled route, one `pool off` route to the same backend.
const POOLED: &str = "listen 127.0.0.1:8080;
limits { backend_idle_timeout 3s; }
host example.test {
  path /api { proxy 127.0.0.1:3000; strip_prefix; max_request_body 1MiB; max_response_body 1MiB; }
  path /off { proxy 127.0.0.1:3000; pool off; max_request_body 1MiB; max_response_body 1MiB; }
}";

const GET: &[u8] = b"GET /api/x HTTP/1.1\r\nHost: example.test\r\n\r\n";
const FORWARDED: &[u8] = b"GET /x HTTP/1.1\r\nhost: example.test\r\n\
    x-forwarded-for: 192.0.2.1\r\nx-forwarded-proto: http\r\n\r\n";
const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
const RELAYED: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";

fn proxied(c: ConnId) -> Log {
    Log::ProxiedResponse {
        conn: c,
        peer: peer(),
        backend: BackendId(0),
        status: 200,
        request: summary(Method::Get, "/api/x"),
    }
}

/// Two clients, one after the other, over one backend connection: each
/// gets its own response, and the backend got two whole requests.
#[test]
fn two_clients_share_one_backend_connection() {
    let mut sim = Sim::new(POOLED);
    let a = request(&mut sim, GET);
    let up = only_upstream(&sim);
    assert_eq!(sim.take_backend_request(up), FORWARDED, "{}", sim.trace());
    sim.backend_send(up, OK);
    sim.run_until_idle();
    assert_eq!(sim.take_received(a), RELAYED, "{}", sim.trace());
    assert_eq!(
        sim.upstream_holder(up),
        Some(UpstreamHolder::Pool),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_upstreams(), 1);
    let b = request(&mut sim, GET);
    assert_eq!(sim.upstreams(), [up], "no new connect\n{}", sim.trace());
    assert_eq!(sim.take_backend_request(up), FORWARDED, "{}", sim.trace());
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nbye");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(b),
        b"HTTP/1.1 200 OK\r\ncontent-length: 3\r\n\r\nbye",
        "{}",
        sim.trace()
    );
    assert!(sim.take_received(a).is_empty());
    assert_eq!(sim.logs(), &[proxied(a), proxied(b)], "{}", sim.trace());
    // The idle timeout closes it, with no client left.
    sim.reset(a);
    sim.reset(b);
    sim.advance(Duration::from_secs(3));
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
}

/// Review Focus 1: a byte the backend sends while idle closes the
/// connection; the next request connects afresh and never sees the byte.
#[test]
fn a_byte_while_idle_closes_the_connection() {
    let mut sim = Sim::new(POOLED);
    let a = request(&mut sim, GET);
    let up = only_upstream(&sim);
    sim.backend_send(up, OK);
    sim.run_until_idle();
    sim.take_received(a);
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nevil");
    sim.run_until_idle();
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    sim.send(a, GET);
    sim.run_until_idle();
    assert_eq!(sim.upstreams(), [up, UpstreamId(2)], "{}", sim.trace());
    sim.backend_send(UpstreamId(2), OK);
    sim.run_until_idle();
    assert_eq!(sim.take_received(a), RELAYED, "{}", sim.trace());
}

/// FR-PROXY-26: the backend closed the pooled connection just as the
/// request went out on it: the GET is replayed once on a fresh
/// connection, logged, and the client never sees the failure.
#[test]
fn a_stale_connection_is_retried_for_a_get() {
    let mut sim = Sim::new(POOLED);
    let a = request(&mut sim, GET);
    let up = only_upstream(&sim);
    sim.backend_send(up, OK);
    sim.run_until_idle();
    sim.take_received(a);
    sim.send(a, GET);
    sim.run_until_idle();
    assert_eq!(
        sim.take_backend_request(up),
        [FORWARDED, FORWARDED].concat()
    );
    sim.backend_eof(up);
    sim.run_until_idle();
    let fresh = UpstreamId(2);
    assert_eq!(sim.upstreams(), [up, fresh], "{}", sim.trace());
    assert_eq!(
        sim.take_backend_request(fresh),
        FORWARDED,
        "{}",
        sim.trace()
    );
    sim.backend_send(fresh, OK);
    sim.run_until_idle();
    assert_eq!(sim.take_received(a), RELAYED, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[
            proxied(a),
            Log::BackendRetried {
                conn: a,
                peer: peer(),
                backend: BackendId(0),
                kind: BackendFailure::ClosedEarly,
                request: summary(Method::Get, "/api/x"),
            },
            proxied(a),
        ],
        "{}",
        sim.trace()
    );
}

/// A POST on a stale pooled connection is not replayed: 502.
#[test]
fn a_stale_connection_is_a_502_for_a_post() {
    let mut sim = Sim::new(POOLED);
    let a = request(&mut sim, GET);
    let up = only_upstream(&sim);
    sim.backend_send(up, OK);
    sim.run_until_idle();
    sim.take_received(a);
    sim.send(
        a,
        b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nContent-Length: 2\r\n\r\nhi",
    );
    sim.run_until_idle();
    sim.backend_reset(up);
    sim.run_until_idle();
    assert_eq!(sim.upstreams(), [up], "{}", sim.trace());
    assert!(
        sim.take_received(a)
            .starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"),
        "{}",
        sim.trace()
    );
    assert!(
        !sim.logs()
            .iter()
            .any(|l| matches!(l, Log::BackendRetried { .. })),
        "{}",
        sim.trace()
    );
}

/// `pool off` closes after every response, and takes no pooled
/// connection either.
#[test]
fn pool_off_closes_after_each_response() {
    let mut sim = Sim::new(POOLED);
    let a = request(&mut sim, GET);
    let up = only_upstream(&sim);
    sim.backend_send(up, OK);
    sim.run_until_idle();
    sim.take_received(a);
    sim.send(a, b"GET /off HTTP/1.1\r\nHost: example.test\r\n\r\n");
    sim.run_until_idle();
    let off = UpstreamId(2);
    assert_eq!(sim.upstreams(), [up, off], "{}", sim.trace());
    assert!(
        sim.take_backend_request(off)
            .ends_with(b"connection: close\r\n\r\n")
    );
    sim.backend_send(off, OK);
    sim.run_until_idle();
    assert_eq!(sim.take_received(a), RELAYED, "{}", sim.trace());
    assert_eq!(sim.upstream_holder(off), None, "{}", sim.trace());
    assert_eq!(sim.upstream_holder(up), Some(UpstreamHolder::Pool));
}

/// Owner decision 2026-11-07, the review's scenario: A's HEAD is answered
/// head-only and the backend sends a body anyway, late; B's GET must never
/// get it. No pooling after a response to HEAD.
#[test]
fn a_late_body_to_a_head_never_reaches_the_next_client() {
    let mut sim = Sim::new(POOLED);
    let a = request(
        &mut sim,
        b"HEAD /api/x HTTP/1.1\r\nHost: example.test\r\n\r\n",
    );
    let up = only_upstream(&sim);
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(sim.upstream_holder(up), None, "{}", sim.trace());
    let b = request(&mut sim, GET);
    let fresh = UpstreamId(2);
    assert_eq!(sim.upstreams(), [up, fresh], "{}", sim.trace());
    sim.backend_send(fresh, OK);
    sim.run_until_idle();
    assert_eq!(sim.take_received(b), RELAYED, "{}", sim.trace());
    let _ = a;
}

/// Owner decision 2026-11-07: stray bytes after a complete response that
/// the idle watch has not seen yet (`hold_idle_reads`) are caught by the
/// silence check before the reuse: B's request connects afresh and gets
/// its own response, the failed check logged.
#[test]
fn stray_bytes_fail_the_silence_check() {
    let mut sim = Sim::new(POOLED);
    sim.hold_idle_reads(true);
    let a = request(&mut sim, GET);
    let up = only_upstream(&sim);
    sim.backend_send(up, OK);
    sim.run_until_idle();
    sim.take_received(a);
    assert_eq!(sim.upstream_holder(up), Some(UpstreamHolder::Pool));
    sim.backend_send(up, b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nevil");
    let b = request(&mut sim, GET);
    let fresh = UpstreamId(2);
    assert_eq!(sim.upstreams(), [up, fresh], "{}", sim.trace());
    assert_eq!(sim.upstream_holder(up), None, "{}", sim.trace());
    assert!(
        sim.trace().contains("up=1 -> CheckSilent"),
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.take_backend_request(fresh),
        FORWARDED,
        "{}",
        sim.trace()
    );
    sim.backend_send(fresh, OK);
    sim.run_until_idle();
    assert_eq!(sim.take_received(b), RELAYED, "{}", sim.trace());
    // The failed check is logged with B's request (stage 7 review).
    let not_silent = Log::BackendNotSilent {
        conn: b,
        peer: peer(),
        backend: BackendId(0),
        request: summary(Method::Get, "/api/x"),
    };
    assert_eq!(
        sim.logs(),
        &[proxied(a), not_silent, proxied(b)],
        "{}",
        sim.trace()
    );
}

/// One idle connection to `up`'s backend, after a first exchange of `c`.
fn idle_after_one(sim: &mut Sim) -> (ConnId, UpstreamId) {
    let a = request(sim, GET);
    let up = only_upstream(sim);
    sim.backend_send(up, OK);
    sim.run_until_idle();
    assert_eq!(sim.take_received(a), RELAYED, "{}", sim.trace());
    (a, up)
}

/// FR-PROXY-26: the replay goes out on a fresh connection, which is never
/// retried: when it dies too, the client gets the 502.
#[test]
fn a_replay_that_fails_again_is_a_502() {
    let mut sim = Sim::new(POOLED);
    let (a, up) = idle_after_one(&mut sim);
    sim.send(a, GET);
    sim.run_until_idle();
    sim.backend_eof(up);
    sim.run_until_idle();
    let fresh = UpstreamId(2);
    assert_eq!(
        sim.take_backend_request(fresh),
        FORWARDED,
        "{}",
        sim.trace()
    );
    sim.backend_reset(fresh);
    sim.run_until_idle();
    assert_eq!(
        sim.upstreams(),
        [up, fresh],
        "no second retry\n{}",
        sim.trace()
    );
    assert!(
        sim.take_received(a)
            .starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"),
        "{}",
        sim.trace()
    );
    let tail: Vec<&Log> = sim.logs().iter().skip(1).collect();
    assert!(
        matches!(
            tail.as_slice(),
            [
                Log::BackendRetried {
                    kind: BackendFailure::ClosedEarly,
                    ..
                },
                Log::BackendFailed {
                    kind: BackendFailure::Reset,
                    ..
                },
                Log::Response {
                    status: Status::BadGateway,
                    ..
                },
            ]
        ),
        "{}",
        sim.trace()
    );
}

/// FR-PROXY-02: at most `backend_idle_max` idle per backend; a park
/// beyond it closes the oldest idle connection.
#[test]
fn the_idle_cap_closes_the_oldest() {
    let mut sim = Sim::new(&POOLED.replace(
        "limits { backend_idle_timeout 3s; }",
        "limits { backend_idle_timeout 3s; backend_idle_max 1; }",
    ));
    let a = request(&mut sim, GET);
    let b = request(&mut sim, GET);
    let (ua, ub) = (UpstreamId(1), UpstreamId(2));
    assert_eq!(sim.upstreams(), [ua, ub], "{}", sim.trace());
    sim.backend_send(ua, OK);
    sim.run_until_idle();
    sim.backend_send(ub, OK);
    sim.run_until_idle();
    assert_eq!(sim.take_received(a), RELAYED);
    assert_eq!(sim.take_received(b), RELAYED);
    assert_eq!(sim.upstream_holder(ua), None, "{}", sim.trace());
    assert_eq!(sim.upstream_holder(ub), Some(UpstreamHolder::Pool));
    assert_eq!(sim.idle_closes().get(&IdleClose::Room), Some(&1));
}

/// Review Focus 5: a drain closes the idle pool at once (both kinds).
#[test]
fn a_drain_closes_the_pool() {
    for target in [DrainTarget::Shutdown, DrainTarget::Restart] {
        let mut sim = Sim::new(POOLED);
        let (_, up) = idle_after_one(&mut sim);
        sim.begin_drain(target);
        assert_eq!(sim.upstream_holder(up), None, "{target:?}\n{}", sim.trace());
        assert_eq!(sim.idle_closes().get(&IdleClose::Drain), Some(&1));
    }
}

/// Nightly issue #3 (seed 48287): a complete, keep-alive, chunked response
/// whose backend resets right at its body end. The bytes that would have
/// followed the body are never sent, so nothing followed its end in the
/// read that completed it: the connection is pooled (FR-PROXY-02,
/// FR-PROXY-22 only closes on bytes that arrive).
const CHUNKED_201: &[u8] =
    b"HTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
const CHUNKED_201_RELAYED: &[u8] =
    b"HTTP/1.1 201 Created\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
const POST: &[u8] = b"POST /api/x HTTP/1.1\r\nHost: example.test\r\nContent-Length: 2\r\n\r\nhi";
const POST_FORWARDED: &[u8] = b"POST /x HTTP/1.1\r\nhost: example.test\r\n\
    content-length: 2\r\nx-forwarded-for: 192.0.2.1\r\nx-forwarded-proto: http\r\n\r\nhi";

/// A's first exchange, answered by `CHUNKED_201` and pooled.
fn pooled_after_chunked(sim: &mut Sim) -> (ConnId, UpstreamId) {
    let a = request(sim, GET);
    let up = only_upstream(sim);
    assert_eq!(sim.take_backend_request(up), FORWARDED, "{}", sim.trace());
    sim.backend_send(up, CHUNKED_201);
    sim.run_until_idle();
    assert_eq!(sim.take_received(a), CHUNKED_201_RELAYED, "{}", sim.trace());
    assert_eq!(
        sim.upstream_holder(up),
        Some(UpstreamHolder::Pool),
        "{}",
        sim.trace()
    );
    (a, up)
}

fn proxied_201(c: ConnId) -> Log {
    Log::ProxiedResponse {
        conn: c,
        peer: peer(),
        backend: BackendId(0),
        status: 201,
        request: summary(Method::Get, "/api/x"),
    }
}

/// Seed 48287's case: the reset arrives only after the next request (a
/// POST, never replayed) went out on the reused connection: the POST is
/// answered 502 (FR-PROXY-26), and the client never sees anything of the
/// first exchange's backend beyond its complete response.
#[test]
fn a_reset_at_the_body_end_seen_after_the_reuse_is_a_502_for_a_post() {
    let mut sim = Sim::new(POOLED);
    let (a, up) = pooled_after_chunked(&mut sim);
    sim.send(a, POST);
    sim.run_until_idle();
    assert_eq!(sim.upstreams(), [up], "reused\n{}", sim.trace());
    assert_eq!(
        sim.take_backend_request(up),
        POST_FORWARDED,
        "{}",
        sim.trace()
    );
    sim.backend_reset(up);
    sim.run_until_idle();
    assert_eq!(sim.upstreams(), [up], "no retry\n{}", sim.trace());
    assert_eq!(sim.open_upstreams(), 0, "{}", sim.trace());
    let received = sim.take_received(a);
    assert!(
        received.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"),
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.logs(),
        &[
            proxied_201(a),
            Log::BackendFailed {
                conn: a,
                peer: peer(),
                backend: BackendId(0),
                kind: BackendFailure::Reset,
            },
            Log::Response {
                conn: a,
                peer: peer(),
                status: Status::BadGateway,
                request: Some(summary(Method::Other("POST".to_owned()), "/api/x")),
            },
        ],
        "{}",
        sim.trace()
    );
}

/// The same reset seen before the reuse, by the silence check (the idle
/// watch held back): the POST connects afresh, nothing was sent on the
/// dead connection, and the failed check is logged.
#[test]
fn a_reset_at_the_body_end_seen_before_the_reuse_connects_afresh() {
    let mut sim = Sim::new(POOLED);
    sim.hold_idle_reads(true);
    let (a, up) = pooled_after_chunked(&mut sim);
    sim.backend_reset(up);
    sim.send(a, POST);
    sim.run_until_idle();
    let fresh = UpstreamId(2);
    assert_eq!(sim.upstreams(), [up, fresh], "{}", sim.trace());
    assert_eq!(
        sim.take_backend_request(fresh),
        POST_FORWARDED,
        "{}",
        sim.trace()
    );
    sim.backend_send(fresh, OK);
    sim.run_until_idle();
    assert_eq!(sim.take_received(a), RELAYED, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[
            proxied_201(a),
            Log::BackendNotSilent {
                conn: a,
                peer: peer(),
                backend: BackendId(0),
                request: summary(Method::Other("POST".to_owned()), "/api/x"),
            },
            Log::ProxiedResponse {
                conn: a,
                peer: peer(),
                backend: BackendId(0),
                status: 200,
                request: summary(Method::Other("POST".to_owned()), "/api/x"),
            },
        ],
        "{}",
        sim.trace()
    );
}
