# Stage 2 (Static files) — review record

Plan: `docs/superpowers/plans/2026-10-25-stage-2-static-files.md`. Range `ff5480a..ec4a800`. Final review fixes: `f51bf19`…`24d76bc`; owner-approved second fix round `a001ac1`, `ec4a800` (decoded-segment routing, `%3B`). Re-review: all findings addressed; 265 respelling probes route identically.

## Rulings made on the owner's behalf

- Task 1: process: tests and impl written together, RED via stub (self-reported). Ruling: keep — tests are spec-derived exact values; reviewer to confirm they'd catch a wrong impl
- Task 1: Ruling: fix round adds encoded-dot tests, leading-dot table test, and defense-in-depth guard (empty segment, decoded `/` or NUL, missing leading `/` → error) so the function is safe on its own
- Task 2: Ruling: FileIds sequential from 1 (core); RootId = index into distinct static roots in config order (identical paths deduplicated)
- Task 2: Ruling: in this task the ChunkReady stamp is recorded but not acted on (Task 4); any FileFailed simply closes the connection (Task 4 adds rules/logs); HEAD, non-GET, dot/bad segments, directories, non-regular files on static routes still answer 501 (Task 3)
- Task 2: Ruling: waiting on file events lives in a Responding-like state with the send_idle_timeout deadline (every state has a deadline); expiry closes the connection and its file
- Task 2: Ruling: sim keeps its own queue of delayed file completions merged with core deadlines in advance(); delay 0 completes within run_until_idle
- Task 2: Ruling: send_idle deadline resets on FileOpened/ChunkReady (disk progress counts as progress) — consistent with "waiting on disk counts against send_idle_timeout"; Task 4 adds Timeout(Disk)
- Task 2: Ruling: regular file requested with trailing slash → 501 now (Task 3 decides: 404)
- Task 2: Ruling: OpenFile always answered; CloseFile supersedes a pending Readahead (no answer delivered); driver keeps the fd until an in-flight readahead worker returns, then drops the completion
- Task 2: Ruling: an orphaned open keeps counting against max_connections until its FileOpened/FileFailed arrives (honest backpressure under a hung disk: new connections get 503); per-IP slot released at close
- Task 2: Ruling: fix round also: readiness rule covers send_file (EAGAIN unreported, short count = socket full, 0 return on writable socket mid-segment = Truncated); sim disk queue sorted by due time; sim sendfile reads the opened snapshot not the path; e2e tests for reset during open/readahead/ready segment and pipelined GET after a file response; rename CHUNK_SIZE → FILE_CHUNK; document FileId exhaustion as impossible
- Task 3: Ruling: a regular file requested with a trailing slash → 404; 301 Location = raw path + "/" (+ "?" + raw query); HeadOnly for every response to a request whose method parsed as HEAD (incl. 301, 404, 405, 413, 501); parse errors keep a body (method unknown)
- Task 3: Ruling: 405 takes precedence over 413 (POST with body → 405 + close); non-dot path errors → 404 (unreachable after validation)
- Task 4: Ruling: include Task 3 deferred test (Truncated/Changed answering OpenFile → driver bug → Close); TimeoutKind::Disk logged when send_idle expires while waiting on FileOpened/ChunkReady (replaces Timeout(Send) there); example config documents on_file_change
- Task 4: Ruling: on_file_change on proxy → new OnFileChangeOnProxy (follows StripPrefixOnStatic pattern); readahead in flight with head bytes still queued logs Timeout(Disk)
- Task 5: Ruling: include deferred tests — reset/timeout during index.html open; HEAD on 405 and proxy 501; first-chunk change under Abort (change_during_send after 0); unit table for check_stamp; disk-timeout orphan slot released after late FileOpened/FileFailed
- Task 5: Ruling: "HEAD on 405" impossible (HEAD allowed) → replaced by POST 405 keeps body + HEAD same path head-only
- Ruling: C1 — a redirect whose head would exceed SEND_CAPACITY is answered 414 instead (bounded memory, no buffer growth)
- Ruling: C2 — percent-encoded unreserved characters (ALPHA DIGIT - . _ ~) in the PATH → 400 (new TargetError::EncodedUnreserved); the raw path is then truly canonical for routing and forwarding
- Ruling: I2 — an orphaned open keeps BOTH its global and its per-IP slot until the disk answers (reverses Task 2 per-IP release)
- Ruling: HEAD with a parse error after the request line (method known) → head only (amendment 5)
- Ruling: I1 contract doc gaps fixed now (doc only); I3 core.rs split = first task of stage 3; I4 spec fix now
- Ruling (owner-approved): router compares decoded segments (no allocation); raw path still forwarded; EncodedUnreserved kept as defence in depth; %3B rejected in paths; spec + fd budget doc fixes

## Deferred findings

- Task 1: minor (deferred): skipped (route) segments are not decoded/checked beyond emptiness — they matched the route literally, fine
- Task 2: minor (deferred): release_orphan assumes orphan was Counted::Yes (true today; add debug_assert/flag)
- Task 2: minor (deferred → stage 4/5): one IP doing GET+reset against a hung disk can occupy all global slots (per-IP freed at close); needs driver-side open timeout answering FileFailed
- Task 2: minor (deferred): sim copies full file bytes per open (test-only)
- Task 3: minor (deferred → Task 4): Truncated/Changed as an answer to OpenFile lost its test (closes connection as driver bug)
- Task 3: minor (deferred → Task 5): reset/timeout during the index.html open; HEAD on 405 and proxy 501 in the sim; random schedule lacks directory/301/405/413
- Task 3: minor (deferred): static_request == None branches unreachable (defensive)
- Task 4: minor (deferred → Task 5): first-chunk change under Abort untested (change_during_send after 0); no unit table for check_stamp; disk-timeout orphan slot release not checked end to end
- Task 4: minor (deferred): Disk blamed when client also behind (documented); abort vs timeout action order differs; driver-bug close of an open logs nothing; OnFileChangeOnProxy checked before MissingBodyLimit
- Task 5: minor (deferred → final fix wave): grown counter thin (4/1500); periodic 509-byte file content (shift by k·509 undetected by content compare)

Carry-forward to stage 3: split `core.rs` as the first task (I3), making `ConnState` non-Copy so the static request lives in the opening state; stale proptest comment in `target.rs` ("raw path is canonical").

## Final whole-branch review

## Stage 2 (static files): final review, ff5480a..5943245

Reviewer: Opus, read-only. I read the final files under `core/` and `sim/` directly instead of the diff package: first the contract and state machine (`core.rs`, `conn.rs`, `file.rs`), then the HTTP helpers (`static_path.rs`, `mime.rs`, `target.rs`, `response.rs`), then the sim (`fs.rs`, `lib.rs`), then the tests (`static_files.rs`, `faults.rs`, config tests), then the docs (spec, plan, example config, ledger).

Checks run at HEAD 5943245, in place: `cargo fmt --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked` (247 tests) and `scripts/check-product-deps.sh` all pass. CI is green for every stage-2 push.

I ran two probe tests in a throw-away `/tmp` worktree, now removed. Both confirmed a finding: C1 and C2 below.

Random-run coverage over 1500 seeds:

```
200=300 301=115 405=173 413=264 disk_timeout=180 truncated=58 changed=20
first_chunk=26 multi_chunk=23 grown=4 head_ok=123
```

### Strengths

- **Clear, honest file contract.** The `core.rs` module docs (lines 64-117) state the rules that matter:
  - who allocates ids;
  - every `OpenFile` and every `Readahead` gets exactly one answer;
  - `CloseFile` supersedes a readahead in flight, and the driver keeps the fd until the worker returns;
  - `send_buffer` is written before `send_file`;
  - EAGAIN is never reported, and a 0 return from `sendfile` means `Truncated`;
  - aborts cut the connection with no lingering.

  Almost all of these are enforced in the sim (`sim/src/lib.rs:690-848`): ids are sequential, there is no `CloseFile` before `FileOpened` and no second `CloseFile`, no `Readahead` goes beyond the size seen at open, segments are offered only after their `ChunkReady` and only while the send buffer is empty, and no file is left open after its connection closes.
- **File bytes never enter the core.** The core only does `Content-Length` arithmetic. Sends are capped at the size seen at open, so `allow_growth` can never send bytes beyond `Content-Length`. Every shrink path ends in a cut connection, never in a short body that looks complete. The randomized net checks this exactly: the body must be cut at precisely the fault offset (`faults.rs:1162-1189`).
- **Orphaned opens are handled with real care.** They keep their global slot until the disk answers. A late `FileOpened` gets `CloseFile` and a late `FileFailed` is dropped. This is tested at the unit level, end to end, and in the random run's last check, which connects a full set of new clients after the run.
- **Traversal defence in depth.** `parse_target` rejects encoded `.`, `/`, `\`, NUL and control characters. `relative_segments` re-checks on its own (dot prefix, `/`, NUL, empty segments). The fs model resolves like `openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS)`. Symlinks, denied entries, special files, dotfiles and missing files all give the same 404. The `.well-known` route works as specified.
- **Exhaustive matching.** `file_opened` decides with an explicit `(kind, step, slash)` table and no wildcards. No denied lint is suppressed in core. The `_ =>` arms in core all match bytes or strings, not enums.
- **Strong randomized net.** It builds a model tree, checks rendering byte-exactly against both persistences, models faults, ties logs to bytes, and counts coverage for every status and abort kind. It also checks after the run that every slot is free.
- **Strong config and example.** `on_file_change` is validated: bad value, missing value, two values, duplicate key, and use on a proxy route are all rejected. The defaults in the example config match `Limits::default()`, and a test keeps the example loadable.
- **Stage-1 carry-forwards were picked up:** M6 (HEAD error bodies) for every response after a successful parse, the `Log::Response` wording, the `validate.rs` test split, and logging only the raw path.

### Issues

#### Critical

**C1. A 301 overflows the fixed send buffer, breaking the per-connection memory bound and the sim's own invariant.**

- Where: `core/src/core.rs:1329-1334` and `core/src/http/response.rs` (`render_redirect`); the limit is `SEND_CAPACITY = 1024` (`core/src/conn.rs:17`).
- What: `Location` is the raw path plus `/` plus `?query`, and both are bounded only by `max_request_line` (8 KiB by default, configurable up to 1 MiB). Probe: a directory with a 600-byte name, requested without a slash and with a 3000-byte query, gives `sim invariant violated: conn=1 send_buffer 3765 bytes exceeds capacity 1024`.
- Why: the `Vec` grows instead of panicking, so the blast radius is small: at most about `max_request_line` extra per connection. But it breaks a documented invariant ("at most SEND_CAPACITY") that the stage-3 streaming and stage-4 write paths will rely on. The random net misses it because `RANDOM_CONFIG` has `max_request_line 64`.
- Fix, either:
  - (a) Answer `414 URI Too Long` (already a `Status`) when the rendered redirect would exceed `SEND_CAPACITY`. Or simpler: refuse to redirect when `path.len() + query.len()` is over a fixed bound, and drop the query in that case.
  - (b) Size the send buffer as `SEND_CAPACITY.max(max_request_line + REDIRECT_OVERHEAD)` and document it.

  Add a named regression test with a long path and a long query, and add long-path templates to the random run.

**C2. Percent-encoded unreserved characters route differently from their decoded form ("canonical path" claim is false).**

- Where: `core/src/http/target.rs:118-130` accepts `%61` etc. The router matches raw segments; static serving decodes.
- What: with `path / { static /srv/www; }` and `path /assets { static /srv/assets; }`, a request for `/%61ssets/a.css` misses the `/assets` route, falls back to `/` and serves `/srv/www/assets/a.css`. Probe confirmed: it returned the shadowed file. The spec says that after validation "the raw path is canonical; routing and forwarding use it as-is" (spec lines 143-144). It is not: RFC 3986 §6.2.2.2 treats `%61` and `a` as equivalent.
- Why: it does not escape a root. But it defeats route shadowing: content the operator hid behind a more specific route can be reached under the less specific static root. In stage 3 it gets worse. A proxy backend that decodes before routing sees `/%61dmin` as `/admin`, a path the operator routed away from it. The same class covers hex case (`%c3` vs `%C3`) in configured route prefixes.
- Fix (cheap, in line with "when in doubt, reject"): in `check_bytes(is_path)`, reject `%XX` that decodes to an unreserved character (`ALPHA DIGIT - _ ~`; `.` is already rejected) with a new `TargetError::EncodedUnreserved` → 400. Then the raw path really is canonical, apart from hex case. For hex case, either require uppercase hex in paths or compare route segments case-insensitively on the escapes. Add a stage-1-style row-table test and a sim test for this probe. If the owner prefers leniency, the alternative is to route on decoded segments, but forwarding the raw path then needs the same normalisation in stage 3.

#### Important

**I1. The file contract is not yet enough to implement the stage-4 driver without guessing.** All of these are docs, plus small sim or test changes where noted. Write them into the `core.rs` contract, and the spec's Drivers section, before the stage-4 plan; doing it now is cheap.

1. **Opens block too.** `openat2` and `fstat` can block on a cold dentry or inode cache, just like readahead. The spec says readahead is "the only place that may block on disk" (spec line 122). State that `OpenFile` also runs on the worker pool and that answers for different files arrive in any order. The core already tolerates this; the sim always completes in FIFO order unless the delay changes mid-run.
2. **Open flags.**
   - `O_RDONLY | O_NONBLOCK | O_NOCTTY | O_CLOEXEC`, never `O_CREAT`, `O_TRUNC` or a write mode.
   - `O_NONBLOCK` is a must: a FIFO in a static root opened plain `O_RDONLY` blocks a worker until a writer appears. That is a worker-pool DoS, which `FileKind::Other` cannot prevent because the hang comes before the `fstat`.
   - Decide and document whether `RESOLVE_NO_XDEV` is set; mount points beneath a root are the operator's call.
   - Note that hard links inside a root are not detectable.
3. **The root itself.** "No segments means the root itself" must say: open `"."`. `openat2(rootfd, "")` fails with ENOENT, which would turn `GET /` into 404 instead of serving `index.html`.
4. **errno → `FileError` table.**
   - ENOENT, ENOTDIR, ENAMETOOLONG → `NotFound`.
   - ELOOP → `Symlink`.
   - EACCES, EPERM → `PermissionDenied`.
   - EXDEV, EIO, EMFILE, ENFILE and anything else → `Other`.

   Decoded paths can be up to `max_request_line` long, beyond `PATH_MAX`. EMFILE/ENFILE currently become silent 404s; the driver should also log them.
5. **Stamp derivation.** `size = st_size`, `modified_ns = st_mtime*1e9 + nsec`. Define what happens to pre-1970 mtimes (the field is `u64`). Document that mtime granularity (often one jiffy) lets a same-size rewrite inside one tick go undetected.
6. **`sendfile` error mapping.** EPIPE/ECONNRESET → `Failed { conn }`. EIO and other file-side errors → `FileFailed { Other }`. Only EAGAIN and a 0 return are covered today.
7. **Known limit: `sendfile` itself can block.** Readahead is advisory and pages can be evicted between `ChunkReady` and `sendfile`, so the event loop can still block on the disk. Document it now; mitigations (an `RWF_NOWAIT` probe, `mincore`) belong in stage 7.
8. **Stage-1 M5, still half open.** "`Sent`/`FileSent` must be the very next `handle` call after the write" is not stated. Only "never 0" is. Stage 3 refills the send buffer while responding, so it matters there. State it, and add a sim "pending write" check like `pending_read`.
9. **Fd budget.** Open file fds are at most `max_connections`, orphans included. The driver must check `RLIMIT_NOFILE` against `max_connections + roots + listeners + eventfd` at startup.

**I2. One IP plus a slow disk can fill every global slot (I disagree with the Task 2 ruling).**

- Where: `core/src/core.rs:415-421` releases the per-IP slot when an orphaned open's connection closes, while the global slot is kept.
- What: one peer looping GET-then-reset against a slow or hung disk accumulates orphans up to `max_connections`. Everyone else then gets 503, and in stage 3 that includes proxy routes, which do not touch the disk at all.
- Why the deferred fix is wrong: the ledger defers to "a driver-side open timeout answering FileFailed", but a driver cannot cancel an `openat2` blocked in D state. It could only lie to the core and leak the fd and the thread.
- Fix (about 10 lines): `FileUse::Orphan { peer }` keeps the per-IP slot too, and `release_orphan` releases both. The attacker is then capped at `max_connections_per_ip` orphans, and the backpressure stays honest. Test: same IP, hung disk, GET+reset 64 times, then a different IP is still accepted.

**I3. Split `core.rs` (2589 lines: about 1500 code, 1090 tests) as the first task of stage 3, and settle where non-`Copy` state lives.**

- Stage 3 adds backend connect, forward and stream states, which would push the file past 3000 lines.
- Suggested layout:
  - `core/mod.rs`: `Core`, `handle`, `forget`, limits, deadlines;
  - `core/contract.rs` or the module docs: the contract;
  - `core/static_files.rs`: `plan`, `respond`'s static half, `file_opened`, `file_failed`, `chunk_ready`, `file_sent`, `not_served`;
  - `core/tests/*.rs`.
- Design point to settle in the same move: `Conn::static_request: Option<StaticRequest>` lives outside the `ConnState` enum only because `ConnState` is `Copy`. That goes against the spec principle that "variants carry only data valid in that state". It is also what creates the unreachable `static_request == None` branches (`core.rs:746-755`, `1337`). Stage-3 states will hold buffers and backend ids that are not `Copy` either. Make `ConnState` non-`Copy` (transitions via `std::mem::replace`) and move the request into `OpeningFile`.
- The three `#[allow(clippy::too_many_arguments)]` on `finish`, `not_served` and `respond` are a symptom. A shared `Exchange { persistence, buffered, body }` struct reused by `Responding`, `OpeningFile` and `SendingFile` removes them.

**I4. Spec text drift.** Spec lines 99-100 still say "Connections and files are referred to by opaque ids allocated by the driver". Stage 2 made `FileId`s core-allocated (a plan ruling), but the spec was never amended. Fix the sentence. This is a plan defect: the amendments list omitted it.

#### Minor

- **M1. HEAD with a late parse error still gets a body.** `core/src/core.rs:1127-1132` renders every parse error `BodyMode::Full`, so `HEAD / HTTP/1.1` with no Host gets a 400 with a body. Amendment 5 says "any request whose method was parsed", and stage-1 M6 asked for "have the parser report the method alongside late errors". The connection closes, so this is cosmetic. Fix: `HeadParser` remembers the parsed method, and `try_parse` uses it on `Err`. Or reword amendment 5 to "every response after a successful parse". See Rulings.
- **M2. A change racing the last chunk's `sendfile` is never detected.** Stamps are only checked at each `ChunkReady`. That cannot be fixed, because the bytes are already out with the full length. Document it in the contract and the example config ("abort detects changes seen at chunk boundaries").
- **M3. Sim invariants missing for two Global Constraints.** "At most one open file per connection" and "one `Readahead` in flight per file, at the end of the bytes already sent" are checked only by one trace-parsing test (`serves_file_larger_than_one_chunk`), not by the sim. Add both checks to `check_files` and `apply_file`.
- **M4. Random-net gaps.**
  - No percent-encoded paths: the tree is all ASCII, and there are no `%20`, `%C3%A4` or encoded-dot 400s.
  - No query strings, so `Location` + `?query` is not covered.
  - No long paths (see C1) and no `.well-known` route.
  - No readahead or sendfile EIO: `FileFailed { Other }` mid-transfer appears only in the core unit test.
  - The disk delay is global, so completions only reorder when the delay changes mid-run. A per-op random jitter would model a thread pool.
- **M5. Readahead is strictly serialized with the network.** The next chunk is requested only after the current one was fully accepted. With a 10 ms disk that caps a connection at about 25 MB/s. The plan chose this ("never ahead of what the client has taken"). Note it for stage 7 (start the next readahead at half the chunk).
- **M6. The example config omits some behaviour.** It does not mention the 301 for a directory without a slash, that symlinks are 404, or that `/api` currently answers 501. No test enforces "documents every key". Add a test that checks every config key name appears in the example.
- **M7. Abort and timeout emit their actions in different orders.** Abort: `CloseFile` → `Log` → `Close`. Timeout: `Log` → `CloseFile` → `Close`. Unify them during the I3 split.

### Deferred-minor triage

| Ledger entry | Triage | Reason |
|---|---|---|
| T1: skipped route segments not decoded or checked beyond emptiness | later (none) | They matched the route literally. Once C2 is fixed, raw and decoded routing agree anyway. |
| T2: `release_orphan` assumes the orphan was `Counted::Yes` | later (stage 3, during the I3 split) | Make it structural (`FileUse::Orphan { peer }` from I2 carries it) instead of a `debug_assert`. Fix it together with I2 if I2 is done now. |
| T2: one IP doing GET+reset against a hung disk can hold every global slot | fix before merge | See I2. The proposed driver-side open timeout cannot cancel a blocked `openat2`, and the core fix is about 10 lines. |
| T2: sim copies the full file bytes per open | later (stage 5, long runs) | Test-only, and cheap at today's 1 MiB maximum. |
| T3: (→ Task 4) `Truncated`/`Changed` in answer to `OpenFile` lost its test | resolved | `impossible_open_errors_close_the_connection` (`core.rs:2283`). |
| T3: (→ Task 5) reset/timeout during the `index.html` open; HEAD on 405 and proxy 501 in the sim; random run lacks directory/301/405/413 | resolved | `reset_during_index_open_orphans_it`, `disk_timeout_during_index_open_orphans_it`, `head_on_405_path_and_proxy_501_is_head_only`. Coverage shows 301=115, 405=173, 413=264. |
| T3: `static_request == None` branches unreachable | later (stage 3, I3) | Move the request into `OpeningFile`, so the impossible state cannot be represented. |
| T4: (→ Task 5) first-chunk change under `abort`; `check_stamp` table; disk-timeout orphan slot end to end | resolved | `change_on_first_chunk_aborts_before_the_body`, `check_stamp_table`, `disk_timeout_orphan_slot_is_released_by_any_late_answer`. |
| T4: Disk blamed when the client is also behind | later (none) | Documented, and only a log label. |
| T4: abort vs timeout action order differs | later (stage 3, I3) | Cosmetic; unify during the split (M7). |
| T4: driver-bug close of an open logs nothing | later (stage 4) | Add `Log::DriverBug` (or similar) once a real driver can have bugs. |
| T4: `OnFileChangeOnProxy` checked before `MissingBodyLimit` | later (none) | Either error is correct; only the order of reports differs. |
| T5: grown counter thin (4/1500) | fix before merge | Cheap: bias `Change` toward growth on `/g`. Four hits are one refactor away from zero, and `Coverage::check` would then fail anyway. |
| T5: periodic 509-byte content (a shift by k·509 goes undetected) | fix before merge | Cheap: mix the position into each byte, e.g. `block[i % 509] ^ (i / 509) as u8`. Byte-exactness is this stage's headline guarantee. |

### Rulings I disagree with

1. **Task 2: "per-IP slot released at close" for orphaned opens.** Keep the per-IP slot until the disk answers (I2). The deferred driver-timeout remedy is not implementable for a blocked `openat2`.
2. **Task 3: "parse errors keep a body (method unknown)".** For errors after the request line (MissingHost, header and framing errors), the method *is* known. This contradicts amendment 5 and leaves stage-1 M6 half-done. Either carry the method or amend the spec (M1). Low stakes.
3. **Task 3: "301 Location = raw path + / (+ ?query)".** The rule is right, but it needed a size bound (C1).
4. **Task 2: "FileIds sequential from 1 (core)".** Fine in itself, but it needed the matching spec amendment (I4).

All other rulings (405 before 413, Disk blamed while head bytes are queued, deadline reset on disk progress, the sim disk queue, a trailing slash on a regular file → 404, 404 for unreachable path errors) are sound.

### Declined to judge

- TLS, HTTP/2, range requests, compression, conditional requests: non-goals or later stages per the spec.
- Proxy behaviour beyond "answers 501, close": stage 3.
- Real epoll/openat2/sendfile behaviour: no driver exists yet. I judged only whether the contract is implementable (I1).
- Landlock and seccomp containment: stage 5.
- Throughput and congestion numbers, timer-index performance (`next_deadline` O(n), stage-1 T9): stage 7 per the stage-1 ruling.
- Late-timer delivery in the sim (stage-1 M4): stage 4 per the stage-1 review.
- Whether the content-type table is complete: the spec leaves the exact table to the builder, and the plan's table is implemented exactly.
- `--check`, CLI and log line formatting: stage 4.
- Hard links inside a static root: operator responsibility, beyond what `openat2` can detect. Noted only as a doc line in I1.
- Process notes (RED via stub in Task 1, the fmt slip in 01436bd): already ruled on and fixed in the ledger; no product effect.

### Verdict

**Ready after fixes.**

Fix before merge:

- C1: bound the 301 to the send buffer.
- C2: reject percent-encoded unreserved characters so routing sees canonical paths.
- I2: keep the orphan's per-IP slot.
- T5: the two cheap random-net fixes.
- I4: the one-line spec fix.

I1 (the driver-contract additions) and I3 (splitting `core.rs`) should land before the stage-4 plan and as the first task of stage 3 respectively. They do not block merging stage 2.
