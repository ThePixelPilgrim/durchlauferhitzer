# Stage 3: Reverse Proxy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (mandatory in this repo) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. In this repo, `superpowers:<name>` is the project skill `<name>`.

**Goal:** Proxy routes forward requests to their backend and stream bodies in both directions through the sans-IO core, with strict framing on both sides, bounded memory, backpressure and clear failure semantics — all driven and tested through the simulation with scripted backends.

**Architecture:** First split the 2,700-line `core.rs` into focused modules (no behaviour change). Then add pure building blocks (response-head parser, chunked-framing validator, request rewriter), then an upstream interface mirroring the client one: the core allocates upstream ids, asks the driver to connect to a backend resolved at startup, and owns the upstream's buffers. Body bytes are forwarded **without copying**: the bytes the core hands out to write on one side are the region of the other side's receive buffer that holds validated body bytes. Only rewritten heads are rendered into send buffers.

**Tech Stack:** Rust 2024, std only in core; sim test-only.

**Spec:** `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md` — "Proxy routes", "Streaming and flow control", "Limits and timeouts", "HTTP behavior". The amendments below are applied to the spec together with this plan. Carry-forwards: `docs/superpowers/reviews/2026-10-25-stage-1-review.md`, `docs/superpowers/reviews/2026-10-26-stage-2-review.md`.

## Spec amendments (decided while planning; listed for the owner's review)

1. **Ids and resolution.** Upstream connection ids are allocated by the core (like file ids). Backends are listed by `Core::backends()` (distinct `host:port` in config order, `BackendId` = index); the driver resolves each once at startup and connects by id.
2. **Zero-copy body forwarding, fill-drain-refill.** Validated body bytes are written to the other side straight out of the receiving side's buffer. That buffer is read into only when it is completely empty (from offset 0, offering its full capacity); while it holds unsent bytes the core does not read that side. No compaction, so no body byte is ever copied. Leftover bytes of a pipelined next request after a body end are moved to the front once, as for heads. The strategy is internal (driver API unchanged), so stage 7 may swap in double buffering after measuring.
3. **Chunked framing is strict and forwarded verbatim** after validation: hex sizes without leading `+`, no chunk extensions, no non-empty trailers, CRLF only, size bounded by `u64`. A violation from the client → 400 (before the head is forwarded) or both connections cut (after); from the backend → 502 or cut.
4. **`Expect: 100-continue`.** The core answers `100 Continue` itself once the request head passed all checks (route, limits), and strips `Expect` toward the backend. Any other `Expect` value → 417.
5. **Backend interim responses.** `1xx` other than `101` are dropped; `101 Switching Protocols` (no Upgrade support) → 502.
6. **Response framing to the client:** backend `Content-Length` and `chunked` pass through; a close-delimited backend response is forwarded with `Connection: close` and the client connection closes at its end. HEAD, `204` and `304` never carry a body, whatever the backend sends.
7. **Request head toward the backend:** request line `METHOD <stripped path>[?query] HTTP/1.1`; hop-by-hop headers removed (`Connection` and every header it names, `Keep-Alive`, `Proxy-Connection`, `Proxy-Authorization`… — all `Proxy-*`, `TE`, `Trailer`, `Upgrade`, `Expect`); `Host` unchanged; `X-Forwarded-For` = the client's existing value (if any) + `, ` + peer address (canonical form); `X-Forwarded-Proto: http` replaces any client value; `Content-Length` re-rendered without leading zeros; `Connection: close` added.
8. **Response head toward the client:** hop-by-hop headers removed as above; status line re-rendered as `HTTP/1.1 <code> <backend reason>` (reason restricted to printable ASCII, else the standard reason); `Connection: close` added when the client connection will close. Backend `Date` and `Server` pass through unchanged.
9. **Client persistence after a proxied response:** keep-alive only if the request asked for it, its body was fully read, and the response was not close-delimited.
10. **New limits** (in `limits`, with defaults): `backend_connect_timeout 5s`, `backend_response_timeout 60s` (from the request head being fully written to the backend until its response head is complete), `body_idle_timeout 30s` (no body progress in either direction). Backend response heads obey `max_head` and `max_headers`.
11. **Failure matrix:** connect refused/reset or a malformed/oversized response head before anything was sent to the client → 502; `backend_connect_timeout` or `backend_response_timeout` → 504; any failure after the response head was queued → both connections closed (cut body); the client resetting or timing out → the upstream is closed at once.
12. **Accounting:** each client has at most one upstream at a time; upstream sockets are part of the file-descriptor budget (one per counted connection).

## Global Constraints

- Everything in the stage 1 and 2 Global Constraints still holds (zero product dependencies in core, no `unsafe`, denied clippy lints in core, no `_ =>` on enums, enums over bools, strict red/green TDD — failing test first and run before any implementation, bounded memory).
- Body bytes never get copied by the core (only leftover pipelined bytes after a body end are moved once); every buffer has a fixed capacity derived from `Limits`.
- Every upstream is closed exactly once on every path; the sim checks this.
- Nothing from a backend reaches the client unvalidated: response heads are parsed strictly, bodies are framed strictly, and nothing after a framing error is written.
- The decoded request path is never logged; logs keep the raw path.

## Review Focus

1. Request smuggling toward the backend — `Content-Length` + `Transfer-Encoding`, duplicate lengths, chunk-extension and trailer tricks, a pipelined second request hidden in a body: the backend must see exactly one well-framed request. Tests in Tasks 3, 4, 6.
2. Response splitting / desync from a hostile backend — extra bytes after a `Content-Length` body, a second response, CR/LF in reason phrases or header values: never forwarded. Tests in Tasks 2, 7.
3. A slow backend and a fast client (and the reverse) with bodies far larger than the buffers — memory stays bounded, bytes arrive intact, reading pauses while the buffer holds unsent bytes and resumes only when it is empty, with full-capacity reads. Tests in Tasks 6, 7.
4. Failure after the response head was sent — the client sees a cut connection, never a short complete-looking body. Tests in Tasks 7, 8.
5. Client gone while connect or response is pending — the upstream is closed at once and its late events are harmless. Tests in Tasks 5, 8.

---

## File Structure (after Task 1)

```
core/src/core/mod.rs          Core, handle dispatch, buffers API, next_deadline
core/src/core/contract.rs     driver contract docs (module-level), Event, Action, Interest, ids
core/src/core/request.rs      reading heads, routing, fixed responses, keep-alive/linger
core/src/core/limits.rs       connection counting, refusals, orphan slots
core/src/core/static_files.rs static route handling (open, index, send, abort)
core/src/core/proxy.rs        proxy route handling (Tasks 5–8)
core/src/core/timers.rs       deadlines and TimerFired
core/src/conn.rs              ConnState (non-Copy) and per-connection data
core/src/upstream.rs          UpstreamId, BackendId, upstream state and buffers (Task 5)
core/src/http/response_head.rs  backend response-head parser (Task 2)
core/src/http/chunked.rs        streaming chunked-framing validator (Task 3)
core/src/http/forward.rs        request/response head rewriting (Task 4)
sim/src/backend.rs            scripted backends
sim/tests/proxy.rs            end-to-end proxy tests
```

Exact module names inside `core/src/core/` are the implementer's call in Task 1; the split must follow responsibilities as above.

---

### Task 1: Split `core.rs` and make `ConnState` non-`Copy`

Pure refactor; no behaviour change, no test weakened.

**Files:**
- Create: `core/src/core/` modules as above (the public API path `durchlauferhitzer_core::core::*` / crate-root re-exports stay identical)
- Modify: `core/src/conn.rs`, `core/src/lib.rs`, `core/src/http/target.rs` (stale proptest comment "raw path is canonical")

**Interfaces:**
- Produces: identical public API. Internally, `ConnState` drops `Copy`; the static request (`Conn::static_request`) moves into `ConnState::OpeningFile`, removing the unreachable `static_request == None` branches; `release_orphan` gets an explicit assertion or type for "orphan came from a counted connection" (stage 2 deferred minor).

- [ ] **Step 1: Record the baseline** — `cargo test --workspace --locked 2>&1 | grep "test result"`; save the per-suite counts in the report.
- [ ] **Step 2: Move code** module by module; after each move run `cargo test --workspace --locked`.
- [ ] **Step 3: Make `ConnState` non-`Copy`**, move the static request into `OpeningFile`, delete the unreachable branches.
- [ ] **Step 4: Run everything** — `cargo fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked && scripts/check-product-deps.sh` → PASS with the same test counts (or more); no file in `core/src/` over ~900 lines.
- [ ] **Step 5: Commit** — `"Split core.rs into modules; ConnState owns the static request"`

---

### Task 2: Backend response-head parser

**Files:**
- Create: `core/src/http/response_head.rs`
- Modify: `core/src/http/mod.rs`

**Interfaces:**
- Consumes: header-line rules and `HeadLimits` from `core/src/http/request.rs` (reuse the line judging; do not duplicate it)
- Produces:
  - `pub struct ResponseHead { pub status: u16, pub reason: String, pub headers: Vec<Header>, pub body: ResponseBody }`
  - `pub enum ResponseBody { None, Length(u64), Chunked, UntilClose }`
  - `pub enum RequestKind { Head, Other }` (input: HEAD requests and 204/304/1xx get `None`)
  - `pub enum ResponseError { HeadTooLarge, TooManyHeaders, BadStatusLine, BadVersion, BadHeaderName, BadHeaderValue, BareLf, ObsFold, ContentLengthWithTransferEncoding, BadContentLength, DuplicateContentLength, UnsupportedTransferEncoding, SwitchingProtocols }`
  - `pub struct ResponseParser` (resumable like `HeadParser`) with `fn advance(&mut self, buf: &[u8], limits: &HeadLimits, kind: RequestKind) -> Result<ResponseStatus, ResponseError>`; `pub enum ResponseStatus { Incomplete, Interim { consumed: usize }, Complete { head: ResponseHead, consumed: usize } }` — `Interim` for 100/102/103 (caller drops it and parses again), `SwitchingProtocols` error for 101

Rules: `HTTP/1.1` or `HTTP/1.0` + SP + three digits 100–599 + SP + reason (printable ASCII and HTAB; may be empty) + CRLF; header lines exactly as for requests; framing as for requests (CL+TE, duplicate CL, non-`chunked` TE → error); no framing header → `UntilClose`; status 1xx/204/304 or `RequestKind::Head` → `None`. Same split-independence guarantee as the request parser.

- [ ] **Step 1: Write the failing tests** — `parses_simple_200` (exact `ResponseHead`), `interim_100_is_reported_and_skipped`, `switching_protocols_is_an_error`, table `rejects_malformed_responses` (one row per `ResponseError`, incl. `"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n"`, reason with `\r`, status `099`/`600`/`20`, `HTTP/2.0`), `body_framing_cases` (CL, chunked, until-close, HEAD, 204, 304), proptests `never_panics` and `prefix_never_contradicts_full`.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run everything** (pre-commit checks) → PASS.
- [ ] **Step 5: Commit** — `"Strict backend response-head parser"`

---

### Task 3: Streaming chunked-framing validator

**Files:**
- Create: `core/src/http/chunked.rs`

**Interfaces:**
- Produces:
  - `pub struct ChunkedValidator` — no allocation, constant memory
  - `pub fn ChunkedValidator::advance(&mut self, bytes: &[u8]) -> Result<ChunkProgress, ChunkedError>`; `pub struct ChunkProgress { pub consumed: usize, pub data: u64, pub done: Done }`, `pub enum Done { No, Yes }` — `consumed` bytes of input are valid framing+data and may be forwarded verbatim; `data` counts payload bytes in them (for `max_*_body`); `Yes` after the final `0\r\n\r\n` (bytes after it are not consumed)
  - `pub enum ChunkedError { BadSize, SizeOverflow, Extension, Trailer, BadLineEnd }`

Rules: spec amendment 3. Validator state survives arbitrary splits of the input (byte by byte must give the same result).

- [ ] **Step 1: Write the failing tests** — `simple_body`, `zero_length_body`, `extension_rejected` (`5;x=y\r\n`), `trailer_rejected` (`0\r\nX: y\r\n\r\n`), `leading_plus_and_whitespace_rejected`, `overflow_rejected` (17 hex digits), `bare_lf_rejected`, `stops_at_end` (bytes after `0\r\n\r\n` not consumed — the smuggling case), proptest `split_independent` (any split of valid and invalid inputs gives the same total result).
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run everything** → PASS.
- [ ] **Step 5: Commit** — `"Strict streaming chunked-framing validator"`

---

### Task 4: Head rewriting for both directions

**Files:**
- Create: `core/src/http/forward.rs`

**Interfaces:**
- Consumes: `RequestHead` (stage 1), `ResponseHead` (Task 2), `strip_segments` (routing), `Persistence`
- Produces:
  - `pub fn render_backend_request(head: &RequestHead, path: &str, peer: IpAddr, out: &mut Vec<u8>)` — amendment 7 exactly
  - `pub fn render_client_response(head: &ResponseHead, persistence: Persistence, out: &mut Vec<u8>)` — amendment 8 exactly
  - `pub fn backend_request_len(..) -> usize` / `client_response_len(..) -> usize` (or a capacity check) so callers know a head fits before rendering
  - `pub fn is_hop_by_hop(name: &str, connection_tokens: &[&str]) -> bool`

- [ ] **Step 1: Write the failing tests** — exact bytes for: simple GET with strip; query kept; `Connection: keep-alive, X-Secret` removes `X-Secret`; `TE`, `Upgrade`, `Proxy-Authorization`, `Expect` removed; `X-Forwarded-For` appended to an existing value and set when absent (IPv4, IPv6 canonical, v4-mapped canonicalised); `X-Forwarded-Proto: https` from the client replaced by `http`; `Content-Length: 007` → `7`; chunked request keeps `Transfer-Encoding: chunked`; response with a hop-by-hop header removed and `Connection: close` added for `Close`; bad reason → standard reason; lengths match rendered bytes.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run everything** → PASS.
- [ ] **Step 5: Commit** — `"Rewrite request and response heads for proxying"`

---

### Task 5: Upstream interface and scripted backends

Like stage 1's Task 8a and stage 2's Task 2: contract and sim first, with only the simplest behaviour (body-less request, `Content-Length` response).

**Files:**
- Create: `core/src/upstream.rs`, `core/src/core/proxy.rs`, `sim/src/backend.rs`, `sim/tests/proxy.rs`
- Modify: core modules from Task 1, `core/src/log.rs`, `sim/src/lib.rs`

**Interfaces:**
- Produces (core, re-exported):
  - `pub struct UpstreamId(pub u64)` (core-allocated, from 1, never reused), `pub struct BackendId(pub u32)`, `pub fn Core::backends(&self) -> &[BackendAddr]`
  - `Action::Connect { up: UpstreamId, backend: BackendId }`, `Action::SetUpstreamInterest { up: UpstreamId, interest: Interest }`, `Action::CloseUpstream { up: UpstreamId }`
  - `Event::UpstreamConnected { up }`, `Event::UpstreamFailed { up, error: ConnError }`, `Event::UpstreamReceived { up, len: usize }`, `Event::UpstreamReadClosed { up }`, `Event::UpstreamSent { up, len: usize }`
  - `pub fn Core::upstream_recv_buffer(&mut self, up: UpstreamId) -> &mut [u8]`, `pub fn Core::upstream_send_buffer(&self, up: UpstreamId) -> &[u8]`
  - `Log::Response` gains the proxied status; `Log::BackendFailed { conn: ConnId, peer: IpAddr, backend: BackendId, kind: BackendFailure }`, `pub enum BackendFailure { Refused, Reset, BadResponse(ResponseError), ConnectTimeout, ResponseTimeout }`
- Produces (sim): `Sim::backend(&mut self, addr: &str) -> BackendHandle`-style API: `set_connect(addr, ConnectBehaviour::{Accept, Refuse, Hang})`, `take_backend_request(up) -> Vec<u8>`, `backend_send(up, &[u8])`, `backend_eof(up)`, `backend_reset(up)`, `set_backend_window(up, Window)`, `upstreams(&self) -> Vec<UpstreamId>`, `open_upstreams(&self) -> usize`

Contract additions (core module docs, in the style of the file contract): the driver resolves `backends()` at startup; `Connect` is a non-blocking connect answered by exactly one `UpstreamConnected` or `UpstreamFailed` unless `CloseUpstream` comes first (then nothing more for that id); the same buffer, readiness and "report before the next call" rules as client sockets apply; `CloseUpstream` means close the socket; the upstream fd budget. Behaviour in this task: a GET/HEAD/DELETE-style request without body on a proxy route → `Connect`; on connect, the rewritten head (Task 4) is written; the response head is parsed (Task 2); the rewritten response head and a `Content-Length` body (forwarded zero-copy from the upstream's receive buffer, amendment 2) go to the client; then `CloseUpstream` and keep-alive/close per amendment 9. Connect refused/reset or bad response head → 502 with `Log::BackendFailed`. Requests with bodies and other response framings still answer 501 here (Tasks 6–7).

Sim invariants added: during a body phase, a read is never offered into a buffer that still holds unsent body bytes and is always offered the full capacity (fill-drain-refill); actions for unknown or closed upstreams, a second `CloseUpstream`, upstream buffers over capacity, upstream left open after its client closed, more than one upstream per client.

- [ ] **Step 1: Write the failing tests** (config: `path /api { proxy 127.0.0.1:3000; strip_prefix; max_request_body 1MiB; max_response_body 1MiB; }` on host `example.test`):
  - `forwards_get_and_relays_response` — backend sees exactly the amendment-7 bytes for `GET /api/x?y=1`; client gets exactly the amendment-8 rewrite + body; `open_upstreams() == 0`; client `Open` (keep-alive)
  - `large_content_length_body_is_streamed_intact` — 3 MiB response through a 16 KiB buffer; client window `Bytes(1000)`; memory bounded (sim capacity checks), bytes intact
  - `connect_refused_is_502`, `backend_reset_before_head_is_502`, `malformed_backend_head_is_502`
  - `client_reset_while_connecting_closes_upstream` (Review Focus 5)
  - `backends_are_listed`
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement** (contract docs first, then core, then sim).
- [ ] **Step 4: Run everything** → PASS.
- [ ] **Step 5: Commit** — `"Upstream interface and scripted backends"`

---

### Task 6: Request bodies

**Files:**
- Modify: `core/src/core/proxy.rs`, `core/src/conn.rs`, `core/src/upstream.rs`
- Test: `sim/tests/proxy.rs`

Rules: `Content-Length` and chunked request bodies stream client → backend zero-copy from the client's receive buffer (chunked validated by Task 3 and forwarded verbatim); `max_request_body` per spec (413 before forwarding when `Content-Length` exceeds it; cut both connections once a chunked body passes it); backpressure per amendment 2 (no client reads while its buffer holds unsent body bytes; read again, with full capacity, once it is empty); `Expect: 100-continue` per amendment 4; a backend that answers before the body is complete → forward the response, stop forwarding the body, close the client after the response (unread body); bytes after the body end stay for the next request (pipelining), never reach the backend.

- [ ] **Step 1: Write the failing tests** — `post_with_content_length_body`, `chunked_upload_forwarded_verbatim`, `upload_larger_than_buffer_with_slow_backend` (Review Focus 3: 5 MiB, backend window `Bytes(512)`, bounded memory, intact bytes, trace shows no client read while body bytes are unsent and every body read offered the full capacity), `content_length_over_limit_is_413_before_connect`, `chunked_over_limit_cuts_both`, `expect_100_continue_answered_and_stripped`, `other_expect_is_417`, `early_backend_response_closes_client`, `pipelined_request_after_body_is_not_forwarded` (Review Focus 1), `chunk_extension_from_client_is_400`.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run everything** → PASS.
- [ ] **Step 5: Commit** — `"Stream request bodies to backends"`

---

### Task 7: Response bodies

**Files:**
- Modify: `core/src/core/proxy.rs`, `core/src/upstream.rs`
- Test: `sim/tests/proxy.rs`

Rules: chunked responses validated (Task 3) and forwarded verbatim; close-delimited per amendment 6; HEAD/204/304 bodies never forwarded (extra backend bytes discarded and the upstream closed); bytes after a `Content-Length` or chunked body end are never forwarded (upstream closed — Review Focus 2); `max_response_body` per spec (502 before anything is sent when `Content-Length` exceeds it; cut once a chunked or close-delimited body passes it); backpressure toward a slow client (no upstream reads while its buffer holds unsent body bytes; read again once it is empty); a framing error after the head → cut both (Review Focus 4).

- [ ] **Step 1: Write the failing tests** — `chunked_response_forwarded_verbatim`, `close_delimited_response_closes_client`, `head_response_carries_no_body`, `no_content_and_not_modified_have_no_body`, `extra_bytes_after_content_length_never_forwarded`, `second_response_never_forwarded`, `response_over_limit_is_502_or_cut`, `slow_client_large_response_bounded` (Review Focus 3), `bad_chunk_after_head_cuts_both` (Review Focus 4), `interim_100_from_backend_dropped`, `switching_protocols_is_502`.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run everything** → PASS.
- [ ] **Step 5: Commit** — `"Stream backend responses to clients"`

---

### Task 8: Proxy timeouts, limits and the failure matrix

**Files:**
- Modify: `core/src/config/model.rs`, `core/src/config/validate/` (new limit keys + tests), `core/src/core/{proxy.rs,timers.rs}`, `examples/durchlauferhitzer.conf` (document the three new limits and the proxy behaviour: forwarded headers, 502/504, Expect), spec failure-matrix wording if implementation shows a gap
- Test: `sim/tests/proxy.rs`, config tests

Rules: amendments 10 and 11; every proxy state carries a deadline; body idle timeout applies in both directions; client reset/EOF/timeout closes the upstream at once; upstream reset/EOF mid-body cuts the client; 504s and 502s logged with `BackendFailure`.

- [ ] **Step 1: Write the failing tests** — `connect_timeout_is_504`, `response_timeout_is_504` (backend reads the request, never answers), `request_body_idle_timeout_cuts`, `response_body_idle_timeout_cuts`, `client_reset_mid_response_closes_upstream`, `backend_reset_mid_body_cuts_client`, `client_send_timeout_closes_upstream`, config: the three keys with defaults and validation, example config still loads.
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run everything** → PASS.
- [ ] **Step 5: Commit** — `"Proxy timeouts, limits and failure handling"`

---

### Task 9: Proxying in the randomized simulation

**Files:**
- Modify: `sim/tests/faults.rs`

- [ ] **Step 1: Extend the tests** — proxy routes (with and without `strip_prefix`, varied body limits) next to the static ones; per seed, random backend behaviours (refuse, hang, slow accept; well-formed CL / chunked / close-delimited / no-body responses of 0 B–2 MiB; interim 100; malformed heads; extra bytes after the body; resets and EOF at random points; random windows) and random client requests (bodies by CL and chunked, `Expect`, oversized bodies, pipelining, smuggling attempts). Oracle: the backend receives exactly the amendment-7 request and exactly the client's body; the client receives exactly the amendment-8 head and exactly the backend's body bytes, or a cut connection with a matching log, or the predicted 4xx/5xx; nothing after a framing end is ever forwarded. End-of-run: no open upstreams, files or connections. Coverage must reach 502, 504, 413, 417, 100-continue, every `ResponseBody` kind, cut-after-head, early backend response.
- [ ] **Step 2: Run** — any failure is a core bug: fix with a pinned-seed regression test.
- [ ] **Step 3: Run everything** (keep under ~10 s debug) → PASS.
- [ ] **Step 4: Commit** — `"Randomized simulation covers proxying"`
