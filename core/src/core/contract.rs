//! The driver contract and its vocabulary: connection ids, the events a
//! driver delivers, the actions it performs and the interest it waits for.
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
//! - The driver calls [`Core::set_date`] with the wall-clock time (whole
//!   seconds since the Unix epoch) whenever the second changes, before its
//!   next `handle` call (FR-HTTP-22a); it emits no action. Every final
//!   response the core renders from then on carries that second as
//!   `Date` (an interim `100 Continue` does not; a relayed response keeps
//!   the backend's). Until the first call no `Date` is sent.
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
//! - Every entry of [`Core::backends`] (distinct, in config order) is an
//!   IP address and port taken from the config, where host names are
//!   refused (FR-CFG-16), so the driver resolves nothing and does no DNS.
//!   The config also refuses unspecified, broadcast and multicast
//!   addresses; a numeric IPv6 scope id (link-local) is kept in the
//!   `SocketAddr` and used as given.
//!   A [`BackendId`] is an index into that list; `Connect` names the
//!   backend by id.
//! - The core allocates [`UpstreamId`]s, sequentially from 1, and never
//!   reuses one. A connection holds at most one upstream at a time (one
//!   fresh backend connection per request, FR-PROXY-02).
//! - `Connect { up, backend }` is a non-blocking `connect` (`SOCK_NONBLOCK
//!   | SOCK_CLOEXEC`). It is answered by exactly one `UpstreamConnected`
//!   (the socket became writable and `SO_ERROR` is 0) or `UpstreamFailed`
//!   (any connect error; reported as `ConnError::Other`, since the core
//!   treats every failed connect alike), unless `CloseUpstream { up }`
//!   comes first: then nothing more is delivered for that id. The answer
//!   always comes as a later event, never inside the `handle` call that
//!   emitted the `Connect`: a `connect()` that succeeds or fails at once
//!   is still reported as its own `UpstreamConnected` or `UpstreamFailed`
//!   after the actions of that call are done. A `socket()` that fails
//!   (`EMFILE`, `ENFILE`, `ENOBUFS`, ...) is a failed connect too:
//!   `UpstreamFailed { error: Other }`, so the client gets a 502
//!   ([`BackendFailure::Refused`]); the driver logs `EMFILE`/`ENFILE`
//!   distinctly (FR-LOG-07), since they mean the descriptor budget is
//!   wrong.
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
//!   body closes both connections, with no response, logged as
//!   [`Log::Rejected`] with `Body(ClosedEarly)`.
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
//! Deadlines are judged only when [`Event::TimerFired`] is handled
//! (owner decision 2026-10-27, DRV-06: delivery order decides). An event
//! handled before it counts even when `now` is already past its
//! connection's deadline: a body byte is progress, a completed head is
//! served, an answered open goes on. A driver that wakes late with I/O
//! and an overdue timer both ready may deliver either first, so every
//! deadline may be overrun, the total ones (`header_timeout`, the response
//! timeout, a response head's cap) and the disk wait included, by as long
//! as the loop was late: the events handled in that one late wake, before
//! `TimerFired`, still count.
//!
//! # Shutdown
//!
//! Three calls serve FR-PROC-05; each is made between two `handle` calls
//! (never between a read and its report), and `handle` keeps working
//! after any of them:
//!
//! - [`Core::begin_drain`]: on the first SIGTERM, after the driver closed
//!   its listeners. Idle keep-alive connections and connections without a
//!   byte of their request are closed at once (`Close`, nothing logged); a
//!   partly received head completes and is answered `503` with
//!   `Connection: close`, as is any request completed later, one
//!   pipelined behind a head queued keep-alive before the drain included;
//!   exchanges in flight finish (a head already queued keeps what it said,
//!   any head rendered from now on says `Connection: close`, and a request
//!   pipelined behind such a head is not answered), then take the normal
//!   close path (lingering close). An `Accepted` after it is answered 503 and
//!   closed like a refused connection (`Rejected` with `ShuttingDown`).
//! - [`Core::is_drained`]: true once no connection, upstream, open file or
//!   orphaned open is left; during the drain the driver polls it after
//!   every call and exits when it is true.
//! - [`Core::abort_all`]: on a second SIGTERM, a SIGINT or when
//!   `shutdown_timeout` expires. Every connection is closed at once, per
//!   connection `CloseFile`, `CloseUpstream`, `Close` as usual, each
//!   exactly once; each connection with an exchange in flight is logged
//!   first as `Rejected` with `ShuttingDown`, idle, lingering and refused
//!   connections are cut silently. Unanswered opens become orphaned opens
//!   (the core would answer a late `FileOpened` with `CloseFile` as
//!   always); the driver exits after this call without waiting for them
//!   (DRV-14), so `is_drained` is not polled after it.
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

use std::net::IpAddr;

#[cfg(doc)]
use super::Core;
#[cfg(doc)]
use crate::conn::{LINGER_SCRATCH, LINGER_TIMEOUT};
use crate::file::{FileError, FileId, FileMeta, FileStamp, RootId};
#[cfg(doc)]
use crate::log::BackendFailure;
use crate::log::Log;
#[cfg(doc)]
use crate::upstream::UPSTREAM_RECV_MIN;
use crate::upstream::{BackendId, UpstreamId};

/// Opaque connection id, allocated by the driver and never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnId(pub u64);

/// Transport-level failure reported by the driver, for a client
/// connection or an upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnError {
    /// The peer reset the connection (`ECONNRESET`, `EPIPE`).
    Reset,
    /// Any other socket error, including every reason a connect fails
    /// (`ECONNREFUSED`, `ETIMEDOUT`, `ENETUNREACH`, ...).
    Other,
}

/// Something that happened in the outside world. Each is delivered with the
/// current time via [`Core::handle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A new client connection was accepted. Always the first event for
    /// `conn`.
    Accepted { conn: ConnId, peer: IpAddr },
    /// The driver wrote `len` bytes into the slice returned by the last
    /// [`Core::recv_buffer`] call for `conn`, starting at its first byte.
    /// `len` is never 0 (end of stream is [`Event::ReadClosed`]). It must be
    /// the very next `handle` call after that read: no other event may come
    /// in between, or the core may have moved or reused that space.
    Received { conn: ConnId, len: usize },
    /// The peer closed its sending side (a read returned end of stream).
    ReadClosed { conn: ConnId },
    /// The socket accepted the first `len` bytes of [`Core::send_buffer`].
    Sent { conn: ConnId, len: usize },
    /// The socket failed; the driver will not do further I/O on it except
    /// what the core's resulting actions ask for (`Close`).
    Failed { conn: ConnId, error: ConnError },
    /// The instant last returned by [`Core::next_deadline`] has passed.
    /// After handling it, every deadline at or before `now` has been
    /// handled: `next_deadline` returns `None` or an instant after `now`.
    TimerFired,
    /// The `OpenFile` for `file` succeeded; `meta` describes the open
    /// descriptor.
    FileOpened { file: FileId, meta: FileMeta },
    /// An `OpenFile`, `Readahead` or `sendfile` for `file` failed.
    FileFailed { file: FileId, error: FileError },
    /// The last `Readahead` for `file` completed; `stamp` is the file's
    /// stamp when the chunk was read.
    ChunkReady { file: FileId, stamp: FileStamp },
    /// The socket accepted the first `len` bytes of the segment last
    /// returned by [`Core::send_file`] for `conn`.
    FileSent { conn: ConnId, len: u64 },
    /// The non-blocking connect of [`Action::Connect`] for `up` completed.
    UpstreamConnected { up: UpstreamId },
    /// The connect for `up` failed, or (after `UpstreamConnected`) its
    /// socket failed. The driver does no further I/O on it except what the
    /// core's resulting actions ask for (`CloseUpstream`).
    UpstreamFailed { up: UpstreamId, error: ConnError },
    /// The driver wrote `len` bytes (never 0) into the slice returned by the
    /// last [`Core::upstream_recv_buffer`] call for `up`, starting at its
    /// first byte. Like [`Event::Received`], it is the very next `handle`
    /// call after that read.
    UpstreamReceived { up: UpstreamId, len: usize },
    /// The backend closed its sending side (a read returned end of stream).
    UpstreamReadClosed { up: UpstreamId },
    /// The socket accepted the first `len` bytes (never 0) of
    /// [`Core::upstream_send_buffer`] for `up`.
    UpstreamSent { up: UpstreamId, len: usize },
}

/// Which readiness the driver should wait for on a connection or an
/// upstream. Replaces the previous interest entirely; a new connection or
/// upstream starts at `None` until the core says otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interest {
    /// Wait for nothing (e.g. receive buffer full: backpressure).
    None,
    /// Read into [`Core::recv_buffer`] (an upstream: into
    /// [`Core::upstream_recv_buffer`]) when readable.
    Read,
    /// Write from [`Core::send_buffer`], then from [`Core::send_file`]
    /// (an upstream: from [`Core::upstream_send_buffer`]), when writable.
    Write,
    /// Upstreams only: both at once. Read into
    /// [`Core::upstream_recv_buffer`] when readable and write from
    /// [`Core::upstream_send_buffer`] when writable. Each read and each
    /// write is still reported as the very next `handle` call, and the
    /// driver asks for both buffers afresh after every call (either may
    /// have become empty, or the interest may have changed). The core sets
    /// it while request body bytes wait for the backend, so a response the
    /// backend sends before taking the whole body is seen (FR-PROXY-27).
    /// Never set for a client connection.
    ReadWrite,
}

/// Something the driver must do, in the order emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Change what the driver waits for on `conn`.
    SetInterest { conn: ConnId, interest: Interest },
    /// Shut down the sending side of `conn` (half-close); reading may go on.
    ShutdownWrite { conn: ConnId },
    /// Close `conn`. The last action the core emits for it.
    Close { conn: ConnId },
    /// Open `path` (relative segments) beneath static root `root`, without
    /// following symlinks and without leaving the root, as `file`.
    OpenFile {
        file: FileId,
        root: RootId,
        path: Vec<Vec<u8>>,
    },
    /// Read `len` bytes of `file` from `offset` into the page cache, then
    /// report `ChunkReady`. Never beyond the size reported at open.
    Readahead { file: FileId, offset: u64, len: u64 },
    /// Close `file`; drop any completion still pending for it.
    CloseFile { file: FileId },
    /// Start a non-blocking connect to backend `backend` (an index into
    /// [`Core::backends`]) as upstream `up`; answer it with
    /// [`Event::UpstreamConnected`] or [`Event::UpstreamFailed`].
    Connect { up: UpstreamId, backend: BackendId },
    /// Change what the driver waits for on upstream `up`.
    SetUpstreamInterest { up: UpstreamId, interest: Interest },
    /// Close upstream `up`'s socket (also while its connect is pending).
    /// The last action the core emits for it.
    CloseUpstream { up: UpstreamId },
    /// Emit a log entry.
    Log(Log),
}
