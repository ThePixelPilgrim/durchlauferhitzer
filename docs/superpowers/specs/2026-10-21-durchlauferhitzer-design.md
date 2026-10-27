# durchlauferhitzer — Design

Date: 2026-10-21 · Requirements consolidated: 2026-10-25
Status: approved — **single source of truth** for what the product must do.

## How to read this spec

- Every requirement has a stable ID (`FR-HTTP-07`, `NFR-RES-04`, `DRV-05`).
  IDs are never renumbered or reused; a dropped requirement stays listed as
  *withdrawn*. Plans, reviews, ledgers and commit messages cite IDs instead
  of restating requirements. A new owner decision is written here first
  (new ID or edited text), then cited.
- Status tags: **[done]** implemented and tested (stages 1–2, and stage 3
  Tasks 1–3); **[stage N]** due in that stage of the timeline; **[open]**
  waits for an owner decision (see "Open decisions" at the end). "since
  stage N" notes when a decision was taken.
- Rationale is kept short and next to its requirement, in *italics*.
- Exact enum names, module layout and buffer sizes are implementation
  details unless a requirement names them; the driver contract lives in the
  module docs of `core/src/core/mod.rs` (summarised as DRV-…).

## Intent

A barebones, security-first HTTP server for Linux that serves static files
and forwards requests to backends (a minimal reverse proxy), routed by
virtual host and path prefix. "Barebones" is the feature: little code, almost
no dependencies, strict parsing, every case handled explicitly.

It will not face the internet until its owner trusts it. Trust comes from
strict red/green TDD against a fully simulated environment (network, clock,
file system, backends) in which clients and backends can misbehave in every
way that matters.

## Goals

- Virtual hosts selected by the `Host` header (FR-ROUTE).
- Per-host path-prefix routes, each either serving static files (FR-STATIC)
  or proxying to one backend (FR-PROXY); optional `strip_prefix;` on proxy
  routes (segment-wise, no regex).
- A small hand-rolled config format with SCSS-like nested blocks (FR-CFG).
- A sans-IO core: all protocol logic is deterministic state machines that
  never touch a real socket, clock, or file. The same core runs under the
  simulation in tests and under an `epoll` driver in production.
- Pervasive enums with exhaustive matching, so the compiler points at every
  place a new state, event, or error must be handled (NFR-MAINT).
- Performance work comes later, but nothing wasteful now: no needless copies,
  no unbounded buffers, no busy-waiting (NFR-RES, NFR-PERF).

## Non-goals (for now)

- HTTPS / TLS of any kind (later stage; would need a pure-Rust crypto provider).
- Forward-proxy mode (absolute-form request targets, `CONNECT`).
- HTTP/2, HTTP/3, WebSockets / `Upgrade`.
- Regex or rewrite rules, wildcard hosts, load balancing across backends.
- Config hot-reload (restart instead; zero-downtime restart is stage 6).
- Directory listings, range requests, compression.
- Response caching (last stage, see timeline).
- Minimum transfer rates (slow-but-moving peers are bounded by caps and
  idle timeouts only).
- io_uring (rejected: bypasses seccomp; poor kernel security track record).
  The driver interface is completion-shaped so it could be added later.

## Anti-goals

These would make the project a failure even if everything "works":

- Any C code in the repo or the product's dependency tree (glibc is the sole
  exception) — NFR-MAINT-01.
- A dependency tree that grows for convenience — NFR-MAINT-03.
- Being usable as an open proxy: the destination of a proxied request is
  only ever a backend named in the config — NFR-SEC-08.
- Escaping a static root by any means (`..`, percent-encoding, symlinks) —
  NFR-SEC-04.
- Request smuggling: any ambiguity in message framing is a hard rejection —
  NFR-SEC-01.
- A slow client, slow disk, or slow backend stalling other connections —
  NFR-SEC-11.

## Hard constraints (from the owner)

Each is a requirement below: Rust only, glibc the only C (NFR-MAINT-01/02);
minimal crates, `libc` the only planned product dependency (NFR-MAINT-03/04);
strict red/green TDD (NFR-MAINT-09); everything tested on a simulated
network/clock/file system/backends with fault injection (NFR-TEST-01);
`epoll` for sockets, `sendfile` for static files (DRV-11, DRV-08); core as
explicit state machines, not `async` (NFR-MAINT-08); exhaustive enums
enforced by lints (NFR-MAINT-06); `strip_prefix` is the option name
(FR-CFG-17).

## Architecture

```
          ┌──────────────── core (no I/O, no unsafe, no deps) ───────────────┐
 Events → │ config · HTTP/1.1 parsers · routing · connection state machines  │ → Actions
          └──────────────────────────────────────────────────────────────────┘
                 ▲                                              ▲
       simulation driver (tests)                     epoll driver (production)
       virtual net · clock · fs · backends           sockets · eventfd · worker pool
```

### The core

- Input: `Event` enum, e.g. client accepted, bytes received, peer closed,
  write completed, backend connected / failed / received, file opened /
  chunk ready / file error, timer fired. Every `handle` call carries the
  current monotonic time.
- Output: `Action` enum, e.g. set interest, shut down writing, close, open a
  backend connection, open a file, `readahead` a chunk, close a file, emit a
  log entry. Bytes are not carried in actions: the driver reads into and
  writes from core-owned buffers, and `sendfile`s the file segment the core
  exposes (DRV-04, DRV-08).
- Per-connection state is an enum whose variants carry only data valid in
  that state (e.g. only a forwarding state owns an upstream id), each with
  its own deadline. Each transition consumes the old state and returns the
  new one plus actions.
- Connections, files and upstreams are referred to by opaque ids:
  connection ids are allocated by the driver; file ids and upstream ids by
  the core (sequentially from 1, never reused); backends by index into
  `Core::backends()`, static roots by index into `Core::static_roots()`.
  The core never holds OS handles.
- Single-threaded, no locks. Config is immutable after load.
- Lints: NFR-ROB-01 (no panics), NFR-MAINT-05 (`unsafe`), NFR-MAINT-06
  (exhaustive enums). Tests may use `unwrap`/`expect`/`panic!`/indexing
  (`clippy.toml` `allow-*-in-tests`).

### Drivers

- **Simulation driver** (tests, `sim/`): see NFR-TEST.
- **epoll driver** (production, stage 4): non-blocking sockets on `epoll`;
  timers via the `epoll_wait` timeout from the core's next deadline;
  monotonic clock. Static files: a small worker pool runs the opens
  (`openat2` + `fstat`) and `readahead` for the next chunk (the places that
  may block on disk) and reports completion through an `eventfd`; answers
  may arrive in any order. The loop then does a non-blocking `sendfile` of
  that chunk, and parks the connection on `EPOLLOUT` if the client is slow.
  A slow client costs a parked connection, never a thread. Requirements on
  every driver: DRV-…; the full contract is in the `core` module docs.
- All threading and multiplexing lives in the driver. Multi-core later =
  N driver threads, each with its own `epoll` and core, sharing the port via
  `SO_REUSEPORT` (NFR-PERF-07).

---

## Functional requirements

### FR-CFG — Configuration language and validation

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

`key`, `name` are words; `value`, `arg` are words or strings.

Example (the full, commented reference is `examples/durchlauferhitzer.conf`):

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

Syntax:

- **FR-CFG-01** [done] The config is parsed by the grammar above. A setting
  with no value is a flag.
- **FR-CFG-02** [done] Whitespace separates tokens; `;`, `{`, `}` are
  single tokens; `"` and `#` end a preceding word. `#` starts a comment to
  end of line, also in the middle of a word. `:` is an ordinary character,
  so `127.0.0.1:8080` and `[::1]:8080` are plain words. *(since stage 1:
  settings are `key value;` without a colon)*
- **FR-CFG-03** [done] A string is double-quoted on one line; inside it
  only `\"` and `\\` are escapes, and `#`, `;`, `{`, `}`, `:` and spaces
  are ordinary. Any other escape is an error at the backslash; a newline or
  end of input before the closing quote is an error at the opening quote.
- **FR-CFG-04** [done] Control characters other than TAB, LF and CR are
  rejected anywhere, strings included.
- **FR-CFG-05** [done] Blocks nest at most 8 deep (counting the block
  itself); a 9th level is an error at its name. *Hostile configs cannot
  recurse deeply.*
- **FR-CFG-06** [done] Every syntax or validation error stops startup and
  names the 1-based line and column of the offending item
  (`"<line>:<column>: <message>"`).

Structure and values:

- **FR-CFG-07** [done] Unknown keys, and known names used in the wrong form
  (setting vs. block), are errors.
- **FR-CFG-08** [done] Duplicate keys at any level are errors, including a
  second `limits` block.
- **FR-CFG-09** [done] Every setting takes exactly its documented number of
  values; a flag takes none. A missing value, or a value on a flag, is an
  error.
- **FR-CFG-10** [done] Top level allows only `listen` (required; one or
  more IP socket addresses; no duplicates; no port 0), `user` (optional),
  at most one `limits` block and one or more `host` blocks.
  Overlapping binds such as `0.0.0.0:80` + `127.0.0.1:80` are only detected
  at bind time [stage 4].
- **FR-CFG-11** [done] `user` matches `[a-z_][a-z0-9_-]{0,31}`.
- **FR-CFG-12** [done] A `host` block takes one or more names: ASCII
  letters, digits, `-` and `.` without a leading or trailing `.`, an IPv4
  literal, or a bracketed IPv6 literal (`[::1]`); names are stored
  lowercase. A name appearing in two `host` blocks is an error. A `host`
  block contains only `path` blocks. (Known gaps: `-a-`, `999.999.1.1`,
  DNS label lengths and IPv6 spelling normalisation are not checked — see
  open decisions.)
- **FR-CFG-13** [done] A `path` block takes exactly one argument that
  starts with `/` and obeys the request-path rules FR-HTTP-12…16; a `?` in
  it is an error. A trailing `/` is insignificant. Two `path` blocks in one
  host whose segments are equal after percent-decoding (e.g. `/api` and
  `/api/`, `/@a` and `/%40a`) are duplicates. *(since stage 2: decoded
  comparison)*
- **FR-CFG-14** [done] A route has exactly one of `static <root>` or
  `proxy <backend>`.
- **FR-CFG-15** [done] A static root is an absolute path without `.`, `..`
  or empty components and without a trailing `/` (except `/` itself). That
  it exists is checked by the driver at startup [stage 4].
- **FR-CFG-16** [done] A proxy backend is `host:port`, split at the last
  `:`; host is a host name (FR-CFG-12 rules), an IPv4 literal or a
  bracketed IPv6 literal; port 1–65535.
- **FR-CFG-17** [done] `strip_prefix;` is a flag, allowed only on proxy
  routes; on route `/` it is accepted and has no effect. *(since stage 1
  Task 11: the earlier `strip_prefix <value>` form is withdrawn)*
- **FR-CFG-18** [done] Every proxy route sets both `max_request_body` and
  `max_response_body`, each a size or `unlimited`; there are no defaults.
  Static routes may set neither.
- **FR-CFG-19** [done] `on_file_change abort | allow_growth;` is optional
  on static routes (default `abort`) and an error on proxy routes.
- **FR-CFG-20** [done] The `limits` block may override these defaults:

  | key | default | status |
  |---|---|---|
  | `max_connections` | 1024 | [done] |
  | `max_connections_per_ip` | 64 | [done] |
  | `max_request_line` | 8KiB (excluding CRLF) | [done] |
  | `max_head` | 16KiB (request line + headers + final CRLF) | [done] |
  | `max_headers` | 100 | [done] |
  | `header_timeout` | 10s | [done] |
  | `keepalive_timeout` | 60s | [done] |
  | `send_idle_timeout` | 30s | [done] |
  | `backend_connect_timeout` | 5s | [done] |
  | `backend_response_timeout` | 60s | [done] |
  | `body_idle_timeout` | 30s | [done] |

- **FR-CFG-21** [done] Counts and sizes in `limits` are ≥ 1, durations are
  non-zero, `max_request_line` ≤ `max_head`, and `max_head` ≤ 1 MiB.
  (Upper bounds for the other limits: open decision.)
- **FR-CFG-22** [done] Durations are `<u64>` followed by `ms`, `s` or `m`;
  sizes are plain bytes or `<u64>` followed by `KiB`, `MiB` or `GiB`; any
  other form, or an overflowing value, is an error.
- **FR-CFG-23** [done] The config is immutable after load; there is no
  hot reload (FR-PROC-07 covers restarts).
- **FR-CFG-24** [done] `examples/durchlauferhitzer.conf` documents every
  config key with its default and the user-visible behaviour; a test keeps
  it loadable. Each stage that adds a key updates it (stage 3: the three
  backend limits, forwarded headers, 502/504, `Expect`).

### FR-HTTP — Request parsing, targets, framing, fixed responses

HTTP/1.1 and 1.0, origin-form targets only. Strict by default: when in
doubt, reject.

Request head:

- **FR-HTTP-01** [done] Versions `HTTP/1.1` and `HTTP/1.0` are accepted;
  another `HTTP/<digit>.<digit>` → 505; anything else → 400.
- **FR-HTTP-02** [done] The request line is `method SP target SP version
  CRLF` with single spaces; leading empty lines are not skipped (→ 400).
  The method is an RFC 9110 token, case-sensitive (`GET`, `HEAD`).
- **FR-HTTP-03** [done] Lines end in CRLF only; a bare LF → 400.
- **FR-HTTP-04** [done] A header line is `name ":" OWS value OWS CRLF`;
  the name is a token (whitespace before the colon → 400); a line starting
  with SP or HTAB (obs-fold) → 400.
- **FR-HTTP-05** [done] Header values consist of HTAB, SP and VCHAR
  only. CR, LF, NUL, other control bytes and obs-text (bytes ≥ 0x80) →
  400. (Owner decision: obs-text is rejected rather than forwarded, to make
  the ecosystem safer; previously accepted per RFC 9110. Backend side:
  FR-PROXY-13.)
- **FR-HTTP-06** [done] Head limits: a request line longer than
  `max_request_line` → 414; more than `max_headers` header lines → 431; a
  head longer than `max_head` → 431. Each verdict is given as soon as it is
  certain (no LF within `max_request_line + 2` bytes → 414 while the head is
  still incomplete). A receive buffer that fills without a complete head is
  answered by the same rules. *The `+ 2` keeps the verdict split-independent:
  `max_request_line + 1` bytes could be a full line plus its CR.*
- **FR-HTTP-07** [done] `Host`: an HTTP/1.1 request carries exactly one
  (missing or duplicated → 400); HTTP/1.0 may omit it. The value is a
  reg-name of letters, digits, `-` and `.`, or a bracketed IPv6 literal,
  with an optional `:port` of 1–65535; anything else → 400. It is compared
  lowercase. Host checks run before framing checks.
- **FR-HTTP-08** [done] Body framing (all → 400): `Transfer-Encoding` in
  HTTP/1.0; both `Content-Length` and `Transfer-Encoding`; more than one
  `Content-Length`; a `Content-Length` that is not 1+ digits or does not
  fit `u64`; more than one `Transfer-Encoding` header, or a value other
  than exactly `chunked` (case-insensitive). Leading zeros in
  `Content-Length` are accepted; `Content-Length: 0` means no body. The
  checks run in this order, so the reported error is deterministic.
- **FR-HTTP-09** [done] Persistence: HTTP/1.1 stays open unless
  `Connection` contains the token `close` (comma-separated,
  case-insensitive); HTTP/1.0 always closes (`Connection: keep-alive` is
  ignored).
- **FR-HTTP-09a** [done] A `Connection` header that lists
  `content-length`, `transfer-encoding` or `host` (case-insensitive, in any
  `Connection` line) → 400, on every route. It is the first check on the
  whole head (after all line checks), before the Host and framing checks,
  so the reported error is deterministic. *Owner decision 2026-10-26:
  removing those as hop-by-hop headers (FR-PROXY-06) would change the
  message's framing or target on the way to the backend, a request-smuggling
  vector; RFC 9110 §7.6.1 already forbids senders to list them.*
- **FR-HTTP-10** [done] Every parse error is answered with its status
  (FR-HTTP-01, -06; all others 400) and closes the connection
  (FR-CONN-05).

Request target:

- **FR-HTTP-11** [done] Only origin-form targets (`/path[?query]`) are
  accepted; absolute-form, authority-form and asterisk-form → 400. The
  target splits at the first `?`.
- **FR-HTTP-12** [done] Path and query reject (400) bytes ≥ 0x80, bytes
  ≤ 0x20 and 0x7F, `\`, and `#` (no fragments). Every `%` is followed by
  two hex digits (either case), else 400.
- **FR-HTTP-13** [done] The path rejects (400) a `.` or `..` segment and an
  empty segment (`//`); a trailing `/` is allowed.
- **FR-HTTP-14** [done] The path rejects (400) the escapes `%2E` (dot),
  `%2F` (`/`), `%5C` (`\`), `%00`, `%01`–`%1F` and `%7F` (controls), in
  either hex case. *Static serving decodes the path; decoded controls would
  allow log injection and odd file names.*
- **FR-HTTP-15** [done] The path rejects (400) `;`, raw or as `%3B`.
- **FR-HTTP-15a** [done] The path rejects (400) `%3F` (encoded `?`): a
  backend that decodes before splitting off the query would see a
  different path and query than the router did.
  *(since stage 2; blocks the Tomcat-style `..;/` bypass when the raw path
  is forwarded)*
- **FR-HTTP-16** [done] The path rejects (400) an escape that decodes to an
  unreserved character (ALPHA, DIGIT, `-`, `_`, `~`; `.` per FR-HTTP-14),
  e.g. `/%61ssets`. *(since stage 2) RFC 3986 treats `%61` and `a` as the
  same; allowing both spellings would let a request route past a more
  specific route. Kept as defence in depth next to FR-ROUTE-05.*
- **FR-HTTP-17** [done] Other escapes (reserved and other characters such
  as `%20`, `%25`, `%40`, `%C3%A4`) may be sent raw or escaped, in
  either hex case. A path is percent-decoded at most once (static serving);
  `%25` never leads to a second decode.
- **FR-HTTP-18** [done] The query is subject only to FR-HTTP-12; it may
  contain `;`, `%3B` and encoded unreserved characters, and is never
  canonicalised.

Core-generated responses:

- **FR-HTTP-19** [done] Responses are `HTTP/1.1`. A fixed (error or
  status) response is exactly: status line, `Content-Type: text/plain;
  charset=utf-8`, `Content-Length`, `X-Content-Type-Options: nosniff`,
  `Connection: close` only when the connection will close, blank line, body
  `"<code> <reason>\n"`.
- **FR-HTTP-20** [done] 405 carries `Allow: GET, HEAD`.
- **FR-HTTP-21** [done] Every response to a request whose method was parsed
  as `HEAD` carries the head only (with the full `Content-Length`), errors
  included: missing `Host`, header and framing errors after the request
  line, 301, 404, 405, 413, the 414 for an oversized redirect, and the
  408, 502 and 504 of proxy routes. An error in the request line itself is
  answered with a body, since no method was accepted. *(since stage 2;
  supersedes the stage 2 Task 3 ruling "parse errors keep a body")*
- **FR-HTTP-22** [done] Core-generated responses carry no `Server` header.
- **FR-HTTP-22a** [stage 4] Core-generated responses carry a `Date` header;
  the driver supplies the wall-clock time with each event (the core stays
  free of clocks).

### FR-ROUTE — Hosts, segments, decoded matching

- **FR-ROUTE-01** [done] The virtual host is chosen by `Host`: lowercased,
  port removed (for `[v6]:port` the bracketed part is kept), then compared
  exactly against the configured names. No other normalisation: a trailing
  dot does not match.
- **FR-ROUTE-02** [done] No `Host` (HTTP/1.0) or an unknown host → 404.
- **FR-ROUTE-03** [done] Route prefixes are lists of path segments
  (`/api/v1` is `["api", "v1"]`, `/` is `[]`); a trailing `/` is
  insignificant. *(since stage 1 Task 11)*
- **FR-ROUTE-04** [done] A route matches when the request path's segments
  start with the route's segments: `/api` matches `/api`, `/api/` and
  `/api/x`, never `/apix`.
- **FR-ROUTE-05** [done] Segments are compared percent-decoded (hex case
  ignored), so `/%40a/x` and `/%c3%a4/x` reach routes `/@a` and `/%C3%A4`.
  *(since stage 2, owner-approved)* Because `%2F` is rejected, decoding
  never changes the segment count, and stripping a route's segments keeps
  the rest of the raw path.
- **FR-ROUTE-06** [done] The matching route with the most segments wins; no
  match → 404.
- **FR-ROUTE-07** [done] A 404 for an unknown host or path keeps the
  request's persistence.
- **FR-ROUTE-08** [retired in stage 3 Task 8] Until stage 3 Task 5, a
  matched proxy route answered 501 with `Connection: close`. Proxy routes
  forward since Task 5, the last 501 went in Task 7, and Task 8 removed
  the 501 status from the core.

### FR-STATIC — Static routes

- **FR-STATIC-01** [done] Static routes strip their own prefix (alias
  semantics): on `path /assets { static /srv/assets; }`, `/assets/a.css`
  serves `/srv/assets/a.css`.
- **FR-STATIC-02** [done] The remaining path is percent-decoded exactly once
  into byte segments (non-UTF-8 names allowed); the empty segment after a
  trailing `/` is dropped.
- **FR-STATIC-03** [done] Methods `GET` and `HEAD` only; any other → 405
  with `Allow: GET, HEAD`.
- **FR-STATIC-04** [done] A request with a body (any framing) → 413 and
  close. 405 takes precedence over 413 (a `POST` with a body gets 405 and
  closes).
- **FR-STATIC-05** [done] A directory requested with a trailing `/` serves
  its `index.html` if that is a regular file; otherwise (missing, a
  directory, a non-regular file) 404. No listings.
- **FR-STATIC-06** [done] A directory requested without the trailing `/`
  → `301 Moved Permanently`, `Location` = raw path + `/` (+ `?` + raw query
  if present), fixed body `"301 Moved Permanently\n"`.
- **FR-STATIC-07** [done] A redirect whose response would not fit the fixed
  per-connection send buffer (path and query are bounded only by
  `max_request_line`) → 414 instead, with the same persistence and HEAD
  rules. *(since stage 2; NFR-RES-03)*
- **FR-STATIC-08** [done] A regular file requested with a trailing `/`
  → 404.
- **FR-STATIC-09** [done] Symlinks anywhere in the path, dotfiles (any
  decoded segment starting with `.`), missing files, permission denied and
  non-regular files (devices, sockets, FIFOs) all → the same 404, so the
  response does not reveal which.
- **FR-STATIC-10** [done] Dotfiles are reachable only through an explicit
  route whose prefix names them, e.g. `path /.well-known { static
  /srv/well-known; }` (ACME, security.txt); dotfiles below that route are
  still 404.
- **FR-STATIC-11** [done] `200 OK` heads are: status line, `Content-Type`,
  `Content-Length` = size at open, `X-Content-Type-Options: nosniff`,
  `Connection: close` only when closing. Exactly that many body bytes
  follow, or the connection is cut (FR-STATIC-15).
- **FR-STATIC-12** [done] `Content-Type` comes from the last `.` of the
  file name, ASCII case-insensitive: `html htm` text/html; charset=utf-8,
  `css` text/css; charset=utf-8, `js mjs` text/javascript; charset=utf-8,
  `json` application/json, `txt` text/plain; charset=utf-8, `xml`
  application/xml, `svg` image/svg+xml, `png` image/png, `jpg jpeg`
  image/jpeg, `gif` image/gif, `webp` image/webp, `avif` image/avif, `ico`
  image/x-icon, `woff` font/woff, `woff2` font/woff2, `pdf`
  application/pdf, `wasm` application/wasm, `mp4` video/mp4, `webm`
  video/webm; anything else application/octet-stream.
- **FR-STATIC-13** [done] `HEAD` gets the head with the full
  `Content-Length` and no body; the file is opened for its metadata only
  (no readahead) and closed right after the head is queued.
- **FR-STATIC-14** [done] `on_file_change` (checked at every chunk
  boundary against the stamp — size and mtime — seen at open): `abort` cuts
  the connection on any change (growth, rewrite or shrink); `allow_growth`
  keeps sending exactly the size seen at open when the file only grew,
  while a same-size rewrite (`Changed`) or a shrink (`Truncated`) still
  aborts. No setting tolerates rewrites or shrinks. Documented caveats: a
  rewrite that also grows the file looks like growth; a same-size rewrite
  within one mtime tick, or a change racing the last chunk, is not
  detected.
- **FR-STATIC-15** [done] Any failure after the head was queued (file error,
  `Truncated`, failed stamp check) cuts the connection at once (no
  lingering, no further writes) and is logged (`FileAborted`); the client
  never sees a short body that looks complete.
- **FR-STATIC-16** [done] Waiting on the disk (open or readahead in
  flight) counts against `send_idle_timeout`, which disk progress resets;
  on expiry the connection closes, logged as a disk timeout (FR-CONN-08).
- **FR-STATIC-17** [done] Files are sent in chunks (`FILE_CHUNK` =
  256 KiB): the next chunk is read ahead only after the previous one was
  fully accepted by the client socket. A connection holds at most one file.
- **FR-STATIC-18** [stage 7] Conditional requests (ETag / Last-Modified).
  Range requests and compression are non-goals.

### FR-PROXY — Proxy routes

Implemented building blocks: response-head parser (FR-PROXY-14…18) and
chunked validator (FR-PROXY-19) [done in stage 3 Tasks 2–3]; the rest
of the proxy in stage 3 Tasks 4–8. Each requirement carries its status.

Forwarding and identity:

- **FR-PROXY-01** [done] Any method token is forwarded.
- **FR-PROXY-02** [done] One fresh backend connection per request, with
  `Connection: close` toward the backend; a client has at most one upstream
  at a time. (Pooling: stage 7.)
- **FR-PROXY-03** [done] `Core::backends()` lists the distinct
  `host:port` backends in config order (`BackendId` = index); the driver
  resolves each once at startup and connects by id (DRV-10). Upstream ids
  are core-allocated, from 1, never reused.
- **FR-PROXY-04** [done] The request line toward the backend is
  `METHOD <path>[?query] HTTP/1.x`, where `<path>` is the raw path as
  received (after `strip_prefix`), the query is unchanged and the version
  mirrors the client's: `HTTP/1.0` for an HTTP/1.0 client, `HTTP/1.1`
  otherwise. A backend response head with any `Transfer-Encoding` to an
  HTTP/1.0 client's request → 502 before any byte reaches the client
  (logged as `BackendFailed { TransferEncodingToHttp10 }`), also when no
  body follows (a response to HEAD, a 204 or 304): the backend is at
  fault, so it is a 5xx. *Owner decisions 2026-10-27: an HTTP/1.0 client
  cannot take a chunked body, which reverses stage 3 Task 4's "always
  HTTP/1.1"; and no `Transfer-Encoding` header ever reaches an HTTP/1.0
  client, body or not.*
- **FR-PROXY-05** [done] `strip_prefix;` removes
  the route's own segments: on route `/api`, `/api/x` → `/x`, `/api/x/` →
  `/x/`, `/api` and `/api/` → `/`. An empty result is `/`. Partial
  stripping is not supported.

Request head toward the backend:

- **FR-PROXY-06** [done] Hop-by-hop headers are removed: `Connection`
  and every header it names, `Keep-Alive`, every `Proxy-*` (incl.
  `Proxy-Connection`, `Proxy-Authorization`), `TE`, `Trailer`, `Upgrade`,
  and `Expect`.
- **FR-PROXY-07** [done] `Host` is forwarded unchanged.
- **FR-PROXY-08** [done] `X-Forwarded-For` = the client's existing value
  (if any) + `, ` + the peer address in canonical form (IPv4-mapped IPv6
  shown as IPv4); set to the peer address when absent. A request with more
  than one `X-Forwarded-For` header line → 400 (owner decision
  2026-10-26: strict and simple; backends must trust only the last entry
  either way).
- **FR-PROXY-09** [done] `X-Forwarded-Proto: http` replaces any client
  value.
- **FR-PROXY-10** [done] A request `Content-Length` is re-rendered
  without leading zeros; a chunked request keeps `Transfer-Encoding:
  chunked`.
- **FR-PROXY-11** [done] `Connection: close` is added.

Response head toward the client:

- **FR-PROXY-12** [done] The same hop-by-hop headers are removed (the
  whole FR-PROXY-06 list, `Expect` included: one list for both directions,
  owner decision 2026-10-26); the
  status line is re-rendered as `HTTP/1.1 <code> <reason>` with the
  backend's reason; `Connection: close` is added when the client
  connection will close; backend `Date`, `Server` and `Content-Length`
  pass through exactly as received (owner decision: easier debugging; the
  value is already validated as digits only). An empty backend reason
  phrase stays empty. (The parser only admits printable ASCII/HTAB reasons,
  FR-PROXY-15; the renderer still substitutes the standard reason for any
  other byte, as defence in depth, and an empty reason for a code that has
  no standard one, owner decision 2026-10-26.)
- **FR-PROXY-13** [done] Header values containing obs-text (bytes ≥
  0x80) are never forwarded: from a client → 400 (FR-HTTP-05); from a
  backend → 502 (FR-PROXY-26). Owner decision.

Backend response parsing:

- **FR-PROXY-14** [done] Backend response heads obey `max_head` and
  `max_headers`; a violation → 502 (FR-PROXY-26).
- **FR-PROXY-15** [done] The status line is `HTTP/1.1` or `HTTP/1.0`, SP,
  exactly three digits 100–599, SP (required even when the reason is
  empty), a reason of printable ASCII and HTAB, CRLF. Header lines follow
  FR-HTTP-03…05.
- **FR-PROXY-16** [done] Response framing follows FR-HTTP-08
  (`Content-Length` + `Transfer-Encoding`, duplicate or bad
  `Content-Length`, a `Transfer-Encoding` other than `chunked`, and
  `Transfer-Encoding` in an HTTP/1.0 response are errors); these checks
  also apply to 1xx, 204, 304 and responses to HEAD, and so does
  FR-HTTP-09a (a `Connection` listing `content-length`,
  `transfer-encoding` or `host` → 502). After all line checks, the
  FR-HTTP-09a check runs first, then `Transfer-Encoding` in an HTTP/1.0
  response, then the other framing checks, so the reported error is
  deterministic. `Content-Length: 0` means no body (the header is kept).
  No framing header means the body ends when the backend closes.
- **FR-PROXY-17** [done] A head is judged line by line as its LF arrives;
  the parser is resumable and split-independent (NFR-ROB-02).
- **FR-PROXY-18** [done] Interim responses: a 1xx other than 101 is
  dropped and the next head is parsed afresh, with the limits applied anew;
  `101 Switching Protocols` → 502; more than 8 consecutive interim heads →
  502 (`BadResponse(TooManyInterimHeads)`). (The cap
  of 8 is an owner-confirmed constant: real backends send at most a
  `100` and a `103`; a longer stream of 1xx heads would otherwise run
  until `backend_response_timeout`.) *Builder calls (stage 3 Task 7):* the
  bytes after an interim head move to the front of the upstream's buffer
  once, as after a request head; an interim head does not restart
  `backend_response_timeout` (it is not the response head); an interim
  head that arrives while the request body streams is dropped the same
  way and the upload goes on.

Bodies:

- **FR-PROXY-19** [done] Chunked framing (both directions) is strict:
  chunk sizes are hex digits in either case, leading zeros allowed,
  at most 32 characters and 16 significant digits (fits `u64`), no sign,
  whitespace or empty size; no chunk extensions; no trailers (the last
  chunk is exactly `0` + CRLF + CRLF); CRLF only; data longer than its size
  is an error. Valid bytes are forwarded verbatim. A violation
  from the client → 400 before the head was forwarded, else both
  connections are cut; from the backend → 502
  before the response head was queued, else both are cut (logged as
  `BackendFailed { BadChunk(..) }`).
  *Builder call (stage 3 Task 6):* the client's body is read only once the
  request head is fully written to the backend, so "before the head was
  forwarded" means the body bytes that arrived with the head, judged
  before the connect; a violation in any later byte cuts both. A body
  error is logged as `Rejected { Body(..) }`, the 400 like a parse error
  (no `Response` entry). *Builder call (stage 3 Task 7):* likewise the
  response body bytes that arrive with the response head are judged
  before the head is queued, so a violation in them is a 502; a violation
  in any later byte cuts both.
- **FR-PROXY-20** [done] Bodies stream in both directions with bounded
  buffers and fill-drain-refill backpressure (NFR-RES-04). A response
  streams as soon as its head is validated; chunked framing is validated
  chunk by chunk in flight.
- **FR-PROXY-21** [done] Response framing to the client: backend
  `Content-Length` and `chunked` pass through (`Transfer-Encoding` only
  to an HTTP/1.1 client, FR-PROXY-04); a close-delimited response is
  forwarded with `Connection: close` and the client connection closes at
  its end (a lingering close: the response is complete). Responses to
  HEAD and 204/304 never carry a body, whatever the
  backend sends (extra bytes are discarded and the upstream closed).
- **FR-PROXY-22** [done] Bytes after a `Content-Length` or chunked body
  end are never forwarded, in either direction: from the backend they are
  dropped and the upstream closed; from the client they stay buffered as
  the next pipelined request.
- **FR-PROXY-23** [done] `max_request_body`: a `Content-Length` over the
  limit → 413 before connecting; a chunked body passing it → both
  connections cut. Payload bytes are counted with saturating arithmetic.
  *Builder call (stage 3 Task 6):* "passing" is more payload bytes than
  the limit (exactly the limit is fine); this holds also when the bytes
  that arrived with the head already pass it: then the client is cut
  before any connect, with no 413.
- **FR-PROXY-24** [done] `max_response_body`: a backend
  `Content-Length` over the limit → 502 before any response byte reaches
  the client; a chunked or close-delimited body passing it → both
  connections cut (the client sees an incomplete response, never a
  silently truncated "complete" one). Both are logged as `BackendFailed {
  BodyTooLarge }`. *Builder calls (stage 3 Task 7):* "passing" is more
  payload bytes than the limit (exactly the limit is fine; chunked framing
  does not count); body bytes that arrive with the head and already pass
  it are judged before the head is queued, so that is a 502 too
  (FR-PROXY-19's builder call).
- **FR-PROXY-25** [done] `Expect: 100-continue`: the core answers
  `100 Continue` itself once the request head passed all checks (route,
  limits) and strips `Expect` toward the backend; any other `Expect` value,
  more than one `Expect` header, or a list such as `100-continue, foo` → 417
  (owner decision 2026-10-26: when in doubt, reject). *Builder call (stage
  3 Task 6):* the `Expect` check is a request-head check like those of
  FR-HTTP-08: it runs last, after the FR-HTTP-09a, Host and framing checks
  (so their errors win), and its 417 is a parse error: logged as rejected
  and closed (FR-HTTP-10). *Builder call (stage 3 Task 6):* the `100
  Continue` is queued when the backend's connect completes
  (`UpstreamConnected`): by then the head passed route, limits,
  `check_forwardable` and the judging of the body bytes that came with
  it, and a refused or timed-out connect is answered 502/504 without a
  `100` first. It is sent only for a request with a body, also when the
  body already arrived.
- **FR-PROXY-25a** [done] Outside proxy routes (static routes, route
  misses) `Expect: 100-continue` is never answered with `100 Continue`: the
  normal final response (413, 404, …) is sent at once. Any other `Expect`
  value → 417 everywhere. HTTP/1.0 requests ignore `Expect` (it does not
  exist in HTTP/1.0).

Failures, persistence, timeouts:

- **FR-PROXY-26** [done] Failure matrix. Before the response head was
  queued to the client: connect refused or reset, a reset before the
  response head, an end of stream before the response head
  (`ClosedEarly`), a malformed or oversized response head, 101, too many
  interim heads, an oversized `Content-Length` → 502;
  `backend_connect_timeout` or `backend_response_timeout` → 504. After the
  response head was queued: any failure (backend reset or EOF mid-body,
  framing error, limit, timeout) cuts both connections. The client
  resetting or timing out closes the upstream at once. A client that only
  closes its sending side after a complete request (half-close) keeps its
  upstream: the response is delivered, then both close. Fault
  determines the status class: a client that stalls its request body
  (body idle timeout before the response head) → 408, not 502; 5xx only
  when the server or backend is at fault. Builder calls (stage 3 Task 5):
  a backend that ends its connection (EOF) before its response head is
  complete is answered 502 (logged as `BackendFailed { ClosedEarly }`
  since stage 3 Task 8; `Reset` before); a
  502 or 504 generated by the core keeps the request's own persistence
  (controller ruling: the request had no unread body, so the client stays
  usable). (Until stage 3 Task 7 an interim head, a chunked and a
  close-delimited response answered 501; Task 7 relays them and no 501 is
  left.) Builder calls (stage 3 Task
  6): with a request body, such a 502/504 keeps the request's persistence
  only if the body was fully read from the client, else it closes
  (FR-PROXY-28); the body idle timeout before the response head is a 408
  (logged as `Rejected { Timeout(RequestBody) }`, then close) when the
  core waited for the client's body bytes, and a 504 (`BackendFailed {
  BodyTimeout }`, its own kind so the log says what stalled; owner policy
  2026-10-26) when body bytes waited for the backend: a backend that
  stops taking the body is at fault. A 408 or 504 to a HEAD request is
  head-only, like every response to HEAD. *Owner decision 2026-10-26:*
  while body bytes wait for the backend, the upstream is read too (the
  driver contract's upstream-only `Interest::ReadWrite`), so a backend
  that stops taking the body and answers has its response relayed at once
  (FR-PROXY-27); only one that neither reads nor answers gets the 504. A
  client that ends its stream mid-body closes both connections at once.
  Builder calls (stage 3 Task 7): failures after the head are logged with
  precise kinds — `BadChunk(..)` for bad chunked framing, `BodyTooLarge`
  for `max_response_body`, `Reset` for a reset or an end of stream before
  a `Content-Length` or chunked body is complete (`ClosedEarly` for the
  end of stream since stage 3 Task 8), `ResponseTimeout` for a body that
  stalls — and the cut is `Log`, `CloseUpstream`, `Close`, with no
  lingering close. *Builder call (stage 3 Task 8):* an end of stream
  (EOF) before the response is complete is logged as `ClosedEarly`, not
  `Reset`, both before the head (502) and before a `Content-Length` or
  chunked body is complete (cut): the log tells a backend that closed
  from one whose socket failed.
  *Builder call (stage 3 Task 8):* the half-close rule holds because
  the client is not read while its request is proxied, except for the
  body (the driver contract's interest rules): an end of stream behind a
  complete request is seen only once the response is out, and then
  closes the connection like any client EOF (silently, FR-CONN-12, or as
  the end of a lingering close). This holds with and without a body,
  and whether the end of stream comes before or after the response head
  (test `client_half_close_still_gets_response`).
- **FR-PROXY-27** [done] A backend that answers before the request body
  is complete: the response is forwarded, body forwarding stops, and the
  client closes after the response (its body is unread). This holds also
  while body bytes wait for a backend that stopped taking them: the
  upstream is read while it is written (`Interest::ReadWrite`, DRV-10;
  owner decision 2026-10-26). *Builder call (stage 3 Task 6):* if the
  whole body was already read from the client, the unsent body bytes are
  dropped and the client keeps its persistence (FR-PROXY-28).
- **FR-PROXY-28** [done] After a proxied response the client stays
  keep-alive only if the request asked for it, its body was fully read and
  the response was not close-delimited.
- **FR-PROXY-29** [done] Timeouts: `backend_connect_timeout`;
  `backend_response_timeout` from the request head being fully written to
  the backend until the response head is complete; `body_idle_timeout`, no
  body progress in either direction; the client's `send_idle_timeout`
  still applies, and its expiry releases the upstream. Every proxy state
  carries a deadline. *Builder call (stage 3 Task 6):* for a request with
  a body, `backend_response_timeout` runs from the whole request (head and
  body) being written; while the body streams, `body_idle_timeout`
  applies, reset by every body byte read from the client or written to
  the backend. *Builder call (stage 3 Task 8):* the three
  are `limits` keys (FR-CFG-20) and the core reads them from the config;
  no named default constant is left. *Builder call (stage 3 Task 8):*
  while the request body streams, response head bytes (interim heads
  included) read from the backend count as progress too and restart
  `body_idle_timeout`; on expiry the blame still follows what the core
  waited for (408 for the client's body bytes, 504 `BodyTimeout` for a
  backend that stopped taking them). Once the whole request is written,
  `backend_response_timeout` is a total deadline: head bytes do not
  extend it. Known bound (for the owner to accept or cap): in that
  streaming phase a backend that trickles response head bytes can
  postpone the 408/504 by up to one `body_idle_timeout` per byte. The
  bytes are bounded (at most `MAX_INTERIM_HEADS` + 1 = 9 heads, each
  at most the upstream receive capacity, `max(16 KiB, max_head)`), but
  there is no total time cap in that phase; a client trickling its body
  has the same property.
- **FR-PROXY-30** [done] Nothing from a backend reaches the client
  unvalidated: heads are parsed strictly, bodies framed strictly, and
  nothing after a framing error is written.
- **FR-PROXY-31** [done] Backend failures are logged
  (`BackendFailed`: refused, reset, closed early (an end of stream before
  the response was complete) [done in stage 3 Task 8], bad response with its parse error,
  connect timeout, response timeout, body timeout: a backend that stopped
  taking the request body [done in stage 3 Task 6]; bad chunked framing of
  the response body, a response body over `max_response_body`, a
  `Transfer-Encoding` toward an HTTP/1.0 client [done in stage 3 Task 7]);
  proxied responses
  are logged as
  `ProxiedResponse` with the backend's status (FR-LOG-02).
- **FR-PROXY-32** [stage 8] Backend-directed file serving (an
  `X-Accel-Redirect`-like header): see Timeline.

### FR-CONN — Connections: keep-alive, pipelining, close, timeouts, caps

- **FR-CONN-01** [done] Keep-alive with clients (FR-HTTP-09).
- **FR-CONN-02** [done] One request at a time per connection: while a
  response is in progress the connection does not read; pipelined bytes
  wait in the receive buffer (within its capacity) and are moved to its
  front once.
- **FR-CONN-03** [done] Pipelined requests are answered in order; a request
  following one that asked for `Connection: close` is never answered.
- **FR-CONN-04** [done] A request whose body the core does not read (any
  route other than a proxy route) is answered with `Connection: close`,
  then closed with a lingering close.
- **FR-CONN-05** [done] Every close after a complete response (parse
  errors, 408, 503, `Connection: close`, unread body) is a lingering close:
  shut down the sending side, read and discard input until EOF or
  `LINGER_TIMEOUT` (2 s, fixed at entry; input does not extend it), then
  close. Aborts (FR-STATIC-15, FR-PROXY-26) and send timeouts close at once.
  *A plain close with unread input sends a reset that can destroy the
  response before the client reads it.*
- **FR-CONN-06** [done] `header_timeout` is a total deadline for the whole
  request head, started at accept and on every entry into reading a head
  (first byte after idle, or pipelined leftover); trickling does not extend
  it. On expiry: 408 and close if any byte of the request arrived, else a
  silent close.
- **FR-CONN-07** [done] `keepalive_timeout`: an idle keep-alive connection
  is closed silently.
- **FR-CONN-08** [done] `send_idle_timeout`: reset whenever the client
  socket accepts bytes (and while waiting on the disk, whenever the disk
  completes); on expiry the connection closes without further writes,
  logged as a send timeout (or disk timeout while waiting on the disk,
  also when a readahead is in flight while head bytes are still queued).
- **FR-CONN-09** [done] A connection accepted while `max_connections`
  connections, or `max_connections_per_ip` from the same peer, are counted
  is answered 503 and closed (lingering), and logged. Refused connections
  count toward neither limit. Peers are counted by canonical address (an
  IPv4-mapped IPv6 peer shares its IPv4 slot). Counts drop when a
  connection closes.
- **FR-CONN-10** [done] At most `max_connections` refused connections are
  kept at once; beyond that a new connection is logged as rejected and
  closed at once with nothing written.
- **FR-CONN-11** [done] An orphaned open (a connection that closed or timed
  out while its file open was unanswered) keeps both its global and its
  per-IP slot until the disk answers. *(since stage 2, ruling I2; supersedes
  "per-IP slot released at close") A host looping request-then-reset
  against a hung disk is capped at `max_connections_per_ip` orphans.*
- **FR-CONN-12** [done] A client reset closes the connection and is logged;
  a client EOF outside the lingering close closes it silently.
- **FR-CONN-13** [stage 5] Per-IP grouping of IPv6 peers by prefix (e.g.
  /64), and fairness of the refused-connection pool; both before internet
  exposure.

### FR-LOG — Logging

- **FR-LOG-01** [done] The core emits `Log` actions as enum variants, one
  per kind; tests assert on them directly.
- **FR-LOG-02** [done] Kinds: `Response` (status and a request summary of
  method, host and raw path; emitted when the response is queued),
  `Rejected` (parse error, header/send/disk timeout, connection limits),
  `Reset`, `FileAborted` (with the file error);
  `ProxiedResponse { status: u16, backend, request }` (a backend's
  response head was queued; its status is any 100–599, so a number rather
  than one of the core's statuses) and `BackendFailed { backend, kind }`
  (`kind` per FR-PROXY-31, `ClosedEarly`, `BodyTimeout`, `BadChunk`,
  `BodyTooLarge` and `TransferEncodingToHttp10` included). A 502 or 504 the core
  generates for a failed backend is a `Response`, preceded by its
  `BackendFailed`. [Stage 3 Task 6:] `Rejected` also covers a request body
  that broke a rule (`Body(Chunked(..) | TooLarge)`) and a stalled request
  body (`Timeout(RequestBody)`).
- **FR-LOG-03** [done] One entry per request: a routed request or route
  miss logs `Response`; a parse error logs `Rejected` only. A later failure
  of the same exchange (timeout, abort) adds its own entry.
- **FR-LOG-04** [done] Logs carry the raw (validated, ASCII) path; the
  decoded path is never logged or echoed.
- **FR-LOG-05** [stage 4] The driver writes each entry as one `key=value`
  line to stderr, escaping untrusted fields.
- **FR-LOG-06** [stage 4] Logging never blocks: if stderr is backed up,
  entries are dropped and counted, and the count is logged once output
  catches up.
- **FR-LOG-07** [stage 4] `Rejected` for a parse error after the request
  line carries the method and path. The driver logs `EMFILE`/`ENFILE` and
  driver-bug closes distinctly.

### FR-PROC — Process: CLI, privileges, systemd, signals, restart

- **FR-PROC-01** [stage 4] `durchlauferhitzer <config-path>` runs the
  server; `durchlauferhitzer --check <config-path>` validates the config
  (FR-CFG-06 errors) and exits.
- **FR-PROC-02** [stage 4] If systemd passes listening sockets (socket
  activation, `LISTEN_FDS`), they are used and the process never needs
  root.
- **FR-PROC-03** [stage 4] Otherwise the process binds itself: it starts as
  root only if it needs a port below 1024, binds, then switches to `user`;
  it refuses to keep running as root if `user` is unset.
- **FR-PROC-04** [stage 4] At startup the driver resolves the backends,
  opens every static root, checks `openat2` support and the descriptor
  budget (NFR-RES-05), and fails with a clear error otherwise.
- **FR-PROC-05** [stage 4] Shutdown has two speeds and two targets (owner
  decision):
  - *Graceful* (SIGTERM): stop accepting work, let in-flight requests
    finish, keep-alive connections are closed after their current
    response, then exit. Bounded by `limits { shutdown_timeout 30s; }`;
    when it expires the fast path runs.
  - *Fast* (a second SIGTERM, or SIGINT): close every connection, upstream
    and file at once and exit.
  - *Target shutdown*: work arriving during the drain is rejected — the
    listening sockets are closed, and a request that arrives on an
    already-open connection gets 503 with `Connection: close`.
  - *Target restart* (the restart signal of FR-PROC-07): work arriving
    during the drain is queued — the listening sockets are handed to the
    new process, so new connections wait in the kernel's accept queue
    until it accepts them; the old process only drains what it already
    holds.
- **FR-PROC-05a** [stage 4] Exit codes: 0 success (including `--check` on a
  valid config), 1 invalid config, 2 startup failure (e.g. a port in use,
  a missing static root).
- **FR-PROC-06** [stage 5] Containment: Landlock restricts file access to
  the static roots (a clear warning at startup if unavailable, and the
  server runs); seccomp allow-lists the syscalls.
- **FR-PROC-07** [stage 6] Zero-downtime restart. With systemd: socket
  activation; the old process drains while new connections wait in the
  accept queue. Without systemd: on a signal, exec the new binary with the
  listening sockets inherited; the new process validates its config (on
  error it exits and the old keeps serving), then accepts; the old stops
  accepting, drains and exits. Such a restart may change hosts, routes,
  backends and limits only; a new `listen`, `user` or static root is
  rejected with a clear error (never half-applied) and needs a full
  restart.

---

## Non-functional requirements

### NFR-SEC — Security

- **NFR-SEC-01** [done; proxy stage 3] No request smuggling: every framing
  ambiguity is a hard rejection (FR-HTTP-03…08, -09a, FR-PROXY-16, -19).
- **NFR-SEC-02** [stage 3] A backend receives exactly one well-framed
  request per client request: `Content-Length` + `Transfer-Encoding`,
  duplicate lengths, chunk-extension and trailer tricks, and a second
  request hidden in a body never get through (FR-PROXY-10, -19, -22).
- **NFR-SEC-03** [stage 3] No response splitting or desync from a hostile
  backend: extra bytes after a body, a second response, CR/LF in reasons or
  header values are never forwarded (FR-PROXY-15, -22, -30).
- **NFR-SEC-04** [done in core; driver stage 4] No escape from a static
  root: dot segments and their encodings, encoded `/`, `\`, NUL and
  controls are rejected (FR-HTTP-13/14), dotfiles are 404
  (FR-STATIC-09), and the driver resolves strictly beneath the root without
  following symlinks (DRV-08).
- **NFR-SEC-05** [done] One spelling per path: encoded unreserved
  characters and `;`/`%3B` are rejected and routing compares decoded
  segments, so a more specific route cannot be bypassed by respelling
  (FR-HTTP-15/16, FR-ROUTE-05).
- **NFR-SEC-06** [done] No header injection: CR, LF, NUL and other control
  bytes are never accepted in header values, reasons or targets, and only
  validated bytes are rendered into heads.
- **NFR-SEC-07** [done; driver stage 4] No log injection: decoded control
  bytes cannot reach the core (FR-HTTP-14), logs keep the raw path
  (FR-LOG-04), and the driver escapes untrusted fields (FR-LOG-05).
- **NFR-SEC-08** [done by construction] Not an open proxy: a proxied
  request only ever goes to a backend named in the config; absolute-form
  and `CONNECT` are rejected (FR-HTTP-11).
- **NFR-SEC-09** [done] Static errors do not reveal why a file is not served
  (FR-STATIC-09).
- **NFR-SEC-10** [done] No CPU amplification: request-head parsing is
  resumable, with total work O(`max_head`) per head however it is split; the
  chunked validator re-judges at most 35 bytes per call. *(stage 1 review
  I1: a stateless rescan let ~7 MB/s of input saturate the loop)*
- **NFR-SEC-11** [done static; proxy stage 3; driver stage 4] A slow
  client, slow disk or slow backend never stalls other connections: the
  core never blocks, disk work runs on driver workers, and every waiting
  state has a deadline.
- **NFR-SEC-12** [stage 4/5] Least privilege: never root after binding
  (FR-PROC-03); Landlock and seccomp (FR-PROC-06).
- **NFR-SEC-13** [stage 5] Parsers and the config language are fuzzed.

### NFR-RES — Resource bounds

- **NFR-RES-01** [done; upstream buffers stage 3] Memory per connection is
  fixed by the config:
  - a counted connection: one receive buffer of
    `max(max_head, max_request_line + 2)` bytes plus one send buffer of
    `SEND_CAPACITY` (1 KiB); from stage 3 an active proxy exchange adds its
    upstream's receive and send buffers, of fixed capacities (each more
    than 35 bytes). Builder call (stage 3 Task 5): the upstream receive
    buffer is `max(16 KiB, max_head)` (it holds a whole response head);
    the send ("head") buffer is `max_head + max_headers + 128`, enough for
    any rewritten head within the limits; it carries the request head to
    the backend, then the response head to the client, since a backend
    head may exceed the client's 1 KiB send buffer;
  - a refused (503) connection: the send buffer only;
  - a lingering connection: no buffer of its own; all lingering
    connections discard into one shared scratch buffer of 4 KiB.

  So total buffer memory ≤ `max_connections` × (receive + send + upstream
  buffers) + `max_connections` × send + 4 KiB, plus small fixed per-entry
  state. A 50 GB transfer uses the same memory as a 5 KB one.
- **NFR-RES-02** [done] No body is ever held whole;
  file bytes never pass through core memory (only heads go through the
  send buffer).
- **NFR-RES-03** [done] No buffer grows beyond its capacity; a response
  that would not fit is answered differently (FR-STATIC-07) rather than
  growing the buffer.
- **NFR-RES-04** [done] Fill, drain, refill: a body
  buffer is filled by one read, sent until it is empty, and only then read
  into again, from offset 0 and offering its full capacity; while it holds
  unsent body bytes that side is not read. There is no partial compaction,
  so body bytes are never copied: validated body bytes are written to the
  other side straight out of the receiving side's buffer. Two exceptions
  move bytes once to the front: the leftover of a pipelined next request
  after a body end (as for heads), and an incomplete chunked-framing tail
  of at most 35 bytes (`MAX_UNCONSUMED`: size line or CRLF, never body
  data) — the buffer counts as drained when only that tail remains. Buffer
  capacities exceed 35 bytes. The strategy is internal to the core
  (drivers see only "a slice to read into" / "a slice to send from"), so
  stage 7 may swap in double buffering after measuring (NFR-PERF-05).
  *While our buffer drains, the kernel's socket buffers keep both TCP
  connections busy, so the pause is one system call long.*
  [Request bodies done in stage 3 Task 6.] *Builder calls (stage 3 Task
  6):* body bytes that arrive in the same read as the request head are
  forwarded where they landed, not moved; only what follows the body
  moves. The client's receive buffer (`max(max_head, max_request_line +
  2)` bytes) always exceeds 35 bytes when it carries a chunked body: such
  a request has an HTTP/1.1 head with `Host` and `Transfer-Encoding:
  chunked`, at least 51 bytes (`X / HTTP/1.1`, `Host:a`,
  `Transfer-Encoding:chunked`, the blank line), and that head had to fit
  in the buffer.
- **NFR-RES-05** [core bound done; startup check stage 4] File-descriptor
  budget: at most `max_connections` files are open or being opened
  (orphans included, FR-CONN-11); at most 2 × `max_connections` client
  sockets (refused ones included); from stage 3 at most one upstream socket
  per counted connection; plus the static roots, listeners and the eventfd.
  The driver checks `RLIMIT_NOFILE` against this at startup.
- **NFR-RES-06** [done; upstreams stage 3] Every opened file is closed
  exactly once and every upstream is closed exactly once, on every path
  (normal end, error, reset, timeout, orphan).
- **NFR-RES-07** [done] Body transfers in either direction are bounded by
  idle timeouts (stalls), not by total duration; the request head has a
  total deadline (FR-CONN-06).
- **NFR-RES-08** [done] No busy waiting: the core only reacts to events
  and exposes its next deadline.

### NFR-ROB — Robustness

- **NFR-ROB-01** [done] No panics on any input: untrusted input yields
  error enums; `unwrap`, `expect`, `panic!`, indexing/slicing and wildcard
  enum matches are denied lints in non-test code.
- **NFR-ROB-02** [done] Split independence: request heads, response heads,
  chunked bodies and the core's responses are identical however the input
  is split, down to single bytes, and a judged line's verdict never changes
  with more bytes.
- **NFR-ROB-03** [done] The parsers hold their guarantees for any limits,
  not just validated config values.
- **NFR-ROB-04** [done; upstreams stage 3] The core tolerates driver
  mistakes without panicking: events for unknown connections, files or
  upstreams are ignored, a duplicate `Accepted` is ignored, a file event
  that does not fit the file's state is ignored, and reported lengths
  larger than offered are clamped; an impossible answer (`Truncated` or
  `Changed` to an `OpenFile`) closes the connection.
- **NFR-ROB-05** [done; upstreams stage 3] Late events after a connection
  closed are harmless: a late `FileOpened` is answered with `CloseFile`, a
  late `FileFailed` ignored; nothing is delivered for an upstream after
  `CloseUpstream`.
- **NFR-ROB-06** [done] Deterministic: single-threaded, no locks,
  deterministic iteration order (connections in id order).

### NFR-PORT — Platform

- **NFR-PORT-01** [stage 4] Linux ≥ 5.14, x86_64 and aarch64.
- **NFR-PORT-02** [stage 4] Links against the system glibc; no other C.
- **NFR-PORT-03** [stage 4] `openat2` is required (startup error without
  it).
- **NFR-PORT-04** [stage 5] Landlock is used when available (FR-PROC-06).
- **NFR-PORT-05** [stage 4] systemd is optional (FR-PROC-02/03).
- **NFR-PORT-06** [done] Stable Rust (toolchain pinned in
  `rust-toolchain.toml`), edition 2024.

### NFR-MAINT — Maintainability and supply chain

- **NFR-MAINT-01** [done] Pure Rust: no C in the repo or in the product's
  dependency tree; glibc is the only exception. No `cc`/`cmake` build
  scripts, no `*-sys` crates except `libc`, no OpenSSL, `ring` or
  `aws-lc-rs`; TLS, if ever, uses a pure-Rust provider.
- **NFR-MAINT-02** [done] Test-only tooling (e.g. `cargo-fuzz`) may contain
  C/C++ as long as none of it reaches the binary.
- **NFR-MAINT-03** [done] Minimal product dependencies: the core has zero
  `[dependencies]` and no build script; the planned product total is
  `libc` (driver only). A new dependency needs a concrete reason std cannot
  cover, is justified in its commit message, pinned in the committed
  `Cargo.lock` (enforced with `--locked`), uses minimal features, and has
  its full transitive tree checked for build scripts and C sources.
  Test-only crates (property testing, seeded randomness) are free.
- **NFR-MAINT-04** [done; hardening stage 4] CI fails if the product's
  dependency tree contains anything but workspace crates and `libc`
  (`scripts/check-product-deps.sh`). Stage 4: match workspace members
  rather than a name prefix, check all targets, add a self-test.
- **NFR-MAINT-05** [done; driver stage 4] `unsafe` is forbidden in the
  core; the driver confines it to a small, documented `libc` wrapper
  module.
- **NFR-MAINT-06** [done] Exhaustive enum handling: no `_ =>` arm on an
  enum anywhere (tests' helpers included);
  `clippy::wildcard_enum_match_arm` is denied.
- **NFR-MAINT-07** [done] Enums instead of `bool` parameters and flag pairs
  in public APIs; illegal states unrepresentable (state variants carry only
  their own data).
- **NFR-MAINT-08** [done] The core is explicit state machines, not
  `async`/`await`.
- **NFR-MAINT-09** [done] Strict red/green TDD: every behaviour starts as a
  failing test that was run and seen to fail; a found bug gets a named
  regression test (a pinned seed for randomized runs).
- **NFR-MAINT-10** [done] Each crate has a README describing its purpose and
  layout, updated with module changes; source files stay small (no file in
  `core/src/` over ~900 lines).
- **NFR-MAINT-11** [done] Every change passes `cargo fmt --check`,
  `cargo clippy --workspace --all-targets --locked -- -D warnings`,
  `cargo test --workspace --locked` and `scripts/check-product-deps.sh`,
  locally and in CI.

### NFR-PERF — Performance

Targets are set in stage 7 from measurements; none are invented here.

- **NFR-PERF-01** [stage 7] Static-file throughput per core: target to be
  set in stage 7.
- **NFR-PERF-02** [stage 7] Proxy throughput and added latency: target to
  be set in stage 7.
- **NFR-PERF-03** [stage 7] Concurrent connections per core at a given
  memory budget: target to be set in stage 7.
- **NFR-PERF-04** [stage 4] Writes in large pieces with `TCP_NODELAY`;
  `SO_RCVBUF`/`SO_SNDBUF` are never set (kernel autotuning stays on).
- **NFR-PERF-05** [stage 7] TCP behaviour under `tc netem` (delay, loss,
  recovery after pauses); fill-drain-refill vs. double buffering and the
  body buffer size decided by measurement: target to be set in stage 7.
- **NFR-PERF-06** [stage 7] `MSG_MORE` to coalesce heads with file data;
  `splice` for proxied bodies with a known `Content-Length`; readahead
  overlap with sending; deadline index instead of the O(n) scan.
- **NFR-PERF-07** [stage 7] N driver threads with `SO_REUSEPORT`; backend
  connection pooling.

### NFR-TEST — Verification

- **NFR-TEST-01** [done; backends stage 3] Every behaviour is tested
  through a simulated network, clock, file system and (stage 3) scripted
  backends, with fault injection: arbitrary chunking down to single bytes,
  slow and stalled readers, EOF and resets, slow disks, symlinks, denied and
  special files, files that shrink or change mid-transfer, backends that
  refuse, hang, misframe or reset.
- **NFR-TEST-02** [done] The sim records every event and action; a failure
  prints the exact transition sequence.
- **NFR-TEST-03** [done; upstreams stage 3] The sim checks the driver
  contract from the core's side after every step: no actions for unknown
  or closed connections, files or upstreams; no second close; buffers within
  capacity; no stalled output; files and upstreams closed exactly once and
  never left open after their connection; at most one file and one upstream
  per connection; segments offered only after their `ChunkReady` and with
  an empty send buffer; no readahead beyond the size at open; no deadline
  left in the past; fill-drain-refill reads (NFR-RES-04).
- **NFR-TEST-04** [done] Seeded randomized runs (hundreds to thousands of
  seeds) mix valid and hostile requests, faults and clock jumps; a failure
  prints `SIM_SEED=<seed>` for replay. The run checks every response byte
  against a model, and fails if any expected outcome (every status, abort
  and timeout kind; stage 3: 502, 504, 413, 417, 100-continue, every
  response framing, cut-after-head, early backend response) never occurs
  across seeds. End of run: no open connections, files or upstreams, and
  every slot free. It stays under ~10 s in a debug build.
- **NFR-TEST-05** [done] Property tests: parsers never panic; a prefix
  never contradicts the full input; accepted paths never contain dot
  segments, `//`, `\` or NUL after decoding.
- **NFR-TEST-06** [done static; proxy stage 3] Bounded memory, intact bytes,
  fill-drain-refill and write sizes are asserted with bodies far larger
  than the buffers.
- **NFR-TEST-07** [stage 4] Smoke tests over real loopback sockets; a sim
  mode with late timers (I/O delivered before an overdue `TimerFired`).
- **NFR-TEST-08** [stage 5] Long randomized runs and fuzzing.

---

## Driver requirements (DRV)

The binding text is the contract in the module docs of
`core/src/core/mod.rs`; these are its headlines. Status: [done] where the
sim implements and checks it; the epoll driver is [stage 4].

- **DRV-01** Connection ids are allocated by the driver and never reused;
  the first event for a connection is `Accepted`.
- **DRV-02** The actions of one `handle` call are performed in order before
  the next event is delivered; buffer slices are valid only until the next
  `handle` call.
- **DRV-03** Readiness is level-triggered: read until `recv_buffer` is
  empty or the socket would block; write `send_buffer`, then the
  `send_file` segment, until done or blocked. An edge-triggered driver
  loops to `EAGAIN`. `EAGAIN` is never reported; `Sent`/`FileSent` are
  never 0.
- **DRV-04** A read, write or `sendfile` is reported (`Received`, `Sent`,
  `FileSent`/`FileFailed`) as the very next `handle` call after it. An
  empty buffer means "do not read now" / "nothing to write".
- **DRV-05** After `Close` the driver closes the socket and delivers no
  further events for that id.
- **DRV-06** Timers are armed from `next_deadline` after every `handle`
  call; `TimerFired` is delivered once that instant has passed.
- **DRV-07** Static roots are opened once at startup (`RootId` indexes
  `static_roots()`); an empty segment list means the root itself (open
  `"."`).
- **DRV-08** Files are opened with `openat2(RESOLVE_BENEATH |
  RESOLVE_NO_SYMLINKS)` relative to the root and `O_RDONLY | O_NONBLOCK |
  O_NOCTTY | O_CLOEXEC` (never create, truncate or write; `O_NONBLOCK` so a
  FIFO cannot hang a worker); `RESOLVE_NO_XDEV` is not set. Opens, `fstat`
  and `readahead` run on the worker pool; answers may come in any order.
  Bytes go to the client by non-blocking `sendfile`.
- **DRV-09** Every `OpenFile` and every `Readahead` gets exactly one answer
  with the real outcome — the driver never fabricates one (a blocked
  `openat2` cannot be cancelled; the core waits, FR-CONN-11). `CloseFile`
  supersedes a readahead in flight; the descriptor stays open until that
  worker returns. Errors map to `FileError` by the errno table; stamps are
  `st_size` and mtime in nanoseconds (pre-1970 clamps to 0); a `sendfile`
  returning 0 mid-segment is `Truncated`; `EPIPE`/`ECONNRESET` are the
  socket's (`Failed`); other file-side errors are `FileFailed { Other }`.
  Known limit: `sendfile` may still block on evicted pages (stage 7).
- **DRV-10** [done] Upstreams: `backends()` resolved once at startup;
  `Connect` is a non-blocking connect answered by exactly one
  `UpstreamConnected` or `UpstreamFailed` unless `CloseUpstream` comes
  first (then nothing more for that id); upstream buffers follow DRV-02…04;
  `CloseUpstream` closes the socket. An upstream (never a client) may get
  `Interest::ReadWrite`: the driver then reads and writes as readiness
  allows, reports each read and write as the very next call, and asks for
  both buffers afresh after every call (owner decision 2026-10-26, for
  FR-PROXY-27).
- **DRV-11** [stage 4] Sockets are non-blocking on `epoll`, with
  `TCP_NODELAY` (NFR-PERF-04); `RLIMIT_NOFILE` is checked against
  NFR-RES-05 at startup.
- **DRV-12** [stage 4] Logs are formatted and written per FR-LOG-05/06.

---

## Timeline

Each stage is a separately planned and reviewed chunk that leaves the
project working and tested.

1. **Foundations** [done] — workspace, lints, config parser + validation,
   core `Event`/`Action`/state enums, simulation harness (net + clock),
   strict HTTP/1.1 request parsing, vhost + path routing answering
   404/400/4xx with fixed responses, limits and timeouts. No real I/O.
2. **Static files** [done] — simulated file system, path decoding and
   validation, open/readahead/sendfile actions, index.html, content types,
   HEAD, mid-transfer failures.
3. **Reverse proxy** [in progress: Tasks 1–3 done] — backend connections in
   the simulation, header handling, `strip_prefix`, body streaming with
   backpressure, 502/504, misbehaving backends.
4. **epoll driver + binary** — the real driver, worker pool, `eventfd`,
   `sendfile`, `openat2`, logging output, CLI, privilege drop, systemd
   socket activation, SIGTERM drain, a few smoke tests over real loopback
   sockets. First runnable release.
5. **Containment** — Landlock (restrict file access to static roots),
   seccomp (allow-list of syscalls), long randomized simulation runs,
   fuzzing, IPv6 per-IP grouping.
6. **Zero-downtime restart** — FR-PROC-07.
7. **Scaling and efficiency** — N driver threads with `SO_REUSEPORT`,
   backend connection pooling, conditional requests (ETag /
   Last-Modified), `MSG_MORE`, `splice` for proxied bodies with a known
   `Content-Length`, the `tc netem` throughput benchmarks, profiling,
   NFR-PERF targets.
8. **Backend-directed file serving** — a backend response header (like
   nginx's `X-Accel-Redirect`) asks the proxy to serve a file via
   `sendfile`. Only honored on proxy routes that name an explicit root
   (`internal_files <dir>;`); the path goes through the same validation and
   `openat2` jail as static routes; the header is never accepted from
   clients and always stripped; backend status and headers such as
   `Content-Type` / `Content-Disposition` are kept, its body discarded.
   Useful when the files are on the proxy's machine (same host or shared
   storage).
9. **Later, maybe** — *Hung-disk handling (recorded for a better future
   solution):* today a hung `openat2` keeps its connection slot until the
   disk answers (FR-CONN-11), which bounds the damage at `max_connections`
   but lets one dead mount occupy slots. A driver-side timeout that answers
   `FileFailed` early was rejected: the syscall cannot be cancelled, so
   worker threads would pile up instead of slots. Candidates: a per-root
   health breaker (after N slow or hung opens, mark the root unhealthy and
   answer its requests 503 at once, without opening, until a probe open
   succeeds); cancellable opens via io_uring `OPENAT` with linked timeouts
   (conflicts with the seccomp decision, revisit with the io_uring
   driver). HTTPS with a pure-Rust crypto provider, io_uring
   driver, file-descriptor passing from same-host backends (`SCM_RIGHTS`).
10. **Caching, after everything else** — a small Varnish-style response
    cache in front of proxy routes, with far fewer features than Varnish.

## Left to the builder

- Exact default values for limits and timeouts not fixed above
  (conservative, documented in the example config).
- Module layout within crates, names of enum variants, buffer sizes not
  fixed above (e.g. upstream buffers, subject to NFR-RES-04).
- Edge-triggered vs level-triggered `epoll`, readahead chunk size, worker
  pool size.

## Open decisions (owner)

Each blocks or shapes the requirement named; until decided, the
recommendation (if any) is not implemented.

1. FR-CFG-21 — upper bounds for `max_connections`, `max_request_line` and
   the other limits (each connection's receive buffer is allocated eagerly).
2. FR-CFG-12 — stricter host-name checks (`-a-`, `999.999.1.1`, label
    lengths, IPv6 spellings).