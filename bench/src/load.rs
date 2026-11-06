//! The load: `concurrency` client threads send one scenario's request
//! for a fixed time, each waiting for the whole response before the next.
//! A request counts only when its response is complete and correct (the
//! expected status and body length); anything else (a refused or reset
//! connection, a timeout, another status, a short or overlong body) is an
//! error, and the connection is replaced.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

/// Whether each request gets a new connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reuse {
    /// One connection per client thread, reused.
    KeepAlive,
    /// A new connection per request (`Connection: close`).
    Fresh,
}

/// What a correct response looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expect {
    pub status: u16,
    pub body_len: u64,
}

/// One scenario's traffic.
#[derive(Debug, Clone)]
pub struct Target {
    pub addr: SocketAddr,
    /// The whole request (head only; no body).
    pub request: Vec<u8>,
    pub reuse: Reuse,
    pub expect: Expect,
}

/// The outcome of a load window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tally {
    /// Complete, correct responses.
    pub requests: u64,
    /// Everything else.
    pub errors: u64,
    /// The first error's description.
    pub first_error: Option<String>,
}

/// `GET <path>` for host `host`, with `extra` header lines (each ending
/// in CRLF), closing the connection for [`Reuse::Fresh`].
pub fn request(host: &str, path: &str, extra: &str, reuse: Reuse) -> Vec<u8> {
    let close = match reuse {
        Reuse::KeepAlive => "",
        Reuse::Fresh => "Connection: close\r\n",
    };
    format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{extra}{close}\r\n").into_bytes()
}

/// How long one read or write may block.
const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest response head taken.
const MAX_HEAD: usize = 64 * 1024;
/// The pause after an error, so a failing target is not hammered.
const ERROR_PAUSE: Duration = Duration::from_millis(1);

/// Drives `target` from `concurrency` threads for `duration`; every
/// thread finishes its request under way and closes its connection
/// before this returns.
pub fn drive(target: &Target, concurrency: usize, duration: Duration) -> Tally {
    let end = Instant::now() + duration;
    let tallies: Vec<Tally> = thread::scope(|scope| {
        let workers: Vec<_> = (0..concurrency)
            .map(|_| scope.spawn(|| client(target, end)))
            .collect();
        workers
            .into_iter()
            .map(|w| {
                w.join().unwrap_or_else(|_| Tally {
                    requests: 0,
                    errors: 1,
                    first_error: Some("a client thread panicked".to_string()),
                })
            })
            .collect()
    });
    let mut total = Tally::default();
    for t in tallies {
        total.requests += t.requests;
        total.errors += t.errors;
        if total.first_error.is_none() {
            total.first_error = t.first_error;
        }
    }
    total
}

/// One client thread: requests until `end`.
fn client(target: &Target, end: Instant) -> Tally {
    let mut tally = Tally::default();
    let mut conn: Option<Conn> = None;
    let mut scratch = vec![0u8; 64 * 1024];
    while Instant::now() < end {
        let result = match conn.take() {
            Some(c) => Ok(c),
            None => Conn::open(target.addr),
        }
        .and_then(|mut c| {
            c.stream
                .write_all(&target.request)
                .map_err(|e| format!("write: {e}"))?;
            let head = c.response(&mut scratch)?;
            check(&head, target.expect)?;
            Ok((c, head.close))
        });
        match result {
            Ok((c, close)) => {
                tally.requests += 1;
                if target.reuse == Reuse::KeepAlive && !close {
                    conn = Some(c);
                }
            }
            Err(e) => {
                tally.errors += 1;
                if tally.first_error.is_none() {
                    tally.first_error = Some(e);
                }
                thread::sleep(ERROR_PAUSE);
            }
        }
    }
    tally
}

fn check(head: &Head, expect: Expect) -> Result<(), String> {
    if head.status != expect.status {
        return Err(format!(
            "status {} (expected {})",
            head.status, expect.status
        ));
    }
    let body = if head.status == 304 {
        0
    } else {
        expect.body_len
    };
    if head.content_length != Some(body) {
        return Err(format!(
            "content-length {:?} (expected {body})",
            head.content_length
        ));
    }
    Ok(())
}

/// A parsed response head.
struct Head {
    status: u16,
    content_length: Option<u64>,
    close: bool,
    headers: Vec<(String, String)>,
}

/// A client connection with the bytes read past the last response.
struct Conn {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl Conn {
    fn open(addr: SocketAddr) -> Result<Conn, String> {
        let stream = TcpStream::connect(addr).map_err(|e| format!("connect: {e}"))?;
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(IO_TIMEOUT)))
            .and_then(|()| stream.set_nodelay(true))
            .map_err(|e| format!("socket options: {e}"))?;
        Ok(Conn {
            stream,
            buf: Vec::new(),
        })
    }

    fn fill(&mut self, scratch: &mut [u8]) -> Result<usize, String> {
        match self.stream.read(scratch) {
            Ok(0) => Err("the connection closed early".to_string()),
            Ok(n) => {
                self.buf
                    .extend_from_slice(scratch.get(..n).unwrap_or_default());
                Ok(n)
            }
            Err(e) => Err(format!("read: {e}")),
        }
    }

    /// Reads one whole response; its body is checked for length and
    /// dropped. No byte may follow it (requests are not pipelined).
    fn response(&mut self, scratch: &mut [u8]) -> Result<Head, String> {
        let end = loop {
            if let Some(end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break end + 4;
            }
            if self.buf.len() > MAX_HEAD {
                return Err("response head too long".to_string());
            }
            self.fill(scratch)?;
        };
        let head = parse_head(self.buf.get(..end).unwrap_or_default())?;
        let body = match head.status {
            100..=199 | 204 | 304 => 0,
            _ => head
                .content_length
                .ok_or_else(|| "no content-length".to_string())?,
        };
        let have = (self.buf.len() - end) as u64;
        if have > body {
            return Err("bytes after the response".to_string());
        }
        self.buf.clear();
        let mut left = body - have;
        while left > 0 {
            let want = scratch
                .len()
                .min(usize::try_from(left).unwrap_or(usize::MAX));
            let n = match self
                .stream
                .read(scratch.get_mut(..want).unwrap_or_default())
            {
                Ok(0) => return Err("the body ended early".to_string()),
                Ok(n) => n,
                Err(e) => return Err(format!("read body: {e}")),
            };
            left -= n as u64;
        }
        Ok(head)
    }
}

fn parse_head(bytes: &[u8]) -> Result<Head, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "a non-UTF-8 head".to_string())?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .strip_prefix("HTTP/1.1 ")
        .or_else(|| status_line.strip_prefix("HTTP/1.0 "))
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| format!("bad status line {status_line:?}"))?;
    let mut head = Head {
        status,
        content_length: None,
        close: false,
        headers: Vec::new(),
    };
    for line in lines.filter(|l| !l.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| format!("bad header line {line:?}"))?;
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        match name.as_str() {
            "content-length" => {
                head.content_length = Some(
                    value
                        .parse()
                        .map_err(|_| format!("content-length {value:?}"))?,
                );
            }
            "connection" => head.close |= value.eq_ignore_ascii_case("close"),
            _ => {}
        }
        head.headers.push((name, value));
    }
    Ok(head)
}

/// One request on a new connection: the value of response header `name`
/// (case-insensitive), if the response had it.
pub fn header_of(addr: SocketAddr, request: &[u8], name: &str) -> Option<String> {
    let mut conn = Conn::open(addr).ok()?;
    conn.stream.write_all(request).ok()?;
    let mut scratch = vec![0u8; 64 * 1024];
    let head = conn.response(&mut scratch).ok()?;
    let name = name.to_ascii_lowercase();
    head.headers
        .into_iter()
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A server answering every request head on a connection with
    /// `response`, keeping the connection open.
    fn fixed_server(response: &'static [u8]) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                thread::spawn(move || answer(stream, response));
            }
        });
        addr
    }

    fn answer(mut stream: TcpStream, response: &[u8]) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                buf.drain(..end + 4);
                if stream.write_all(response).is_err() {
                    return;
                }
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    fn target(addr: SocketAddr, reuse: Reuse) -> Target {
        Target {
            addr,
            request: request("bench.test", "/", "", reuse),
            reuse,
            expect: Expect {
                status: 200,
                body_len: 5,
            },
        }
    }

    const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nETag: \"1-2\"\r\n\r\nhello";
    const UNAVAILABLE: &[u8] =
        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 5\r\n\r\nbusy!";

    #[test]
    fn the_request_line_and_headers() {
        let r = request(
            "bench.test",
            "/small",
            "If-None-Match: \"x\"\r\n",
            Reuse::Fresh,
        );
        assert_eq!(
            r,
            b"GET /small HTTP/1.1\r\nHost: bench.test\r\nIf-None-Match: \"x\"\r\nConnection: close\r\n\r\n"
        );
        let r = request("bench.test", "/", "", Reuse::KeepAlive);
        assert_eq!(r, b"GET / HTTP/1.1\r\nHost: bench.test\r\n\r\n");
    }

    #[test]
    fn correct_responses_count_as_requests() {
        let addr = fixed_server(OK);
        for reuse in [Reuse::KeepAlive, Reuse::Fresh] {
            let t = drive(&target(addr, reuse), 2, Duration::from_millis(200));
            assert!(t.requests > 10, "{reuse:?}: {t:?}");
            assert_eq!(t.errors, 0, "{reuse:?}: {t:?}");
        }
    }

    #[test]
    fn a_server_answering_503_counts_errors_not_requests() {
        let addr = fixed_server(UNAVAILABLE);
        let t = drive(
            &target(addr, Reuse::KeepAlive),
            2,
            Duration::from_millis(200),
        );
        assert_eq!(t.requests, 0, "{t:?}");
        assert!(t.errors > 0, "{t:?}");
        assert!(t.first_error.unwrap().contains("503"));
    }

    #[test]
    fn a_wrong_body_length_is_an_error() {
        let addr = fixed_server(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nhell");
        let t = drive(
            &target(addr, Reuse::KeepAlive),
            1,
            Duration::from_millis(100),
        );
        assert_eq!(t.requests, 0, "{t:?}");
        assert!(t.errors > 0, "{t:?}");
    }

    #[test]
    fn a_refused_connection_is_an_error() {
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let t = drive(&target(addr, Reuse::Fresh), 2, Duration::from_millis(100));
        assert_eq!(t.requests, 0, "{t:?}");
        assert!(t.errors > 0, "{t:?}");
    }

    #[test]
    fn a_header_value() {
        let addr = fixed_server(OK);
        let req = request("bench.test", "/", "", Reuse::Fresh);
        assert_eq!(header_of(addr, &req, "etag").as_deref(), Some("\"1-2\""));
        assert_eq!(header_of(addr, &req, "last-modified"), None);
    }
}
