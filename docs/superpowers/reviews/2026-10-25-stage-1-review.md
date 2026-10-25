# Stage 1 (Foundations) — review record

Plan: `docs/superpowers/plans/2026-10-21-stage-1-foundations.md`. Branch range reviewed: `a22cc50..4bd0ba0`; final fix wave `885da13`, `7c920dc`, `91aba76` (re-review: all findings addressed).

## Rulings made on the owner's behalf

- Task 1: Ruling: mid-word `#` ends a word and starts a comment — spec grammar excludes `#` from tokens; brief wording was looser — if wrong, `#` cannot appear in config values (none need it)
- Task 1c: amendment (user: no clever disambiguation; SCSS in spirit only). Ruling: settings are `key value;` without colon — simplest grammar, one meaning per character — if wrong, configs read less like SCSS. BASE 6937422
- Task 2: Ruling: plan says spec example has "3 top-level items"; it has 4 (listen, user, limits, host) — plan count error — if wrong, test count adjusts
- Task 2: Ruling: depth = number of enclosing blocks incl. itself; 8 nested blocks OK, 9th → UnexpectedToken at its name — plan said "capped at 8" — cheap to change
- Task 2: Ruling: EmptyValue reported at the key's position; stray `}` at top level → UnexpectedToken; EOF inside a setting → UnexpectedEnd at EOF pos — plan left open
- Task 2: process: implementer wrote code and tests together, then stubbed parse to show RED (self-reported). Ruling: keep the work — tests demonstrably fail against a stub, so they pin behaviour — future dispatches state test-first order explicitly — if wrong, tests may mirror implementation rather than spec; reviewer checks tests against brief
- Task 3: Ruling: proxy value split at last ':'; host = hostname (same rules as host names) or IPv4 or [IPv6]; port 1..=65535; else InvalidValue — plan left format open — cheap to change
- Task 3: Ruling: static root must be absolute, no `.`/`..`/empty components, no trailing `/` except root itself — security; existence checked by driver later
- Task 3: Ruling: user = [a-z_][a-z0-9_-]{0,31} — POSIX-portable user names
- Task 3: Ruling: unknown name, or known name in wrong form (setting vs block) → UnknownKey(name); duplicate limits block → DuplicateKey("limits"); MissingKey at 1:1 for top level, at the block's pos inside a block
- Task 4: Ruling: trailing `/` allowed (`/api/`); EmptySegment only for `//` anywhere; empty input → Empty — `/api/` route prefixes require it
- Task 4: Ruling: `#` anywhere (path or query) → Fragment; percent-decoding happens exactly once downstream (no double decode), so `%25` is fine
- Task 4: Ruling: percent-encoded control bytes (%01-%1F, %7F) must be rejected too (new TargetError::EncodedControl) — 'when in doubt reject'; decoded control chars in file names enable log injection and odd FS names — cost if wrong: some legit odd URLs 400
- Task 4: Ruling: query = byte-class rules + well-formed escapes (BadPercentEncoding still applies), no EncodedDot/Slash/Backslash/Nul/Control semantics — brief says byte-class only; malformed escapes are never useful
- Task 4: Ruling: `;` anywhere in the path → new TargetError::Semicolon — blocks Tomcat-style `..;/` bypass when forwarding raw; cost if wrong: matrix-param URLs get 400
- Task 4: Ruling: config path args must not contain `?` (validate_path rejects `?` → new TargetError::QueryInPath) — such routes could never match
- Task 5: Ruling: lines judged when their LF arrives; a judged line's verdict never changes with more bytes (split-independent); framing/Host checks at end of head
- Task 5: Ruling: method = RFC 9110 tchar token (case-sensitive GET/HEAD); empty/non-tchar → BadMethod
- Task 5: Ruling: Host = same charset as config host names (letters digits - .) or [IPv6], optional :digits port; lowercased
- Task 5: Ruling: Content-Length: 0 → BodyFraming::None; leading zeros OK; multiple TE headers → UnsupportedTransferEncoding
- Task 5: Ruling: request line length excludes CRLF; RequestLineTooLong when no LF within max_request_line+2 bytes or line > max_request_line; max_head counts all head bytes incl. final CRLF
- Task 5: Ruling: framing error precedence: TransferEncodingInHttp10, then ContentLengthWithTransferEncoding, then DuplicateContentLength/BadContentLength/UnsupportedTransferEncoding; HTTP/1.0 Connection: keep-alive ignored
- Task 5: Ruling: RequestLineTooLong test uses max+2 bytes without LF (brief said max+1) — max+1 could be max bytes + CR, so max+1 would break split-independence — plan wording corrected by this ruling
- Task 5: Ruling: at end of head, Host checks precede framing checks — order just needs to be fixed
- Task 5: Ruling: fix in the parser (holds for any HeadLimits, not just validated config): while request line incomplete, only RequestLineTooLong may fire; HeadTooLarge only when certain
- Task 5: Ruling: Host port must be 1..=65535 (cheap hardening, reviewer minor #3); IPv6 shape check deferred
- Task 8a: Ruling: fix both Important now (doc + sim assertion), plus cheap minors: stall check `interest != Write`, pump step cap, TimerFired invariant next_deadline > now, core capacity query to end formula drift, reword Accepted doc
- Task 8a: Ruling: revise earlier ruling — connections in BTreeMap<ConnId, Conn> instead of HashMap — deterministic iteration for TimerFired/trace; cost: O(log n) lookups
- Task 8b: Ruling: one log per request — matched route or route miss → Log::Response{status, request: Some(summary)}; parse error → Log::Rejected{Http(e)} only (RejectReason::Route stays unused in stage 1)
- Task 8b: Ruling: while Responding the connection does not read (interest Write); pipelined bytes wait in the buffer; on entering Lingering the buffer is emptied and input is discarded
- Task 8b: Ruling: full receive buffer with Incomplete parse → 431 HeadTooLarge (implementer choice) — consistent with parser: capacity >= max_head, so one more byte would be HeadTooLarge anyway
- Task 8b: Ruling: unread-body test uses an unknown host (brief's /nowhere matches `path /` → 501) — brief error
- Task 8b: Ruling: 8a test receive_buffer_applies_backpressure rewritten as stalled-client test — full buffer of non-HTTP bytes now yields 414 by design
- Task 9: Ruling: brief's `bytes_seen` is 8b's `buffered`; header deadline starts at accept and on every entry into ReadingHead (first byte after Idle, or pipelined leftover); 408 is followed by Lingering like other close paths
- Task 9: Ruling: unread-body test uses an unknown host (as in 8b) so the answer is 404
- Task 9: Ruling: refused (503) connections exist in the core map but are excluded from both counts; they follow Responding→Lingering with the normal deadlines
- Task 9: Ruling: Lingering deadline is fixed at entry (input does not extend it); TimerFired handles every conn with deadline <= now in ConnId order; next_deadline may scan all conns (O(n), stage 7 perf)
- Task 9: Ruling: refused connections allocate no receive buffer; every Lingering connection drops its own receive buffer and discards into one shared core scratch buffer (4 KiB); refused-but-open connections capped at max_connections, beyond which Rejected{Limit} + immediate Close with nothing inserted
- Task 9: Ruling: normalise peer with IpAddr::to_canonical() before per-IP counting; IPv6 prefix grouping deferred to stage 5
- Task 9: Ruling: accept Window::Budget sim addition (scope addition acknowledged)
- Task 10: Ruling: add the three 8b deferred tests (no answer after Connection: close pipelined request; partial pipelined leftover completed later; HTTP/1.0 closes); fastrand allowed as sim dev/test dependency
- Task 10: Ruling: add response-byte checks (known status lines, ≤1 response per request, nothing after close) and an aggregated coverage assertion across seeds; plus cheap minors (split baselines asserted exactly, 414/431 split shapes, occasional long clock jumps)
- Ruling: fix I1 now (not stage 7) — reviewer measured ~7 MB/s attacker input saturating the loop; spec anti-goal — overrides Task 5/8b deferral rulings
- Ruling: final fix wave = I1, I2, M1, M2 + trivial docs (README HTTP-only wording, Log::Response "queued" wording); other minors stay deferred per final-review triage

## Deferred findings (from per-task reviews)

- Task 1: minor (deferred): edge-case lexer tests (CRLF, control char in comment, multibyte column, empty input)
- Task 1: minor (deferred): is_forbidden_control checked in 3 places; could centralize in cursor
- Task 1: minor (deferred): dep script prefix match `durchlauferhitzer-*` would admit registry crates with that prefix; match workspace members instead
- Task 1: minor (deferred): dep script has no self-test proving it rejects a disallowed dep
- Task 1: minor (deferred): PRODUCT_PACKAGE env override in check-product-deps.sh is an undocumented test seam
- Task 1b: minor (deferred): untested edges (backslash+newline/EOF, backslash+control char, \t/\r in string, empty "", word Joined after closing quote)
- Task 1b: minor (deferred): newline/control check duplicated in read_quoted
- Task 1c: minor (deferred): no standalone test that `a:b` / bare `:` stays one Word
- Task 2: minor (deferred): `Some(_) | None` arm at syntax.rs:166 hides unreachability
- Task 2: minor (deferred): untested paths ({/; as first token, EOF after name / after `{`, `a { b c`, nested unclosed reports innermost, quoted block arg, empty input)
- Task 2: minor (deferred): next() clones each token
- Task 2: minor (deferred): no doc comments on public syntax types
- Task 3: minor (deferred): empty strip_prefix "" accepted (no-op)
- Task 3: minor (deferred): duplicate-key tests in routes only cover `static`
- Task 3: minor (deferred): route parsing uses `_ =>` on &str as implicit max_response_body (validate.rs:522-528, :562) — fragile; prefer a private RouteKey enum (fits owner's "enums everywhere")
- Task 3: minor (deferred): parse_limits duplicates key list; unreachable fallthrough arm
- Task 3: minor (deferred): IPv6 host spellings not normalised for duplicate detection; DNS label lengths not enforced
- Task 3: minor (deferred): syntax errors display via Debug
- Task 3: minor (deferred): validate.rs 1102 lines (670 tests) — consider splitting tests
- Task 3: minor (deferred): parse_limits `seen` could be a set of LimitKey
- Task 4: minor (deferred): decoded %80-%FF allowed (needed for UTF-8 file names); downstream must log raw path only — record for stage 2
- Task 4: minor (deferred): no RED run for config test path_rejects_hostile_targets; proptests vacuous in RED
- Task 4: minor (deferred): %3B / %3F accepted in path; revisit for proxy forwarding in stage 3 (backend decoding behaviour)
- Task 5: minor (deferred → Task 8 dispatch): parse_head re-scans from byte 0 each call; Core must call it only when a new LF arrived (or track offset) to avoid slowloris CPU amplification
- Task 5: minor (deferred): never_panics proptest shallow; Host charset-only accepts `[:]`, `..`, `-`
- Task 6: minor (deferred): per-request to_ascii_lowercase allocation; linear route scan
- Task 6: minor (deferred): strip_port malformed inputs (a:, a:b, [::1, ::1) not pinned by tests; nested match could be rsplit_once
- Task 7: minor (deferred): render_fixed builds intermediate Strings
- Task 8a: minor (deferred): no Interest::ReadWrite (proxy stage may need full duplex); sim invariant panics untested; recv buffer zeroed eagerly, no config upper bound
- Task 8a: minor (deferred): send-stall and TimerFired sim checks untested until 8b/9 exercise them; stall check would fire if a future design defers writes on purpose
- Task 8b: minor (deferred → Task 10 dispatch): test that a pipelined request after `Connection: close` is never answered; partial pipelined leftover completed later; HTTP/1.0 keep-alive closes
- Task 8b: minor (deferred): Responding with empty send buffer → Interest::None (unreachable; add debug_assert/comment); `other => other` catch-all on Result at core.rs:301
- Task 8b: minor (deferred): parse_head rescans per new LF, O(lines × max_head) per head — stage 7 perf
- Task 9: minor (deferred → stage 5): per-IP cap is per exact address (IPv6 /64 bypass; v4-mapped v6 counted separately)
- Task 9: minor (deferred): timer_fired allocates Vec per pass; next_deadline O(n) — deadline index in stage 7
- Task 9: minor (deferred → stage 5): refused-slot pool is shared; one over-limit peer can exhaust refused slots so others get silent close (memory still bounded)
- Task 10: minor (deferred): split_responses may read a response cut exactly at head end as head-only (latent flake if generator changes); pipelined-pair counter loose; keep-alive idle timeout not observable in coverage

Carry-forward from the fix-wave re-review: a proxy route whose `path` has no trailing `/` and an equal `strip_prefix` yields an empty upstream path for an exact match — stage 3 must map it to `/` (the spec already requires this).

## Final whole-branch review

## Stage 1 (Foundations): final review, a22cc50..4bd0ba0

Reviewer: Opus (read-only). Passes:
1. Spec, plan and ledger (all `Ruling:` and `minor (deferred)` lines).
2. Core state machine and driver contract (`core.rs`, `conn.rs`, `log.rs`, `time.rs`).
3. Parsers and config (`request.rs`, `target.rs`, `routing.rs`, `response.rs`, `status.rs`, `config/*`).
4. Sim and tests (`sim/src/lib.rs`, `sim/tests/*`), CI, the dependency script and the lockfile.
5. Probes in a separate `/tmp` worktree (since removed): config edge cases, half-close, 400 pipelined requests, a check that the dependency script rejects a real dependency, and parser rescan cost in debug and release builds.

Run in place at 4bd0ba0: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` (153 tests including the 500-seed run) and `scripts/check-product-deps.sh` all pass. The GitHub Actions CI run for 4bd0ba0 is green.

### Strengths

- **The core is small and fully deterministic.** It has zero dependencies and `unsafe` is forbidden. None of the denied lints fire. In non-test core code, every `match` on an enum lists every variant; the only exception is a catch-all on a `Result`, listed below. Every connection state carries its own deadline. The `forget()` choke point releases the connection counts on every close path, and unit tests cover each of those paths.
- **The request-head parser is strict and consistent across splits.** A line is judged only when its LF arrives, and a verdict never changes when more bytes come in. Every framing rule from the spec is enforced: CL+TE, duplicate CL, TE other than exactly `chunked`, TE in HTTP/1.0, obs-fold, bare LF, CR/NUL in header values. Proptests cover the prefix-consistency property with a hostile alphabet.
- **Target validation is stronger than the spec asks.** It also rejects `;` (blocks Tomcat-style `..;/`), encoded control bytes, and `?` in config paths. It is applied exactly once and documented as "decoded once downstream". The proptests use hostile alphabets, so they are not vacuous.
- **Memory is bounded under attack.** Refused connections get no receive buffer, and at most `max_connections` of them are kept. Lingering connections drop their own buffer and share one 4 KiB scratch buffer. Peers are counted by canonical address, so IPv4-mapped IPv6 shares the IPv4 slot. This came out of a good Task 9 review round.
- **The driver contract in `core.rs` is unusually explicit:** level-triggered semantics, `Received` must be the very next call, `recv_buffer`/`send_buffer` are borrow-scoped, ids are never reused, and after `TimerFired` no deadline is left at or before `now`. The sim checks most of these rules (see the gaps below).
- **The randomized test net has real teeth.** Received bytes must split exactly into the 32 known fixed responses. They must form a prefix of the logged statuses. Nothing may arrive after a close or shutdown, and nothing may follow a `Connection: close` response. Every connection must end closed and forgotten by the core. Across the seeds, every status and every reject reason must show up at least once. `SIM_SEED` replay works.
- **Product dependency hygiene is in place.** `Cargo.lock` is committed. The script fails closed: I confirmed it exits 1 when `fastrand` is added to core. CI runs fmt, clippy, tests and the dependency check.
- **The ledger is honest about scope.** Rulings come with a stated cost if they are wrong, which made this review much faster.

### Issues

#### Critical (Must Fix)

None.

#### Important (Should Fix)

**I1. A client can make the server rescan its head about 50 times; one modest link can pin the event loop.**
- Where: `core/src/core.rs:499-531` (`try_parse`) together with `core/src/http/request.rs:111-175` (`parse_head`).
- What: `parse_head` is stateless. `try_parse` calls it again whenever a read delivers a new LF, and every call rescans the whole buffer from byte 0. Every call also re-allocates a `String` name and a `Vec` value for every header parsed so far.
- Measured: a 15.7 KB head sent one header line per read makes the parser scan 778 KB, 49 times the input. In a release build that takes 2.1 ms of CPU per head.
- Why it matters: the core is single-threaded, so roughly 7 MB/s of attacker input (a ~60 Mbit/s link, via keep-alive 404s that never close) saturates the loop. Every other connection stalls. That breaks the spec anti-goal "a slow client ... stalling other connections".
- The ledger (Task 5, Task 8b) files this under stage-7 performance. It is a DoS vector, not a performance item.
- Fix: before stage 4 (first runnable release) at the latest; ideally now, while the parser is fresh. Make parsing resumable:
  - Keep `{ judged_upto, header_count, request_line_done }` in `ConnState::ReadingHead`.
  - Have `parse_head` validate only the newly completed lines (no allocation, just checks and counts).
  - Build `RequestHead` in one final pass when the blank line arrives.
  - Total work is then O(max_head) per head.
  - The split-independence tests and proptests already pin the observable behaviour, so the refactor is safe to make.
  - Add a regression test: a sim run with `set_read_chunk` at one line per read, asserting the parser does at most about 2x max_head of work. This needs a counter or a `#[doc(hidden)]` statistic.

**I2. Config validation accepts a `strip_prefix` that yields an invalid forwarded path.**
- Where: `core/src/config/validate.rs:354`.
- What: the only check is `prefix.starts_with(strip)`. These were all accepted in a probe:
  - `path /api/ { strip_prefix /ap; }` turns `/api/x` into `i/x`.
  - `strip_prefix /api/` turns `/api/x` into `x`.
  - `path / { strip_prefix /; }` turns every path into one without a leading `/`.
- Why it matters: in stage 3 these produce request targets that are not origin-form, so the backend gets malformed requests. That is exactly the class of input this project rejects from clients. A reasonable operator expects either a working route or a startup error naming the line. The spec only says "must be a prefix", but that silence is not permission.
- Fix (cheap, and the code is stage-1 code):
  - Reject a `strip_prefix` that ends in `/`.
  - Reject one where the byte of `path` right after it exists and is not `/`.
  - Then `/api` on `/api/` and `/api` on `/api` are fine, and the "empty result becomes `/`" rule covers the rest.
  - Add a test row for each case.

#### Minor (Nice to Have)

- **M1. A catch-all arm survives where the owner wants exhaustive matches.** At `core/src/core.rs:530` the arm `other => other` passes through a `Result<ParseStatus, _>`. It is harmless today, but the owner has asked for catch-alls to be removed before (route keys, Task 3). Fix: match `Ok(ParseStatus::Incomplete)` and the other variants explicitly, or do the "full means HeadTooLarge" mapping inside the `Ok(Incomplete)` arm.
- **M2. CI does not enforce the lockfile.** `.github/workflows/ci.yml:17,19`: clippy and test run without `--locked`, so a stale or edited `Cargo.lock` would be silently re-resolved. CLAUDE.md requires pinning via `Cargo.lock`. Fix: add `--locked` to clippy and test.
- **M3. The dependency script checks only the host target.** `scripts/check-product-deps.sh:10`: `cargo tree` resolves only the host target, so a `cfg(target_arch = "aarch64")` dependency of the product would be missed. This matters from stage 4 (libc, aarch64 is a supported platform). Fix: add `--target all`.
- **M4. The sim fires every timer exactly on time.** `sim/src/lib.rs:292` delivers `TimerFired` exactly at the deadline. It never models a late wakeup with socket events processed before the timer, which is normal for epoll with millisecond timeouts. The core handles `deadline <= now` correctly, but nothing tests it. Fix (stage 4): add a sim mode where `advance` jumps past deadlines in random steps and delivers pending I/O before `TimerFired`.
- **M5. Contract gap on `Sent`.** The contract requires `Received` to be the very next call after a read, but says nothing equivalent for `Sent` after a write. It also does not say whether `Sent { len: 0 }` is legal. That is safe today because nothing appends to the send buffer while responding, but stages 2 and 3 will refill it. Fix (stage 2): state both rules in the `core.rs` docs and check them in the sim.
- **M6. Error answers to `HEAD` carry a body.** `core/src/core.rs:552` always uses `BodyMode::Full`, even when the request line was parsed as `HEAD` and the error came later (for example `MissingHost`). The connection closes, so the impact is cosmetic. Fix (stage 2, which owns HEAD): have the parser report the method alongside late errors.
- **M7. Some log docs and fields fall short.**
  - `Log::Response` is documented as "a response was sent" (`core/src/log.rs:17`) but is emitted when the response is queued. A send timeout later logs a second entry. Reword the doc, or log on completion.
  - `Rejected{Http}` drops method and path even when the request line parsed. Operators will want them (stage 4).
  - `RejectReason::Route` is a public variant that is never emitted.
- **M8. Config accepts some entries that will only fail at bind or never match.**
  - Duplicate `listen` addresses and port 0 are accepted (`validate.rs:131`).
  - Host names `-a-` and `999.999.1.1` are accepted.
  - `max_head 1GiB` is accepted with no upper bound. Each accepted connection then allocates and zeroes that much.
  - Fix in stage 4, when `--check` and binding exist.
- **M9. Some docs are stale or missing.**
  - `README.md` still says "HTTP/HTTPS proxy", but HTTPS is a spec non-goal.
  - The spec says default limits are "documented"; today they exist only in `model.rs`.
  - Fix in stage 4, with the binary.
- **M10. Some files are large.** `validate.rs` is 1198 lines, of which about 720 are tests. Move its tests to `validate/tests.rs` when stage 2 or 3 adds keys. `core.rs` (983 lines) and `request.rs` (895) are fine for now.
- **M11. The connection-close probe is weak.** The random test only checks that a response comes from a known set and is logged in order. Matching each request template to its expected status is left to the deterministic tests. That is acceptable, but it means the random net would not catch a consistent misroute.

#### Plan defects (separate from implementation)

- Task 8b prescribed "parse again only on a new LF" as the fix for rescanning. That does not bound the work (see I1).
- The Task 3 rule "strip_prefix must be a prefix of path" is too weak (see I2); the spec shares the wording.
- Task 1 specified the dependency allow-list as a `durchlauferhitzer-*` name prefix. A registry crate with that prefix would pass.
- Task 9 originally left refused connections unbounded; the ledger fixed this.
- Task 2's item count (3 versus 4) and Task 8b's `/nowhere` example (matches `path /`) were both wrong; rulings corrected them.
- Task 10 assumes the sim's exact-time timers are realistic (see M4).

### Deferred-minor triage

One line per ledger entry. Seven entries are already resolved.

| Task | Ledger entry | Triage |
|---|---|---|
| T1 | Lexer edge-case tests (CRLF, control char in comment, multibyte column, empty input) | later (stage 4) — add with `--check` error output. Low risk, because no panic is possible. |
| T1 | `is_forbidden_control` checked in 3 places | later (any stage) — cosmetic. |
| T1 | Dependency script's `durchlauferhitzer-*` prefix match would admit a registry crate | later (stage 4) — core has no deps now. When libc and the binary crate arrive, match on workspace path sources (`{p}` prints `(/path)` for path crates) or on `cargo metadata` workspace members. |
| T1 | Dependency script has no self-test | later (stage 4) — together with the previous entry. I checked by hand that it fails on an added dependency. |
| T1 | `PRODUCT_PACKAGE` override is undocumented | later (stage 4) — the script must then point at the binary package anyway. Document it at that point. |
| T1b | Untested string edges | later (stage 5, config fuzzing). |
| T1b | Newline/control check duplicated in `read_quoted` | later (any stage) — cosmetic. |
| T1c | No standalone test that `a:b` / `:` stays one Word | later — already covered in effect by `127.0.0.1:8080` and `[::1]:80` in `tokenizes_setting_with_positions`. |
| T2 | `Some(_) \| None` arm at syntax.rs:166 (now :140) | later (any stage) — exhaustive over `Option`, cosmetic. |
| T2 | Untested syntax paths | later (stage 5, fuzzing). |
| T2 | `next()` clones each token | later (never needed) — config only, run once. |
| T2 | No doc comments on public syntax types | later (stage 4). |
| T3 | Empty `strip_prefix ""` accepted | resolved in 8e1993e (rejected now). |
| T3 | Route duplicate-key tests only cover `static` | later (stage 3) — add when proxy route keys get exercised. |
| T3 | Route `_ =>` catch-all | resolved (`RouteKey`). |
| T3 | `parse_limits` duplicates key list | resolved (`LimitKey`). |
| T3 | IPv6 host spellings not normalised; DNS label lengths not enforced | later (stage 4) — harmless 404 or extra accepts today; see M8. |
| T3 | Syntax errors display via `Debug` | later (stage 4) — users see these via `--check`. |
| T3 | `validate.rs` size | later (stage 2/3) — split tests out when adding keys (M10). |
| T3 | `parse_limits` `seen` could be a set of `LimitKey` | later (any stage) — cosmetic. |
| T4 | Decoded %80-%FF allowed; log the raw path only | later (stage 2) — carry into the stage-2 plan as an explicit requirement. |
| T4 | No RED run for config hostile-path test; proptests vacuous in RED | later (none) — process note. The hostile-alphabet proptests are not vacuous now. |
| T4 | `%3B` / `%3F` accepted in path | later (stage 3) — decide when forwarding raw paths to backends that decode before normalising. |
| T5 | `parse_head` rescans from byte 0 | **fix before stage 4 (preferably now)** — see I1. The 8b mitigation is not enough. |
| T5 | `never_panics` proptest shallow; Host charset accepts `[:]`, `..`, `-` | later (stage 5) — these only cause 404s. |
| T6 | Per-request lowercase allocation; linear route scan | later (stage 7). |
| T6 | `strip_port` malformed inputs not pinned by tests | later (any stage) — the parser validates Host first. |
| T7 | `render_fixed` builds intermediate Strings | later (stage 7). |
| T8a | No `Interest::ReadWrite` | later (stage 3) — the proxy needs full duplex. Design it into the stage-3 plan. |
| T8a | Sim invariant panics untested | later (stage 4) — 2 `should_panic` tests exist. Add one per invariant when the sim grows. |
| T8a | Receive buffer zeroed eagerly; no config upper bound | later (stage 4) — add upper bounds (M8). |
| T8a | Send-stall and `TimerFired` sim checks untested | resolved — exercised by Task 9/10 tests. |
| T8a | Stall check would fire on deliberately deferred writes | later (stage 2/3) — revisit if the file or proxy design defers writes. |
| T8b | Pipelined `Connection: close`, partial leftover, HTTP/1.0 tests | resolved in Task 10. |
| T8b | `Responding` with empty send buffer gives `Interest::None`; `other => other` at core.rs:301 (now :530) | **fix before merge** for the catch-all (M1) — trivial, and matches the owner's demonstrated preference. The `Interest::None` arm already has its comment. |
| T8b | `parse_head` rescans per LF, O(lines x max_head) | **fix before stage 4 (preferably now)** — same as I1; stage 7 is too late. |
| T9 | Per-IP cap per exact address (IPv6 /64) | later (stage 5) — must land before internet exposure. |
| T9 | `timer_fired` allocates a Vec per pass; `next_deadline` O(n) | later (stage 7). |
| T9 | Refused-slot pool shared across peers | later (stage 5). |
| T10 | `split_responses` head-only ambiguity | later (stage 2) — becomes real when non-fixed responses appear. The test is deterministic today, so it cannot flake. |
| T10 | Pipelined-pair counter loose | later (any stage). |
| T10 | Keep-alive idle timeout not observable in coverage | later (any stage) — covered by `keepalive_idle_closes_after_60s`. |

### Rulings I disagree with

- **Task 8b, "parse_head rescans per new LF — stage 7 perf" (and the Task 5 entry).** This is a CPU-amplification DoS: 49x measured, about 7 MB/s of input saturates the loop. It is not a performance nicety, and stage 7 comes after the first runnable release. Reschedule to before stage 4 (I1).
- **Task 3, the `strip_prefix` rule (implicit, by following the plan).** "Is a prefix" admits configs whose forwarded paths are invalid. Tighten it as in I2.

All other rulings look right to me. Some have consequences worth carrying forward:

- **Task 9, refused connections are uncounted.** Up to 2 x `max_connections` sockets can be open at once, plus backend sockets from stage 3. Stage 4 must size `RLIMIT_NOFILE` for that and say so.
- **Task 5, leading zeros are allowed in `Content-Length`.** That is fine for parsing. In stage 3 the proxy should send a normalised `Content-Length` to the backend rather than the raw value.

### Declined to judge

- Static file and proxy behaviour (matched routes answer 501): stages 2 and 3.
- `Expect: 100-continue`: left to the builder, and only meaningful once bodies are read (stage 2/3).
- IPv6 prefix grouping for the per-IP cap: deferred to stage 5 by ruling, which I accept.
- Throughput and complexity of `next_deadline`, `timer_fired` and route lookup: stage 7, with no security impact at the configured limits.
- `Date` / `Server` headers: excluded by the plan.
- epoll mechanics (EPOLLRDHUP/HUP mapping to events, edge vs level, fd limits, accept-error handling): stage 4. I judged only the contract wording.
- Fuzzing of the parser and config: stage 5 per plan.
- Minimum transfer rates and slow-reader holding with 1-byte windows: out of scope per spec.
- Rejecting leading empty lines before the request line (the RFC says SHOULD ignore): the plan explicitly chose strictness.
- 400 rather than 501 for an unknown `Transfer-Encoding`: the spec says 400.
- Log line format and escaping of untrusted fields: stage 4 driver.
- Privilege drop and refusing to run as root without `user`: stage 4.
- Unicode format characters (e.g. bidi overrides) in config words: the config is trusted operator input. This is a cosmetic hardening question, not a stage-1 requirement.

### Verdict

**Ready after fixes.** Stage 1 meets its plan: the core is careful, the driver contract is well specified, and the simulation net is strong. Before merging, fix the small items:
- I2: `strip_prefix` validation.
- M1: the `other => other` catch-all.
- M2: `--locked` in CI.

Then schedule I1 (resumable head parsing, to remove the 49x rescan amplification) as a hard prerequisite for stage 4; doing it now is cheaper.
