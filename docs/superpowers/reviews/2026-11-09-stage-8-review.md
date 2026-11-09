# Stage 8 (backend-directed files, ranges) — review record

Plan: `docs/superpowers/plans/2026-11-07-stage-8-internal-files.md` (owner-approved `b8da4d4`; the spec, `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md`, is binding where they differ; stage 8 spec `862e951`). Range `862e951..` on `main`; the tasks landed one by one after their task reviews (`b8da4d4..e14ffa8`), the final review's fix wave on top (`34faa70..3121587`). Every task was implemented by an Opus subagent (Task 2: Sonnet), reviewed by a separate Opus reviewer, fixed until approved (every finding, Minor included), gated on a fresh build and fast-forwarded with CI green.

Gate at the end of the fix wave: 1160 Rust tests (369 of them again under the seccomp kill switch), 41 bench tests, 92 Python tests; fmt, clippy `-D warnings`, the product-deps check, the docs build with `-D warnings`; the driver's `internal_files` with Landlock `required`; the sim nets over 20 000 seeds (both nets also over 200 000 in the review); CI green on glibc and musl, x86_64 and arm64, the root step included.

## The tasks

| Task | What | Range on `main` |
|---|---|---|
| 1 | single-range requests, `If-Range`, 206/416 (FR-STATIC-19) | `b8da4d4..91fae48` |
| 2 | `internal_files` config, root opened and Landlocked (FR-CFG-26) | `91fae48..73a246f` |
| 3 | file headers stripped both ways, Rack hints, hints in the head bound | `73a246f..7ff8c96` |
| 4 | backend file path rules (`backend_path.rs`) | `7ff8c96..8f68af3` |
| 5 | serving the backend's file (FR-PROXY-32) | `8f68af3..dc6b463` |
| 6 | through the binary, the random proxy net, bench `proxy-file` | `dc6b463..159317f` |
| 7 | statuses, docs | `159317f..e14ffa8` |

## Owner decisions taken during stage 8 (all in the spec)

- 2026-11-07 (brainstorm): both `X-Sendfile` and `X-Accel-Redirect`, each route trusting the one it names; the file behaves like a static file, `Content-Type` from the backend wins; only a backend `200` triggers; missing file 404, bad path 502; the backend's body never read, pooled only after `Content-Length: 0`; nginx's other `X-Accel-*` features out of scope; HEAD and any method get the file.
- 2026-11-07 (builder check): ranges for static and backend files (a non-goal lifted); Rack's hint headers stripped from clients and set by the proxy.
- 2026-11-09 (final review): client headers spelled with `_` for `-` stripped like the hyphenated ones (file and identity headers); an `internal_files` root equal to or below a static root refused; a `Range` on an empty file ignored (whole 200); the two older test files over 900 lines split.

## Rulings made on the owner's behalf

- Brainstorm: a 304 keeps the backend's headers, a 404 and 416 are plain; `max_response_body` does not apply to the file; a merged head that would not fit is a 502; a served file logs once as its own kind.
- Task 1: a single range only (several → 200); `If-Range` dates compare by exact bytes; `Accept-Ranges` on 200/206/416; a failed `If-Range` with an unsatisfiable range → 200.
- Task 2: an internal root of `/` refused (whole filesystem readable); internal roots listed after all static roots.
- Task 3: the hint bytes counted in the head buffer and the memory budget inside Task 3 (not deferred); the route's setting carried as an `Arc` handle, one lookup per request.
- Task 4: `?`, `=` and `,` refused in an internal root, `,` in a prefix (a `?` makes every `X-Sendfile` a 502; Rack splits `X-Accel-Mapping` on `,` and `=`); `%2e` is a dotfile (404), a raw `.`/`..` a 502.
- Task 5: conditionals only for GET and HEAD; the route header counted on every line, one named in `Connection` included.
- Task 6: `proxy-file` on its own server so every other scenario keeps its pre-stage-8 config; no target until the owner sets one; `Date:` and `Content-Length: 00` model calls recorded.
- Final review: the `head_pending` reuse guard is unreachable by events and pinned by a unit test of the shared rule; `file-must-park-reused` counted, not gated (a deterministic test holds it per push); the reload helper's fork waits on a "go" FIFO (not the brief's read-write open, which could leave a server unlisted).

## Final whole-stage review (at `e14ffa8`)

Two Opus reviewers in parallel, read-only: (A) product code and the sandbox, with strace on a live release binary; (B) tests, sim, bench, CI, docs and traceability, with 23 own mutations and the nets over 200 000 seeds.

**Strengths.** Every backend-named file opens with `openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS)` below its root's descriptor (strace); no new system call; the range path reads only its range; no backend byte reaches a client or the next pooled client (live, pipelined); the head bound exact (401 bytes, rechecked); one shared post-open decision for static and backend files; the sim models written from the spec; all 23 review mutations caught.

**Findings.** 0 Critical.
- A-1 (Important, reproduced live): client headers spelled with `_` (`X_Accel_Mapping`, `X_Forwarded_For`) reached the backend; CGI servers read them as the hyphenated key, so a client could forge Rack's mapping and get another user's file within the root.
- Minors: stale example memory figures; an error kind's name; duplicated client-end logic and header names; a two-level route lookup with an unreachable 502; a per-416 allocation; three Task 6 calls and the trigger's 2xx edge and a range edge pinned only by random nets; prefix-only head assertions; thin static range gates; a one-sided pooling oracle; README and spec wording; two older test files over 900 lines.
- Owner decisions requested: the underscore rule's scope, an internal root under a static root, an empty file's range, the file splits.

## Fix wave

Spec first (`34faa70`). Two parallel implementers, combined by rebase onto `stage8-final`:
- Product (wave A): the underscore rule (byte-wise, no allocation); the root-overlap check; empty-file ranges ignored; the cleanups; deterministic core tests for every finding, each proven by its mutation.
- Tests and docs (wave B): exact heads in the driver and sim tests; non-2xx file headers relayed; floors for the rare range kinds; a two-sided pooling oracle; the example's memory figures with a test; the file splits (pure moves).

**Re-reviews.** Fix wave: all addressed; four new Minors (a doc line, the pooling oracle on reused upstreams, the underscore rule not in the random net or the binary tests, the static head's `format!`) fixed in a follow-up round, re-reviewed clean. Wave C (below): Needs fixes (a failed listing still started the binary; `helper.rs` over 900 lines; a skip that must fail under `REQUIRE_ROOT`; leaked test processes; `stop`'s KILL loop by pid alone), fixed and re-reviewed clean (three cosmetic leftovers fixed after).

## Root causes found on the way

- **Reload helper fork-to-exec race (final review, wave C):** the helper listed the new pid while it was still its own fork on the way to `setsid` and the binary, but OpenRC's `stop_post` also required that pid to run the binary, so a helper killed in that window left a process `stop` skipped (and deleted the list); one killed before the reader fork left the fork blocked forever on the readiness FIFO (7/200 under fork-heavy load; found as a once-only test failure that was investigated, not rerun). Fixed by matching on pid and start time, a "go" FIFO that holds the fork until it is listed, every FIFO end opened before the fork, a failed listing failing closed, and `stop` rechecking start times before every signal; gated `setsid`/`mv` stubs reproduce both windows. Seven blocked shells left by test runs in the container were ended.
- **Proxy net replay attribution (follow-up round):** a reset right after a response, meeting a request the core had already put on the reused connection, was blamed on the previous exchange — a net bug, not the core.
- **Hint bytes outside the head slack (Task 3):** a request at `max_head` on a route with long hints got a 431; the hints are now in the head capacity and the memory budget.

## Accepted gaps

- A reuse that pools and takes an upstream in one core call is not seen by the net's park count (only stricter, never looser).
- The pid-reuse-within-one-poll case of `stop` has no test (cannot be forced deterministically).
- The reload stub tests skip where `sh` ignores `PATH` (Ubuntu's busybox); both CI legs run them, and a skip fails under `REQUIRE_ROOT`.
- Pre-existing thin gates as in the stage 7 record; "restart: closed before a request" still never reached.
- Task 5's implementer wrote part of the code before its tests (recorded; the gap it left was closed test-first).

## Process notes

- Several of my briefs carried names or claims the code had outgrown (Task 5's types, a B-M1 case, wave C's step 3); implementers checked against the spec and code, and the spec won each time.
- Two implementers clobbered one shared scratch gate script; later briefs give each its own.
- Container restarts and a full disk (stale build caches, cleared) interrupted work; the ledger and pushed branches carried it.

## Deferred

- Owner: the `proxy-file` NFR-PERF target from its first nightlies (local: ~22 system calls, ~81 µs per request).
- Later: multipart ranges; nginx's other `X-Accel-*` features; TLS (pure Rust); Landlock network rules; reporting the kernel reuseport race to netdev; the efficiency carry-forwards of the stage 7 record.
