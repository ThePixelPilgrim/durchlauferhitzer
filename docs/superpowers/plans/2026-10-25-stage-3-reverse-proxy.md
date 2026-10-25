# Stage 3: Reverse Proxy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (mandatory in this repo) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. In this repo, `superpowers:<name>` is the project skill `<name>`.

**Goal:** Proxy routes forward requests to their backend through the sans-IO core: one fresh backend connection per request, strictly parsed framing in both directions, bodies streamed through fixed buffers with backpressure, and 502/504/cut connections for every way a backend can misbehave. The sim gains scriptable backends.

**Architecture:** First `core.rs` is split and `ConnState` stops being `Copy` (stage-2 review I3), so states can own buffers. Three pure HTTP pieces follow: a strict chunked-body validator, a response-head parser, and the renderers for the forwarded request head and response head. A small `Pipe` buffer implements hysteresis backpressure. The core then gets a second kind of socket, the *upstream* (one connection to a backend). It has core-allocated ids, its own events and actions, and its own receive and send buffers, under the same direct-buffer rules as clients. On the client side the driver contract does not change: while proxying, `recv_buffer` hands out room for request body bytes and `send_buffer` hands out the forwarded response.

**Tech Stack:** Rust 2024, std only in core; sim is test-only (fastrand is already present). No new dependencies.

**Spec:** `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md`, sections "Proxy routes", "Streaming and flow control", "HTTP behavior", "Limits and timeouts", "Slow clients". The amendments below are applied to the spec in Task 9.

## Spec amendments (decided while planning; listed for the owner's review)

1. **Ids.** `BackendId(u32)` indexes `Core::backends()`: the distinct backend addresses, in config order, with identical ones listed once. The driver resolves each one once at startup. `UpstreamId(u64)` is one connection to a backend. The core allocates these sequentially from 1 and never reuses one. A client connection holds at most one upstream.
2. **New limits** (in the `limits` block):
   - `backend_connect_timeout`: default 5s;
   - `backend_response_timeout`: default 60s. It runs from the last request byte the backend accepted until the response head is complete. Interim responses and partial heads do not extend it.
   - `body_idle_timeout`: default 30s. It is reset by progress while a body streams.
3. **Backend response heads** are bounded by the same `max_head` and `max_headers` as request heads, and parsed just as strictly: CRLF only, header names are tokens, no obs-fold. Violations get 502.
4. **Chunked bodies, both directions.** The rules:
   - a chunk size is 1–16 hex digits;
   - no chunk extensions (`;`);
   - no trailer fields (the body ends with exactly `0\r\n\r\n`);
   - CRLF only.

   Valid chunked bytes are forwarded unchanged, so the receiving side sees exactly the framing that was validated. A request chunk error is answered 400 if no response has started, otherwise both connections are cut. A response chunk error cuts both.
5. **`Connection` may not name a framing header.** If a `Connection` header lists `content-length`, `transfer-encoding` or `host`, the request is answered 400. A backend response that does the same gets 502. This closes the hop-by-hop smuggling trick, and it applies to static routes too.
6. **`Expect`.** `Expect: 100-continue` on an HTTP/1.1 request is handled by the proxy and never forwarded. Once the backend connection is up and the request has a body, the core sends `HTTP/1.1 100 Continue\r\n\r\n` and starts reading the body. Any other `Expect` value gets 417 and the connection closes. On HTTP/1.0, and on static routes, `Expect` is ignored.
7. **Request to the backend.**
   - Request line: `<method> <path>[?<query>] HTTP/<client's version>`. The path is the raw path, or with `strip_prefix` the raw path minus the route's segments (`routing::strip_segments`). Sending the client's version means a backend never chunks a response to an HTTP/1.0 client.
   - Headers: the client's headers in order, with names lowercased. Removed: hop-by-hop headers (`connection` and every token it lists, `keep-alive`, `te`, `trailer`, `upgrade`, every `proxy-*`), `expect`, and `x-forwarded-proto`.
   - Then appended, in this order: `x-forwarded-for: <canonical peer IP>` (the client's own `x-forwarded-for` lines stay in place before it), `x-forwarded-proto: http`, `connection: close`.
   - `Content-Length` and `Transfer-Encoding: chunked` are forwarded as received.
8. **Response to the client.**
   - Status line: `HTTP/1.1 <code> <reason>`, with the backend's reason phrase after validation (HTAB, SP, VCHAR, obs-text; it may be empty).
   - Headers: the backend's headers in order, with names lowercased and hop-by-hop headers removed. Then `connection: close` if the client connection will not persist. Nothing else is added.
   - No body for a HEAD request or for status 204 or 304. `Content-Length` sets the body length; `Transfer-Encoding: chunked` (exactly that) is passed through. With neither, the body runs until the backend closes, and the client connection closes after it.
   - The following are 502: `Content-Length` together with `Transfer-Encoding`; several or non-numeric `Content-Length`; any other `Transfer-Encoding`; chunked to an HTTP/1.0 client; and `101`.
   - Interim `1xx` responses (other than 101) are parsed and dropped.
   - Bytes past a `Content-Length` body are dropped.
9. **Client persistence after a proxied response.** The client connection stays open only if all of these hold:
   - the client asked for keep-alive;
   - the response is length-delimited, chunked or body-less;
   - the request body was read completely.

   Otherwise the client gets `connection: close` and a lingering close, as for any other response.
10. **Early responses.** If the response head arrives while the request body is still being forwarded, forwarding stops. The response is then forwarded, and the client connection closes after it.
11. **Failure mapping.**

    | Situation | Before the response head reached the client | After |
    |---|---|---|
    | connect refused or failed, backend reset or closed early, malformed response head, 101, response `Content-Length` over `max_response_body` | 502 | — |
    | connect timeout, response timeout, backend stops reading the request body for `body_idle_timeout` | 504 | — |
    | client stops sending its request body for `body_idle_timeout` | 408 | — |
    | request chunk error | 400 | cut both |
    | request `Content-Length` over `max_request_body` | 413, before connecting | — |
    | chunked request body passes `max_request_body` | cut both | cut both |
    | response body (chunked or until-close) passes `max_response_body`, response chunk error, backend closed or reset mid-body, backend body idle, client send idle | — | cut both |

    Every response the core generates on a proxy route closes the connection, and a HEAD request gets it head-only. A client that stalls the response releases the backend when its `send_idle_timeout` fires.
12. **Buffers.**
    - The request body flows through the client's existing receive buffer, with no new allocation.
    - The response flows through one buffer per exchange of `proxy_capacity = max(PROXY_BUFFER, max_head + 128)` bytes, with `PROXY_BUFFER = 64 KiB`. It is allocated at `Connect` and freed when the exchange ends. The rendered response head and the forwarded request head are bounded by the same capacity.
    - Backpressure: a buffer that fills stops being read until it has drained to half (hysteresis).
13. **Logs.**
    - `Log::Proxied { conn, peer, status: u16, request }` when a response head is queued for the client.
    - `Log::Backend { conn, peer, error: BackendError }` for every backend failure. If the failure is answered 502/504, `Log::Response` follows.
    - Client-side cuts use `Log::Rejected`, with new reasons:
      - `Http(HttpError::BadChunk(_))`;
      - `Limit(LimitKind::RequestBody)`;
      - `Timeout(TimeoutKind::Body)`;
      - `RejectReason::IncompleteBody`, when the client closed mid-body.

## Global Constraints

- Everything in the Global Constraints of stages 1 and 2 still holds:
  - zero product dependencies;
  - no `unsafe`;
  - the denied clippy lints;
  - no `_ =>` on enums;
  - enums over bools;
  - strict red/green TDD;
  - bounded memory;
  - file bytes never enter the core.
- No body is ever held whole. Per exchange, memory is bounded by the client's receive buffer, the send buffer, and `proxy_capacity` for each of: the response buffer, the rendered response head, and the forwarded request head.
- The destination of a proxied request is only ever a backend named in the config (`Action::Connect` names a `BackendId`, never an address from the request).
- Every `Connect` gets exactly one `CloseUpstream`, on every path. No event for an upstream is delivered after its `CloseUpstream`. The sim checks this.
- The raw path is forwarded (minus the route's prefix under `strip_prefix`). The decoded path is never forwarded, logged or echoed.
- Before every commit: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `scripts/check-product-deps.sh`. Push after every commit.

## Review Focus

1. **A smuggling attempt through the proxy.** `Transfer-Encoding: chunked` with a `0\r\n\r\n` body followed by a second request on the same connection: the backend must see exactly one request, and the second must be parsed by the core as the client's next request. Test in Task 7.
2. **A backend that sends a complete response and then keeps sending** (bytes past `Content-Length`, or a second response): the client must see exactly one response, and the next client request is unaffected. Test in Task 8.
3. **A client that sends its request body slower than the backend answers** (an early 4xx from the backend): the client gets the backend's response, its connection closes, and no request bytes reach the backend after the response head. Test in Task 7.
4. **A backend that answers HEAD with a `Content-Length` and then sends body bytes anyway:** the client gets the head only, then keep-alive works for the next request. Test in Task 8.
5. **Proxy and static on one host, with a hung disk on the static route:** proxied requests are still served (they hold no file). Test in Task 9.

---

### Task 1: Split `core.rs`; non-`Copy` connection states (stage-2 review I3, M7)

A pure refactor plus the two carry-forwards. Every existing test keeps passing, and no behaviour changes except the action order in M7.

**Files:**
- Delete: `core/src/core.rs`
- Create: `core/src/core/mod.rs`: `Core`, `Event`, `Action`, `ConnId`, `ConnError`, `Interest`, `handle`, `forget`, limits, deadlines, buffers, `finish`. Module docs: the general contract, deadlines, connection limits.
- Create: `core/src/core/static_files.rs`: `plan`, the static half of `respond`, `file_opened`, `file_failed`, `chunk_ready`, `file_sent`, `not_served`, `abort_transfer`, `Opened`, `NotServed`, `INDEX`. Module docs: the "Rules for files" section, moved verbatim; `mod.rs` links to it.
- Create: `core/src/core/tests/mod.rs` (helpers), `core/src/core/tests/connections.rs`, `core/src/core/tests/static_files.rs`: the existing tests, moved unchanged apart from M7.
- Modify: `core/src/conn.rs`, `core/src/http/target.rs` (the stale comment).

**Interfaces:**
- Produces:
  - `pub(crate) struct Exchange { pub(crate) persistence: Persistence, pub(crate) buffered: usize }` in `conn.rs`.
  - `ConnState` (no longer `Copy`, still `Debug`):
    - `Responding { exchange: Exchange, unread_body: UnreadBody, deadline: Instant }`;
    - `OpeningFile { file, slash, step, body: BodyMode, exchange: Exchange, request: StaticRequest, deadline }`;
    - `SendingFile { file, size, opened, on_change, offset, chunk, exchange: Exchange, deadline }`;
    - the other variants are unchanged.
  - `Conn::static_request` is removed.
  - `fn finish(conn: ConnId, c: &mut Conn, exchange: Exchange, unread_body: UnreadBody, now: Instant, limits: &Limits, actions: &mut Vec<Action>) -> ConnState`, with no `#[allow(clippy::too_many_arguments)]`.
  - `not_served` and `respond` lose that allow too: pass `Exchange` and fold `body`/`deadline` into the arguments as needed. The goal is no `too_many_arguments` allow left in core.
  - Transitions that need owned data take the state with `std::mem::replace(&mut c.state, …)`, and the placeholder is overwritten before the function returns.
  - `pub(crate) fn take_state(&mut self) -> ConnState` on `Conn` is the one place that does that replace (placeholder: `Lingering { deadline: Instant::from_nanos(0) }`), so the trick is written once.

- [ ] **Step 1: Move the code and tests into the new layout without changing them.** Run `cargo test --workspace`. Expected: PASS, same test count as before (record it).
- [ ] **Step 2: Write the M7 test first.** In `core/src/core/tests/static_files.rs`, assert that `abort_transfer` and a send-idle timeout while sending emit the same order: `Log(..)`, then `CloseFile { file }`, then `Close { conn }`.

```rust
#[test]
fn abort_and_timeout_emit_log_close_file_close() {
    // Two connections sending a file. One gets FileFailed { Other } for its
    // chunk; the other's send_idle deadline passes with a chunk ready.
    // For each, the actions are exactly:
    //   [Log(FileAborted{..}) | Log(Rejected{Timeout(Send)}), CloseFile{file}, Close{conn}]
}
```

  Run it. Expected: FAIL, abort currently emits `CloseFile` before `Log`.
- [ ] **Step 3: Make `ConnState` non-`Copy`, move the request into `OpeningFile`, introduce `Exchange`, and unify the order to Log → CloseFile → Close.** This deletes the unreachable `static_request == None` branches in `file_opened` and `not_served`: the request is now always present by construction. Fix the stale comment in `target.rs` (near line 321, "The raw path is canonical…"): routing compares decoded segments, and the unreserved-escape rule is defence in depth.
- [ ] **Step 4: Run all four pre-commit checks.** Expected: all pass. The test count is the old count plus 1, and `grep -rn "too_many_arguments" core/src` prints nothing.
- [ ] **Step 5: Commit** "Split core.rs; connection states own their data" and push.

---

### Task 2: Strict chunked-body validator

**Files:**
- Create: `core/src/http/chunked.rs` (register it in `http/mod.rs`)

**Interfaces:**
- Produces:

```rust
/// Incremental validator of a chunked body (amendment 4). Each byte is
/// judged exactly once; it never allocates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkedBody { /* state */ }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chunked {
    /// All of `input` is a valid prefix of a chunked body; more is needed.
    More,
    /// The body ended after the first `used` bytes of `input` (the final
    /// `0\r\n\r\n` included); the rest belongs to whatever follows.
    Done { used: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkError { BadSize, SizeTooLong, Extension, Trailer, BadLineEnding }

impl ChunkedBody {
    pub const fn new() -> ChunkedBody;
    /// Judge `input`, the bytes that follow those already judged.
    pub fn advance(&mut self, input: &[u8]) -> Result<Chunked, ChunkError>;
    /// Payload bytes (chunk data, without framing) judged so far.
    pub fn data_len(&self) -> u64;
}
```

  After `Done` or an error, `advance` returns that same result again for any input; a spent validator is never reused.

- [ ] **Step 1: Write the failing tests** in `chunked.rs`:
  - `valid_bodies_table`: rows of `(input, expected, data_len)`:
    - `b"0\r\n\r\n"` → `Done{used:5}`, 0;
    - `b"5\r\nhello\r\n0\r\n\r\nGET"` → `Done{used:15}`, 5;
    - `b"A\r\n0123456789\r\n0\r\n\r\n"` → `Done{used:20}`, 10;
    - `b"a\r\n"` → `More`, 0;
    - a size of 16 digits `b"000000000000000F\r\n"` → `More`, 0;
    - leading zeros `b"0005\r\nhello\r\n0\r\n\r\n"` → `Done{used:18}`, 5.
  - `invalid_bodies_table`:
    - `b"\r\n"` → `BadSize`;
    - `b"g\r\n"` → `BadSize`;
    - `b" 5\r\n"` → `BadSize`;
    - `b"5 \r\n"` → `BadSize`;
    - 17 hex digits → `SizeTooLong`;
    - `b"5;x=y\r\n"` → `Extension`;
    - `b"0\r\nX-T: 1\r\n\r\n"` → `Trailer`;
    - `b"5\nhello\r\n"` → `BadLineEnding`;
    - `b"5\r\nhelloX\r\n"` → `BadLineEnding`;
    - `b"0\r\n\n"` → `BadLineEnding`;
    - `b"0\r\nX"` → `Trailer`.
  - `size_overflow_is_too_long`: `b"FFFFFFFFFFFFFFFF\r\n"` (16 digits, `u64::MAX`) is `More`; the validator then needs that many data bytes and never overflows `data_len`.
  - `split_anywhere_gives_the_same_verdict` (proptest): for every valid and invalid row, feeding the input in arbitrary splits gives `More` for each prefix piece, then the same final verdict. The `used` offsets, adjusted for the piece boundary, add up to the whole-input `used`.
  - `spent_validator_repeats_its_verdict`.
- [ ] **Step 2: Run** `cargo test -p durchlauferhitzer-core chunked`. Expected: FAIL (does not compile: `ChunkedBody` is missing).
- [ ] **Step 3: Implement** as a byte-at-a-time state machine (`Size { digits }`, `SizeCr`, `Data { left: u64 }`, `DataCr`, `DataLf`, `LastCr`, `LastLf`, `Done`, `Failed(ChunkError)`). Inside `Data`, skip `min(left, input.len())` bytes without looking at them.
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Run all four pre-commit checks, commit** "Strict chunked body validator", and push.

---

### Task 3: Response-head parser; request-side `Connection` and `Expect` rules

**Files:**
- Create: `core/src/http/response_head.rs`
- Modify: `core/src/http/request.rs`: make `split_header`/`parse_header` `pub(crate)`. Add `HttpError::BadConnection`, `HttpError::BadExpect`, `HttpError::BadChunk(ChunkError)`, and the `expect` field.
- Modify: `core/src/http/status.rs`: add `ExpectationFailed` (417), `BadGateway` (502), `GatewayTimeout` (504). Map `BadExpect` → 417, and `BadConnection` and `BadChunk(_)` → 400.
- Modify: `core/src/http/response.rs`: the exhaustive status match in `render_fixed`.

**Interfaces:**
- Consumes: `HeadLimits`, `Header`, `Version`, `BodyMode` (as "was the request HEAD").
- Produces:

```rust
// request.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect { None, Continue }
// RequestHead gains `pub expect: Expect`: `Continue` only for exactly one
// `Expect` header equal (ASCII case-insensitive) to `100-continue` on
// HTTP/1.1; another value or a second `Expect` header is `BadExpect`; on
// HTTP/1.0 any `Expect` is ignored (`Expect::None`).
// A `Connection` token equal to content-length, transfer-encoding or host
// (case-insensitive) is `HttpError::BadConnection` (amendment 5).

// response_head.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    pub version: Version,
    pub status: u16,
    pub reason: Vec<u8>,
    pub headers: Vec<Header>,
    pub framing: ResponseFraming,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseFraming { NoBody, Length(u64), Chunked, UntilClose }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseError {
    HeadTooLarge, TooManyHeaders, BadStatusLine, BadVersion, BadStatus, BadReason,
    BareLf, BadHeaderName, BadHeaderValue, ObsFold, BadConnection,
    ContentLengthWithTransferEncoding, BadContentLength, DuplicateContentLength,
    UnsupportedTransferEncoding, SwitchingProtocols,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseStatus {
    Incomplete,
    /// A 1xx (not 101) head of `consumed` bytes, to be dropped.
    Interim { consumed: usize },
    Complete { head: ResponseHead, consumed: usize },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseParser { /* scanned so far */ }
impl ResponseParser {
    pub const fn new() -> ResponseParser;
    /// `buf` is every byte received since the previous head ended; `request`
    /// is `HeadOnly` for a HEAD request.
    pub fn advance(&mut self, buf: &[u8], limits: &HeadLimits, request: BodyMode)
        -> Result<ResponseStatus, ResponseError>;
}
```

  Rules:
  - Status line: `HTTP/1.1` or `HTTP/1.0`, SP, exactly 3 digits from 100 to 599, SP, then the reason. A missing reason with its SP still present is fine; without the SP it is `BadStatusLine`.
  - The parser looks for the end of the head incrementally: no byte is searched twice. It validates the head once, when the blank line arrives. The head is at most `max_head` bytes (`HeadTooLarge` as soon as more than `max_head` bytes contain no blank line) and at most `max_headers` lines.
  - Framing is decided in this order:
    1. `101` → `SwitchingProtocols`; other 1xx → `Interim`;
    2. the header errors, applied as for requests;
    3. `request == HeadOnly`, 204 or 304 → `NoBody`;
    4. `Content-Length` → `Length(n)` (also for `n == 0`);
    5. exactly `chunked` → `Chunked`;
    6. neither → `UntilClose`.

- [ ] **Step 1: Write the failing tests.**
  - In `request.rs`: `connection_naming_framing_headers_is_rejected`, with rows `Connection: close, Content-Length`, `Connection: transfer-encoding`, `Connection: HOST` → `BadConnection`, and `Connection: keep-alive, x-foo` → OK.
  - `expect_rules` (rows):
    - `100-continue` → `Continue`;
    - `100-Continue` → `Continue`;
    - `100-continue` twice → `BadExpect`;
    - `foo` → `BadExpect`;
    - HTTP/1.0 + `foo` → `Expect::None`.

    Also assert that `BadExpect.status()` is 417.
  - In `response_head.rs`:
    - `parses_a_simple_response`: `b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-A: b\r\n\r\nhello"` → `Complete`, `consumed` = head length, `Length(5)`, headers lowercased, reason `b"OK"`.
    - `framing_table`:
      - 204 with `Content-Length: 5` → `NoBody`;
      - 304 → `NoBody`;
      - HEAD request + `Content-Length: 5` → `NoBody`;
      - chunked → `Chunked`;
      - none → `UntilClose`;
      - HTTP/1.0 none → `UntilClose`;
      - `Content-Length: 0` → `Length(0)`.
    - `interim_heads_are_reported`: `100` → `Interim{consumed}`; `103` with headers → `Interim`; `101` → `SwitchingProtocols`.
    - `rejects_malformed_heads` (rows):
      - `HTTP/2 200 OK` → `BadVersion`;
      - `HTTP/1.1 20 OK` → `BadStatus`;
      - `HTTP/1.1 600 X` → `BadStatus`;
      - `HTTP/1.1 200` (no SP) → `BadStatusLine`;
      - a bare LF → `BareLf`;
      - obs-fold → `ObsFold`;
      - CL+TE → `ContentLengthWithTransferEncoding`;
      - `Content-Length: 5, 5` → `BadContentLength`;
      - two CL lines → `DuplicateContentLength`;
      - `Transfer-Encoding: gzip, chunked` → `UnsupportedTransferEncoding`;
      - a reason containing `\x01` → `BadReason`;
      - `Connection: content-length` → `BadConnection`.
    - `head_limits_apply`: more than `max_head` bytes without a blank line → `HeadTooLarge` (also when fed one byte at a time); `max_headers + 1` lines → `TooManyHeaders`.
    - `split_anywhere_gives_the_same_verdict` (proptest), with the property used in `request.rs`.
    - `scanning_is_linear`: one-byte feeds of a 16 KiB head search each byte once (use the `work` counters pattern from `request.rs`, or count inside the parser in tests).
- [ ] **Step 2: Run** `cargo test -p durchlauferhitzer-core`. Expected: FAIL (compile errors for the new items).
- [ ] **Step 3: Implement.** Reuse `parse_header` and the framing logic of `check_framing` (factor out a shared `fn framing_headers(&[Header]) -> Result<…>` if it keeps both readable). Add the new statuses to every exhaustive match.
- [ ] **Step 4: Run** the tests. Expected: PASS, including every existing parser test.
- [ ] **Step 5: Run all four pre-commit checks, commit** "Response head parser; Connection and Expect rules", and push.

---

### Task 4: Forwarded heads

**Files:**
- Create: `core/src/http/forward.rs`

**Interfaces:**
- Consumes: `RequestHead`, `ResponseHead`, `Persistence`, `Method`, `Version`.
- Produces:

```rust
/// Appends the head sent to the backend (amendment 7). `path` is the raw
/// path as forwarded (already stripped under `strip_prefix`).
pub fn render_request_head(head: &RequestHead, path: &str, peer: IpAddr, out: &mut Vec<u8>);
/// Appends the head sent to the client (amendment 8).
pub fn render_response_head(head: &ResponseHead, persistence: Persistence, out: &mut Vec<u8>);
/// Extra bytes either renderer may add beyond its input head's size.
pub const FORWARD_SLACK: usize = 128;
```

- [ ] **Step 1: Write the failing tests** in `forward.rs`:
  - `request_head_is_rewritten`: input `POST /api/x?q=1 HTTP/1.1` with these headers: `Host: a.test`, `Connection: keep-alive, X-Drop`, `X-Drop: 1`, `Keep-Alive: 5`, `TE: trailers`, `Trailer: X`, `Upgrade: h2c`, `Proxy-Authorization: x`, `Expect: 100-continue`, `X-Forwarded-For: 10.0.0.1`, `X-Forwarded-Proto: https`, `Content-Length: 3`, `User-Agent: t`. Called with path `/x` and peer `::ffff:192.0.2.7`. The exact expected bytes:

```text
POST /x?q=1 HTTP/1.1\r\n
host: a.test\r\n
x-forwarded-for: 10.0.0.1\r\n
content-length: 3\r\n
user-agent: t\r\n
x-forwarded-for: 192.0.2.7\r\n
x-forwarded-proto: http\r\n
connection: close\r\n
\r\n
```

  - `request_line_keeps_method_version_and_query`: rows:
    - `GET` HTTP/1.0, no query → `GET /p HTTP/1.0`;
    - `PURGE` with an empty query `?` → `PURGE /p? HTTP/1.1`. Check what `Target.query` holds for a trailing `?` and keep it byte-exact.
  - `response_head_is_rewritten`: backend `HTTP/1.0 404 Nope` with `Connection: keep-alive`, `Keep-Alive: 1`, `Content-Length: 2`, `X-A: b` → with `Persistence::KeepAlive`, the expected bytes are `HTTP/1.1 404 Nope\r\ncontent-length: 2\r\nx-a: b\r\n\r\n`; with `Close`, the same plus `connection: close\r\n` before the blank line.
  - `empty_reason_is_kept`: `HTTP/1.1 200 \r\n…` → `HTTP/1.1 200 \r\n…`.
  - `slack_bounds_the_growth` (proptest): for any parseable request head of at most `max_head` bytes and any peer, the rendered length is at most the raw head's length plus `FORWARD_SLACK`. Likewise for response heads.
- [ ] **Step 2: Run** `cargo test -p durchlauferhitzer-core forward`. Expected: FAIL (does not compile).
- [ ] **Step 3: Implement** both renderers and a private `fn is_hop_by_hop(name: &str, listed: &[&[u8]]) -> bool`, where `listed` holds the tokens of every `connection` header, trimmed and lowercased.
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Run all four pre-commit checks, commit** "Render forwarded request and response heads", and push.

---

### Task 5: `Pipe`: a fixed buffer with hysteresis

**Files:**
- Create: `core/src/pipe.rs` (register it in `lib.rs` as a private module)

**Interfaces:**
- Produces:

```rust
/// A fixed byte buffer one side writes into and the other drains from the
/// front. Once full it offers no space until it has drained to half its
/// capacity (hysteresis), so neither side does tiny reads or writes.
#[derive(Debug)]
pub(crate) struct Pipe { /* buf: Box<[u8]>, start, end, paused */ }
impl Pipe {
    /// Wraps `buf`, whose first `filled` bytes are already data.
    pub(crate) fn new(buf: Box<[u8]>, filled: usize) -> Pipe;
    pub(crate) fn capacity(&self) -> usize;
    /// Room to write into: empty while paused or full.
    pub(crate) fn spare(&mut self) -> &mut [u8];
    /// `n` bytes were written into `spare()` (clamped to its length).
    pub(crate) fn filled(&mut self, n: usize);
    /// The unread bytes, oldest first.
    pub(crate) fn data(&self) -> &[u8];
    /// Drop `n` bytes (clamped) from the front of `data()`.
    pub(crate) fn consume(&mut self, n: usize);
    /// The buffer back, with the unread bytes moved to its front, and
    /// their count.
    pub(crate) fn into_parts(self) -> (Box<[u8]>, usize);
}
```

  Rules:
  - The pipe pauses when `end == capacity`.
  - While paused, `spare()` is empty. Once `data().len() <= capacity / 2`, the unread bytes are moved to the front (at most half the capacity is copied) and the pipe unpauses.
  - When `data()` becomes empty, `start = end = 0`, with no copy.
  - Otherwise nothing is moved.

- [ ] **Step 1: Write the failing tests** in `pipe.rs`:
  - `fills_pauses_and_resumes_at_half`: capacity 8, fill 8 → `spare()` empty. `consume(3)` → still empty (5 > 4). `consume(1)` → `spare().len() == 4` and `data()` is the last 4 bytes, in order.
  - `empty_resets_without_copy`: fill 5, consume 5 → `spare().len() == 8`.
  - `not_full_is_never_paused`: fill 7, consume 6 → `spare().len() == 1`.
  - `into_parts_compacts`: fill 6, consume 2 → `(buf, 4)` with `buf[..4]` the last 4 bytes.
  - `clamps_overlong_counts`.
  - `stream_through_is_byte_exact` (proptest): random write and read sizes over 1 MiB of patterned data through capacity 64 come out identical, and every non-empty `spare()` is at least 1 byte. Add a second property: once paused, `spare()` stays empty until `data().len() <= 32`.
- [ ] **Step 2: Run** `cargo test -p durchlauferhitzer-core pipe`. Expected: FAIL (does not compile).
- [ ] **Step 3: Implement** `Pipe` per the rules.
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Run all four pre-commit checks, commit** "Pipe buffer with hysteresis backpressure", and push.

---

### Task 6: The upstream contract, sim backends, and body-less requests end to end

This task wires everything for requests without a body and responses with `Content-Length` or no body. It covers every failure before the response head (502/504) and the limits and config needed for that.

**Files:**
- Create: `core/src/core/proxy.rs`: the `Proxy` exchange state and the handlers for upstream events.
- Modify: `core/src/core/mod.rs`:
  - new events, actions, buffer accessors and `Interest::ReadWrite`;
  - contract docs: a new section "Rules for upstreams"; the descriptor budget gains `max_connections` backend sockets.
- Modify: `core/src/conn.rs`: `ConnState::Proxying(Box<Proxy>)`.
- Modify: `core/src/log.rs`: `Log::Proxied`, `Log::Backend`, `BackendError`.
- Modify: `core/src/lib.rs`: exports.
- Modify: `core/src/config/model.rs`, `core/src/config/validate/mod.rs`, `core/src/config/validate/tests/limits.rs`: the three limits.
- Modify: `examples/durchlauferhitzer.conf`; `core/src/config/validate/tests/mod.rs` (the M6 key test).
- Create: `sim/src/backend.rs`.
- Modify: `sim/src/lib.rs`.
- Create: `sim/tests/proxy.rs`.
- Modify: `sim/tests/static_files.rs`: the stage-2 proxy-501 test (`head_on_405_path_and_proxy_501_is_head_only`) becomes HEAD on a proxy route with a backend that refuses, giving a head-only 502.

**Interfaces:**
- Consumes:
  - `ResponseParser`/`ResponseHead` (Task 3);
  - `render_request_head`, `render_response_head`, `FORWARD_SLACK` (Task 4);
  - `Pipe` (Task 5);
  - `routing::strip_segments`;
  - `Exchange`, `finish` (Task 1).
- Produces (public, exported from `lib.rs`):

```rust
pub struct BackendId(pub u32);          // Copy, Eq, Ord, Hash, Debug
pub struct UpstreamId(pub u64);         // Copy, Eq, Ord, Hash, Debug
pub const PROXY_BUFFER: usize = 64 * 1024;
pub enum Interest { None, Read, Write, ReadWrite }   // ReadWrite: both at once
// Event additions
UpstreamConnected { upstream: UpstreamId },
UpstreamReceived { upstream: UpstreamId, len: usize },
UpstreamReadClosed { upstream: UpstreamId },
UpstreamSent { upstream: UpstreamId, len: usize },
UpstreamFailed { upstream: UpstreamId, error: ConnError },
// Action additions
Connect { upstream: UpstreamId, backend: BackendId },
SetUpstreamInterest { upstream: UpstreamId, interest: Interest },
CloseUpstream { upstream: UpstreamId },
// Core additions
pub fn backends(&self) -> &[BackendAddr];
pub fn upstream_recv_buffer(&mut self, upstream: UpstreamId) -> &mut [u8];
pub fn upstream_send_buffer(&self, upstream: UpstreamId) -> &[u8];
#[doc(hidden)] pub fn proxy_capacity(&self) -> usize; // max(PROXY_BUFFER, max_head + FORWARD_SLACK)
// Log additions
Log::Proxied { conn: ConnId, peer: IpAddr, status: u16, request: RequestSummary },
Log::Backend { conn: ConnId, peer: IpAddr, error: BackendError },
pub enum BackendError {
    ConnectFailed, ConnectTimeout, ResponseTimeout, BodyTimeout,
    Reset, ClosedEarly, BadHead(ResponseError), BadChunk(ChunkError),
    ResponseTooLarge, ChunkedToHttp10,
}
// Limits additions (validated like the existing durations)
pub backend_connect_timeout: Duration,   // 5s
pub backend_response_timeout: Duration,  // 60s
pub body_idle_timeout: Duration,         // 30s
```

  Rules for upstreams (write them into the `mod.rs` contract):
  - The core allocates `UpstreamId`s sequentially from 1. The driver starts a non-blocking `connect` (`TCP_NODELAY` set) to the resolved address of `backend` and answers with exactly one `UpstreamConnected` or `UpstreamFailed`.
  - Every `Connect` is followed by exactly one `CloseUpstream`, also after `UpstreamFailed` and while a connect is still in flight. After it the driver closes the socket and delivers nothing more for that id. No orphans: a connect can be abandoned at once.
  - The readiness, buffer and very-next-call rules for clients apply unchanged to `upstream_recv_buffer`/`UpstreamReceived` and `upstream_send_buffer`/`UpstreamSent`.
  - `Interest::ReadWrite` means both, for clients and upstreams.
  - Upstream interest starts at `None`.

  Exchange behaviour in this task:
  - A request on a proxy route with `BodyFraming::None`:
    1. allocate an upstream; emit `Connect`;
    2. render the forwarded head into the upstream send side;
    3. on `UpstreamConnected`, write it;
    4. once it is fully accepted, wait for the response under `backend_response_timeout`;
    5. parse the response in the response `Pipe`, dropping interim heads;
    6. on `Complete`, render the response head and queue `Log::Proxied`;
    7. stream a `Length(n)` body (or none), then `CloseUpstream` and `finish` with the amendment-9 persistence.
  - While proxying, `send_buffer(conn)` returns the first non-empty one of: `Conn::send` (core-generated bytes), the rendered response head, the forwardable response bytes. `Sent` drains the one it returned.
  - Deadlines in this task: `Connecting` uses `backend_connect_timeout`; `Forwarding` (the head being written) uses `body_idle_timeout`, reset on progress, and expires as 504 with `Backend { BodyTimeout }`; `Awaiting` uses `backend_response_timeout`; `Streaming` uses `send_idle_timeout` while response bytes wait for the client and `body_idle_timeout` otherwise, reset on progress. Failures map per amendment 11.
  - Request bodies and chunked or until-close responses answer 501 for now, Close (Tasks 7 and 8 replace that).

- [ ] **Step 1: Write the failing config tests.**
  - `limits.rs`: `proxy_timeouts_have_defaults_and_parse` (defaults 5s/60s/30s; `backend_connect_timeout 250ms;` parses; a bad value names the key).
  - `mod.rs`: `example_documents_every_key`. Every key the validator accepts appears as a word in `examples/durchlauferhitzer.conf`. List the keys in a `pub(crate) const KEYS: &[&str]` next to the matches in `validate/mod.rs`, and add a test that each key in `KEYS` is accepted, so the list cannot drift.
- [ ] **Step 2: Write the failing sim API.** Add `sim/src/backend.rs` with the sim's view of upstreams, and these `Sim` methods:

```rust
pub enum BackendPolicy { Accept, Refuse, Hang }     // per "host:port" as in the config
pub fn set_backend(&mut self, addr: &str, policy: BackendPolicy); // default Accept
pub fn upstreams(&self) -> Vec<UpstreamId>;          // in Connect order
pub fn backend_received(&mut self, u: UpstreamId) -> Vec<u8>;  // take bytes the backend got
pub fn backend_send(&mut self, u: UpstreamId, bytes: &[u8]);
pub fn backend_send_eof(&mut self, u: UpstreamId);
pub fn backend_reset(&mut self, u: UpstreamId);      // delivers UpstreamFailed { Reset }
pub fn set_backend_window(&mut self, u: UpstreamId, w: Window);
pub fn set_backend_read_chunk(&mut self, u: UpstreamId, max: usize);
pub fn backend_queued(&self, u: UpstreamId) -> usize; // bytes the backend sent, not yet read
pub fn upstream_state(&self, u: UpstreamId) -> SimConnState;
```

  `Accept` connects within `run_until_idle`. `Refuse` answers `UpstreamFailed { Other }`. `Hang` never answers.

  Sim contract checks, each a panic with the trace (add unit tests next to the existing `*_is_a_violation` tests):
  - `Connect` for a `BackendId` outside `backends()`;
  - a second `CloseUpstream`;
  - an upstream action after `CloseUpstream`;
  - an upstream still open after its client connection's `Close`;
  - an upstream slice longer than `proxy_capacity()` (or the client's receive capacity, for request body bytes);
  - `send_buffer(conn)` longer than `max(send_capacity, proxy_capacity)`. Replace the "send capacity never changes" record: `send_capacity(conn)` now reports the bound of the current state.
  - Pending write (stage-2 I1.8): set while bytes written from a send slice are unreported; delivering any other event meanwhile is a violation. Do this for clients, files and upstreams.
- [ ] **Step 3: Write the failing end-to-end tests** in `sim/tests/proxy.rs`. The config has host `a.test` with `path / { static /srv/www; }`, `path /api { proxy 127.0.0.1:3000; strip_prefix; max_request_body 1KiB; max_response_body 1MiB; }` and `path /raw { proxy 127.0.0.1:3001; max_request_body unlimited; max_response_body unlimited; }`.
  - `get_is_forwarded_and_answered`: client `GET /api/x?q=1 HTTP/1.1\r\nHost: a.test\r\n\r\n`, then run.
    - `backend_received` equals the exact bytes `GET /x?q=1 HTTP/1.1\r\nhost: a.test\r\nx-forwarded-for: 192.0.2.1\r\nx-forwarded-proto: http\r\nconnection: close\r\n\r\n`.
    - The backend sends `HTTP/1.1 200 OK\r\nContent-Length: 5\r\nServer: b\r\n\r\nhello`. The client receives `HTTP/1.1 200 OK\r\ncontent-length: 5\r\nserver: b\r\n\r\nhello`.
    - The upstream is `Closed`; the client connection stays `Open` with interest `Read`.
    - Logs: `[Proxied { status: 200, request: GET a.test /api/x }]`.
    - A second request on the same connection gets a new upstream (`UpstreamId(2)`).
  - `route_without_strip_forwards_the_raw_path`: `GET /raw/%40a/` reaches the backend as `GET /raw/%40a/ HTTP/1.1`.
  - `backends_are_distinct_in_config_order`: `backends()` is `[127.0.0.1:3000, 127.0.0.1:3001]` for a config that names 3000 twice.
  - `connect_refused_is_502`: `set_backend("127.0.0.1:3000", Refuse)` → client gets `render_fixed(BadGateway, Close, Full)` and a lingering close. Logs: `Backend { ConnectFailed }`, `Response { 502 }`. The upstream is closed.
  - `connect_timeout_is_504`: `Hang`, advance `backend_connect_timeout` → 504 and `Backend { ConnectTimeout }`. At `timeout - 1ns`, nothing has happened yet.
  - `response_timeout_is_504_and_not_extended_by_interim_or_partial_heads`: the backend sends `HTTP/1.1 100 Continue\r\n\r\n` and `HTTP/1.1 2` at 30s → still 504 at 60s.
  - `reset_or_close_before_head_is_502`: two sub-cases, `backend_reset` (`Reset`) and `backend_send_eof` (`ClosedEarly`).
  - `malformed_response_head_is_502`: `HTTP/1.1 200 OK\nX: y\r\n\r\n` → 502 with `BadHead(BareLf)`; `HTTP/1.1 101 Switching Protocols\r\n\r\n` → 502 with `BadHead(SwitchingProtocols)`.
  - `response_length_over_limit_is_502`: `Content-Length: 1048577` → 502 with `ResponseTooLarge`. No backend body bytes reach the client.
  - `interim_responses_are_dropped`: `100` and `103` heads before the `200` → the client sees only the 200.
  - `head_request_gets_a_head_only_response`: `HEAD /api/x` → the backend sees `HEAD /x HTTP/1.1`; the response `Content-Length: 5` and no body → the client gets the head only, and keep-alive holds.
  - `no_body_for_204_and_304`.
  - `head_on_proxy_with_refusing_backend_is_head_only_502` (replaces the stage-2 501 test).
  - `client_reset_while_connecting_closes_the_upstream`: `CloseUpstream` immediately; the sim's open-upstream check passes.
  - `client_stalling_the_response_releases_the_backend`: client `Stalled`, backend sends a 200 with 1000 bytes. At `send_idle_timeout`: `Rejected { Timeout(Send) }`, the upstream is closed and the client connection is closed.
  - `http10_client_is_forwarded_as_http10`: `GET /api/x HTTP/1.0\r\nHost: a.test\r\n\r\n` → `GET /x HTTP/1.0`, and the response to the client carries `connection: close`.
  - `expect_other_than_continue_is_417`: on the proxy route → 417 and Close, with no `Connect`.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: FAIL (compile errors: the new API).
- [ ] **Step 5: Implement** the config limits, the sim backend, and the core. In `Proxy`, keep the phase as an enum `{ Connecting, Forwarding, Awaiting, Streaming }`, plus the upstream id, `Exchange`, summary, the forwarded head bytes not yet written, the response `Pipe`, the response parser or the rendered response head and the body framing state, and the deadline. Route upstream events through `BTreeMap<UpstreamId, ConnId>` in `Core`. `forget` emits `CloseUpstream` for a proxying connection; order: `Log` (if any), `CloseUpstream`, `Close`. Write the "Rules for upstreams" docs.
- [ ] **Step 6: Run** `cargo test --workspace`. Expected: PASS.
- [ ] **Step 7: Run all four pre-commit checks, commit** "Proxy routes: upstream contract, sim backends, body-less requests". The commit message notes there are no new dependencies. Push.

---

### Task 7: Request bodies

**Files:**
- Modify: `core/src/core/proxy.rs`, `core/src/conn.rs` (if needed), `core/src/log.rs` (`RejectReason::IncompleteBody`, `LimitKind::RequestBody`, `TimeoutKind::Body`)
- Modify: `sim/tests/proxy.rs`

**Interfaces:**
- Consumes: `ChunkedBody` (Task 2), `Pipe` (Task 5), `Expect` (Task 3).
- Produces: request bodies in the `Forwarding` phase.
  - While forwarding, the client's `recv` box is moved into a request `Pipe` (`Pipe::new(recv, leftover)`) and moved back with `into_parts` when the exchange ends. The leftover bytes past the body become `buffered` for the next request.
  - Forwardable bytes are the judged body bytes: `min(data, remaining)` for `Length`, and everything `ChunkedBody::advance` accepted for `Chunked`.
  - `recv_buffer(conn)` offers `Pipe::spare()` while body bytes are still owed.
  - `upstream_send_buffer` returns the forwarded head first, then the forwardable body bytes.
  - The client's `Expect::Continue` with a body queues `HTTP/1.1 100 Continue\r\n\r\n` in `Conn::send` on `UpstreamConnected`. The body is not read before that.
  - Deadline in `Forwarding`: `body_idle_timeout`, reset on progress in either direction. On expiry: if the client owes body bytes and none are waiting to be written to the backend, the client is blamed (408, `Rejected { Timeout(Body) }`); otherwise the backend is (504, `Backend { BodyTimeout }`).
  - The upstream reads its response in `Forwarding` too (interest `ReadWrite`), so an early response head switches to `Streaming` (amendment 10).

- [ ] **Step 1: Write the failing tests** in `sim/tests/proxy.rs`:
  - `length_body_is_streamed_byte_exact`: `POST /raw` with a 3 MiB patterned body, client read chunk 1000, backend window `Bytes(7000)` → the backend receives the forwarded head plus exactly the body. Response 200 `Content-Length: 0`; keep-alive holds. The sim's slice bounds hold throughout, so the request was never held whole.
  - `chunked_body_is_passed_through_unchanged`: the body `5\r\nhello\r\n0\r\n\r\n` reaches the backend byte for byte.
  - `pipelined_request_after_a_chunked_body_is_not_forwarded` (Review Focus 1): one client send of a chunked POST with body `0\r\n\r\n`, immediately followed by `GET /api/y HTTP/1.1\r\nHost: a.test\r\n\r\n`. Upstream 1 receives exactly the POST head and `0\r\n\r\n`. After its response, upstream 2 receives the GET.
  - `length_over_limit_is_413_before_connecting`: `POST /api` with `Content-Length: 1025` → 413, Close, no `Connect`, and `Response { 413 }` is logged.
  - `chunked_over_limit_cuts_both`: chunks totalling 1025 data bytes → `Rejected { Limit(RequestBody) }`; both are closed. No data byte past 1024 reaches the backend; check `backend_received` length ≤ head + framing + 1024.
  - `bad_chunk_before_response_is_400`: `5;x\r\n` → 400 with `Http(BadChunk(Extension))`, and the upstream is closed.
  - `expect_continue_gets_100_then_body`:
    - with `Expect: 100-continue` and `Content-Length: 5`, the client is sent nothing before the upstream connects;
    - then the client receives exactly `HTTP/1.1 100 Continue\r\n\r\n`;
    - the client sends `hello`;
    - the backend's head has no `expect` line;
    - the final response follows the 100 on the client side.
  - `expect_continue_without_body_sends_no_100`.
  - `early_response_stops_forwarding` (Review Focus 3): the client sends the head plus 10 of 100 body bytes, then the backend answers `HTTP/1.1 413 Nope\r\nContent-Length: 0\r\n\r\n`. Then:
    - the client gets the 413 with `connection: close` and a lingering close;
    - later client body bytes never reach the backend (`backend_received` is unchanged after the client sends the rest);
    - `Proxied { status: 413 }` is logged.
  - `client_body_stall_is_408`: the client sends the head and 1 of 5 body bytes, then pauses past `body_idle_timeout` → 408 with `Rejected { Timeout(Body) }`; the upstream is closed.
  - `backend_not_reading_the_body_is_504`: backend window `Stalled`, a body larger than the client's receive buffer → 504 with `Backend { BodyTimeout }`.
  - `client_eof_mid_body_cuts_both`: `send_eof` after 2 of 5 bytes → `Rejected { IncompleteBody }`; both are closed.
  - `request_watermark_hysteresis`: receive capacity `C` (from `max_head`), backend window `Stalled`. The client sends the head alone and the sim runs, then the client queues `4C` body bytes. After running, the client has `queued_to_server == 4C - C` (the pipe is full; in this test the head was parsed out of the buffer before the body filled it). Set the backend window to `Budget(C/2 - 1)` and run: `queued_to_server` is unchanged (no read yet). `Budget(1)` more: the client is read again.
- [ ] **Step 2: Run** `cargo test -p durchlauferhitzer-sim --test proxy`. Expected: the new tests FAIL (501 answers).
- [ ] **Step 3: Implement** the body forwarding per the Interfaces block.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Run all four pre-commit checks, commit** "Proxy request bodies: length, chunked, Expect, limits", and push.

---

### Task 8: Response bodies, limits and backpressure

**Files:**
- Modify: `core/src/core/proxy.rs`
- Modify: `sim/tests/proxy.rs`

**Interfaces:**
- Consumes: `ChunkedBody`, `ResponseFraming`, `Pipe`.
- Produces: the `Streaming` phase for every framing.
  - `Length(n)`: forward exactly `n` bytes, then `CloseUpstream`; drop any bytes past `n`.
  - `Chunked`: forward what `ChunkedBody` judged valid; the end is `Done`.
  - `UntilClose`: forward everything until `UpstreamReadClosed`; the client connection then closes after the response.
  - Response chunked to an HTTP/1.0 client → 502 with `ChunkedToHttp10`.
  - The `max_response_body` limit counts payload bytes (`data_len` for chunked). When the next bytes would pass it: `Backend { ResponseTooLarge }`, cut both, and none of those bytes is forwarded.
  - Upstream interest: `Read` while the response pipe offers spare and the response is unfinished.

- [ ] **Step 1: Write the failing tests** in `sim/tests/proxy.rs`:
  - `large_length_response_streams_with_bounded_memory`: a 10 MiB patterned body, client window `Bytes(3000)`, backend read chunk 5000 → byte-exact; every `send_buffer` slice ≤ `proxy_capacity()` (sim check); the upstream is closed at the end; keep-alive holds.
  - `response_watermark_hysteresis`: let `P = proxy_capacity()`, `h` = the raw head length and `r` = the rendered head length. Client `Stalled`; the backend queues the head plus `3P` body bytes in one send. After running, `backend_queued == h + 3P − P`: the pipe read `P` bytes, the head was parsed out, and the `P − h` body bytes left keep it paused. Then `Budget(r + P/2 − h − 1)` the client and run: `backend_queued` is unchanged (`P/2 + 1` bytes unread). Add `Budget(1)` and run: `backend_queued` drops.
  - `chunked_response_is_passed_through`: the client gets the backend's chunked bytes unchanged, and keep-alive holds.
  - `until_close_response_closes_the_client`: no CL or TE; `backend_send_eof` → the client gets the body and `connection: close`, then a lingering close.
  - `bytes_past_content_length_are_dropped` (Review Focus 2): the backend sends a full response plus `HTTP/1.1 200 OK\r\n…` garbage → the client sees exactly one response, and the next client request works on a new upstream.
  - `head_response_with_body_bytes_is_head_only` (Review Focus 4): HEAD, and the backend sends `Content-Length: 5` and `hello` → the client gets the head only; then a GET on the same connection works.
  - `chunked_response_over_limit_cuts_both`: on `/api` (1 MiB), chunks totalling 1 MiB + 1 → cut, `Backend { ResponseTooLarge }`, and the client received fewer than 1 MiB + 1 data bytes.
  - `until_close_over_limit_cuts_both`.
  - `bad_response_chunk_cuts_both`: `BadChunk(BadSize)`.
  - `backend_close_mid_body_cuts_the_client`: CL 10, 4 bytes, EOF → `Backend { ClosedEarly }`; the client is closed (not lingering) and holds exactly head + 4 bytes.
  - `backend_reset_mid_body_cuts_the_client`: `Backend { Reset }`.
  - `backend_body_stall_is_logged_and_cut`: the backend stops mid-body for `body_idle_timeout` → `Backend { BodyTimeout }`; both are closed.
  - `chunked_to_http10_client_is_502`.
- [ ] **Step 2: Run** `cargo test -p durchlauferhitzer-sim --test proxy`. Expected: the new tests FAIL.
- [ ] **Step 3: Implement** per the Interfaces block. Remove the Task 6 501 answers.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Run all four pre-commit checks, commit** "Proxy response bodies: framing, limits, backpressure", and push.

---

### Task 9: Randomized proxy net, mixed-route isolation, spec and docs

**Files:**
- Create: `sim/tests/proxy_faults.rs`
- Modify: `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md` (apply amendments 1–13)
- Modify: `examples/durchlauferhitzer.conf` (the proxy behaviour comments)
- Modify: `README.md` (only if it describes the stage status)

**Interfaces:**
- Consumes: the sim backend API (Task 6) and the existing random-net helpers in `sim/tests/faults.rs`. Copy their seed handling: the seed is printed on failure and an env var replays it, if `faults.rs` has one; reuse its exact mechanism.

- [ ] **Step 1: Write the tests.**
  - `slow_disk_does_not_stall_proxied_requests` (Review Focus 5): one host with a static `/` and proxy `/api`, and the disk delay set to an hour. Ten static GETs are opened (all hang), then a proxied GET completes normally and is logged `Proxied { 200 }`.
  - `random_proxy_net` (1500 seeds, like the static random net). Per seed, some clients run random exchanges with random:
    - methods;
    - bodies: none, length or chunked, with random chunk splits;
    - `Expect`;
    - read and write chunk sizes and windows;
    - and one fault per exchange from: none, client reset at a random point, client stall, client EOF mid-body, backend refuse/hang/reset/EOF at a random point, a malformed head, a bad chunk, over-limit bodies, bytes past CL.

    The model predicts each exchange's outcome: the status the client sees (backend status, 400/408/413/417/502/504), or a cut at a known point. Assert, per exchange:
    - (a) the backend received exactly the expected forwarded head and the body prefix the model allows;
    - (b) the client received exactly the expected bytes;
    - (c) the logs match;
    - (d) after the run, no upstream is open, every slot is free (a full set of new clients is accepted), and `open_files() == 0`.

    Count outcomes in a `Coverage` and require every bucket above zero, the same mechanism as the static random net.
- [ ] **Step 2: Run** `cargo test -p durchlauferhitzer-sim --test proxy_faults`. These tests exercise code that already exists, so a failure here is a bug: fix it in the core with its own regression test in `sim/tests/proxy.rs`, under red/green, before moving on. Record each such fix in the commit message. Expected at the end: PASS, with the coverage printed.
- [ ] **Step 3: Apply amendments 1–13 to the spec.** Put them in their sections: "Proxy routes", "Streaming and flow control", "Limits and timeouts", "HTTP behavior" (the `Connection` and chunked rules), "Logging", and "Drivers" (the upstream contract summary and the descriptor budget). Update the example config's proxy section to say what a client sees for each failure.
- [ ] **Step 4: Run all four pre-commit checks, commit** "Randomized proxy net; spec amendments for stage 3", and push.
