//! Reading request heads, routing them (static or proxy), fixed
//! responses, and what follows a response: keep-alive or a lingering close.

use super::proxy::{self, ProxyPlan};
use super::static_files::{Plan, plan};
use super::{Action, ConnId, Core};
use crate::config::{Limits, RouteAction};
use crate::conn::{Conn, ConnState, LINGER_TIMEOUT, UnreadBody};
use crate::file::Chunk;
use crate::http::request::{
    BodyFraming, HeadParser, HttpError, KnownMethod, Method, ParseStatus, Persistence, RequestHead,
};
use crate::http::response::{BodyMode, render_fixed};
use crate::http::status::Status;
use crate::log::{Log, RejectReason, RequestSummary};
use crate::time::Instant;

impl Core {
    pub(super) fn received(
        &mut self,
        now: Instant,
        conn: ConnId,
        len: usize,
        actions: &mut Vec<Action>,
    ) {
        let header_timeout = self.config.limits.header_timeout;
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let capacity = c.recv.len();
        // A length beyond the spare space is a driver bug: clamp.
        match c.state {
            ConnState::ReadingHead {
                buffered,
                parser,
                deadline,
            } => {
                let added = len.min(capacity.saturating_sub(buffered));
                if added == 0 {
                    return;
                }
                // The header deadline is total: new bytes do not extend it.
                c.state = ConnState::ReadingHead {
                    buffered: buffered.saturating_add(added),
                    parser,
                    deadline,
                };
            }
            ConnState::Idle { .. } => {
                let added = len.min(capacity);
                if added == 0 {
                    return;
                }
                c.state = ConnState::ReadingHead {
                    buffered: added,
                    parser: HeadParser::new(),
                    deadline: now.saturating_add(header_timeout),
                };
            }
            // Input after our response: discarded, the buffer is reused. It
            // does not extend the linger deadline.
            ConnState::Lingering { .. } => return,
            // Not reading: nothing was offered, so nothing can have arrived.
            ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Connecting { .. }
            | ConnState::AwaitingResponse { .. }
            | ConnState::Relaying { .. } => return,
        }
        self.try_parse(now, conn, actions);
        self.sync_interest(conn, actions);
    }

    pub(super) fn sent(
        &mut self,
        now: Instant,
        conn: ConnId,
        len: usize,
        actions: &mut Vec<Action>,
    ) {
        let limits = &self.config.limits;
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        // A relayed response is written from the exchange's buffers.
        if let ConnState::Relaying { .. } = c.state {
            self.relay_sent(now, conn, len, actions);
            return;
        }
        let len = len.min(c.send.len());
        c.send.drain(..len);
        // Progress resets the send idle deadline.
        if len > 0 {
            c.state
                .set_deadline(now.saturating_add(limits.send_idle_timeout));
        }
        if c.send.is_empty() {
            match c.state {
                ConnState::Responding {
                    persistence,
                    unread_body,
                    buffered,
                    ..
                } => {
                    c.state = finish(
                        conn,
                        c,
                        AfterResponse {
                            persistence,
                            unread_body,
                            buffered,
                        },
                        now,
                        limits,
                        actions,
                    );
                }
                ConnState::SendingFile {
                    file,
                    chunk: Chunk::Done,
                    persistence,
                    buffered,
                    ..
                } => {
                    self.files.release(file);
                    actions.push(Action::CloseFile { file });
                    c.state = finish(
                        conn,
                        c,
                        AfterResponse {
                            persistence,
                            unread_body: UnreadBody::None,
                            buffered,
                        },
                        now,
                        limits,
                        actions,
                    );
                }
                ConnState::SendingFile { .. }
                | ConnState::ReadingHead { .. }
                | ConnState::Idle { .. }
                | ConnState::OpeningFile { .. }
                | ConnState::Connecting { .. }
                | ConnState::AwaitingResponse { .. }
                | ConnState::Relaying { .. }
                | ConnState::Lingering { .. } => {}
            }
        }
        self.try_parse(now, conn, actions);
        self.sync_interest(conn, actions);
    }

    /// In `ReadingHead`, parse the buffered bytes. The parser resumes where
    /// it stopped, so only newly arrived bytes are looked at.
    pub(super) fn try_parse(&mut self, now: Instant, conn: ConnId, actions: &mut Vec<Action>) {
        let send_deadline = now.saturating_add(self.config.limits.send_idle_timeout);
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let ConnState::ReadingHead {
            buffered,
            mut parser,
            deadline,
        } = c.state
        else {
            return;
        };
        let capacity = c.recv.len();
        let filled = c.recv.get(..buffered).unwrap_or_default();
        let full = buffered >= capacity;
        let result = match parser.advance(filled, &self.head_limits) {
            // A full buffer that holds no complete head can never hold one:
            // the head would exceed `max_head`.
            Ok(ParseStatus::Incomplete) if full => Err(HttpError::HeadTooLarge),
            Ok(ParseStatus::Incomplete) => Ok(ParseStatus::Incomplete),
            Ok(ParseStatus::Complete { head, consumed }) => {
                Ok(ParseStatus::Complete { head, consumed })
            }
            Err(error) => Err(error),
        };
        c.state = ConnState::ReadingHead {
            buffered,
            parser,
            deadline,
        };
        match result {
            Ok(ParseStatus::Incomplete) => {}
            Ok(ParseStatus::Complete { head, consumed }) => {
                // Bytes after the head belong to the next request: move them
                // to the front.
                let leftover = buffered.saturating_sub(consumed);
                if consumed <= buffered {
                    c.recv.copy_within(consumed..buffered, 0);
                }
                self.respond(now, conn, head, leftover, actions);
            }
            Err(error) => {
                // Once the request line is valid the method is known: a HEAD
                // request's error is head-only too. An error in the request
                // line itself keeps its body.
                let body = match parser.method() {
                    KnownMethod::Head => BodyMode::HeadOnly,
                    KnownMethod::Unknown | KnownMethod::Get | KnownMethod::Other => BodyMode::Full,
                };
                render_fixed(error.status(), Persistence::Close, body, &mut c.send);
                actions.push(Action::Log(Log::Rejected {
                    conn,
                    peer: c.peer,
                    reason: RejectReason::Http(error),
                }));
                c.state = ConnState::Responding {
                    persistence: Persistence::Close,
                    unread_body: UnreadBody::None,
                    buffered: 0,
                    deadline: send_deadline,
                };
            }
        }
    }
}

/// How a connection goes on once its response is fully written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AfterResponse {
    pub(super) persistence: Persistence,
    pub(super) unread_body: UnreadBody,
    /// Pipelined input waiting at the front of `Conn::recv`.
    pub(super) buffered: usize,
}

/// The response on `conn` is fully written: keep the connection for the
/// next request, or shut down its sending side and linger.
pub(super) fn finish(
    conn: ConnId,
    c: &mut Conn,
    after: AfterResponse,
    now: Instant,
    limits: &Limits,
    actions: &mut Vec<Action>,
) -> ConnState {
    let AfterResponse {
        persistence,
        unread_body,
        buffered,
    } = after;
    match (persistence, unread_body) {
        (Persistence::KeepAlive, UnreadBody::None) => {
            if buffered > 0 {
                ConnState::ReadingHead {
                    buffered,
                    parser: HeadParser::new(),
                    deadline: now.saturating_add(limits.header_timeout),
                }
            } else {
                ConnState::Idle {
                    deadline: now.saturating_add(limits.keepalive_timeout),
                }
            }
        }
        (Persistence::Close, UnreadBody::None | UnreadBody::Pending)
        | (Persistence::KeepAlive, UnreadBody::Pending) => {
            actions.push(Action::ShutdownWrite { conn });
            // Lingering input goes to the shared scratch buffer.
            c.recv = Box::default();
            ConnState::Lingering {
                deadline: now.saturating_add(LINGER_TIMEOUT),
            }
        }
    }
}

impl Core {
    /// Answer a complete request head on `conn`: a route miss or a rule
    /// that needs no file or backend is answered at once (the connection
    /// then responds until the send idle deadline, unless the socket makes
    /// progress); a static route opens its file, a proxy route connects to
    /// its backend. `leftover` bytes at the front of the connection's
    /// receive buffer wait for the next request.
    fn respond(
        &mut self,
        now: Instant,
        conn: ConnId,
        head: RequestHead,
        leftover: usize,
        actions: &mut Vec<Action>,
    ) {
        let send_deadline = now.saturating_add(self.config.limits.send_idle_timeout);
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let route = self.router.route(head.host.as_deref(), &head.target.path);
        let unread_body = match head.body {
            BodyFraming::None => UnreadBody::None,
            BodyFraming::Length(_) | BodyFraming::Chunked => UnreadBody::Pending,
        };
        // Every response to a HEAD request is head only.
        let body = match head.method {
            Method::Head => BodyMode::HeadOnly,
            Method::Get | Method::Other(_) => BodyMode::Full,
        };
        let (status, persistence) = match route {
            Err(_) => (Status::NotFound, head.persistence),
            Ok(route) => match &route.action {
                RouteAction::Static {
                    root,
                    on_file_change,
                } => match plan(route, root, *on_file_change, &head, &self.static_roots) {
                    Plan::Open(open) => {
                        self.open_static(now, conn, head, open, leftover, actions);
                        return;
                    }
                    Plan::Respond(status, persistence) => (status, persistence),
                },
                RouteAction::Proxy(target) => {
                    match proxy::plan(route, target, &head, &self.backends) {
                        ProxyPlan::Forward(forward) => {
                            self.forward(now, conn, head, forward, leftover, actions);
                            return;
                        }
                        ProxyPlan::Respond(status, persistence) => (status, persistence),
                    }
                }
            },
        };
        // An unread body leaves the stream unframed: it cannot be reused.
        let persistence = match unread_body {
            UnreadBody::None => persistence,
            UnreadBody::Pending => Persistence::Close,
        };
        render_fixed(status, persistence, body, &mut c.send);
        actions.push(Action::Log(Log::Response {
            conn,
            peer: c.peer,
            status,
            request: Some(RequestSummary {
                method: head.method,
                host: head.host,
                path: head.target.path,
            }),
        }));
        c.state = ConnState::Responding {
            persistence,
            unread_body,
            buffered: leftover,
            deadline: send_deadline,
        };
    }
}

#[cfg(test)]
mod tests {
    use crate::core::test_support::{PEER, core};
    use crate::core::{Action, ConnId, Core, Event, Interest};
    use crate::http::request::HttpError;
    use crate::log::{Log, RejectReason};
    use crate::time::Instant;

    #[test]
    fn oversized_received_is_clamped() {
        let mut core = core();
        let mut actions = Vec::new();
        let conn = ConnId(1);
        let now = Instant::from_nanos(0);
        core.handle(now, Event::Accepted { conn, peer: PEER }, &mut actions);
        assert_eq!(core.recv_buffer(conn).len(), 10);
        actions.clear();
        core.handle(now, Event::Received { conn, len: 1000 }, &mut actions);
        // Clamped to the 10 spare bytes: the full buffer of zeros is an
        // over-long request line, answered at once.
        assert_eq!(
            actions,
            [
                Action::Log(Log::Rejected {
                    conn,
                    peer: PEER,
                    reason: RejectReason::Http(HttpError::RequestLineTooLong),
                }),
                Action::SetInterest {
                    conn,
                    interest: Interest::Write
                }
            ]
        );
        assert!(core.recv_buffer(conn).is_empty());
        actions.clear();
        core.handle(now, Event::Received { conn, len: 5 }, &mut actions);
        assert!(actions.is_empty());
    }

    /// A maximal head sent one line per read costs linear parser work: every
    /// byte is searched for an LF once and judged once, and the finished head
    /// is built once (regression test for rescanning the whole buffer on
    /// every new line).
    #[test]
    fn head_sent_line_by_line_is_parsed_in_linear_work() {
        use crate::http::request::work;
        let mut core = Core::new(
            crate::config::load("listen 127.0.0.1:8080; host a.test { path / { static /srv; } }")
                .unwrap(),
        );
        let limits = core.config.limits.clone();
        let mut lines: Vec<Vec<u8>> =
            vec![b"GET / HTTP/1.1\r\n".to_vec(), b"Host: a.test\r\n".to_vec()];
        let fixed: usize = lines.iter().map(Vec::len).sum();
        let count = limits.max_headers - 1;
        let width = (limits.max_head - fixed - 2) / count;
        for i in 0..count {
            let mut line = format!("X-Pad-{i:03}: ").into_bytes();
            line.resize(width - 2, b'x');
            line.extend_from_slice(b"\r\n");
            lines.push(line);
        }
        lines.push(b"\r\n".to_vec());
        let total: usize = lines.iter().map(Vec::len).sum();
        assert!(total <= limits.max_head && total > limits.max_head - width);

        let conn = ConnId(1);
        let now = Instant::from_nanos(0);
        let mut actions = Vec::new();
        core.handle(now, Event::Accepted { conn, peer: PEER }, &mut actions);
        work::take();
        for line in &lines {
            let spare = core.recv_buffer(conn);
            spare.get_mut(..line.len()).unwrap().copy_from_slice(line);
            core.handle(
                now,
                Event::Received {
                    conn,
                    len: line.len(),
                },
                &mut actions,
            );
        }
        let used = work::take();
        // The complete head was answered: `/` is opened as a file.
        assert!(
            actions.iter().any(|a| matches!(a, Action::OpenFile { .. })),
            "{actions:?}"
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::Log(Log::Rejected { .. }))),
            "{actions:?}"
        );
        assert!(used.scanned <= total, "{used:?} for a {total}-byte head");
        assert!(used.judged <= total, "{used:?} for a {total}-byte head");
        assert!(used.built <= total, "{used:?} for a {total}-byte head");
    }
}
