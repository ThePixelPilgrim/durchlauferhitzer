//! The sans-IO core and its contract with drivers.
//!
//! # Contract
//!
//! The core never touches sockets, clocks or files. A *driver* (the
//! simulation in tests, the epoll driver in production) owns all I/O and talks
//! to the core through exactly five channels:
//!
//! 1. **Events in.** The driver calls [`Core::handle`] with the current time
//!    and one [`Event`]. The core appends zero or more [`Action`]s to the
//!    caller's vector, in the order the driver must perform them.
//! 2. **Receive buffer.** Before reading from a socket the driver asks for
//!    [`Core::recv_buffer`], reads *directly* into the returned slice (no
//!    intermediate copy), and then reports how many bytes it wrote with
//!    [`Event::Received`]. The bytes land in core-owned memory, so per
//!    connection memory is bounded by the buffer's fixed capacity.
//! 3. **Send buffer.** When the connection's interest is
//!    [`Interest::Write`], the driver writes *directly* from
//!    [`Core::send_buffer`] and reports how many bytes the socket accepted
//!    with [`Event::Sent`]; the core then drops those bytes from the front.
//! 4. **Files.** File bytes never pass through the core. The core asks the
//!    driver to open a file ([`Action::OpenFile`]), to read a chunk of it
//!    into the page cache ([`Action::Readahead`]) and to close it
//!    ([`Action::CloseFile`]); the driver answers with [`Event::FileOpened`],
//!    [`Event::ChunkReady`] or [`Event::FileFailed`]. Once a chunk is ready
//!    the driver `sendfile`s the range [`Core::send_file`] returns and
//!    reports how many bytes the socket accepted with [`Event::FileSent`].
//! 5. **Upstreams.** For a proxy route the core asks the driver to connect
//!    to a backend ([`Action::Connect`]); the driver answers with
//!    [`Event::UpstreamConnected`] or [`Event::UpstreamFailed`], then reads
//!    into [`Core::upstream_recv_buffer`] and writes from
//!    [`Core::upstream_send_buffer`] exactly as for clients, reporting with
//!    [`Event::UpstreamReceived`], [`Event::UpstreamSent`] and
//!    [`Event::UpstreamReadClosed`], until [`Action::CloseUpstream`].
//!
//! Rules every driver follows:
//!
//! - The driver allocates [`ConnId`]s and never reuses one, not even after
//!   the connection closed. The first event for a connection is
//!   [`Event::Accepted`].
//! - The driver performs the actions of one `handle` call in order before
//!   delivering the next event. A slice obtained from `recv_buffer` or
//!   `send_buffer` is only valid until the next `handle` call (the borrow
//!   checker enforces this).
//! - A connection that ends emits, in this order: its [`Action::Log`] (if
//!   any), [`Action::CloseFile`] (if a file is open),
//!   [`Action::CloseUpstream`] (if an upstream is open), then
//!   [`Action::Close`].
//! - After [`Action::Close`] the core forgets the connection; the driver
//!   closes the socket and delivers no further events for that id. The core
//!   emits no further actions for it. Stray I/O events for a forgotten id are
//!   ignored, but an `Accepted` for it creates a *new* connection: the core
//!   cannot tell reuse from a fresh id, so never reusing ids is the driver's
//!   obligation.
//! - Readiness is level-triggered: while interest is [`Interest::Read`], the
//!   driver keeps reading into `recv_buffer` until it is empty (zero spare)
//!   or the socket would block; while interest is [`Interest::Write`], it
//!   keeps writing from `send_buffer` until it is empty or the socket would
//!   block, and once it is empty, from [`Core::send_file`] until the segment
//!   is done or the socket would block. An edge-triggered driver must loop
//!   to `EAGAIN`. `EAGAIN` is not reported (`Sent` and `FileSent` are never
//!   0); a short count only means the socket is full. A `sendfile` that
//!   returns 0 on a writable socket while the segment is unfinished has hit
//!   end of file: the driver reports `FileFailed { error: Truncated }`.
//! - A read into `recv_buffer(c)` is reported by `Received { conn: c, .. }`
//!   before any other `handle` call (or not at all, if nothing was read).
//!   Likewise a write from `send_buffer(c)` is reported by its `Sent`, and a
//!   `sendfile` of the `send_file(c)` segment by its `FileSent` (or
//!   `FileFailed`), as the very next `handle` call after that write: the
//!   core may change the buffer or the segment in any call, so a count
//!   reported later could describe bytes that are no longer there.
//! - An **empty** `recv_buffer` means "do not read now" (unknown connection,
//!   not in a reading state, or buffer full). An empty `send_buffer` means
//!   "nothing to write". Neither is an error.
//! - The driver arms timers from [`Core::next_deadline`] after every `handle`
//!   call and delivers [`Event::TimerFired`] once that instant has passed.
//!   After `TimerFired`, every deadline at or before `now` has been handled:
//!   `next_deadline` is `None` or later than `now`.
//!
//! Rules for files:
//!
//! - The core allocates [`FileId`]s, sequentially from 1, and never reuses
//!   one. A [`RootId`] indexes [`Core::static_roots`]: the driver opens each
//!   root once at startup.
//! - `OpenFile { root, path }` names a path as relative segments (decoded
//!   bytes, never `.`, `..`, empty, or containing `/` or NUL). The driver
//!   resolves it beneath the root, without following symlinks anywhere in
//!   the path and without ever leaving the root (`openat2` with
//!   `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`), and reports the metadata
//!   (`fstat`) of the open descriptor. An empty segment list means the root
//!   itself: the driver opens `"."` beneath the root descriptor
//!   (`openat2(root, "")` fails with `ENOENT`, which would make `GET /` a
//!   404).
//! - Open flags: `O_RDONLY | O_NONBLOCK | O_NOCTTY | O_CLOEXEC`, never
//!   `O_CREAT`, `O_TRUNC` or a write mode. `O_NONBLOCK` is required: a FIFO
//!   in a root opened without it blocks the opening worker until a writer
//!   appears, before any `fstat` could tell it is not a regular file. The
//!   `RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH` resolve flags take the place of
//!   `O_NOFOLLOW` for every component. `RESOLVE_NO_XDEV` is not set: mount
//!   points beneath a root are the operator's choice. Hard links inside a
//!   root cannot be detected and are the operator's responsibility.
//! - Opens may block (a cold dentry or inode cache), like readahead, so
//!   the driver runs `openat2` and `fstat` on its worker pool too, never on
//!   the event loop. Answers for different files may arrive in any order.
//! - Errors map to [`FileError`] as follows (all of them become a 404, so
//!   the mapping only matters for logs):
//!
//!   | errno                                      | `FileError`        |
//!   |--------------------------------------------|--------------------|
//!   | `ENOENT`, `ENOTDIR`                        | `NotFound`         |
//!   | `ELOOP`, `EXDEV` (refused by `RESOLVE_*`)  | `Symlink`          |
//!   | `EACCES`, `EPERM`                          | `PermissionDenied` |
//!   | anything else (`EIO`, `ENAMETOOLONG`, ...) | `Other`            |
//!
//!   `EMFILE`/`ENFILE` also map to `Other`; the driver logs them, since
//!   they mean the descriptor budget below is wrong, not a missing file.
//! - The stamp is `size = st_size` and `modified_ns = st_mtime` in
//!   nanoseconds (`tv_sec * 10^9 + tv_nsec`); an mtime before 1970 clamps to
//!   0, and an overflowing one saturates. The mtime granularity (often one
//!   kernel tick) means a rewrite of the same size within one tick goes
//!   unnoticed, and stamps are only compared at chunk boundaries, so a
//!   change racing the last chunk's `sendfile` is never detected.
//! - Every `OpenFile` is answered by exactly one `FileOpened` or
//!   `FileFailed`, even after the connection that asked closed: the core
//!   never closes a file whose open is unanswered. It answers a late
//!   `FileOpened` with `CloseFile` and ignores a late `FileFailed`.
//! - A connection holds at most one file id at a time: for a directory
//!   requested with a trailing `/`, the core closes the directory before it
//!   opens its `index.html` under a new id. A HEAD request's file is closed
//!   right after its head is queued, without any `Readahead`.
//! - A file is open from its `FileOpened` until the core's `CloseFile`; a
//!   failed open leaves nothing to close. `CloseFile` comes exactly once per
//!   opened file, also when its connection closes.
//! - While the file is open, every `Readahead` is answered by exactly one
//!   `ChunkReady` or `FileFailed`; `ChunkReady` carries the file's stamp
//!   (of the open descriptor) at the time the chunk was read. `CloseFile`
//!   supersedes a `Readahead` still in flight: after `CloseFile` the driver
//!   delivers no event for that file at all. It keeps the descriptor open
//!   until an in-flight readahead worker has returned and then drops that
//!   completion, so a reused descriptor number can never receive it. (The
//!   core ignores events for closed files anyway.)
//! - The driver writes `send_buffer` before `send_file`: `send_file` returns
//!   a segment only while the send buffer is empty and that chunk's
//!   `ChunkReady` has arrived. Interest is [`Interest::Write`] while either
//!   has bytes, and [`Interest::None`] while the connection waits on the
//!   disk.
//! - `FileSent { len }` reports how many bytes of the segment the socket
//!   accepted (more than the segment is clamped). A `sendfile` that hits end
//!   of file before the segment ends (the file shrank) is reported as
//!   `FileFailed { error: Truncated }`, after a `FileSent` for any bytes it
//!   did send. Other `sendfile` errors: `EAGAIN` means wait for writability
//!   (not reported); `EPIPE`/`ECONNRESET` are the socket's, reported as
//!   `Failed { conn, error: Reset }`; `EIO` and other file-side errors are
//!   reported as `FileFailed { error: Other }`.
//! - Known limit: `sendfile` itself may block on the disk. Readahead is
//!   advisory, and pages can be evicted between `ChunkReady` and the
//!   `sendfile`, which then reads them on the event loop. This is accepted
//!   for now; mitigations (an `RWF_NOWAIT` probe, `mincore`) are stage-7
//!   work.
//! - Descriptor budget: open files are bounded by connections, since each
//!   holds at most one file id and an orphaned open keeps its connection's
//!   slot, so at most `max_connections` file descriptors are open or being
//!   opened at once, orphans included. The driver checks at startup that
//!   `RLIMIT_NOFILE` covers `2 × max_connections` client sockets (refused
//!   connections, at most `max_connections` of them, hold sockets too),
//!   `max_connections` files (orphaned opens included, since they keep
//!   their connection's slot), the static roots, its listeners and its
//!   eventfd (and, from stage 3, the backend sockets).
//! - Every `ChunkReady` stamp is checked against the stamp at open under
//!   the route's `on_file_change`: `abort` (the default) fails the transfer
//!   with `Changed` on any difference; `allow_growth` keeps sending exactly
//!   the size at open when the file only grew, and fails with `Changed` for
//!   the same size with another modification time and with `Truncated` for
//!   a smaller size.
//! - A failure after the response head was queued (any `FileFailed` while
//!   sending, or a stamp that fails the check) aborts the transfer: the core
//!   logs [`Log::FileAborted`], emits `CloseFile` and closes the connection
//!   at once, with no lingering and no further writes, so the client sees a
//!   cut body, never a complete-looking short one. A failed open is
//!   answered 404; `Truncated` or `Changed` in answer to `OpenFile` is a
//!   driver bug and closes the connection.
//! - A directory requested without its trailing `/` is answered 301 to the
//!   path plus `/` (query kept), unless that response would not fit the
//!   send buffer: then 414. The send buffer never holds more than its
//!   capacity.
//!
//! Rules for upstreams:
//!
//! - The driver resolves every entry of [`Core::backends`] (`host:port`,
//!   distinct, in config order) once at startup; a [`BackendId`] is an
//!   index into that list, and `Connect` names the backend by id, never by
//!   name. Resolution failures are a startup error, not a runtime event.
//! - The core allocates [`UpstreamId`]s, sequentially from 1, and never
//!   reuses one. A connection holds at most one upstream at a time (one
//!   fresh backend connection per request, FR-PROXY-02).
//! - `Connect { up, backend }` is a non-blocking `connect` (`SOCK_NONBLOCK
//!   | SOCK_CLOEXEC`). It is answered by exactly one `UpstreamConnected`
//!   (the socket became writable and `SO_ERROR` is 0) or `UpstreamFailed`
//!   (any connect error; reported as `ConnError::Other`, since the core
//!   treats every failed connect alike), unless `CloseUpstream { up }`
//!   comes first: then nothing more is delivered for that id.
//! - While a `Connect` is pending the upstream's interest is
//!   `Interest::None`, yet the driver itself waits for the socket to become
//!   writable (`EPOLLOUT`) to learn that the connect completed, then reads
//!   `SO_ERROR`; that wait is the driver's, not an interest. The core sets
//!   the interest with [`Action::SetUpstreamInterest`] once connected.
//! - A client's socket errors and hang-ups (`EPOLLERR`, `EPOLLHUP`) are
//!   reported whatever the interest, as [`Event::Failed`]: a client error
//!   means the client is gone.
//! - An upstream's socket error is reported as [`Event::UpstreamFailed`]
//!   only when a read or a write hits it (or, for a connect, from
//!   `SO_ERROR`), and never ahead of bytes the backend sent before it.
//!   A backend that closes with unread input (say, a 413 to an upload it
//!   did not read) makes its kernel send RST instead of FIN; the response
//!   it wrote first is still readable until the read that returns
//!   `ECONNRESET`. So: under `Read` or `ReadWrite` the driver reads every
//!   readable byte (each read reported as usual) before it reports the
//!   error. A write that fails under `ReadWrite` is not reported by
//!   itself: the driver stops writing and goes on reading, and reports
//!   what the reads return (bytes, end of stream, then the error); under
//!   `Write` alone a failed write is reported at once. While the
//!   upstream's interest is `None` (the client is still taking relayed
//!   bytes, say) an error is not reported at all until the core asks to
//!   read or write again: then the reads above report it. (epoll reports
//!   `EPOLLERR` and `EPOLLHUP` whatever the events asked for, so the
//!   driver keeps such an upstream out of its poll set, or ignores those
//!   bits for it, until then.) `CloseUpstream` supersedes an error not
//!   yet reported. This keeps an early answer (FR-PROXY-27) and the last
//!   bytes of a relayed body from being lost to a reset.
//! - End of stream is only learnt from a read, so [`Event::ReadClosed`]
//!   and [`Event::UpstreamReadClosed`] come only while the interest is
//!   `Read` (for an upstream, `Read` or `ReadWrite`).
//! - While a request is proxied the client connection is not read (its
//!   interest is `None` or `Write`), except for the request's body, so a
//!   client that half-closes after its complete request is noticed only
//!   after the exchange, when the connection reads again (consistent with
//!   FR-PROXY-26's half-close rule). An end of stream in the middle of a
//!   body closes both connections.
//! - The buffer, readiness and reporting rules of client sockets apply
//!   unchanged to upstreams: reads go
//!   straight into `upstream_recv_buffer(up)` and are reported by
//!   `UpstreamReceived { up, .. }` as the very next `handle` call; writes
//!   go straight from `upstream_send_buffer(up)` and are reported by
//!   `UpstreamSent` as the very next call; readiness is level-triggered
//!   (read until the buffer is empty or the socket would block, write until
//!   the buffer is empty or the socket would block); `EAGAIN` is not
//!   reported and neither count is ever 0; an empty buffer means "do not
//!   read now" or "nothing to write". Upstreams alone may also get
//!   [`Interest::ReadWrite`]: the driver then waits for both readiness
//!   kinds and does both, each read and each write reported as the very
//!   next call, and asks for both buffers afresh after every call, since
//!   either may have become empty or the interest may have changed. A
//!   client connection never gets it. End of stream is
//!   `UpstreamReadClosed`; a socket error after the connect (`ECONNRESET`,
//!   `EPIPE` as `Reset`, anything else as `Other`) is `UpstreamFailed`,
//!   reported by the rule above (after the readable bytes).
//! - `CloseUpstream { up }` means: close the socket, also while its connect
//!   is pending, and deliver no further event for that id. The core emits it
//!   exactly once per upstream on every path (response complete, backend
//!   failure, client reset or close, timeout), never after its
//!   connection's `Close`. Stray events for unknown or closed upstreams are
//!   ignored.
//! - Request bodies are never copied either: once the request head is
//!   written, [`Core::upstream_send_buffer`] hands out the region of the
//!   client's receive buffer the body bytes landed in (bytes after the
//!   body's end are the next request and never reach the backend). The
//!   client is read for its body only once the request head is written and
//!   every body byte of the previous read was written, and then into the
//!   whole buffer but an incomplete chunked-framing tail of at most 35 bytes
//!   moved to its front (NFR-RES-04). While body bytes wait for the
//!   backend the upstream's interest is `ReadWrite` (`Write` only while the
//!   response buffer is full); otherwise it is `Read`. Either way a
//!   response that comes before the body is complete is seen and relayed,
//!   and the rest of the body is not forwarded (FR-PROXY-27). A backend
//!   that stops taking the body and does not answer is noticed by the body
//!   idle timeout (504).
//! - `Expect: 100-continue`: the core queues `HTTP/1.1 100 Continue` in the
//!   connection's send buffer at `UpstreamConnected`, for a request with a
//!   body (FR-PROXY-25); the driver writes it like any other bytes, before
//!   the response.
//! - Body bytes are never copied by the core: the bytes [`Core::send_buffer`]
//!   hands out for a relayed response body are the region of the upstream's
//!   receive buffer the backend's bytes landed in, validated (a chunked
//!   body chunk by chunk, forwarded verbatim). That buffer is offered
//!   for a body read only once every byte of the previous read was sent,
//!   and then with its full capacity, from offset 0, but an incomplete
//!   chunked-framing tail of at most 35 bytes moved to its front (fill,
//!   drain, refill; NFR-RES-04). A close-delimited body ends with
//!   [`Event::UpstreamReadClosed`]; any other ends at its framing, and the
//!   upstream is closed then, so bytes after a body's end are never sent
//!   (FR-PROXY-22). A response to HEAD, a 204 or a 304 has no body: what
//!   the backend sends after its head is dropped and the upstream closed.
//!   Interim (1xx) heads are dropped and the next head parsed afresh; more
//!   than eight in a row are a 502 (FR-PROXY-18). Its capacity is
//!   `max(16 KiB, max_head)`
//!   ([`UPSTREAM_RECV_MIN`]). The upstream's send buffer holds the
//!   rewritten request head; once that is written, the same buffer holds
//!   the rewritten response head for the client, which `send_buffer`
//!   hands out before the body. Its capacity, `max_head + max_headers +
//!   128`, holds any rewrite of a head within the limits.
//! - Descriptor budget: at most one upstream socket is open or connecting
//!   per counted connection, so at most `max_connections` at once; refused
//!   connections never connect. The driver's `RLIMIT_NOFILE` check at
//!   startup adds them to the budget above.
//! - Failures (FR-PROXY-26): before the response head was queued, a failed
//!   or refused connect is answered 502 ([`BackendFailure::Refused`]), a
//!   reset 502 ([`BackendFailure::Reset`]), an early end of stream 502
//!   ([`BackendFailure::ClosedEarly`]), a
//!   malformed or oversized response head 502
//!   ([`BackendFailure::BadResponse`], also for `101` and too many interim
//!   heads), a body over `max_response_body` 502
//!   ([`BackendFailure::BodyTooLarge`]: its `Content-Length`, or body bytes
//!   that came with the head), bad chunked framing in the body bytes that
//!   came with the head 502 ([`BackendFailure::BadChunk`]), a head with
//!   `Transfer-Encoding` (body or not) to an HTTP/1.0 client (whose
//!   request went out as HTTP/1.0) 502
//!   ([`BackendFailure::TransferEncodingToHttp10`]), a connect or response
//!   timeout 504.
//!   Each is logged as [`Log::BackendFailed`] before the response's
//!   [`Log::Response`], and the client connection stays as the request
//!   asked if its body was fully read (else it closes). After the head was
//!   queued, any failure cuts the client connection at once (logged, then
//!   `CloseUpstream`, then `Close`, no lingering close): a reset (`Reset`),
//!   an end of stream before a `Content-Length` or chunked body is complete
//!   (`ClosedEarly`), bad
//!   chunked framing, a chunked or close-delimited body passing
//!   `max_response_body`, a stalled body. A request body with invalid chunked
//!   framing is answered 400 if it came with the head, else both
//!   connections are cut; a chunked body over `max_request_body` is cut
//!   (both logged as [`Log::Rejected`]).
//!
//! # Deadlines
//!
//! Every connection state carries one deadline, set on entry:
//!
//! - reading a request head: `header_timeout`, a *total* deadline from
//!   accept or from the first byte of the next request; trickling bytes do
//!   not extend it. On expiry the core answers 408 if any bytes arrived and
//!   closes silently otherwise;
//! - idle between keep-alive requests: `keepalive_timeout`, then a silent
//!   close;
//! - writing a response: `send_idle_timeout`, reset whenever the socket
//!   accepts bytes; on expiry the connection is closed without further
//!   writes;
//! - waiting on the disk (open or readahead in flight): the same
//!   `send_idle_timeout`, also reset whenever the disk completes; on expiry
//!   it is logged as a disk timeout and the connection is closed: an open in
//!   flight becomes an orphaned open, an open file is closed with it. (A
//!   readahead in flight counts as waiting on the disk even while the send
//!   buffer still holds part of the head.)
//! - connecting to a backend: `backend_connect_timeout`, then a 504
//!   ([`BackendFailure::ConnectTimeout`]);
//! - writing the request to a backend and waiting for its response head:
//!   `backend_response_timeout`, restarted once the request head is fully
//!   written, then a 504 ([`BackendFailure::ResponseTimeout`]); while a
//!   request body streams, `body_idle_timeout` instead, reset on progress
//!   either way (a body byte read from the client or written to the
//!   backend, or response head bytes read from the backend), then a 408 if
//!   the core waited for the client's body bytes, a 504
//!   ([`BackendFailure::BodyTimeout`]) if they waited for the backend; once
//!   the whole request is written, the response timeout again, a total
//!   deadline that response head bytes do not extend. Each response head
//!   (interim or final) is also capped at the response timeout from its
//!   own first byte, which later bytes of that head do not move; a
//!   completed interim head ends its cap and the next head's first byte
//!   starts a fresh one. The connection times out at the earlier of the
//!   cap and the other deadline; the cap's expiry is a 504
//!   ([`BackendFailure::ResponseTimeout`]) even while the body streams;
//! - while a `100 Continue` the core queued waits for the client
//!   (awaiting the response): also the client's `send_idle_timeout`, from
//!   the moment the `100` is queued, restarted only by a byte the client
//!   takes and dropped once the whole `100` is out; on expiry the
//!   connection is cut, logged as a send timeout, the upstream closed.
//!   Deadlines that fall due at the same instant are taken in this order:
//!   this send deadline, then the current head's cap, then the deadline
//!   that otherwise applies (body idle or response timeout). A response
//!   queued behind an unread `100` (a 408, 502 or 504 of the core's own, or
//!   a proxied response) does not restart the send idle deadline: the one
//!   the `100` started carries over;
//! - relaying a response: the client's `send_idle_timeout` while the client
//!   has bytes to take (or nothing more is due from the backend),
//!   `body_idle_timeout` while it waits for the backend's next body bytes.
//!   Only the client taking bytes restarts the send idle deadline: backend
//!   progress (body bytes, a chunked tail that grew, the end of stream)
//!   keeps it while the client already had bytes to take, starts it from
//!   now when the client had none and now has some, and restarts the body
//!   idle deadline while the client still waits. On expiry the connection
//!   is cut, logged as a send timeout when the client had bytes to take,
//!   as a backend response timeout otherwise;
//! - lingering after a shutdown of the sending side: [`LINGER_TIMEOUT`],
//!   not extended by input.
//!
//! # Connection limits
//!
//! A connection accepted while `max_connections` connections, or
//! `max_connections_per_ip` from the same peer, are open is answered 503 and
//! closed (with a lingering close). Refused connections do not count toward
//! either limit. Peers are counted by their canonical address, so an
//! IPv4-mapped IPv6 peer shares the slot of its IPv4 address.
//!
//! Resource policy: memory stays bounded by the limits even under a flood of
//! refused connections.
//!
//! - A refused connection gets no receive buffer, only the send buffer that
//!   holds its 503.
//! - At most `max_connections` refused connections are kept at once; beyond
//!   that a new one is logged as rejected and closed at once, with nothing
//!   written.
//! - A connection entering the lingering close drops its own receive buffer;
//!   all lingering connections discard their input into one shared scratch
//!   buffer of [`LINGER_SCRATCH`] bytes, whose contents are never inspected.
//! - A connection that closes (or times out) while its `OpenFile` is
//!   unanswered leaves an *orphaned open*. It keeps counting toward
//!   both `max_connections` and its peer's `max_connections_per_ip` until
//!   its `FileOpened` (answered with `CloseFile`) or `FileFailed` arrives,
//!   so a hung disk cannot accumulate more open files than connections, and
//!   one peer looping request-then-reset against it is capped at
//!   `max_connections_per_ip` orphans instead of taking every global slot.
//!   (A driver cannot cancel an open blocked in the kernel, so the core
//!   waits for the answer rather than lying about it.)
//!
//! The core tolerates driver mistakes without panicking: events for unknown
//! connections, files or upstreams are ignored, a duplicate `Accepted` is
//! ignored, a file or upstream event that does not fit its state is
//! ignored, and a `Received`/`Sent`/`FileSent`/`UpstreamReceived`/
//! `UpstreamSent` length larger than the space, bytes or segment offered is
//! clamped.

mod contract;
mod limits;
mod proxy;
mod relay;
mod request;
mod static_files;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
mod timers;
mod upload;

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;

use crate::config::{BackendAddr, Config, RouteAction};
#[cfg(doc)]
use crate::conn::LINGER_TIMEOUT;
use crate::conn::{Conn, ConnState, LINGER_SCRATCH, SEND_CAPACITY};
use crate::file::{Chunk, FileSegment, FileTable};
#[cfg(doc)]
use crate::file::{FileError, FileId, RootId};
use crate::http::request::HeadLimits;
#[cfg(doc)]
use crate::log::BackendFailure;
use crate::log::Log;
pub use crate::request_body::BodyPending;
use crate::routing::Router;
use crate::time::Instant;
#[cfg(doc)]
use crate::upstream::{BackendId, UPSTREAM_RECV_MIN};
use crate::upstream::{UpstreamId, UpstreamTable, head_capacity, recv_capacity};

pub use contract::{Action, ConnError, ConnId, Event, Interest};

/// The I/O-free server core. See the module documentation for the contract.
#[derive(Debug)]
pub struct Core {
    config: Config,
    router: Router,
    head_limits: HeadLimits,
    conns: BTreeMap<ConnId, Conn>,
    /// Open connections that count toward the limits.
    counted: u32,
    /// Counted connections per canonical peer address; entries are removed
    /// at zero.
    counted_per_ip: BTreeMap<IpAddr, u32>,
    /// Refused connections not yet closed; at most `max_connections`.
    refused: u32,
    /// Where every lingering connection discards its input.
    scratch: Box<[u8]>,
    /// Distinct static roots in config order; a `RootId` indexes it.
    static_roots: Vec<PathBuf>,
    files: FileTable,
    /// Distinct backends in config order; a `BackendId` indexes it.
    backends: Vec<BackendAddr>,
    upstreams: UpstreamTable,
    /// Capacity of each exchange's upstream receive buffer.
    upstream_recv_capacity: usize,
    /// Capacity of each exchange's head buffer.
    head_capacity: usize,
}

impl Core {
    /// A core serving `config`, with no connections.
    pub fn new(config: Config) -> Core {
        let mut static_roots: Vec<PathBuf> = Vec::new();
        let mut backends: Vec<BackendAddr> = Vec::new();
        for route in config.hosts.iter().flat_map(|h| &h.routes) {
            match &route.action {
                RouteAction::Static { root, .. } => {
                    if !static_roots.contains(root) {
                        static_roots.push(root.clone());
                    }
                }
                RouteAction::Proxy(proxy) => {
                    if !backends.contains(&proxy.backend) {
                        backends.push(proxy.backend.clone());
                    }
                }
            }
        }
        let limits = &config.limits;
        let upstream_recv_capacity = recv_capacity(limits.max_head);
        let head_capacity = head_capacity(limits.max_head, limits.max_headers);
        Core {
            router: Router::new(&config.hosts),
            head_limits: HeadLimits::from(&config.limits),
            config,
            conns: BTreeMap::new(),
            counted: 0,
            counted_per_ip: BTreeMap::new(),
            refused: 0,
            scratch: vec![0; LINGER_SCRATCH].into_boxed_slice(),
            static_roots,
            files: FileTable::new(),
            backends,
            upstreams: UpstreamTable::new(),
            upstream_recv_capacity,
            head_capacity,
        }
    }

    /// Every distinct backend (`host:port`) in config order (identical
    /// addresses once). A [`BackendId`] is an index into it; the driver
    /// resolves each once at startup.
    pub fn backends(&self) -> &[BackendAddr] {
        &self.backends
    }

    /// Every distinct static root in config order (identical paths once).
    /// A [`RootId`] is an index into it; the driver opens each root once at
    /// startup.
    pub fn static_roots(&self) -> &[PathBuf] {
        &self.static_roots
    }

    /// Capacity of each connection's receive buffer: room for the largest
    /// allowed head, and for `max_request_line + 2` bytes so the parser can
    /// always reach a verdict on an over-long request line.
    fn new_recv_capacity(&self) -> usize {
        let limits = &self.config.limits;
        limits
            .max_head
            .max(limits.max_request_line.saturating_add(2))
    }

    /// Process one event at time `now`, appending the resulting actions.
    pub fn handle(&mut self, now: Instant, event: Event, actions: &mut Vec<Action>) {
        match event {
            Event::Accepted { conn, peer } => self.accepted(now, conn, peer, actions),
            Event::Received { conn, len } => self.received(now, conn, len, actions),
            Event::ReadClosed { conn } => {
                if self.forget(conn, actions).is_some() {
                    actions.push(Action::Close { conn });
                }
            }
            Event::Sent { conn, len } => self.sent(now, conn, len, actions),
            Event::Failed { conn, error: _ } => {
                // Log first (peek at the state), then forget, which closes
                // an open file: the order is Log, CloseFile, Close.
                let Some(c) = self.conns.get(&conn) else {
                    return;
                };
                match c.state {
                    // A reset while discarding input after our response is
                    // the expected end of a lingering close, not news.
                    ConnState::Lingering { .. } => {}
                    ConnState::ReadingHead { .. }
                    | ConnState::Idle { .. }
                    | ConnState::Responding { .. }
                    | ConnState::OpeningFile { .. }
                    | ConnState::SendingFile { .. }
                    | ConnState::Connecting { .. }
                    | ConnState::AwaitingResponse { .. }
                    | ConnState::Relaying { .. } => {
                        actions.push(Action::Log(Log::Reset { conn, peer: c.peer }));
                    }
                }
                self.forget(conn, actions);
                actions.push(Action::Close { conn });
            }
            Event::TimerFired => self.timer_fired(now, actions),
            Event::FileOpened { file, meta } => self.file_opened(now, file, meta, actions),
            Event::FileFailed { file, error } => self.file_failed(now, file, error, actions),
            Event::ChunkReady { file, stamp } => self.chunk_ready(now, file, stamp, actions),
            Event::FileSent { conn, len } => self.file_sent(now, conn, len, actions),
            Event::UpstreamConnected { up } => self.upstream_connected(now, up, actions),
            Event::UpstreamFailed { up, error: _ } => self.upstream_failed(now, up, actions),
            Event::UpstreamReceived { up, len } => self.upstream_received(now, up, len, actions),
            Event::UpstreamReadClosed { up } => self.upstream_read_closed(now, up, actions),
            Event::UpstreamSent { up, len } => self.upstream_sent(now, up, len, actions),
        }
    }

    /// Tell the driver if the interest the connection (or its upstream)
    /// needs has changed.
    fn sync_interest(&mut self, conn: ConnId, actions: &mut Vec<Action>) {
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        // Every render into the send buffer is bounded (fixed responses and
        // file heads by construction, see `every_rendered_head_fits_the_send_buffer`;
        // redirects by an explicit check), and this runs after every one.
        debug_assert!(
            c.send.len() <= SEND_CAPACITY,
            "send buffer holds {} bytes, beyond its capacity",
            c.send.len()
        );
        if let Some((up, interest)) = c.state.upstream_interest() {
            if self.upstreams.set_interest(up, interest) {
                actions.push(Action::SetUpstreamInterest { up, interest });
            }
        }
        let interest = c.wanted_interest();
        if interest != c.interest {
            c.interest = interest;
            actions.push(Action::SetInterest { conn, interest });
        }
    }

    /// Writable spare space for `conn`'s next read. Empty when the connection
    /// is unknown, not reading, or its buffer is full.
    ///
    /// If the driver writes any bytes into it, it reports them with
    /// [`Event::Received`] for `conn` as the very next `handle` call.
    pub fn recv_buffer(&mut self, conn: ConnId) -> &mut [u8] {
        match self.conns.get_mut(&conn) {
            Some(c) => match c.state {
                // Discarded input; the contents are never looked at.
                ConnState::Lingering { .. } => &mut self.scratch,
                ConnState::ReadingHead { .. }
                | ConnState::Idle { .. }
                | ConnState::Responding { .. }
                | ConnState::OpeningFile { .. }
                | ConnState::SendingFile { .. }
                | ConnState::Connecting { .. }
                | ConnState::AwaitingResponse { .. }
                | ConnState::Relaying { .. } => c.recv_spare(),
            },
            None => &mut [],
        }
    }

    /// Bytes waiting to be written to `conn`. Empty when there are none or
    /// the connection is unknown. While a proxied response is relayed they
    /// are any bytes of the connection's own send buffer first (a `100
    /// Continue` of the core's own not yet fully written), then the
    /// response's rewritten head, then its body bytes straight out of the
    /// upstream's receive buffer.
    pub fn send_buffer(&self, conn: ConnId) -> &[u8] {
        match self.conns.get(&conn) {
            Some(c) => match &c.state {
                ConnState::Relaying { body, buffers, .. } => {
                    if !c.send.is_empty() {
                        &c.send
                    } else if buffers.head.is_empty() {
                        body.unsent(&buffers.recv)
                    } else {
                        &buffers.head
                    }
                }
                ConnState::ReadingHead { .. }
                | ConnState::Idle { .. }
                | ConnState::Responding { .. }
                | ConnState::OpeningFile { .. }
                | ConnState::SendingFile { .. }
                | ConnState::Connecting { .. }
                | ConnState::AwaitingResponse { .. }
                | ConnState::Lingering { .. } => &c.send,
            },
            None => &[],
        }
    }

    /// The most [`Core::recv_buffer`] can currently hand out for `conn`:
    /// its own buffer's capacity, or the shared scratch buffer's while it
    /// lingers; 0 for unknown connections. It can change with the
    /// connection's state. For drivers' invariant checks (the simulation),
    /// not for I/O.
    #[doc(hidden)]
    pub fn recv_capacity(&self, conn: ConnId) -> usize {
        self.conns.get(&conn).map_or(0, |c| match c.state {
            ConnState::Lingering { .. } => self.scratch.len(),
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Connecting { .. }
            | ConnState::AwaitingResponse { .. }
            | ConnState::Relaying { .. } => c.recv.len(),
        })
    }

    /// The file range the driver may `sendfile` to `conn` now: only once
    /// [`Core::send_buffer`] is empty and the chunk's `ChunkReady` has
    /// arrived. `None` when there is none or the connection is unknown.
    pub fn send_file(&self, conn: ConnId) -> Option<FileSegment> {
        let c = self.conns.get(&conn)?;
        if !c.send.is_empty() {
            return None;
        }
        let ConnState::SendingFile {
            file,
            offset,
            chunk,
            ..
        } = c.state
        else {
            return None;
        };
        match chunk {
            Chunk::Ready { end, .. } if end > offset => Some(FileSegment {
                file,
                offset,
                len: end.saturating_sub(offset),
            }),
            Chunk::Ready { .. } | Chunk::Reading { .. } | Chunk::Done => None,
        }
    }

    /// The most [`Core::send_buffer`] can currently hand out for `conn`:
    /// its own send buffer's capacity, or while a proxied response is
    /// relayed, the capacity of the exchange buffer it writes from (the
    /// head buffer, then the upstream's receive buffer); 0 for unknown
    /// connections. It can change with the connection's state. For
    /// drivers' invariant checks (the simulation), not for I/O.
    #[doc(hidden)]
    pub fn send_capacity(&self, conn: ConnId) -> usize {
        self.conns.get(&conn).map_or(0, |c| match &c.state {
            ConnState::Relaying { buffers, .. } => {
                if !c.send.is_empty() {
                    SEND_CAPACITY
                } else if buffers.head.is_empty() {
                    buffers.recv.len()
                } else {
                    self.head_capacity
                }
            }
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Connecting { .. }
            | ConnState::AwaitingResponse { .. }
            | ConnState::Lingering { .. } => SEND_CAPACITY,
        })
    }

    /// Writable space for upstream `up`'s next read. Empty when the
    /// upstream is unknown, not reading, or (in a body) still holds body
    /// bytes not yet sent to the client; a body read is always offered the
    /// whole buffer.
    ///
    /// If the driver writes any bytes into it, it reports them with
    /// [`Event::UpstreamReceived`] for `up` as the very next `handle` call.
    pub fn upstream_recv_buffer(&mut self, up: UpstreamId) -> &mut [u8] {
        let Some(conn) = self.upstreams.conn(up) else {
            return &mut [];
        };
        let Some(c) = self.conns.get_mut(&conn) else {
            return &mut [];
        };
        if !c.state.reads_upstream(up) {
            return &mut [];
        }
        match &mut c.state {
            ConnState::AwaitingResponse {
                buffers, filled, ..
            } => buffers.recv.get_mut(*filled..).unwrap_or_default(),
            ConnState::Relaying { body, buffers, .. } => body.spare(&mut buffers.recv),
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Connecting { .. }
            | ConnState::Lingering { .. } => &mut [],
        }
    }

    /// Bytes waiting to be written to upstream `up`: the rewritten request
    /// head, then the request body's bytes straight out of the client's
    /// receive buffer. Empty when there are none or the upstream is
    /// unknown.
    pub fn upstream_send_buffer(&self, up: UpstreamId) -> &[u8] {
        let Some(c) = self
            .upstreams
            .conn(up)
            .and_then(|conn| self.conns.get(&conn))
        else {
            return &[];
        };
        match &c.state {
            ConnState::AwaitingResponse {
                up: own,
                buffers,
                request,
                ..
            } if *own == up => {
                if buffers.head.is_empty() {
                    request.body.unsent(&c.recv)
                } else {
                    &buffers.head
                }
            }
            ConnState::AwaitingResponse { .. }
            | ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Connecting { .. }
            | ConnState::Relaying { .. }
            | ConnState::Lingering { .. } => &[],
        }
    }

    /// Capacity of upstream `up`'s receive buffer; 0 for unknown or closed
    /// upstreams. For drivers' invariant checks (the simulation), not for
    /// I/O.
    #[doc(hidden)]
    pub fn upstream_recv_capacity(&self, up: UpstreamId) -> usize {
        self.upstreams
            .conn(up)
            .map_or(0, |_| self.upstream_recv_capacity)
    }

    /// Capacity of upstream `up`'s send buffer; 0 for unknown or closed
    /// upstreams. For drivers' invariant checks, not for I/O.
    #[doc(hidden)]
    pub fn upstream_send_capacity(&self, up: UpstreamId) -> usize {
        self.upstreams.conn(up).map_or(0, |_| self.head_capacity)
    }

    /// While upstream `up` is relaying a body: how many body bytes of its
    /// receive buffer are not yet sent to the client, and how long a
    /// chunked-framing tail it keeps behind them (moved to the front once
    /// they are sent). `None` outside a body. For drivers' invariant checks
    /// (fill, drain, refill), not for I/O.
    #[doc(hidden)]
    pub fn upstream_body_pending(&self, up: UpstreamId) -> Option<BodyPending> {
        let c = self
            .upstreams
            .conn(up)
            .and_then(|conn| self.conns.get(&conn))?;
        match &c.state {
            ConnState::Relaying { body, .. } if body.upstream() == Some(up) => body.pending(),
            ConnState::Relaying { .. }
            | ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Connecting { .. }
            | ConnState::AwaitingResponse { .. }
            | ConnState::Lingering { .. } => None,
        }
    }

    /// While `conn` forwards a request body: how many body bytes of its
    /// receive buffer are not yet written to the backend, and how long a
    /// chunked-framing tail it keeps at the front. `None` outside a body
    /// (or once it is fully written). For drivers' invariant checks (fill,
    /// drain, refill), not for I/O.
    #[doc(hidden)]
    pub fn request_body_pending(&self, conn: ConnId) -> Option<BodyPending> {
        match &self.conns.get(&conn)?.state {
            ConnState::Connecting { request, .. } | ConnState::AwaitingResponse { request, .. } => {
                request.body.pending()
            }
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Relaying { .. }
            | ConnState::Lingering { .. } => None,
        }
    }

    /// The earliest instant at which the core wants [`Event::TimerFired`],
    /// or `None` if it has no connections (every connection has a deadline).
    pub fn next_deadline(&self) -> Option<Instant> {
        self.conns.values().map(|c| c.state.deadline()).min()
    }
}
