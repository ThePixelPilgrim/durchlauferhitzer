//! FR-PROXY-32 Stripping and Hints: the file headers never pass between a
//! client and a backend, and an `internal_files` route tells the backend
//! which header it trusts (Rack's `X-Sendfile-Type`, `X-Accel-Mapping`).

use super::*;
use crate::config::model::Limits;
use crate::config::{FileHeader, InternalFiles, OnFileChange};
use crate::http::request::{HeadLimits, ParseStatus, parse_head};
use crate::http::response_head::{RequestKind, ResponseParser, ResponseStatus};
use std::net::Ipv4Addr;
use std::path::PathBuf;

fn limits() -> HeadLimits {
    HeadLimits::from(&Limits::default())
}

fn request(raw: &[u8]) -> RequestHead {
    match parse_head(raw, &limits()) {
        Ok(ParseStatus::Complete { head, .. }) => head,
        other => panic!("expected complete head, got {other:?}"),
    }
}

fn response(raw: &[u8]) -> ResponseHead {
    match ResponseParser::new().advance(raw, &limits(), RequestKind::Other) {
        Ok(ResponseStatus::Complete { head, .. }) => head,
        other => panic!("expected complete head, got {other:?}"),
    }
}

const PEER: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));

fn x_sendfile() -> InternalFiles {
    InternalFiles {
        header: FileHeader::XSendfile,
        root: PathBuf::from("/srv/files"),
        on_file_change: OnFileChange::Abort,
    }
}

fn x_accel_redirect() -> InternalFiles {
    InternalFiles {
        header: FileHeader::XAccelRedirect {
            prefix: "/protected/".to_owned(),
        },
        root: PathBuf::from("/srv/files"),
        on_file_change: OnFileChange::Abort,
    }
}

/// The three kinds of proxy route: plain, `x-sendfile`, `x-accel-redirect`.
fn routes() -> [Option<InternalFiles>; 3] {
    [None, Some(x_sendfile()), Some(x_accel_redirect())]
}

/// The backend request head, its length twin checked.
fn backend_head(head: &RequestHead, files: Option<&InternalFiles>) -> String {
    let mut out = Vec::new();
    render_backend_request(head, "/p", PEER, Persistence::Close, files, &mut out);
    assert_eq!(
        backend_request_len(head, "/p", PEER, Persistence::Close, files),
        out.len()
    );
    String::from_utf8(out).unwrap()
}

/// The client response head, its length twin checked.
fn client_head(head: &ResponseHead, files: Option<&InternalFiles>) -> String {
    let mut out = Vec::new();
    render_client_response(head, Persistence::KeepAlive, None, files, &mut out);
    assert_eq!(
        client_response_len(head, Persistence::KeepAlive, None, files),
        out.len()
    );
    String::from_utf8(out).unwrap()
}

/// The values of every header line named `name` (any case).
fn values<'a>(head: &'a str, name: &str) -> Vec<&'a str> {
    head.split("\r\n")
        .skip(1)
        .filter_map(|line| line.split_once(": "))
        .filter(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
        .collect()
}

const FORGED: &[u8] = b"GET /p HTTP/1.1\r\nHost: a\r\nX-Sendfile: /etc/passwd\r\n\
    x-accel-redirect: /x\r\nX-SENDFILE-TYPE: X-Sendfile\r\nX-Accel-Mapping: /=/x/\r\n\
    Connection: x-sendfile\r\n\r\n";

const PLAIN: &[u8] = b"GET /p HTTP/1.1\r\nHost: a\r\n\r\n";

/// A backend `404` with both file headers, two other `X-Accel-*` and an
/// `X-Sendfile-Type` (not a response file header: it passes).
const FILE_404: &[u8] = b"HTTP/1.1 404 Not Found\r\nX-Sendfile: /a\r\n\
    X-Accel-Redirect: /b\r\nX-Accel-Expires: 10\r\nX-Accel-Buffering: no\r\n\
    X-Sendfile-Type: X-Sendfile\r\nContent-Length: 0\r\n\r\n";

#[test]
fn forged_file_headers_never_reach_the_backend() {
    let head = request(FORGED);
    for files in routes() {
        let out = backend_head(&head, files.as_ref());
        assert!(values(&out, "x-sendfile").is_empty(), "{out}");
        assert!(values(&out, "x-accel-redirect").is_empty(), "{out}");
        let (sendfile_type, mapping): (&[&str], &[&str]) = match files.as_ref().map(|f| &f.header) {
            None => (&[], &[]),
            Some(FileHeader::XSendfile) => (&["X-Sendfile"], &[]),
            Some(FileHeader::XAccelRedirect { .. }) => {
                (&["X-Accel-Redirect"], &["/srv/files/=/protected/"])
            }
        };
        assert_eq!(values(&out, "x-sendfile-type"), sendfile_type, "{out}");
        assert_eq!(values(&out, "x-accel-mapping"), mapping, "{out}");
        assert!(
            !out.contains("/etc/passwd") && !out.contains("/=/x/"),
            "{out}"
        );
    }
}

#[test]
fn hints_on_an_x_sendfile_route() {
    let out = backend_head(&request(PLAIN), Some(&x_sendfile()));
    assert_eq!(out.matches("x-sendfile-type: X-Sendfile\r\n").count(), 1);
    assert!(!out.contains("x-accel-mapping"), "{out}");
    // After `x-forwarded-proto`, before `connection: close`.
    assert!(
        out.ends_with(
            "x-forwarded-proto: http\r\nx-sendfile-type: X-Sendfile\r\nconnection: close\r\n\r\n"
        ),
        "{out}"
    );
}

#[test]
fn hints_on_an_x_accel_redirect_route() {
    let out = backend_head(&request(PLAIN), Some(&x_accel_redirect()));
    assert!(
        out.ends_with(
            "x-forwarded-proto: http\r\nx-sendfile-type: X-Accel-Redirect\r\n\
             x-accel-mapping: /srv/files/=/protected/\r\nconnection: close\r\n\r\n"
        ),
        "{out}"
    );
    assert_eq!(out.matches("x-sendfile-type").count(), 1, "{out}");
    assert_eq!(out.matches("x-accel-mapping").count(), 1, "{out}");
}

/// A plain route sends no hint.
#[test]
fn no_hints_on_a_plain_route() {
    let out = backend_head(&request(PLAIN), None);
    assert_eq!(
        out,
        "GET /p HTTP/1.1\r\nhost: a\r\nx-forwarded-for: 192.0.2.1\r\n\
         x-forwarded-proto: http\r\nconnection: close\r\n\r\n"
    );
}

#[test]
fn file_headers_never_reach_the_client() {
    let head = response(FILE_404);
    let kept = "HTTP/1.1 404 Not Found\r\nx-accel-expires: 10\r\nx-accel-buffering: no\r\n\
                x-sendfile-type: X-Sendfile\r\ncontent-length: 0\r\n\r\n";
    assert_eq!(client_head(&head, None), kept);
    assert_eq!(client_head(&head, Some(&x_sendfile())), kept);
    // On an `x-accel-redirect` route every `X-Accel-*` goes.
    assert_eq!(
        client_head(&head, Some(&x_accel_redirect())),
        "HTTP/1.1 404 Not Found\r\nx-sendfile-type: X-Sendfile\r\n\
         content-length: 0\r\n\r\n"
    );
}

#[test]
fn len_twins_match_with_files() {
    let requests = [request(FORGED), request(PLAIN)];
    let responses = [
        response(FILE_404),
        response(
            b"HTTP/1.1 200 OK\r\nX-Accel-Redirect: /protected/a\r\n\
              X-Sendfile: /srv/files/a\r\n\r\n",
        ),
    ];
    for files in routes() {
        let files = files.as_ref();
        for head in &requests {
            for persistence in [Persistence::KeepAlive, Persistence::Close] {
                let mut out = Vec::new();
                render_backend_request(head, "/p", PEER, persistence, files, &mut out);
                assert_eq!(
                    backend_request_len(head, "/p", PEER, persistence, files),
                    out.len()
                );
            }
        }
        for head in &responses {
            for persistence in [Persistence::KeepAlive, Persistence::Close] {
                let mut out = Vec::new();
                render_client_response(head, persistence, None, files, &mut out);
                assert_eq!(
                    client_response_len(head, persistence, None, files),
                    out.len()
                );
            }
        }
    }
}

/// FR-PROXY-32: a backend-directed file keeps the backend's lines as the
/// relay would (hop-by-hop and file headers gone), without the framing
/// and validator headers the core replaces, and says which of
/// `content-type`, `x-content-type-options` and `date` it kept; the length
/// twin agrees.
#[test]
fn kept_lines_drop_framing_validators_and_file_headers() {
    let head = response(
        b"HTTP/1.1 200 OK\r\nConnection: x-hop\r\nX-Hop: 1\r\nKeep-Alive: 5\r\n\
          Content-Type: text/x\r\nContent-Length: 0\r\nETag: \"b\"\r\n\
          Last-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\nAccept-Ranges: none\r\n\
          Content-Range: bytes 0-0/1\r\nX-Accel-Redirect: /protected/a\r\nX-Accel-Expires: 9\r\n\
          X-Sendfile: /srv/files/a\r\nVary: Accept\r\n\r\n",
    );
    for (files, kept) in [
        (
            x_accel_redirect(),
            "content-type: text/x\r\nvary: Accept\r\n",
        ),
        (
            x_sendfile(),
            "content-type: text/x\r\nx-accel-expires: 9\r\nvary: Accept\r\n",
        ),
    ] {
        let mut out = b"x".to_vec();
        let has = render_kept_lines(&head, &files, &mut out);
        assert_eq!(
            String::from_utf8_lossy(&out),
            format!("x{kept}"),
            "{files:?}"
        );
        assert_eq!(kept_lines_len(&head, &files), out.len() - 1);
        assert_eq!(
            has,
            Kept {
                content_type: Sent::Yes,
                nosniff: Sent::No,
                date: Sent::No,
            }
        );
    }
    let head = response(
        b"HTTP/1.1 200 OK\r\nDate: Sun, 06 Nov 1994 08:49:37 GMT\r\n\
          X-Content-Type-Options: nosniff\r\nTransfer-Encoding: chunked\r\n\r\n",
    );
    let mut out = Vec::new();
    let has = render_kept_lines(&head, &x_sendfile(), &mut out);
    assert_eq!(
        String::from_utf8_lossy(&out),
        "date: Sun, 06 Nov 1994 08:49:37 GMT\r\nx-content-type-options: nosniff\r\n"
    );
    assert_eq!(
        has,
        Kept {
            content_type: Sent::No,
            nosniff: Sent::Yes,
            date: Sent::Yes,
        }
    );
}

/// The hints a route adds after `x-forwarded-proto`, by route kind.
fn routes_with_hints() -> [(Option<InternalFiles>, &'static str); 3] {
    [
        (None, ""),
        (Some(x_sendfile()), "x-sendfile-type: X-Sendfile\r\n"),
        (
            Some(x_accel_redirect()),
            "x-sendfile-type: X-Accel-Redirect\r\nx-accel-mapping: /srv/files/=/protected/\r\n",
        ),
    ]
}

/// Owner decision 2026-11-09 (FR-PROXY-32 Stripping): a client header
/// whose name, with `_` read as `-`, is a file header never reaches the
/// backend (CGI servers such as Rack map `X_Accel_Mapping` and
/// `X-Accel-Mapping` to the same key), on every kind of route. Names that
/// differ by more than the spelling pass.
#[test]
fn underscore_spelled_file_headers_never_reach_the_backend() {
    let head = request(
        b"GET /p HTTP/1.1\r\nHost: a\r\nX_Sendfile: /etc/passwd\r\n\
          X_Accel_Redirect: /x\r\nX_Sendfile_Type: X-Accel-Redirect\r\n\
          X_Accel_Mapping: /evil=/x/\r\nx-accel_mapping: /evil=/x/\r\n\
          X_Sendfile_Types: kept\r\nX__Sendfile: kept\r\nX_Sendfile_: kept\r\n\r\n",
    );
    let kept = "GET /p HTTP/1.1\r\nhost: a\r\nx_sendfile_types: kept\r\nx__sendfile: kept\r\n\
                x_sendfile_: kept\r\nx-forwarded-for: 192.0.2.1\r\nx-forwarded-proto: http\r\n";
    for (files, hints) in routes_with_hints() {
        assert_eq!(
            backend_head(&head, files.as_ref()),
            format!("{kept}{hints}connection: close\r\n\r\n")
        );
    }
}

/// Owner decision 2026-11-09 (FR-PROXY-06): the same holds for the client
/// identity headers and for `X-Forwarded-For`, whose only spelling the
/// proxy merges is the hyphenated one, on every kind of route.
#[test]
fn underscore_spelled_identity_headers_never_reach_the_backend() {
    let head = request(
        b"GET /p HTTP/1.1\r\nHost: a\r\nX_Forwarded_For: 6.6.6.6\r\nX_Real_IP: 6.6.6.6\r\n\
          X_Forwarded_Proto: https\r\nX_Forwarded_Host: evil\r\nX_Forwarded_Port: 1\r\n\
          x-forwarded_host: evil\r\nForwarded: for=6.6.6.6\r\nX-Forwarded-For: 1.2.3.4\r\n\
          X_Real_IPs: kept\r\n\r\n",
    );
    let kept = "GET /p HTTP/1.1\r\nhost: a\r\nx_real_ips: kept\r\n\
                x-forwarded-for: 1.2.3.4, 192.0.2.1\r\nx-forwarded-proto: http\r\n";
    for (files, hints) in routes_with_hints() {
        assert_eq!(
            backend_head(&head, files.as_ref()),
            format!("{kept}{hints}connection: close\r\n\r\n")
        );
    }
}
