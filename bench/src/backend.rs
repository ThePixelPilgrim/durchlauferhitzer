//! The stub backend of the proxy scenarios: in this process (its CPU time
//! is never the server's), std only, a thread per connection. Every
//! request head gets a 200 response whose body size follows the path
//! ([`body_len`]: `/medium` [`MEDIUM`], `/large` [`LARGE`], else
//! [`SMALL`]); the connection stays open unless the request asked to
//! close it, so a pooling server can reuse it.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;

use crate::scenario::{LARGE, MEDIUM, SMALL};

/// The listening backend; its threads live as long as the process.
pub struct Backend {
    pub addr: SocketAddr,
}

/// The body size the backend sends for a request path (after the
/// server's `strip_prefix`).
pub fn body_len(path: &str) -> u64 {
    match path {
        "/medium" => MEDIUM,
        "/large" => LARGE,
        _ => SMALL,
    }
}

/// The whole response with a `len`-byte body, built once.
fn response(len: u64) -> &'static [u8] {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {len}\r\n\r\n"
    )
    .into_bytes();
    response.resize(response.len() + len as usize, b'b');
    response.leak()
}

/// The responses by body size.
#[derive(Clone, Copy)]
struct Responses {
    small: &'static [u8],
    medium: &'static [u8],
    large: &'static [u8],
}

impl Responses {
    fn for_head(&self, head: &str) -> &'static [u8] {
        let path = head
            .lines()
            .next()
            .and_then(|l| l.split(' ').nth(1))
            .unwrap_or_default();
        match body_len(path) {
            MEDIUM => self.medium,
            LARGE => self.large,
            _ => self.small,
        }
    }
}

impl Backend {
    /// Listens on `ip` (port chosen by the kernel).
    pub fn start(ip: std::net::IpAddr) -> std::io::Result<Backend> {
        let listener = TcpListener::bind(SocketAddr::new(ip, 0))?;
        let addr = listener.local_addr()?;
        let responses = Responses {
            small: response(SMALL),
            medium: response(MEDIUM),
            large: response(LARGE),
        };
        thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        thread::spawn(move || serve(stream, responses));
                    }
                    Err(e) => eprintln!("bench: backend accept: {e}"),
                }
            }
        });
        Ok(Backend { addr })
    }
}

/// Answers each request head on `stream` (a request body, if one is
/// announced by `Content-Length`, is skipped).
fn serve(mut stream: TcpStream, responses: Responses) {
    let _ = stream.set_nodelay(true);
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head =
                String::from_utf8_lossy(buf.get(..end).unwrap_or_default()).to_ascii_lowercase();
            let body: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            if buf.len() < end + 4 + body {
                break;
            }
            buf.drain(..end + 4 + body);
            if stream.write_all(responses.for_head(&head)).is_err() {
                return;
            }
            if head
                .lines()
                .any(|l| l.replace(' ', "") == "connection:close")
            {
                return;
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(chunk.get(..n).unwrap_or_default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::{self, Expect, Reuse, Target};
    use std::time::Duration;

    #[test]
    fn keep_alive_and_close() {
        let backend = Backend::start([127, 0, 0, 1].into()).unwrap();
        for reuse in [Reuse::KeepAlive, Reuse::Fresh] {
            let target = Target {
                addr: backend.addr,
                request: load::request("b", "/", "", reuse),
                reuse,
                expect: Expect {
                    status: 200,
                    body_len: SMALL,
                },
            };
            let t = load::drive(&target, 2, Duration::from_millis(100));
            assert!(t.requests > 10, "{t:?}");
            assert_eq!(t.errors, 0, "{t:?}");
        }
    }

    #[test]
    fn the_body_size_follows_the_path() {
        let backend = Backend::start([127, 0, 0, 1].into()).unwrap();
        for (path, len) in [("/small", SMALL), ("/medium", MEDIUM), ("/large", LARGE)] {
            let target = Target {
                addr: backend.addr,
                request: load::request("b", path, "", Reuse::KeepAlive),
                reuse: Reuse::KeepAlive,
                expect: Expect {
                    status: 200,
                    body_len: len,
                },
            };
            let t = load::drive(&target, 1, Duration::from_millis(100));
            assert!(t.requests > 0, "{path}: {t:?}");
            assert_eq!(t.errors, 0, "{path}: {t:?}");
        }
    }
}
