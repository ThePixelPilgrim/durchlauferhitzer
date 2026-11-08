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
        response(b"HTTP/1.1 200 OK\r\nX-Accel-Redirect: /protected/a\r\nX-Sendfile: /srv/files/a\r\n\r\n"),
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
