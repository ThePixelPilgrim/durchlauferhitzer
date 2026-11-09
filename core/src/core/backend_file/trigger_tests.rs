//! The edges of the trigger (FR-PROXY-32 Trigger, Bad path; the Task 5
//! and Task 6 implementer calls): `Content-Length: 00` is `0`, the
//! route's header named in `Connection` still counts, obs-text is the
//! parser's `BadResponse`, and only a `200` triggers. Exact bytes and
//! logs; the shared fixtures are in `tests.rs`.

use super::tests::*;
use crate::core::Action;
use crate::core::test_support::PEER;
use crate::http::request::Method;
use crate::http::response_head::ResponseError;
use crate::http::status::Status;
use crate::log::{BackendFailure, Log};
use crate::upstream::{BackendId, UpstreamHolder};

/// The merged head of `a.pdf` (1000 bytes) without backend lines.
fn a_pdf_head() -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nContent-Length: 1000\r\n\
         {VALIDATORS}Accept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\n\r\n"
    )
}

/// `backend` answers `GET`: every action, the file opened as `a.pdf`
/// when the core asks for it.
fn answered(core: &mut crate::core::Core, backend: &[u8]) -> Vec<Action> {
    let mut all = forwarded(core, GET);
    let actions = answer(core, backend);
    let open = opens(&actions);
    all.extend(actions);
    if open {
        all.extend(opened(core, regular(1000)));
    }
    all
}

fn backend_failed(kind: BackendFailure) -> Log {
    Log::BackendFailed {
        conn: CONN,
        peer: PEER,
        backend: BackendId(0),
        kind,
    }
}

/// Task 6 call: `Content-Length: 00` is the value 0 (RFC 9110 section
/// 8.6), so the upstream is pooled as after `Content-Length: 0`.
#[test]
fn content_length_00_parks_the_upstream() {
    let mut core = core();
    let actions = answered(
        &mut core,
        b"HTTP/1.1 200 OK\r\nX-Accel-Redirect: /protected/a.pdf\r\nContent-Length: 00\r\n\r\n",
    );
    assert!(!closes_upstream(&actions), "{actions:?}");
    assert_eq!(core.upstream_holder(UP), Some(UpstreamHolder::Pool));
    assert_eq!(text(core.send_buffer(CONN)), a_pdf_head());
    assert_eq!(
        logs(&actions),
        [backend_file(Status::Ok, Method::Get, "/dl/x")]
    );
}

/// Task 5 call: the route's header named in `Connection` is still the
/// trigger (one line serves the file, `Connection` dropped as
/// hop-by-hop), and still counted (two lines are a bad path).
#[test]
fn route_header_named_in_connection_still_counts() {
    let mut core = core();
    let actions = answered(
        &mut core,
        b"HTTP/1.1 200 OK\r\nX-Accel-Redirect: /protected/a.pdf\r\n\
          Connection: X-Accel-Redirect\r\nContent-Length: 0\r\n\r\n",
    );
    assert_eq!(text(core.send_buffer(CONN)), a_pdf_head(), "{actions:?}");
    assert_eq!(
        logs(&actions),
        [backend_file(Status::Ok, Method::Get, "/dl/x")]
    );

    let mut core = self::core();
    let actions = answered(
        &mut core,
        b"HTTP/1.1 200 OK\r\nX-Accel-Redirect: /protected/a.pdf\r\n\
          Connection: X-Accel-Redirect\r\nX-Accel-Redirect: /protected/a.pdf\r\n\
          Content-Length: 0\r\n\r\n",
    );
    assert_eq!(text(core.send_buffer(CONN)), BAD_GATEWAY);
    assert_eq!(
        logs(&actions),
        [
            backend_failed(BackendFailure::BadFilePath),
            response(Status::BadGateway, Method::Get, "/dl/x"),
        ]
    );
    assert!(closes_upstream(&actions), "{actions:?}");
    assert!(!opens(&actions), "{actions:?}");
}

/// Task 6 call: obs-text in the route header's value is refused by the
/// response head parser before any path rule: `BadResponse`, not
/// `BadFilePath`.
#[test]
fn obs_text_in_the_route_header_is_a_bad_response() {
    let mut core = core();
    let actions = answered(
        &mut core,
        b"HTTP/1.1 200 OK\r\nX-Accel-Redirect: /protected/a\x80.pdf\r\nContent-Length: 0\r\n\r\n",
    );
    assert_eq!(text(core.send_buffer(CONN)), BAD_GATEWAY);
    assert_eq!(
        logs(&actions),
        [
            backend_failed(BackendFailure::BadResponse(ResponseError::BadHeaderValue)),
            response(Status::BadGateway, Method::Get, "/dl/x"),
        ]
    );
    assert!(closes_upstream(&actions), "{actions:?}");
    assert!(!opens(&actions), "{actions:?}");
}

/// Trigger: only a `200` names a file. A `201`, `204`, `206` or `304`
/// carrying the route's header is relayed without it (and without
/// `X-Sendfile`), logged as a proxied response; nothing is opened.
#[test]
fn only_a_200_triggers() {
    let files = "X-Accel-Redirect: /protected/a.pdf\r\nX-Sendfile: /srv/files/a.pdf\r\n";
    for (status, line, framing) in [
        (201, "201 Created", "Content-Length: 0\r\n"),
        (204, "204 No Content", ""),
        (206, "206 Partial Content", "Content-Length: 0\r\n"),
        (304, "304 Not Modified", ""),
    ] {
        let mut core = core();
        let backend = format!("HTTP/1.1 {line}\r\n{files}X-Other: 1\r\n{framing}\r\n");
        let actions = answered(&mut core, backend.as_bytes());
        assert_eq!(
            text(core.send_buffer(CONN)),
            format!(
                "HTTP/1.1 {line}\r\nx-other: 1\r\n{}\r\n",
                framing.to_ascii_lowercase()
            ),
            "{status}"
        );
        assert!(!opens(&actions), "{status}: {actions:?}");
        assert_eq!(
            logs(&actions),
            [Log::ProxiedResponse {
                conn: CONN,
                peer: PEER,
                backend: BackendId(0),
                status,
                request: summary(Method::Get, "/dl/x"),
            }],
            "{status}"
        );
    }
}
