# durchlauferhitzer — Design

Date: 2026-10-21
Status: approved

## Intent

A barebones, security-first HTTP server for Linux that serves static files
and forwards requests to backends (a minimal reverse proxy), routed by
virtual host and path prefix. "Barebones" is the feature: little code, almost
no dependencies, strict parsing, every case handled explicitly.

It will not face the internet until its owner trusts it. Trust comes from
strict red/green TDD against a fully simulated environment (network, clock,
file system) in which clients and backends can misbehave in every way that
matters.

## Goals

- Virtual hosts selected by the `Host` header.
- Per-host path-prefix routes, each either serving static files or proxying
  to one backend.
- Optional `strip_prefix;` flag on proxy routes: the backend sees the path
  with the route's own prefix removed (segment-wise, no regex).
- A small hand-rolled config format with SCSS-like nested blocks.
- A sans-IO core: all protocol logic is deterministic state machines that
  never touch a real socket, clock, or file. The same core runs under the
  simulation in tests and under an `epoll` driver in production.
- Pervasive enums with exhaustive matching, so the compiler points at every
  place a new state, event, or error must be handled.
- Performance work comes later, but nothing wasteful now: no needless copies,
  no unbounded buffers, no busy-waiting.

## Non-goals (for now)

- HTTPS / TLS of any kind (later stage; would need a pure-Rust crypto provider).
- Forward-proxy mode (absolute-form request targets, `CONNECT`).
- HTTP/2, HTTP/3, WebSockets / `Upgrade`.
- Regex or rewrite rules, wildcard hosts, load balancing across backends.
- Config hot-reload (restart instead; zero-downtime restart is a later stage).
- Directory listings, range requests, compression.
- Response caching (last stage, see timeline).
- io_uring (rejected: bypasses seccomp; poor kernel security track record).
  The driver interface is completion-shaped so it could be added later.

## Anti-goals

These would make the project a failure even if everything "works":

- Any C code in the repo or the dependency tree (glibc is the sole exception).
- A dependency tree that grows for convenience.
- Being usable as an open proxy: the destination of a proxied request is
  only ever a backend named in the config.
- Escaping a static root by any means (`..`, percent-encoding, symlinks).
- Request smuggling: any ambiguity in message framing is a hard rejection.
- A slow client, slow disk, or slow backend stalling other connections.

## Hard constraints (from the owner)

- Rust only; glibc is the only C allowed. No `cc`/`cmake` build scripts, no
  `*-sys` crates except `libc`, no `ring`/`aws-lc-rs`/OpenSSL. This binds
  the shipped binary; test tooling (e.g. `cargo-fuzz`) may contain C. CI
  fails if the product's dependency tree contains anything but `libc`.
- Minimal external crates in the shipped product. Planned total: `libc`
  (driver only). Test-only tooling may use established crates freely
  (e.g. property testing, fuzzing, seeded randomness); the simulation
  harness itself is ours, since it drives our sans-IO core.
- Strict red/green TDD: every behavior starts as a failing test.
- Everything runs on a simulated network/clock/file system in tests, with
  fault injection (drops, resets, stalls, byte-by-byte delivery, slow disk,
  files changing or vanishing).
- Async event loop: `epoll` for sockets, `sendfile` for static files.
- Core is explicit state machines (not `async`/`await`).
- Exhaustive enum handling, enforced by lints.
- `strip_prefix` is the option name.

## Architecture

```
          ┌──────────────── core (no I/O, no unsafe, no deps) ───────────────┐
 Events → │ config · HTTP/1.1 parser · routing · connection state machines   │ → Actions
          └──────────────────────────────────────────────────────────────────┘
                 ▲                                              ▲
       simulation driver (tests)                     epoll driver (production)
       virtual net · clock · fs                      sockets · eventfd · readahead pool
```

### The core

- Input: `Event` enum, e.g. client accepted, bytes received, peer closed,
  write completed, backend connected / failed, file opened / chunk ready /
  file error, timer fired. Every event carries the current monotonic time.
- Output: `Action` enum, e.g. write bytes, `sendfile` a file range, open a
  backend connection, open a file, `readahead` a chunk, close, set the next
  deadline, emit a log entry.
- Per-connection state is an enum whose variants carry only data valid in
  that state (e.g. only a forwarding state owns a backend handle). Each
  transition consumes the old state and returns the new one plus actions.
- Connections and files are referred to by opaque ids allocated by the
  driver; the core never holds OS handles.
- Single-threaded, no locks. Config is immutable after load.

### Lints (core and everywhere unless noted)

- `#![forbid(unsafe_code)]` in the core; the driver confines `unsafe` to a
  small, documented `libc` wrapper module.
- Deny: `clippy::wildcard_enum_match_arm`, `clippy::unwrap_used`,
  `clippy::expect_used`, `clippy::panic`, `clippy::indexing_slicing`.
  Untrusted input yields error enums, never panics.

### Drivers

- **Simulation driver** (tests): virtual network with per-connection fault
  scripts, virtual clock advanced explicitly, in-memory file system (with
  symlinks, files that change or vanish, slow reads). Records every event and
  action so failures print the exact transition sequence. A
  seeded PRNG drives randomized fault schedules; the seed is printed on
  failure for replay.
- **epoll driver** (production): non-blocking sockets on `epoll`; timers via
  the `epoll_wait` timeout from the core's next deadline; monotonic clock.
  Static files: a small thread pool runs `readahead` for the next chunk
  (the only place that may block on disk) and reports completion through an
  `eventfd`; the loop then does a non-blocking `sendfile` of that chunk, and
  parks the connection on `EPOLLOUT` if the client is slow. A slow client
  costs a parked connection, never a thread. Files are opened with
  `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` relative to the static
  root.
- All threading and multiplexing lives in the driver. Multi-core later =
  N driver threads, each with its own `epoll` and core, sharing the port via
  `SO_REUSEPORT`.

## HTTP behavior

HTTP/1.1 and 1.0, origin-form targets only. Strict by default: when in doubt,
reject.

- **Framing:** CRLF line endings only; no obs-fold; header names are tokens.
  Reject (400): both `Content-Length` and `Transfer-Encoding`; multiple or
  non-numeric `Content-Length`; any `Transfer-Encoding` other than exactly
  `chunked`; malformed chunk framing.
- **Targets:** absolute-form, authority-form, asterisk-form → 400. Reject
  (400) paths containing `.` or `..` segments, `//`, `%2F`, `%5C`, `%00`,
  backslash, or non-ASCII bytes, and paths with a percent escape that
  decodes to an unreserved character (ALPHA, DIGIT, `-`, `.`, `_`, `~`,
  in either hex case, e.g. `/%61ssets`): RFC 3986 treats `%61` and `a` as
  the same, so allowing both spellings would let a request route past a
  more specific route. The query is not subject to this rule. After this
  check the raw path is canonical (only the hex case of the remaining
  escapes may vary); routing and forwarding use it as-is, static serving
  percent-decodes it. Configured route paths follow the same rules.
- **Host:** required for HTTP/1.1 (400 if missing or duplicated). Matched
  case-insensitively, port ignored, exact names only. No matching host → 404.
- **Routing:** route prefixes are lists of path segments (`/api/v1` is
  `["api", "v1"]`; a trailing `/` in config is insignificant, so `path /api`
  and `path /api/` are the same route). A route matches when the request
  path's segments start with the route's segments, so `/api` matches `/api`,
  `/api/` and `/api/x` but never `/apix`. The route with the most segments
  wins. No match → 404.
- **Keep-alive** with clients; one request at a time per connection (the next
  request is not read until the response is done). Pipelined bytes are
  buffered within limits.
- **Limits and timeouts:** request line, header section size, header count,
  concurrent connections, header-read timeout, keep-alive
  idle timeout, backend connect timeout, backend response timeout, body idle
  timeout, send idle timeout (client stops reading the response), and
  connections per client IP. All have conservative defaults and can be
  overridden in the `limits` block. Exceeding one gives the matching status
  (400/408/413/431/503) and closes the connection.
- **Slow clients (design call):** the header-read timeout is a *total*
  deadline for the whole request head, not an idle timeout, so trickling
  bytes does not extend it. Body transfer in either direction uses idle
  timeouts. A client stalling a proxied response also releases the backend
  connection when its send idle timeout fires. The per-IP connection cap
  keeps a single host from filling `max_connections`. Minimum transfer rates
  are not in scope.

### Streaming and flow control

- No body is ever held whole. Each connection has fixed-size buffers; a
  50 GB transfer uses the same memory as a 5 KB one. Timeouts measure
  stalls, not total duration.
- Backpressure with hysteresis: stop reading a side when its buffer is
  full; resume only when it has drained below the low watermark (half).
  This avoids tiny reads/writes and frequent short pauses (which push the
  upstream TCP connection back into slow start).
- Write in large pieces with `TCP_NODELAY` set; never set `SO_RCVBUF` /
  `SO_SNDBUF` (keeps kernel autotuning).
- Responses stream as soon as their headers are validated; chunked framing
  is validated chunk by chunk in flight.
- Simulation tests assert bounded memory, intact bytes, watermark
  behavior, and write sizes with bodies far larger than the buffers. TCP
  congestion behavior itself is measured in stage 7 over loopback with
  `tc netem` (delay, loss), including recovery after pauses.

### Static routes

- Static routes strip their own prefix (alias semantics): on
  `path /assets { static /srv/assets; }`, `/assets/a.css` serves
  `/srv/assets/a.css`. The remaining path is percent-decoded exactly once into
  byte segments (non-UTF-8 names allowed).
- Methods GET and HEAD; others → 405 with `Allow: GET, HEAD`. A request with
  a body → 413.
- Directory requested with a trailing `/` → its `index.html` if it exists,
  else 404. Without the `/` → 301 to the same path plus `/` (query kept). No
  listings. A redirect that would not fit the fixed per-connection send
  buffer (path and query are bounded only by `max_request_line`) → 414
  instead, with the same persistence and HEAD rules; the send buffer never
  grows beyond its capacity.
- Symlinks anywhere in the path, dotfiles (any decoded segment starting with
  `.`), missing files, permission denied and non-regular files → 404, so the
  response does not reveal which.
- Dotfiles are only reachable through an explicit route whose prefix names
  them, e.g. `path /.well-known { static /srv/well-known; }` (ACME
  challenges, security.txt); dotfiles below that route are still 404.
- `examples/durchlauferhitzer.conf` documents every config key; a test
  keeps it loadable.
- `Content-Type` from a small built-in extension table (see the stage 2
  plan), default `application/octet-stream`; always send
  `X-Content-Type-Options: nosniff`.
- HEAD gets the head with the full `Content-Length` and no body; error
  responses to HEAD carry no body either.
- Waiting on the disk counts against `send_idle_timeout`.
- A file that changes during a transfer: per static route,
  `on_file_change abort | allow_growth;` (optional, default `abort`). The
  values are the only legal combinations. `abort` cuts the connection (after
  the headers were sent) on any change since open — growth, rewrite or
  shrink — and logs it. `allow_growth` keeps sending exactly the size seen at
  open when the file only got larger; a rewrite at the same size or a shrink
  still aborts. No setting tolerates rewrites or shrinks. Caveat: a rewrite
  that also makes the file larger looks like growth and is accepted under
  `allow_growth`.

### Proxy routes

- Any method token is forwarded. One fresh backend connection per request,
  `Connection: close` to the backend (pooling is later).
- `strip_prefix;` (a flag, no value) removes the route's own segments from
  the request path: on route `/api`, `/api/x` → `/x`, `/api/x/` → `/x/`,
  `/api` and `/api/` → `/`. The query string is unchanged. Partial stripping
  is not supported (could return later as a segment count). On route `/` the flag
  is accepted and has no effect.
- Strip hop-by-hop headers in both directions (`Connection` and the headers
  it lists, `Keep-Alive`, `Proxy-*`, `TE`, `Trailer`, `Upgrade`), and append
  `X-Forwarded-For` and set `X-Forwarded-Proto: http`. `Host` is forwarded
  unchanged.
- Bodies stream in both directions with bounded buffers and backpressure;
  framing of both request and response is parsed strictly.
- Backend connect failure or reset before response headers → 502; timeout →
  504. After response headers were sent, failures close the client
  connection.
- Backend addresses are `host:port`, resolved once at startup.
- Every proxy route must set both `max_request_body` and
  `max_response_body` (each a size or `unlimited`); there are no defaults.
- Request body over the limit → 413 before forwarding when
  `Content-Length` says so; otherwise (chunked) both connections are cut
  once the limit is passed.
- Response body over the limit → 502 before any response bytes reach the
  client when the backend's `Content-Length` says so; otherwise both
  connections are cut once the limit is passed (the client sees an
  incomplete response, never a silently truncated "complete" one).

## Configuration

Hand-rolled parser. SCSS-like in spirit (nested blocks, `;`-terminated
settings), not in syntax: wherever plain SCSS syntax would need clever
disambiguation, the grammar is simpler instead. Every character has one
meaning everywhere; no token is split and re-glued.

```
file      = { item }
item      = setting | block
setting   = key { value } ";"         (no value = a flag)
block     = name { arg } "{" { item } "}"
word      = run of non-whitespace excluding ";" "{" "}" "#" '"'
string    = '"' { char | '\"' | '\\' } '"'   (one line)
comment   = "#" to end of line
```

`key`, `name` are words; `value`, `arg` are words or strings. `:` is an
ordinary character, so `127.0.0.1:8080` and `[::1]:8080` are plain words.

Example:

```scss
listen 127.0.0.1:8080;
user www-data;            # drop privileges to this user after binding

limits {
  max_connections 1024;
  header_timeout 10s;
}

host example.com www.example.com {
  path / { static /srv/www; }
  path /api/ {
    proxy 127.0.0.1:3000;
    strip_prefix;
    max_request_body 10MiB;
    max_response_body unlimited;
  }
}
```

- Unknown keys, duplicate keys, duplicate hosts or paths within a host, a
  `path` with both or neither of `static`/`proxy`, `strip_prefix` on a static
  route, a value on `strip_prefix` (it is a flag), a missing value on any
  other setting, two `path` blocks in one host that name the same segments
  (e.g. `/api` and `/api/`), duplicate or port-0 `listen` addresses,
  `max_head` above 1 MiB, a proxy route
  missing `max_request_body` or `max_response_body`, or either of them on a
  static route → startup
  error naming the line and column.
- Durations accept `ms`/`s`/`m`; sizes accept plain bytes and `KiB`/`MiB`/`GiB`.

## Logging

The core emits `Log` actions as enum variants (one per kind: request served,
client error, backend failure, file aborted, limit hit, …). The driver
writes each as one `key=value` line to stderr. Logging never blocks: if
stderr is backed up, entries are dropped and counted, and the count is logged
once output catches up. Tests assert on `Log` actions directly.

## Process

- `durchlauferhitzer <config-path>`; `--check` validates the config and exits.
- **Platform:** Linux ≥ 5.14, x86_64 and aarch64, glibc. `openat2` is
  required (startup error without it). Landlock is used when available;
  if not, a clear warning is logged at startup and the server runs.
- **systemd is optional.** If systemd passes listening sockets (socket
  activation, `LISTEN_FDS`), they are used and the process never needs
  root. Otherwise it binds itself: starts as root only if it needs a port
  below 1024, binds, then switches to `user` (refuses to keep running as
  root if `user` is unset).
- SIGTERM: stop accepting, finish in-flight requests up to a deadline, exit.

## Timeline

Each stage is a separately planned and reviewed chunk that leaves the
project working and tested.

1. **Foundations** — workspace, lints, config parser + validation, core
   `Event`/`Action`/state enums, simulation harness (net + clock), strict
   HTTP/1.1 request parsing, vhost + path routing answering 404/400/4xx with
   fixed responses, limits and timeouts. No real I/O yet.
2. **Static files** — simulated file system, path decoding and validation,
   open/readahead/sendfile actions, index.html, content types, HEAD,
   mid-transfer failures.
3. **Reverse proxy** — backend connections in the simulation, header
   handling, `strip_prefix`, body streaming with backpressure, 502/504,
   misbehaving backends.
4. **epoll driver + binary** — the real driver, readahead pool, `eventfd`,
   `sendfile`, `openat2`, logging output, CLI, privilege drop, systemd
   socket activation, SIGTERM drain.
   A few smoke tests over real loopback sockets. First runnable release.
5. **Containment** — Landlock (restrict file access to static roots),
   seccomp (allow-list of syscalls), long randomized simulation runs.
6. **Zero-downtime restart** — with systemd: socket activation; on
   restart the old process drains while new connections wait in the
   kernel's accept queue, then the new process starts fresh. Without
   systemd: on a signal, exec the new binary with the listening sockets
   inherited; the new process validates config (exits on error, old keeps
   serving), starts accepting; old stops accepting, drains, exits. Because
   the new process inherits the old sandbox, such a restart can change
   hosts, routes, backends and limits only; a new `listen`, `user`, or
   static root needs a full restart (documented, and rejected with a clear
   error rather than half-applied).
7. **Scaling and efficiency** — N driver threads with `SO_REUSEPORT`,
   backend connection pooling, conditional requests (ETag / Last-Modified),
   `MSG_MORE` to coalesce response headers with file data, `splice` for
   proxied bodies with a known `Content-Length`, the `tc netem` throughput
   benchmarks, profiling.
8. **Backend-directed file serving** — a backend response header (like
   nginx's `X-Accel-Redirect`) asks the proxy to serve a file via
   `sendfile`. Only honored on proxy routes that name an explicit root
   (`internal_files <dir>;`); the path goes through the same validation and
   `openat2` jail as static routes; the header is never accepted from
   clients and always stripped; backend status and headers such as
   `Content-Type` / `Content-Disposition` are kept, its body discarded.
   Useful when the files are on the proxy's machine (same host or shared
   storage).
9. **Later, maybe** — HTTPS with a pure-Rust crypto provider, io_uring driver,
   file-descriptor passing from same-host backends (`SCM_RIGHTS`).
10. **Caching, after everything else** — a small Varnish-style response
    cache in front of proxy routes, with far fewer features than Varnish.

## Left to the builder

- Exact default values for limits and timeouts (conservative, documented).
- Module layout within crates, names of enum variants, buffer strategy.
- Edge-triggered vs level-triggered `epoll`, readahead chunk size, pool size.
- Exact content-type table (common web types).
- `Expect: 100-continue` handling, as long as it is explicit and tested.
