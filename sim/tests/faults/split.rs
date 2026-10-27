//! Deterministic schedules: every split point, byte-by-byte delivery,
//! pipelining and connection close.

use super::*;

#[test]
fn every_split_point_gives_identical_responses() {
    let long_line = [b"GET /".as_slice(), &[b'a'; 80], b" HTTP/1.1\r\n\r\n"].concat();
    let endless_head = [
        b"GET / HTTP/1.1\r\nHost: other.test\r\nX-Pad: ".as_slice(),
        &[b'a'; 100],
    ]
    .concat();
    let cases: [(&str, &[u8], Status, Persistence); 4] = [
        (CONFIG, VALID, Status::NotFound, Persistence::KeepAlive),
        (CONFIG, INVALID, Status::BadRequest, Persistence::Close),
        (
            SMALL_HEAD,
            &long_line,
            Status::UriTooLong,
            Persistence::Close,
        ),
        (
            SMALL_HEAD,
            &endless_head,
            Status::RequestHeaderFieldsTooLarge,
            Persistence::Close,
        ),
    ];
    for (config, request, status, persistence) in cases {
        let (whole, trace) = deliver(config, &[request]);
        assert_eq!(
            whole.received,
            fixed(status, persistence, BodyMode::Full),
            "{trace}"
        );
        assert_eq!(whole.logs.len(), 1, "{trace}");
        for split in 1..request.len() {
            let (first, second) = request.split_at(split);
            let (outcome, trace) = deliver(config, &[first, second]);
            assert_eq!(outcome, whole, "split at {split}\n{trace}");
        }
    }
}

#[test]
fn byte_by_byte_delivery() {
    let mut sim = dated_sim(CONFIG);
    let c = sim.connect(peer());
    sim.set_read_chunk(c, 1);
    sim.send(c, b"GET /a HTTP/1.1\r\nHost: other.test\r\n\r\n");
    sim.send(c, b"GET /b HTTP/1.1\r\nHost: other.test\r\n\r\n");
    sim.run_until_idle();
    let mut expected = not_found_keep_alive();
    expected.extend(not_found_keep_alive());
    assert_eq!(sim.take_received(c), expected, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[
            response_log(c, Some("other.test"), "/a"),
            response_log(c, Some("other.test"), "/b"),
        ],
        "{}",
        sim.trace()
    );
}

#[test]
fn pipelined_requests_in_one_read() {
    let a: &[u8] = b"GET /a HTTP/1.1\r\nHost: other.test\r\n\r\n";
    let b: &[u8] = b"HEAD /b HTTP/1.1\r\nHost: other.test\r\n\r\n";
    let (sequential, _) = deliver(CONFIG, &[a, b]);
    let both = [a, b].concat();
    let (pipelined, trace) = deliver(CONFIG, &[&both]);
    assert_eq!(pipelined, sequential, "{trace}");
    assert_eq!(pipelined.logs.len(), 2, "{trace}");
    assert_eq!(pipelined.state, SimConnState::Open, "{trace}");
}

#[test]
fn pipelined_bytes_beyond_head_limit() {
    let mut sim = dated_sim(SMALL_HEAD);
    let c = sim.connect(peer());
    let mut bytes = VALID.to_vec();
    bytes.extend_from_slice(b"GET /y HTTP/1.1\r\nHost: other.test\r\nX-Pad: ");
    bytes.extend_from_slice(&[b'a'; 200]);
    sim.send(c, &bytes);
    sim.run_until_idle();
    let mut expected = not_found_keep_alive();
    expected.extend(fixed(
        Status::RequestHeaderFieldsTooLarge,
        Persistence::Close,
        BodyMode::Full,
    ));
    assert_eq!(sim.take_received(c), expected, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[
            response_log(c, Some("other.test"), "/x"),
            Log::Rejected {
                conn: c,
                peer: peer(),
                reason: RejectReason::Http(HttpError::HeadTooLarge),
            },
        ],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
}

#[test]
fn no_answer_after_connection_close_pipelined() {
    let mut sim = dated_sim(CONFIG);
    let c = sim.connect(peer());
    sim.send(
        c,
        b"GET /x HTTP/1.1\r\nHost: other.test\r\nConnection: close\r\n\r\n\
          GET /y HTTP/1.1\r\nHost: other.test\r\n\r\n",
    );
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        fixed(Status::NotFound, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.logs(),
        &[response_log(c, Some("other.test"), "/x")],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    sim.send_eof(c);
    sim.run_until_idle();
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    assert_eq!(sim.logs().len(), 1, "{}", sim.trace());
}

#[test]
fn partial_pipelined_leftover_completed_later() {
    let mut sim = dated_sim(CONFIG);
    let c = sim.connect(peer());
    let mut bytes = VALID.to_vec();
    bytes.extend_from_slice(b"GET /y HTTP/1.1\r\nHo");
    sim.send(c, &bytes);
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        not_found_keep_alive(),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.logs().len(), 1, "{}", sim.trace());
    sim.send(c, b"st: other.test\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        not_found_keep_alive(),
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.logs(),
        &[
            response_log(c, Some("other.test"), "/x"),
            response_log(c, Some("other.test"), "/y"),
        ],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}

#[test]
fn http10_request_closes_after_response() {
    let mut sim = dated_sim(CONFIG);
    let c = sim.connect(peer());
    sim.send(c, b"GET /x HTTP/1.0\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        fixed(Status::NotFound, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.logs(),
        &[response_log(c, None, "/x")],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
}
