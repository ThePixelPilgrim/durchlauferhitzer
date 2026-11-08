//! Backend-directed files (FR-PROXY-32 Trigger, Bad path, Missing file,
//! the backend's body): a backend's `200` on an `internal_files` route
//! that carries the route's file header is answered with the file it
//! names, opened below the route's root like a static route's path. The
//! backend's body is never read; its upstream is pooled only after
//! `Content-Length: 0` (FR-PROXY-02). Once the file is open, the answer
//! is decided and its merged head rendered in `file_answer.rs`, as for a
//! static route.

use super::pool::{Exchange, release};
use super::proxy::{queued_send_deadline, take_exchange};
use super::{Action, ConnId, Core, InternalRoute};
use crate::config::FileHeader;
use crate::conn::{
    BackendFile, ConnState, FileSource, OpenStep, ProxyRequest, Release, StaticRequest, UnreadBody,
};
use crate::http::backend_path::{Located, accel_path, sendfile_path};
use crate::http::forward::{kept_lines_len, render_kept_lines};
use crate::http::request::Persistence;
use crate::http::response::{BodyMode, FILE_HEAD_EXTRA, render_fixed};
use crate::http::response_head::{RequestKind, ResponseBody, ResponseError, ResponseHead};
use crate::http::status::Status;
use crate::log::{BackendFailure, Log};
use crate::relay_body::rest_after;
use crate::request_body::BodyEnd;
use crate::time::Instant;

/// What a backend's response head names on an `internal_files` route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Named {
    /// The route's header is on two or more lines, or its value is a bad
    /// path (a `502`).
    Bad,
    /// Where its one value points.
    File(Located),
}

/// Whether `head`, the response to `request`, names a file (FR-PROXY-32
/// Trigger): a `200` carrying the route's file header on an
/// `internal_files` route. `None` relays the response as usual (the file
/// headers stripped, FR-PROXY-32 Stripping).
pub(super) fn named_file(request: &ProxyRequest, head: &ResponseHead) -> Option<Named> {
    let route = request.internal.as_deref()?;
    if head.status != 200 {
        return None;
    }
    let name = match route.files.header {
        FileHeader::XSendfile => "x-sendfile",
        FileHeader::XAccelRedirect { .. } => "x-accel-redirect",
    };
    let mut values = head.headers.iter().filter(|h| h.name == name);
    let value = values.next()?;
    if values.next().is_some() {
        return Some(Named::Bad);
    }
    let located = match &route.files.header {
        FileHeader::XSendfile => sendfile_path(
            route.files.root.as_os_str().as_encoded_bytes(),
            &value.value,
        ),
        FileHeader::XAccelRedirect { prefix } => accel_path(prefix.as_bytes(), &value.value),
    };
    Some(match located {
        Ok(located) => Named::File(located),
        Err(_) => Named::Bad,
    })
}

/// Whether a backend's kept lines of `kept_len` bytes and the core's lines
/// around them fit a head buffer of `capacity` (NFR-RES-03): judged before
/// the file is opened, against the most the core adds
/// ([`FILE_HEAD_EXTRA`]). The head buffer is [`FILE_HEAD_EXTRA`] larger on
/// a config with `internal_files` (NFR-RES-01), and the kept lines are
/// never longer than the client response the relay would render, so a
/// head within the limits always fits and the `502` this guards is
/// unreachable by events; it stays as the bound's check.
pub(super) fn fits_backend_file(kept_len: usize, capacity: usize) -> bool {
    kept_len.saturating_add(FILE_HEAD_EXTRA) <= capacity
}

impl Core {
    /// Answer `head` (the first `consumed` bytes of the upstream's receive
    /// buffer), which names a file (`named`), on `conn`, whose exchange
    /// awaits its response: a bad path is a `502` (`BackendFailed`, then
    /// the `Response`, the upstream closed); a name a static route answers
    /// 404 is the plain `404`; otherwise the backend's kept lines are
    /// rendered into the exchange's head buffer, the upstream is released,
    /// the receive buffer freed and the file opened below the route's
    /// root. No `max_response_body` applies, nor the HTTP/1.0 client's
    /// `Transfer-Encoding` check: the backend's body is never read, and
    /// its framing headers are replaced. The client keeps the request's
    /// persistence, or closes when its body was not fully read
    /// (FR-PROXY-27, -28).
    pub(super) fn backend_file(
        &mut self,
        now: Instant,
        conn: ConnId,
        head: ResponseHead,
        consumed: usize,
        named: Named,
        actions: &mut Vec<Action>,
    ) {
        let located = match named {
            Named::Bad => {
                self.fail_before_head(now, conn, BackendFailure::BadFilePath, actions);
                return;
            }
            Named::File(located) => located,
        };
        let head_capacity = self.head_capacity;
        let mode = self.mode;
        let limits = &self.config.limits;
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let ConnState::AwaitingResponse {
            request,
            buffers,
            filled,
            send_deadline: unread_continue,
            ..
        } = &c.state
        else {
            return;
        };
        let Some(route) = request.internal.as_deref() else {
            return;
        };
        match located {
            Located::Open { .. }
                if !fits_backend_file(kept_lines_len(&head, &route.files), head_capacity) =>
            {
                let kind = BackendFailure::BadResponse(ResponseError::HeadTooLarge);
                self.fail_before_head(now, conn, kind, actions);
                return;
            }
            Located::Open { .. } | Located::NotFound => {}
        }
        // FR-PROXY-02: the upstream is reused only if the exchange is
        // reusable on both sides (`Exchange`, as for a relayed response)
        // and the backend sent no body (`Content-Length: 0`; never after a
        // HEAD, whose response says nothing of its body) and nothing behind
        // its head. The body is never read: anything else closes it.
        let release_to = match (
            request.kind,
            head.body,
            Exchange::of(request, buffers, &head).release(request.backend),
        ) {
            (RequestKind::Other, ResponseBody::None, park @ Release::Park(_)) => park,
            (RequestKind::Head | RequestKind::Other, _, _) => Release::Close,
        };
        let rest = rest_after(consumed, *filled, buffers.recv.len());
        let deadline = queued_send_deadline(*unread_continue, now, limits);
        let Some((up, mut request, mut buffers)) = take_exchange(c) else {
            return;
        };
        let (persistence, unread_body, buffered) = match request.body.end(&mut c.recv) {
            BodyEnd::Read { buffered } => (request.persistence, UnreadBody::None, buffered),
            BodyEnd::Unread => (Persistence::Close, UnreadBody::Pending, 0),
        };
        let body = BodyMode::of(&request.summary.method);
        let Some(route) = request.internal.take() else {
            return;
        };
        let InternalRoute { files, root } = &*route;
        c.state = match located {
            // FR-STATIC-09: what a static route answers 404, the backend's
            // headers dropped.
            Located::NotFound => {
                render_fixed(
                    Status::NotFound,
                    persistence,
                    body,
                    self.date.as_ref(),
                    &mut c.send,
                );
                actions.push(Action::Log(Log::Response {
                    conn,
                    peer: c.peer,
                    status: Status::NotFound,
                    request: Some(request.summary),
                }));
                release(
                    &mut self.upstreams,
                    mode,
                    limits,
                    now,
                    up,
                    release_to,
                    rest,
                    actions,
                );
                ConnState::Responding {
                    persistence,
                    unread_body,
                    buffered,
                    deadline,
                }
            }
            Located::Open { segments, slash } => {
                buffers.clear_head();
                let kept = render_kept_lines(&head, files, &mut buffers.head);
                release(
                    &mut self.upstreams,
                    mode,
                    limits,
                    now,
                    up,
                    release_to,
                    rest,
                    actions,
                );
                let file = self.files.allocate(conn);
                actions.push(Action::OpenFile {
                    file,
                    root: *root,
                    path: segments.clone(),
                });
                ConnState::OpeningFile {
                    file,
                    slash,
                    step: OpenStep::Backend,
                    body,
                    persistence,
                    buffered,
                    deadline,
                    request: StaticRequest {
                        summary: request.summary,
                        query: None,
                        root: *root,
                        on_change: files.on_file_change,
                        path: segments,
                        conditional: request.conditional,
                        range: request.range,
                        source: FileSource::Backend(BackendFile {
                            backend: request.backend,
                            // The receive buffer goes with the rest.
                            head: buffers.into_head(),
                            kept,
                        }),
                    },
                }
            }
        };
        self.sync_interest(conn, actions);
    }
}

#[cfg(test)]
mod flow_tests;
#[cfg(test)]
mod tests;
