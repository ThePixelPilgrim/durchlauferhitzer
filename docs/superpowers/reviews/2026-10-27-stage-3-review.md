# Stage 3 (Reverse proxy) — review record

Plan: `docs/superpowers/plans/2026-10-26-stage-3-reverse-proxy.md` (the spec, `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md`, is binding where they differ). Range `576a77c..b583d96` on `main`. Tasks 1–3 were built on `claude/magical-dijkstra-2eoh4y`; a parallel session's duplicate work on `ccr-716d91b0-bff174` was compared and its stronger tests and one fix ported (unification `cc3c21d`..`ad71ca7`, items U1–U17). Tasks 4–9 each landed on `main` after a clean task review. Final whole-stage review: 0 Critical, 3 Important, 9 Minor at `fde50b2`; fix wave `0d89d2e`..`d9743e9` and follow-ups `b583d96`; re-review: all findings addressed. Gate at `b583d96`: 489 tests, fmt, clippy `-D warnings`, product-deps check clean; randomized nets ~6 s.

## Owner decisions taken during stage 3 (all written into the spec)

- 2026-10-25: fill, drain, refill backpressure; obs-text rejected; `%3F` in paths → 400; response `Content-Length` passes through; stalled client body → 408; half-close keeps the upstream; interim cap 8; empty reason kept (FR-HTTP-05, -15a, FR-PROXY-12, -18, -26, NFR-RES-04).
- 2026-10-26 (D1–D5): `Connection` may not name `content-length`, `transfer-encoding` or `host` (FR-HTTP-09a, 400 / 502); one hop-by-hop list for both directions, `Expect` included; duplicate or list `Expect` → 417; more than one `X-Forwarded-For` line → 400; empty substituted reason for a code with no standard reason.
- 2026-10-26: `Interest::ReadWrite` (upstreams only) so an early backend response is seen while body bytes wait (FR-PROXY-27).
- 2026-10-26: process — fix every finding (Minor included) inside its task's loop; `main` is fast-forwarded after every reviewed task; sessions start from `main` and check remote branches first (CLAUDE.md "Branches and main"); MIT license declared.
- 2026-10-27: the request line toward the backend mirrors the client's version; any `Transfer-Encoding` toward an HTTP/1.0 client → 502, also on body-less replies (FR-PROXY-04, -21).
- 2026-10-27: every response head, interim or final, must be complete within `backend_response_timeout` of its own first byte (a completed interim head ends its cap) (FR-PROXY-29).
- 2026-10-27 (final review): client-sent `Forwarded`, `X-Real-IP`, `X-Forwarded-Host`, `X-Forwarded-Port` are stripped toward the backend (FR-PROXY-06); a client EOF mid-body is logged (FR-PROXY-26); backend addresses are IP literals only, no DNS; unspecified, broadcast and multicast backends are rejected, IPv6 scope ids kept (FR-CFG-16, DRV-10).

## Rulings made on the owner's behalf

- Unification: keep `main`'s code in all three parts; port the abandoned branch's tests and its Log → CloseFile → Close teardown order — cost if wrong: none found; re-review approved.
- Task 4: the duplicate-`X-Forwarded-For` rule lives in `check_forwardable`, called before `Connect` — cost if wrong: one more call site to remember.
- Task 4: request line always HTTP/1.1 — superseded by the owner decision of 2026-10-27.
- Task 5: a core-generated 502/504 keeps the request's persistence when nothing is unread — cost if wrong: one header and a lingering close.
- Task 5: the shared `Exchange` struct from the abandoned branch was not adopted (no `too_many_arguments` allow left; states own their data).
- Tasks 5–7: timeouts as named default constants until Task 8 made them `limits` keys.
- Task 8: `ClosedEarly` also logs a backend EOF mid-body (a cut), not only before the head (a 502).
- Task 8: a tie between the head cap and another deadline resolves to the 504.
- Task 9: deadline tie order while a response is awaited — send timeout, then head cap, then body idle / response timeout; a response queued behind an unread `100 Continue` keeps the send deadline that started with the `100`, also for a relayed response (FR-PROXY-25, -29) — cost if wrong: a log label in a rare tie / a non-reading client closed a few seconds earlier.
- Final review: D-A strips the identity headers from requests only (FR-PROXY-06 says they are not hop-by-hop; FR-PROXY-12 now says so explicitly).

## Deferred to later stages

- Before the stage-4 plan / first runnable release: open decision FR-CFG-21 (upper bounds for `limits`; at `max_head 1MiB` an active exchange holds about 3 MiB).
- Stage 4: the epoll driver must follow `core/src/core/contract.rs` exactly (upstream connect, `ReadWrite`, errors after readable bytes, fd budget incl. upstreams, NFR-RES-05 startup check).
- Stage 4/5: draw "bytes, then reset" backends in the randomized net (`backend_reset_after` is used by directed tests only).
- Stage 5: IPv6 per-IP grouping by prefix; refused-slot pool fairness.
- Stage 7: double buffering vs fill-drain-refill; per-request buffer allocation; `next_deadline` O(n).
- Owner, any time: delete the remote branches `ccr-716d91b0-bff174` (abandoned), `claude/magical-dijkstra-2eoh4y`, `unify`, `stage3-task4`…`stage3-task9`, `stage3-final` (all but the first are contained in `main`).

## Final whole-stage review (at `fde50b2`)


Reviewer: Opus, read-only. Range: 78 commits, 86 files. I read the final files directly, not the diff, in this order:

1. Contract and states: `core/src/core/mod.rs`, `contract.rs`, `conn.rs`, `upstream.rs`.
2. Proxy path: `core/{proxy,upload,relay,request,timers,limits}.rs`, `request_body.rs`, `relay_body.rs`.
3. HTTP pieces: `forward.rs`, `response_head.rs`, `head_line.rs` (framing and `Connection`), `target.rs` (query).
4. Config: the stage-3 delta.
5. Sim: `sim/src/{lib,backend}.rs`, `sim/tests/proxy/expect.rs`, `sim/tests/faults/proxy/mod.rs`, `response.rs` (defect → error mapping), `sim/tests/support/model_http.rs`.
6. Spec (FR-PROXY, FR-CONN, FR-LOG, NFR-SEC/RES/ROB/TEST, DRV, Timeline, Open decisions), plan, HANDOFF, ledger extract, example config, both READMEs.

**Gate at HEAD fde50b2, run in place** after touching every `.rs` file: `cargo fmt --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked` (480 tests; the faults binary, with both randomized nets, takes 5.2 s) and `scripts/check-product-deps.sh`. All pass.

**Probes.** I ran three probe tests in a throwaway worktree at fde50b2, since removed:

- *Carry-over with a late body.* The client never reads its `100 Continue`. The response head arrives at 2 s and its body at 5 s. Result: still open at 7 s, closed at 12 s. This confirms I2.
- *Chunk-size trickle.* The client never reads. The backend sends a chunked head, then one `0` byte every 2 s. Result: still open after 60 s, with send idle set to 3 s. This confirms I2.
- *`Content-Length: 0` with `Expect`.* No `100 Continue` is sent, which is correct.

**Remote branches.** No branch has commits that are missing from `main`, except the known abandoned `ccr-716d91b0-bff174` (12 commits). See the triage section.

## Strengths

- **The core is not an open proxy, by construction.** `proxy::plan` only ever produces a `BackendId` drawn from `Core::backends()`. Absolute-form and `CONNECT` are refused by the parser. `Host` only selects among configured hosts, and a miss is a 404. Dot segments, encoded `/` and encoded unreserved characters are rejected before routing, so a raw path that is forwarded cannot be respelled past a route.
- **Smuggling and desync are closed in both directions:**
  - The request goes out with exactly one framing: `Content-Length` (re-rendered) or chunked (verbatim after validation).
  - `Connection: close` is added, and there is one backend connection per request.
  - Bytes after a body end are never written. The upstream send buffer hands out only `recv[start..end]` of the validated body.
  - Responses are parsed by the same line rules as requests (`head_line.rs`), plus FR-HTTP-09a, the 1.0+TE rule and the strict framing checks.
  - The upstream is closed as soon as a body is complete, so a second response or junk is never sent.
  - `TE` toward an HTTP/1.0 client is a 502.
  - `forward.rs` builds `*_len` and the renderer on one `Sink` code path, so the size check and the bytes cannot disagree.
- **Zero-copy fill-drain-refill is clean and symmetric.** `RequestBody` and `RelayBody` share one shape: `start <= end <= filled`, a `judge` / `settle` pair, a tail of at most 35 bytes and saturating counts. Each method is short and documents its invariant. "Nothing of a failing read becomes sendable" holds in both directions.
- **Memory per exchange is fixed and derived from the limits:**
  - `ExchangeBuffers` is allocated at connect and dropped with the state.
  - The head buffer is guarded by an exact length check before every render.
  - `HEAD_SLACK` is justified byte by byte.
  - At most one upstream per counted connection; refused connections never connect.
- **The states carry only their own data.** `ConnState` is no longer `Copy`, the static request lives in `OpeningFile`, and proxy states own their buffers. `Relaying.body` turns "upstream still open" into `Source::Open | Done` instead of an `Option` beside a flag. Every match on an enum is exhaustive. No denied lint is suppressed in the core.
- **The deadline semantics are subtle but implemented exactly as recorded.** They cover the per-head cap with interim reset, the body-idle blame split (408 vs 504 `BodyTimeout`), the tie order (send, then cap, then the rest) in `expire_proxy` arm order, and the carry-over into the `Responding` 408/502/504. Each has a directed test named in the spec.
- **The proxy net is independent and strong.**
  - Expected bytes come from the spec models (`model_http.rs`, `request.rs`, `response.rs`), never from core renderers (grep confirms no renderer use under `sim/tests`).
  - Hostile heads map to their expected `ResponseError` per defect.
  - Coverage buckets are mandatory.
  - The end of a seed checks that no connection, file or upstream is left open and every slot is free.
- **Spec traceability is excellent.** Every builder call, owner decision and controller ruling seen in the ledger extract is written into the FR it changes, with its date and tests.
- **The example config is faithful.** It documents the full failure matrix, the forwarded-header rules and the new limits, and a test keeps it loadable.

## Issues

### Critical (Must Fix)

None.

### Important (Should Fix)

**I1. The upstream contract lets a driver report a socket error before the response bytes that are already readable. That throws away exactly the early responses the `ReadWrite` decision was made for.**

- **Where:** `core/src/core/mod.rs:207-212`, which says socket errors and hang-ups are "reported whatever the interest". Also `mod.rs:236-238` (any socket error after the connect is `UpstreamFailed`) and DRV-10 in the spec.
- **What goes wrong.** A backend rejects an upload early: it writes `413` (or `401`) and closes with the client's body still unread in its socket. Its kernel then sends RST instead of FIN. The response segments arrive before the RST. On Linux they stay readable until the read that returns `ECONNRESET` (other stacks may discard them).
  - While the core reads under `ReadWrite`, epoll reports `EPOLLIN | EPOLLERR | EPOLLHUP` together.
  - The next body write fails with `EPIPE`/`ECONNRESET`.

  The contract allows the driver to report `UpstreamFailed` first. That turns the backend's 413 into our 502. The same applies in `Relaying` while the upstream's interest is `None` (the client is draining): an RST cuts the client although the rest of the response is still in the kernel buffer.
- **Why it matters.** FR-PROXY-27 and the owner decision of 2026-10-26 exist so that an early answer reaches the client. A stage-4 driver that follows the contract to the letter can lose that answer in the most common case: a backend that refuses a large upload. The contract is the stage-3 deliverable for stage 4 and must not leave this to guesswork. The sim cannot show the problem: `backend_reset` (`sim/src/backend.rs:277-288`) clears the queued bytes and fails at once, so only the error-first behaviour is ever exercised.
- **How to fix:**
  1. In the contract and DRV-10: for an upstream, report a socket error only when a read or write hits it, or for a failed connect via `SO_ERROR`.
  2. Under `Read`/`ReadWrite`, read readable bytes before reporting the error. A failed write under `ReadWrite` is reported only after the reads are drained.
  3. While the upstream's interest is `None`, defer the error until the core asks to read or write again.
  4. Clients can keep "whatever the interest": a client error means it is gone.
  5. In the sim, add `backend_reset_after(up, bytes)`, which delivers the queued bytes and then `UpstreamFailed`. Add directed tests: "early 413 then RST during the upload is relayed" and "RST after a complete close-delimited tail while the client drains".

**I2. While a response is relayed, backend progress restarts the client's send idle deadline even when the client has taken nothing. This breaks the controller ruling on carry-over and lets a non-reading client be held about 33 times longer than `send_idle_timeout`.**

- **Where:** `core/src/core/relay.rs:164-165`, in `relay_received`. It computes `pending = client_pending(..)` and then `*deadline = relay_deadline(now, pending, ..)`, which is `now + send_idle_timeout` whenever the client has bytes to take. `relay.rs:204-205`, in `relay_read_closed`, does the same.
- **What goes wrong.** Both probes above confirm it.
  - *Carry-over.* FR-PROXY-25 says "any response queued behind an unread `100` (… or a proxied response) does not restart the send idle deadline … a client that still takes nothing is closed when the deadline started with the `100` runs out". That holds only when the body arrives together with the head, which is the shape the test `unread_100_continue_carries_its_send_deadline_into_a_relay` uses. With the body arriving at 5 s, the client was still open at 7 s and closed at 12 s.
  - *Chunked tail.* With no `100` at all: the client does not read the head, and the backend sends one chunk-size byte every 2 s. Each byte only grows the chunked tail. So `unsent_len` stays 0, the upstream keeps being read, and every read resets the client's deadline. With send idle at 3 s the client was still open after 60 s. The bound is the 32-character size limit plus CRLF, about 35 resets, so up to about 17 minutes at the default 30 s. FR-CONN-08 says the send idle deadline is reset "whenever the client socket accepts bytes".
- **Why it matters.** It is bounded, so it is not critical. But a hostile or broken backend together with a non-reading client can hold a slot well past the client's deadline, and the spec states a guarantee, with a cited test, that the code does not keep. The mod.rs Deadlines bullet ("relaying: … reset on progress either way", `mod.rs:351`) describes the buggy behaviour. That bullet also says nothing about the send deadline while a `100 Continue` is queued, the tie order or the carry-over, all of which FR-PROXY-25/-29 rely on.
- **How to fix:**
  1. In `relay_received` and `relay_read_closed`, only change the deadline when the client side's situation changes. If the client had bytes pending before the event, keep the deadline: backend progress is not client progress. If nothing was pending and now something is, start the send idle deadline from now. If the client is still waiting on the backend, use body idle from now.
  2. Put the "unread `100` carries its deadline" rule into one helper. It is currently written out three times: `proxy.rs:482-498`, `upload.rs:166-168` and `relay.rs:117-119`.
  3. Add regression tests for both probes: a late body behind an unread `100`, and a tail trickle toward a stalled client.
  4. Correct the mod.rs Deadlines section and add the `100 Continue` send deadline, the tie order and the carry-over there.

**I3. Status markers and the handoff are not truthful at stage close.**

- **Where, and what is wrong:**
  - **Spec:**
    - "How to read" (`spec:13-14`) says "[done] … stages 1–2, and stage 3 Tasks 1–3".
    - The Timeline (`spec:1293`) says "Reverse proxy [in progress: Tasks 1–3 done]".
    - The FR-PROXY intro (`spec:491-493`) says "the rest … in stage 3 Tasks 4–8"; Task 9 is missing.
    - `NFR-SEC-11` (`spec:1015`) still says "proxy stage 3".
    - Several mixed markers read as if work were still due in stage 3: "[done; proxy stage 3]", "[done; upstream buffers stage 3]", "[done; upstreams stage 3]", "[done; backends stage 3]". They are on NFR-SEC-01, NFR-RES-01, NFR-RES-06, NFR-ROB-04, NFR-ROB-05, NFR-TEST-01, NFR-TEST-03 and NFR-TEST-06.
  - **HANDOFF:**
    - Line 47 is an empty "Still to carry into later tasks" heading.
    - Line 54 still says "Tasks 5–6 still take the `Exchange` struct and `Expect` parsing from it".
    - Line 74 has a ledger snapshot "as of `1db3e36`", which ends at Task 3.
    - Line 95 still lists stage-3 carry-forwards that are done (normalised `Content-Length`, empty stripped path).
    - Line 3 has an older date than the 2026-10-27 decisions it describes.
- **Why it matters.** `main` is the single source of truth, and the next session starts from these two files. Stale "in progress" or "due in stage 3" markers will make stage-4 planning re-check finished work, or think it unfinished. The stage-2 review held spec drift to Important (I4) for the same reason.
- **How to fix.** In one doc-only commit:
  - Mark stage 3 done in the Timeline and in "How to read".
  - Collapse the mixed markers to `[done]`, or `[done in core; driver stage 4]` where the driver half remains.
  - Rewrite the HANDOFF stage-3 section: drop the stale lines, record the stage-3 deferred items from this review, and update the branch notes (see triage).

### Minor (Nice to Have)

- **M1. Two things about connecting are not specified for the stage-4 driver.**
  - **Multiple addresses.** `mod.rs:188-191` and DRV-10 say a backend `host:port` is "resolved once at startup" but not what happens when the name has several addresses. `localhost` gives both `::1` and `127.0.0.1`; a driver that picks `::1` for a backend listening only on IPv4 answers every request 502. Decide among: the first address only, try in order on refusal, or a startup error when there is more than one. Write it into DRV-10, or into Open decisions if the owner should choose.
  - **Immediate connect results.** Say that a `connect()` which succeeds or fails at once is still answered by a later `UpstreamConnected`/`UpstreamFailed` event, never inside the same `handle`. Say also that `socket()` failing with `EMFILE` is `UpstreamFailed { Other }`, which becomes a 502 `Refused` and is logged distinctly per FR-LOG-07.
- **M2. A client that ends its stream in the middle of a proxied request body leaves no log entry.** `mod.rs:526-530` closes it silently via `forget`. The backend has received a truncated request, and the operator sees nothing. FR-CONN-12 ("EOF … closes it silently") was written for idle connections. Consider a `Rejected { Body(ClosedEarly) }` entry for this case, or confirm with the owner that silence is intended and say so in FR-PROXY-26.
- **M3. NFR-ROB-04 says "an upstream event that does not fit its state is ignored", but `UpstreamReadClosed` is acted on whatever the interest.** For example, while the request head is still being written (interest `Write`), a driver-bug EOF becomes a 502 `ClosedEarly` (`proxy.rs:314-341`). The reaction is not unreasonable. Either check `reads_upstream(up)` as `upstream_received` does, or reword NFR-ROB-04.
- **M4. Duplication across tasks:**
  - `body_mode(method)` is written three times: `proxy.rs:200`, `request.rs:326-329` and `static_files.rs:610`.
  - The send-timeout cut (log `Timeout(Send)`, `forget`, `Close`) appears at `proxy.rs:568-574` and `609-615`, and is similar in `timers.rs:99-105`.
  - The "body idle while the body streams" deadline is inlined at `proxy.rs:385-390` next to `upload::awaiting_deadline`.
  - `take_request` (`proxy.rs:635`) and `take_awaiting` (`relay.rs:331`) are near twins and are not `#[must_use]`, unlike `take_opening`.
  - The carry-over rule written three times is how I2 slipped in. Factor these into helpers next to the code that uses them.
- **M5. File sizes.** `sim/tests/faults/proxy/outcome.rs` has 928 lines, over the 900-line rule. Split the timing scenarios from the exchange outcomes. `core/src/core/mod.rs` has 863 lines, about 410 of them contract prose. Stage 4 will add to it, so move the contract prose into `contract.rs` module docs, as the plan's file structure intended, and update the spec pointer ("contract lives in `core/src/core/mod.rs`").
- **M6. `ResponseBody::None` still says "a 1xx".** At `core/src/http/response_head.rs:53`, a deferred item from Task 2. Interim heads never reach `ResponseBody`.
- **M7. `RequestBody` derives `Clone, Copy` while holding a `ChunkedValidator`** (`request_body.rs:28`). `RelayBody` is deliberately "not `Clone` … must never fork" (`relay_body.rs:31-32`). Make the two consistent; dropping `Copy`/`Clone` on `RequestBody` matches the illegal-states rule.
- **M8. Client-set identity headers other than `X-Forwarded-For` and `X-Forwarded-Proto` pass through unchanged and spoofable:** `Forwarded`, `X-Real-IP`, `X-Forwarded-Host`, `X-Forwarded-Port`. A backend that trusts any of them can be fooled by a client. The spec says nothing on this. Ask the owner: either strip them as FR-PROXY-06 strips `Proxy-*`, or warn about them in the example config.
- **M9. README nit.** `core/README.md` says `upload.rs` handles "400/408/504". The 504 `BodyTimeout` is decided in `proxy.rs::expire_proxy`.

## Deferred-item triage

| Item (ledger extract, HANDOFF) | Triage | Reason |
|---|---|---|
| T1: `take_opening` not `#[must_use]` | resolved | `static_files.rs:434`. Its twins `take_request` and `take_awaiting` are not yet (M4). |
| T1: proxy route dispatch lives in `static_files::plan` | resolved | `request.rs:344-351` dispatches `RouteAction::Proxy`. |
| T2: `ResponseBody::None` doc mentions 1xx | fix before stage 3 closes | One line (M6). |
| T2: response tests lack `Content-Length : 5` and obs-text rows | resolved | `response_head/tests.rs:359-373`. |
| T2 → T7: interim cap; T2 → T4: obs-text | resolved | FR-PROXY-18 cap of 8; FR-PROXY-13. |
| T3 → T6: chunked doc wording; stickiness after a mid-data `BadLineEnd`; saturating `data` | resolved | `chunked.rs:42`, `chunked/tests.rs:308-325`, `request_body.rs:242`. |
| Ruling: T5–T7 named timeout constants, replaced by T8 | resolved | `LimitKey` with the three keys; each tested at a non-default value. |
| U2: shared `Exchange` struct | declined (agree) | No `too_many_arguments` allow is left. The proxy states carry their own data, which is better. |
| T4 ruling "request line always HTTP/1.1" | superseded | Owner decision 2026-10-27 (version mirrored); the code and spec agree. |
| T9 rulings: tie order, carry-over into `Responding` | resolved; the relay half is broken | See I2. |
| HANDOFF carry-forwards for stage 3 (normalised CL, empty stripped path → `/`) | resolved | Remove from HANDOFF (I3). |
| Contract: errors vs readable data on upstreams | fix before stage 3 closes | I1 (doc and sim; no core code change needed). |
| Backend multi-address resolution; immediate connect results | before the stage-4 plan | M1. |
| Open decision 1 (FR-CFG-21 upper bounds) | before stage 4's first runnable release | Stage 3 makes this more pressing. At `max_head 1MiB`, an active exchange adds about 2 MiB (1 MiB upstream receive buffer plus about 1 MiB head buffer) to the 1 MiB client buffer: about 3 GiB at 1024 connections. |
| Stage 4: fd budget incl. upstreams; follow the contract exactly | stage 4 | Unchanged. |
| Stage 5: IPv6 per-IP grouping; refused-pool fairness | stage 5 | Unchanged. |
| Stage 7: double buffering vs fill-drain-refill; per-request `ExchangeBuffers` allocation; `next_deadline` O(n) | stage 7 | Performance only. |
| Branch `ccr-716d91b0-bff174` (abandoned, 12 commits not on `main`) and the merged `stage3-task4…9`, `unify`, `claude/magical-dijkstra-2eoh4y` | owner decision, any time | Everything worth keeping was ported (unification U1–U17). The owner should decide on deletion; the HANDOFF note should say so plainly (I3). |

## Declined to judge

- `ResponseTimeout` is used both for a stalled head and a stalled response body: recorded in FR-PROXY-26 and only a log label.
- The `ConnError` kind is discarded, so an `Other` socket error after connect is logged as `Reset`: this is documented on `BackendFailure::Reset`.
- The 431 fallback in `forward` and the `HeadTooLarge` fallback in `relay` when a rewritten head would not fit: both are unreachable within the limits (`HEAD_SLACK` covers 102 of 128 bytes). They are defence in depth and fine.
- A client or backend trickling a body one byte per idle period holds a connection indefinitely: by design (NFR-RES-07 bounds stalls, not duration), and capped by `max_connections`.
- A half-closed client is noticed only after the exchange; while waiting it holds the upstream up to `backend_response_timeout`: owner decision (FR-PROXY-26).
- A client that keeps writing an unread body without reading the early response stalls until its send idle timeout: inherent to FR-PROXY-27 with fill-drain-refill.
- WebSocket and `Upgrade` (stripped; 101 → 502), HTTP/2, TLS, pooling: non-goals or later stages.
- The sim's "one well-framed request" invariant uses the core's own `parse_head` and `ChunkedValidator` (`sim/src/backend.rs:39-40`): it is a secondary guard, and the randomized net checks every forwarded byte against an independent model.
- Whether backend `Content-Length: 007` should be normalised toward the client: owner decision (FR-PROXY-12, pass through).
- The `Connection` header can name arbitrary end-to-end headers (for example `authorization`), which are then stripped: RFC 9110 behaviour, and the framing names are guarded by FR-HTTP-09a.
- A 400 for duplicate `X-Forwarded-For` is logged as `Response`, not `Rejected`: it is a route rule, not a parse error; FR-LOG-03 is satisfied.
- Real epoll and socket behaviour: there is no driver yet. I judged only whether the contract can be implemented (I1, M1).
- Fuzzing, long runs, late-timer sim mode: stages 4 and 5.

## Verdict

**Ready after fixes.**

Fix before stage 3 closes:

1. **I1.** Specify in the contract and DRV-10 that an upstream's readable bytes are reported before its socket error, and that errors are deferred while the interest is `None`. Add a sim mode for "bytes, then reset" and the two directed tests.
2. **I2.** Stop backend progress from restarting the client's send idle deadline in `relay_received` and `relay_read_closed`. Put the carry-over rule in one helper, add the two regression tests from the probes, and correct the mod.rs Deadlines section.
3. **I3.** Update the status markers in the spec, the Timeline and "How to read", and rewrite the stale HANDOFF sections.
4. **M6.** The one-line `ResponseBody::None` doc fix (an open deferred item).

The other Minor items can go into the stage-4 plan, except M1, which belongs before the stage-4 plan is written.


## Fix wave and re-review

Commits `0d89d2e` (I2), `76b14c7` (I1), `fe11454` (contract prose to `contract.rs`), `11f9e37` (IP-only backends), `3233801` (identity headers, mid-body EOF log), `df01bda` (M3, M4, M7), `d14a9e0` (M5, M6, M9), `d9743e9` (I3 spec markers), `b583d96` (follow-ups: reject unspecified/broadcast/multicast backends, `Waits`-branch tests, wording). The re-review verdicted every item ADDRESSED and confirmed the fixes by mutation (I1 error-first sim, I2 both branches, D-B log removed); its remaining nits were fixed in `b583d96`.
