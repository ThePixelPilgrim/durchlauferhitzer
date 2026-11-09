# Stage 9 (guarantee hardening) — review record

Plan: `docs/superpowers/plans/2026-11-09-stage-9-guarantee-hardening.md`; brainstorm record: `docs/superpowers/reviews/2026-11-09-qmail-lessons.md`; the spec is `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md`. The stage came out of a review of qmail's security record: lessons 5 (never let memory safety depend on the deployment) and 10 (the threat model ages faster than the code) became requirements; both open decisions it raised were taken the same day.

Gate at the end of the fix wave: fmt, clippy `-D warnings`, the workspace tests (1190), the driver suite under the seccomp kill switch (385) and on the static musl target (385), the product-dependency check, docs with `-D warnings`, the proxy net over seeds 0..20000; CI green on `main` since `3bee322` (run 653).

## The tasks

| Task | What | Range on `main` |
|---|---|---|
| 1 | `required`/`required`/`kill` are the containment defaults (FR-CFG-25) | `4c999c1..dccd243` |
| 2 | the kernel memory backstop: `RLIMIT_DATA` from `memory_limit` plus a measured allowance (NFR-RES-09) | `dccd243..92b3c36` |
| — | the stage 8 review's fix wave landed here (another session) | `92b3c36..acc3577` |
| 3 | `--check` warnings, the backstop line, `mode=` on the containment log line, the README's guaranteed install (FR-PROC-08) | `acc3577..8f7abb8` |
| 4 | a body on `GET`/`HEAD` toward a backend is 400 (FR-PROXY-01; NFR-SEC-14's TE.0 row) | `8f7abb8..06e77dc` |
| CI fix | the containers' `nofile` ulimit; both epoll waits and both clone flag sets on every libc | `06e77dc..3bee322` |
| 5 | close-out | `3bee322..e2a031a` |
| nightly | issues #3 and #4 pinned (model bugs; the core was right both times) | `e2a031a..86970a7` |
| final | the fix wave for the whole-stage review | `86970a7..dd8f1c6` (`aca6a90` the wave, `7443a2c` its doc minors, `81beb1a`/`2b133cc`/`72f519e`/`dd8f1c6` test robustness under load, `0d67457` glibc's arena limit), then this record |

## Owner decisions taken during stage 9 (all in the spec)

- 2026-11-09 (qmail review): NFR-SEC-14, the framing-variant ledger, reviewed yearly; NFR-RES-09, the kernel memory backstop; FR-PROC-08, `--check` warnings and the guaranteed install.
- 2026-11-09 (open decisions 3 and 4): a `GET`/`HEAD` whose head frames a body is 400 on proxy routes, static routes keep their 413 (FR-PROXY-01); `required`, `required`, `kill` are the containment defaults (FR-CFG-25).

## Rulings made on the owner's behalf

- Plan: the backstop's function and constants are `pub` so later tests compute the expected number from the code.
- Task 1: tests about `best_effort` write it explicitly, tests about the default expect `kill`; the test kill switch is kept although it now equals the default (every server test runs under kill); implementer commits carry the implementer's own attribution line.
- Task 2: the per-thread term carries glibc's arena and sigaltstack overhead (256 KiB on top of the 2 MiB pinned stack); the base is 4 MiB (twice the derivation); test configs use `memory_limit 2MiB`; the weak single-constant mutation pinning accepted knowingly. **The per-connection constant of 5 KiB measured on idle connections was reversed by the final review** (see below).
- Task 3: the README's systemd unit example carries `Restart=on-abnormal` itself; the README's backstop figure is pinned through the example-config `--check` test; the OpenRC script's missing respawn is the owner's carry-forward.
- Task 4: three test cases that sent `GET`/`HEAD` bodies to backends removed as unreachable (confirmed twice); `bodies.rs` at 895 lines is split by the next task that adds to it.
- CI fix: both `epoll_wait` and `epoll_pwait`, and both thread clone flag sets (glibc's, and the same plus `CLONE_DETACHED`), allowed for every libc as exact matches — a libc update under `kill` must not end the server; the containers get `--ulimit nofile=1048576:1048576` rather than letting the 65536-connection test skip.
- Nightly: FR-PROXY-17 states that limit verdicts need no LF (FR-PROXY-14 as soon as certain); the implementer's rewrite of the buffer clause accepted (`max_head` may exceed 32 KiB); a draw rebalancing keeping `file-416` above quota accepted.
- Final fix wave: the decoded path becomes one allocation per connection moved to the disk job; the per-connection allowance becomes `8 KiB + 3 × max_head`, the factor measured from the worst in-flight request on both route kinds (the implementer's choice of `max_head` over `max_request_line`, since the kept `Host` lies outside the request line, accepted).

## Final whole-stage review (at `e2a031a`, Opus, with a live attack on the release binary)

**Strengths.** The defaults flip in one place; the backstop sits after the descriptor budget and before the privilege drop and any thread, with every expected number computed from the code's constants; `--check` stays side-effect free with the five strings exact; the 400 for a body on `GET`/`HEAD` is one exhaustive arm before any connect or `100`, mirrored by the net.

**Findings.** 1 Critical, 1 Important, 7 Minor.
- C1 (Critical, reproduced live): the per-connection allowance (5 KiB, measured idle) missed in-flight request state — a decoded static path was one `Vec` per segment, cloned for the disk job, about 470 KB for 4000 `/a` segments within an 8 KiB request line; 150–300 such clients at a `memory_limit` near the FR-CFG-21 minimum aborted the server. A remote denial of service the backstop introduced; the shipped 1 GiB default absorbed it.
- I1: FR-PROC-06's seccomp paragraph still called `fail` the default.
- Minors: SIGSYS instead of SIGABRT under `RUST_BACKTRACE` with `kill`; the thread-spawn error naming neither `RLIMIT_DATA` nor the backstop; the reload helper hiding `--check` warnings; the `landlock is required` message without the way out; kernel memory outside `RLIMIT_DATA` undocumented; premature `[done]`; the README figure unpinned.

## Fix wave (`8980dbc`, one Opus implementer)

`RelPath`: one buffer per path, segments joined by `/`, handed to the disk job by move; the per-connection allowance `8 KiB + 3 × max_head` (measured floor 6 717 bytes and worst 2.28 × `max_head` on static musl); two server tests hold the worst-case request on every slot at the minimum `memory_limit` (static and proxy), both aborted the old binary and pass now; the review's exploit got 1000/1000 responses with the server alive; bench system calls per request unchanged. I1 and all seven minors fixed.

**Re-review.** Opus, four passes over the 165 KB diff: all nine findings addressed; the path rules byte-identical to before (FR-HTTP-13/14, FR-STATIC-09, FR-PROXY-32), no ambiguity from joining segments since a decoded `/` or NUL is rejected before; the allowance re-derived against every measured row (tightest 157 560 against 204 800 at `max_head` 64 KiB on static musl); the survive tests not vacuous (every static client must get a 404, the proxy test waits until all 60 requests reach the backend). No Critical or Important breakage; four doc minors (a misplaced doc comment, the backend-file path's bound via the freed upstream buffer, FR-CFG-21's and `Limits::memory_limit`'s lists, an overstated "pinned by tests") fixed in `7443a2c` after the rebase onto `86970a7`, with the gate rerun on the combined tree (1194 workspace tests, 385 under kill, 385 on musl, 20 000 seeds).

## Root causes found on the way

- **glibc's arena limit read under contention (final gate under CPU pressure):** the first time contention forces a new malloc arena, a disk worker thread reads `/sys/devices/system/cpu/online` through plain `openat`, which the allow-list rightly lacks, so the server was killed by SIGSYS; quiet runs never create enough arenas to reach it. Fixed by presetting glibc's arena limit (`mallopt(M_ARENA_MAX, 1)`, `driver/src/sys/malloc.rs`) before the first thread, not by allowing a path-based open. Contention-only libc paths need a run under CPU pressure, which the gate did not have.
- **Idle measurement of a per-connection bound (Task 2 → C1):** an allowance measured without load cannot cover load. Resource bounds are measured under the worst request the limits admit, and a resource-bound change gets an exploit attempt on a built binary before it is declared done.
- **Container root without `CAP_SYS_RESOURCE` (CI fix):** a test that raises a hard limit as root passes on the plain runner and fails in every container; the local gate never sees it because the `REQUIRE_ROOT` flag is CI's. The CI run of `main` is part of a task's done criterion.
- **Libc versions under `kill` (CI fix):** Rust's bundled musl 1.2.5 issues `epoll_wait` and glibc's exact clone flags where Alpine's 1.2.6 issues `epoll_pwait` and adds `CLONE_DETACHED`; a per-libc-version allow-list kills the server on a libc update.
- **Nightly seeds replayed at `main` (issues #3 and #4):** later commits change the random draws, so a nightly seed is replayed at its run's commit; both failures were model bugs, and the cases are pinned deterministically.

## Accepted gaps

- The arena-limit fix has no deterministic test: the path needs contention; the evidence is strace under pressure and eight pressure runs of the backstop binary (recorded in the fix report).
- The survive and smoke-load backstop tests tolerate slow machines with a 60 s read timeout and slot slack rather than exact timing.
- Zeroing one allowance constant alone fails no test (generous constants cannot be pinned one at a time); the survive tests catch a factor of 0, not 1 or 2; the factor rests on the documented measurement.
- The thread-spawn error names `RLIMIT_DATA` whatever limit refused the thread.
- The reload helper shows `--check`'s stderr only when it holds a warning.
- The `fail`-mode run of the driver suite no longer exists (the kill switch equals the default); the explicit `fail` tests remain.
- Dynamic Alpine musl was not measured for the allowance (static musl was); the Alpine CI jobs are green.

## Process notes

- The fix-wave implementer and the controller shared one worktree: the implementer's commit swept the controller's uncommitted record into `81beb1a`, and a later `reset --hard` by the controller discarded uncommitted doc edits, redone from their scripts. One worktree per writer.
- Three task reviews read diffs and approved an allowance that a live attack broke in minutes; the whole-stage review's instinct to build and attack the binary is what caught C1.
- CI went red from Task 2 to Task 4 unnoticed by the local gate; the controller reported tasks done before the CI run of `main`.
- While the controller's session sat inside a worktree, every subagent's shell was confined to it; parallel implementers need the session in the main checkout.
- Task briefs carried wrong assumptions three times (which containment tests write their mode; the allowance's measurement method; which `--check` test held a port); implementers checked and reported instead of forcing the brief.

## Deferred

- Nightly: a job running the driver suite under CPU pressure (busy loops on every core), since contention-only libc paths never show in quiet runs.
- Owner: the OpenRC script has no respawn (add supervision or leave it documented); close nightly issue #5 when the next nightly is green; the `proxy-file` NFR-PERF target (from stage 8).
- Later: the qmail review's carry-forwards (a nightly on the newest kernel and glibc; numeric `uid:gid` for `user`; an opener process when TLS arrives; product line count beside the benchmarks; the supported TLS deployment named); a `fail`-mode suite run if wanted; `sim/tests/proxy/bodies.rs` split at the next addition.
