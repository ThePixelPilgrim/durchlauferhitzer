//! The stub backend of the proxy scenarios: in this process (its CPU time
//! is never the server's), std only, a thread per connection. Every
//! request head gets the same 200 response with a [`SMALL`]-byte body;
//! the connection stays open unless the request asked to close it, so a
//! pooling server can reuse it.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;

use crate::scenario::SMALL;

/// The listening backend; its threads live as long as the process.
pub struct Backend {
    pub addr: SocketAddr,
}

impl Backend {
    pub fn start() -> std::io::Result<Backend> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {SMALL}\r\n\r\n"
        )
        .into_bytes();
        response.resize(response.len() + SMALL as usize, b'b');
        let response: &'static [u8] = response.leak();
        thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        thread::spawn(move || serve(stream, response));
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
fn serve(mut stream: TcpStream, response: &[u8]) {
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
            if stream.write_all(response).is_err() {
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
        let backend = Backend::start().unwrap();
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
}
