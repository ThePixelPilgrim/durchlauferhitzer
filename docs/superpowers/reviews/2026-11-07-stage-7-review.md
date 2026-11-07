# Stage 7 (efficiency) — review record

Plan: `docs/superpowers/plans/2026-11-06-stage-7-efficiency.md` (owner-approved `71d3867`; the spec, `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md`, is binding where they differ). Range `71d3867..` on `main`; the tasks landed one by one after their task reviews (`71d3867..40e3429`), the final review's fix wave on top (`999724c..d67e76e`). Every task was implemented by an Opus subagent, reviewed by a separate Opus reviewer, fixed until approved (every finding, Minor included), gated on a fresh build and fast-forwarded with CI green. Every optimisation was kept or dropped by measurement (the `bench` tool, medians of 5 runs).

Gate at the end of the fix wave: 1032 Rust tests (356 of them again under the seccomp kill switch), 37 bench tests, 91 Python tests, the sim nets over 20 000 seeds; fmt, clippy `-D warnings`, the product-deps check, the docs build with `-D warnings`; CI green on glibc and musl, x86_64 and arm64, the root step included; a nightly dispatch green; netem run 42398454271 green.

## The tasks

| Task | What | Range on `main` |
|---|---|---|
| 1 | `bench` tool, nightly publish to `benchmarks`, Mermaid chart (NFR-PERF-08) | `71d3867..e34cc72` |
| 1b | syscall pass separate from the CPU pass, `bench/bisect.sh` | `d4fc105..9ea1f82` |
| 2 | conditional requests (FR-STATIC-18): 18.9 → 12.6 syscalls | `9ea1f82..151c268` |
| 3 | `MSG_MORE` when a file follows: static-small −5 % CPU | `151c268..dd30494` |
| 4 | deadline index: `next_deadline` 2.7 µs → 3 ns at 1k idle | `dd30494..65d858e` |
| 5 | backend pooling (FR-PROXY-02): proxy-small −43 % CPU | `65d858e..939dc40` |
| 6, 7 | `splice`, N threads: dropped by measurement | `326ac5b` |
| 8 | eager I/O, deferred epoll registrations, 32 KiB buffer, netem: proxy-large 422 → 140 syscalls | `326ac5b..50fbd16` |
| — | statuses, targets ratcheted from the nightly at `50fbd16` | `50fbd16..40e3429` |

## Owner decisions taken during stage 7 (all in the spec)

- 2026-11-06 (brainstorm): mixed static and proxy load, low to medium rate, 2–6 cores, ≤ 2 GiB; CPU and I/O as low as possible; nightly on GitHub runners, CPU and syscalls per request, charted without a server.
- 2026-11-06 (targets): ratchet only towards better; an exception needs the owner's OK and a stated perspective; security trumps performance.
- 2026-11-06 (Task 5): pooling shared across clients, on by default, per-route `pool off`; a safe retry for GET/HEAD before any response byte; a silence check (`MSG_PEEK`) before reuse; no reuse after HEAD, 204 or 304.
- 2026-11-07 (stop review): `splice` and N threads dropped; buffers tuned; the upstream buffer is 32 KiB; NFR-PERF-05 kept (fill-drain-refill), target "no errors under 50 ms, 1 % loss, a 2 s pause; recovery ≤ 1.5 s".
- 2026-11-06/07: process-wide limits; RFC 304 only; NFR-PERF-03 relative at 1k idle, one run nightly; the `MSG_MORE` TTFB trade-off.
- 2026-11-07 (final review): If-None-Match keeps 16 distinct tags and ignores the rest; CPU targets checked against the next two nightlies.

## Rulings made on the owner's behalf

- Task 1: `bench` is an excluded std-only crate; `bench publish` is the only job that writes and runs no code under test; untrusted rows never reach a shell.
- Task 1b: CPU from `/proc/<pid>/stat`, syscalls from a separate `strace -c -f` pass.
- Task 2: ETag `"<size hex>-<mtime ns hex>"`.
- Task 4: a `BTreeSet` index with a debug cross-check against the scan.
- Task 5: LIFO idle list; `backend_idle_timeout 2s`, `backend_idle_max 8`; `recvfrom` allowed only with `MSG_PEEK|MSG_DONTWAIT`.
- Task 8: at most 64 eager attempts per turn; a stale registration is harmless until the wait (DRV-03).
- Final review: `Log::BackendNotSilent`; `Connection: close` toward the backend while draining; manual publish only at the default budget; `--append` takes only rows of the run's commit and date.

## Final whole-stage review (at `40e3429`)

Two Opus reviewers in parallel, read-only: (A) product code in `core`, `driver` and the sandbox; (B) tests, sim, bench, scripts, CI, docs and traceability, with mutations.

**Strengths.** Every kept change was measured and every dropped one recorded; the core stays sans-IO (no contract change in Task 8); the silence check adds a single argument-checked system call; the deadline index is cross-checked in debug; the publish path keeps untrusted data away from shells and code under test.

**Findings.** 0 Critical.
- A-I1 (Important): If-None-Match tags were unbounded (up to 64 KiB per connection, outside the memory budget).
- B-I1 (Important): nothing pinned "`MSG_MORE` only when a file follows". B-I2 (Important): the driver half of the silence check was unpinned. B-I3 (Important): the spec contradicted itself on reuse after HEAD/204/304 (fixed in `999724c`). B-I4 (Important): HANDOFF stale.
- Minors: the no-retry-while-draining rule untested; a stale eager comment and head-size figure; no log for a failed silence check, no request summary on a retry; keep-alive toward the backend while draining; long lines; stale bench and netem docs; the bench readiness race; chart validation tests passing with `match`; `--append` accepting any commit and date; a lowered-budget dispatch publishing; the bench's commit label; a bare `read` in the pool tests; thin sim gates.
- Owner decisions requested: the If-None-Match cap, CPU drift against the targets.

## Fix wave

Spec first (`999724c`). Two parallel implementers, combined by rebase onto `stage7-final`:
- Product (wave A): the 16-tag cap with a sim model; a core test and a net oracle against retries in a drain, `PROXY_SEEDS` 400 → 1000; `kind=backend_not_silent`, the request summary on `backend_retried`; `Connection: close` toward the backend while draining; driver tests for `MSG_MORE` and the silence check, each proven by its mutation.
- Bench/CI/docs (wave B): chart tests that fail under `match`; `--commit` and a commit-and-date filter on `--append`; publish only at the default budget; `BENCH_RUNNER` validated; the bench waits past the bind; netem at 32 KiB (run 42398454271: 0 errors, recovery ≤ 1.25 s).

**Re-reviews.** Wave A: Approved, Minors fixed (the effective drain plan in sim failure messages; the repeated-tag odds raised until the dedup mutation fails the per-push net; an unreachable arm removed; a 150 ms bound on the `MSG_MORE` test; the new log kinds in the log property test). Wave B: Approved, Minors fixed (tests for `--commit`, `--today` and the UTC clock; `BENCH_COMMIT` removed; netem wording).

## Root causes found on the way

- **Startup race (Task 8 CI):** the bind probe is a listener for a moment; a readiness connect landing on it returned early and the test's own connection was reset when the probe closed. Tests and the bench now wait for the signalfd in `/proc/<pid>/fd` (`87f4988`, `d56b9c9`). No request was lost.
- **EINTR (Task 2):** SIGCHLD interrupted test reads under `SO_RCVTIMEO`; `driver/tests/support/read.rs` retries (`731f4ad`).
- **epoll churn (Task 8):** each 16 KiB chunk took the upstream out of the epoll set and back, and flipped the client twice; deferred registrations removed it.

## Accepted gaps

- Thin sim gates, pre-existing: connect timeout with a body read with the head (6 hits), abort closed before a request (7), too many interim heads (11), 101 from a backend (5), LF in a backend head (4); "restart: closed before a request" never occurs (reported, not gated).
- Benchmarks run on shared GitHub runners: CPU drifts (checked against the next two nightlies); syscalls are the stable signal.
- Pooling's residual risk: a backend closing at the instant of reuse; the silence check and the safe retry cover GET/HEAD only.
- One unreproduced workspace-test failure in Task 5 (overlapping runs; 7 later runs clean).
- The scratch branch `stage7-task8-buf64` (a 64 KiB measurement, never merged) waits for the owner to delete it (the proxy refuses branch deletion).

## Process notes

- A controller `git commit -a` in the shared checkout swept an implementer's edits into a spec commit (`6242b54`); fixed forward. The controller no longer commits there while an implementer works.
- A doc-only push went red unnoticed until the next task; CI is checked after every push.
- Container restarts ended two implementers; work resumed from the pushed branches and reports.

## Deferred

- Stage 8: backend-directed file serving (spec timeline item 8).
- Efficiency, later: answering a silence check without deregistering; reusing buffers without zeroing; a regression alarm on syscalls; N threads only if the loop nears 50 % of a core (N threads before seccomp, steering redesign as in the stage 6 record).
- Later: TLS (pure Rust); Landlock network rules; reporting the kernel race to netdev; the Eyra spike's revisit (`docs/superpowers/spikes/2026-11-06-eyra.md`).
