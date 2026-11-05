//! HTTP clients for `restart.rs`: one complete response read by its
//! `Content-Length` ([`read_response`]), a request on a fresh connection
//! ([`fresh_request`]), clients that send requests in a loop until stopped
//! ([`Load`], keep-alive and fresh) and connections queued with a request
//! each ([`queue`], [`collect`]). Every failure is kept as text.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// How long a read may block: longer than any stop of the old process.
const READ_PATIENCE: Duration = Duration::from_secs(15);

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A complete response: its status line, whether it said
/// `Connection: close`, and its body.
#[derive(Debug)]
pub struct Response {
    pub status: String,
    pub close: bool,
    pub body: String,
}

/// Reads one response (with `Content-Length`) from `stream`; an error,
/// EOF or garbage before it is complete is an `Err` naming it.
pub fn read_response(stream: &mut TcpStream) -> Result<Response, String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(format!("EOF in the head after {} bytes", buf.len())),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(err) => return Err(format!("reading the head: {err}")),
        }
    };
    let head = text(&buf[..head_end]);
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or_default().to_owned();
    let mut length = None;
    let mut close = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = value.parse::<usize>().ok();
        } else if name.eq_ignore_ascii_case("connection") && value.eq_ignore_ascii_case("close") {
            close = true;
        }
    }
    let length = length.ok_or_else(|| format!("no Content-Length: {head:?}"))?;
    let mut body = buf[head_end..].to_vec();
    while body.len() < length {
        match stream.read(&mut chunk) {
            Ok(0) => return Err(format!("EOF in the body after {} bytes", body.len())),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(err) => return Err(format!("reading the body: {err}")),
        }
    }
    if body.len() != length {
        return Err(format!("{} bytes after the body", body.len() - length));
    }
    Ok(Response {
        status,
        close,
        body: text(&body),
    })
}

pub const REQUEST: &[u8] = b"GET /a.txt HTTP/1.1\r\nHost: example.test\r\n\r\n";
pub const REQUEST_CLOSE: &[u8] =
    b"GET /a.txt HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n";

pub fn connect(addr: SocketAddr) -> io::Result<TcpStream> {
    let stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(READ_PATIENCE))?;
    Ok(stream)
}

/// The answer a response gives, or why it is not one of `answers`.
pub fn answer(response: Result<Response, String>, answers: &[&str]) -> Result<String, String> {
    let response = response?;
    let answer = response.body.trim_end().to_owned();
    if response.status != "HTTP/1.1 200 OK" || !answers.contains(&answer.as_str()) {
        return Err(format!("unexpected response {response:?}"));
    }
    Ok(answer)
}

/// One request on a fresh connection that asks for `Connection: close`;
/// the connection must end after the response.
pub fn fresh_request(addr: SocketAddr, answers: &[&str]) -> Result<String, String> {
    let mut stream = connect(addr).map_err(|err| format!("connect: {err}"))?;
    stream
        .write_all(REQUEST_CLOSE)
        .map_err(|err| format!("write: {err}"))?;
    let got = answer(read_response(&mut stream), answers)?;
    match stream.read(&mut [0u8; 1]) {
        Ok(0) => Ok(got),
        other => Err(format!("no EOF after a close response: {other:?}")),
    }
}

/// What a load client saw: how many times each answer came, and every
/// failure.
#[derive(Debug, Default)]
pub struct Tally {
    pub answers: BTreeMap<String, usize>,
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum Pattern {
    /// One connection reused until the server says `Connection: close`.
    KeepAlive,
    /// A new connection per request.
    Fresh,
}

/// Clients that issue requests in a loop until stopped.
pub struct Load {
    stop: Arc<AtomicBool>,
    clients: Vec<JoinHandle<Tally>>,
}

impl Load {
    pub fn start(addr: SocketAddr, answers: &'static [&'static str]) -> Load {
        let stop = Arc::new(AtomicBool::new(false));
        let patterns = [
            Pattern::KeepAlive,
            Pattern::KeepAlive,
            Pattern::Fresh,
            Pattern::Fresh,
        ];
        let clients = patterns
            .into_iter()
            .map(|pattern| {
                let stop = Arc::clone(&stop);
                thread::spawn(move || run_client(addr, pattern, &stop, answers))
            })
            .collect();
        Load { stop, clients }
    }

    pub fn finish(self) -> Tally {
        self.stop.store(true, Ordering::Relaxed);
        let mut total = Tally::default();
        for client in self.clients {
            let tally = client.join().unwrap();
            for (answer, n) in tally.answers {
                *total.answers.entry(answer).or_default() += n;
            }
            total.failures.extend(tally.failures);
        }
        total
    }
}

pub fn run_client(
    addr: SocketAddr,
    pattern: Pattern,
    stop: &AtomicBool,
    answers: &[&str],
) -> Tally {
    let mut tally = Tally::default();
    let mut held: Option<TcpStream> = None;
    while !stop.load(Ordering::Relaxed) {
        let result = match pattern {
            Pattern::Fresh => fresh_request(addr, answers),
            Pattern::KeepAlive => keep_alive_request(addr, &mut held, answers),
        };
        match result {
            Ok(answer) => *tally.answers.entry(answer).or_default() += 1,
            Err(failure) => tally.failures.push(format!("{pattern:?}: {failure}")),
        }
    }
    tally
}

pub fn keep_alive_request(
    addr: SocketAddr,
    held: &mut Option<TcpStream>,
    answers: &[&str],
) -> Result<String, String> {
    let mut stream = match held.take() {
        Some(stream) => stream,
        None => connect(addr).map_err(|err| format!("connect: {err}"))?,
    };
    stream
        .write_all(REQUEST)
        .map_err(|err| format!("write: {err}"))?;
    let response = read_response(&mut stream)?;
    let close = response.close;
    let got = answer(Ok(response), answers)?;
    if !close {
        *held = Some(stream);
    }
    Ok(got)
}

/// Waits until the load has made progress (each client a few requests).
pub fn warm_up() {
    thread::sleep(Duration::from_millis(200));
}

/// Connections with a complete request each, queued in the backlog of
/// the (stopped) server at `addr`.
pub fn queue(addr: SocketAddr, n: usize) -> Vec<TcpStream> {
    (0..n)
        .map(|_| {
            let mut stream = connect(addr).unwrap();
            stream.write_all(REQUEST_CLOSE).unwrap();
            stream
        })
        .collect()
}

/// The answers the queued connections got, and their failures.
pub fn collect(queued: Vec<TcpStream>, answers: &[&str]) -> Tally {
    let mut tally = Tally::default();
    for mut stream in queued {
        match answer(read_response(&mut stream), answers) {
            Ok(answer) => *tally.answers.entry(answer).or_default() += 1,
            Err(failure) => tally.failures.push(failure),
        }
    }
    tally
}
