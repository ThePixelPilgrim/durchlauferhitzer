# Stage 4 (epoll driver and the first runnable binary) — review record

Plan: `docs/superpowers/plans/2026-10-27-stage-4-epoll-driver.md` (amendments A1–A11; the spec, `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md`, is binding where they differ). Range `7e1b18e..` on `main`; the ten tasks landed one by one after their task reviews (`7e1b18e..f19e905`), the final review's fix wave on top. Every task was implemented by an Opus subagent, reviewed by a separate Opus reviewer, fixed in its own loop until approved (every finding, Minor included), gated on a fresh build, and fast-forwarded onto `main` with CI green.

Gate at the end of the fix wave: 758 tests, fmt, clippy `-D warnings`, the product-deps check and its self-test, the docs build with `-D warnings`; CI green on x86_64 and arm64, the root step included. Randomized nets about 5 s each (debug).

## The tasks

| Task | What | Range on `main` |
|---|---|---|
| 1 | config bounds, `memory_limit` budget, `disk_threads`, `shutdown_timeout` | `7e1b18e..7a53c4d` |
| 2 | core: `Date`, drain/abort API, FR-LOG-07 fields | `7a53c4d..177eef8` |
| 3 | sim late-timer mode (`TimerOrder`) | `177eef8..a1c5101` |
| 4 | `driver` crate, safe `sys` wrappers (`libc` only) | `a1c5101..333b0b0` |
| 5 | epoll event loop for clients and upstreams | `333b0b0..4783fa0` |
| 6 | disk worker pool and `sendfile` | `8bc43ff..52e7615` |
| 7 | `key=value` logging, non-blocking stderr | `52e7615..8b882eb` |
| 8 | CLI, startup (A11), systemd sockets, privilege drop, descriptor budget | `8b882eb..2309e95` |
| 9 | signals, drain, fast shutdown, bounded teardown | `2309e95..a48240a` |
| 10 | end-to-end smoke suite, docs, spec status, arm64 and root CI | `a48240a..f19e905` |

Between tasks: `e40c7e3` (plan: Task 8's exit enum is `ExitStatus`) and `8bc43ff` (gate: docs build).

## Owner decisions taken during stage 4 (all written into the spec)

- 2026-10-27 (Task 2): during a drain a complete pipelined request buffered behind the in-flight response gets 503 + close (FR-PROC-05b); `abort_all` logs `Rejected { ShuttingDown }` per cut exchange, idle and lingering connections are cut silently (FR-PROC-05c).
- 2026-10-27 (Task 3): late wakes — delivery order decides; every deadline may be overrun by as long as the loop was late (DRV-06).
- 2026-10-27 (Task 4): every IPv6 listener sets `IPV6_V6ONLY` (FR-CFG-10, A6).
- 2026-10-28 (Task 5): non-transient accept failures pause the listeners until any descriptor closes or 1 s passes (DRV-16); a failed `epoll_ctl` fails only that socket (DRV-15).
- 2026-10-28: the docs build with warnings as errors is part of the gate; the deps self-test runs in CI only (CLAUDE.md, NFR-MAINT-11).
- 2026-10-28 (Task 7): proxy log lines carry the backend address (FR-LOG-05).
- 2026-10-28 (Task 8): closed fds 0–2 are left to std's `/dev/null` reopening (FR-PROC-04; replaces an earlier "exit 2 if fd 2 is closed", which std makes unobservable); inconsistent systemd variables, a dual-stack IPv6 or non-TCP systemd socket, and root (any uid 0) without `user` exit 2 (FR-PROC-02/03); no up-front check for ports below 1024 — a refused bind is the error; the descriptor budget counts the descriptors really open at the check (NFR-RES-05).
- 2026-10-29 (Task 10): NFR-PERF-04 closed by pinning kernel-default buffer sizes; NFR-PORT-01 by a full arm64 CI job; NFR-PORT-03 by a seccomp test that hides `openat2`; the root-only tests run in CI via `sudo` with `DURCHLAUFERHITZER_REQUIRE_ROOT=1`.
- 2026-10-29 (final review): a queued readahead is cancelled on `CloseFile`, closed files hold at most `disk_threads` descriptors (only a running job's), and `disk_threads` is in the budget (DRV-09, NFR-RES-05); all capabilities are cleared after binding and checked (FR-PROC-03); a `user` with primary gid 0 is refused; untrusted log fields are capped at 4 KiB with `<key>_truncated=N` (FR-LOG-05); the Task 2 ruling below is confirmed; a head the parser rejects during the drain keeps its own error with close (FR-PROC-05b).

## Rulings made on the owner's behalf

- Task 2: a request pipelined behind a head rendered during the drain is not answered, since that head says close (A4) — confirmed by the owner on 2026-10-29.
- Task 3: `TimerOrder` enum instead of the plan's bool; late mode chosen by `seed % 4`.
- Task 4: `unsafe` only in `driver/src/sys/`; enums for `ConnectStart`, `Openat2`, `Listening`; thread-directed signal test; no `drop_to` in shared test processes.
- Task 5: `Exit` describes how the loop ended; `StopHandle` for tests; the `ExitStatus` rename for Task 8.
- Task 6: the pool duplicates the wake eventfd (`EventFd::try_clone`); `Driver::new` takes `disk_threads` and starts the pool exactly when the core has static roots.
- Task 7: `[` and `]` are plain log characters (IPv6 unquoted); the stderr sink restores only fd 2's blocking mode.
- Task 8: `pre_exec` test-only `unsafe` confined to one helper module.
- Task 9: SIGHUP blocked and ignored in stage 4; pool and log flush each bounded at 1 s; "buffered" in FR-PROC-05b means what the core has read (clarification 2026-10-28).
- Final review: drain and abort added to the randomized nets now rather than deferred.

## Final whole-stage review (at `f19e905`)

Two Opus reviewers in parallel, read-only: (A) the driver's product code, (B) core and sim changes, all stage 4 tests, spec/plan/README/HANDOFF/CI coherence. Gate re-run in place; probes and mutations in scratch copies.

**Strengths (both).** Every `unsafe` block in `driver/src/sys/` sound with accurate SAFETY comments; the driver contract (DRV-01…16) followed end to end, including upstream error ordering and the DRV-15/16 interactions; no descriptor leak on any path, no busy loop on level-triggered hang-ups, no panic reachable from network input; startup and the privilege drop match A11; log escaping closes injection. Core drain/abort invariants tight; the FR-CFG-21 budget uses the core's own capacities; the sim's model stays spec-written; late-timer mode matches the real loop order; tests isolated and deterministic; every ledger owner decision traceable to the spec.

**Findings.** 0 Critical.
- A-I1: files closed with a readahead still queued kept their descriptor outside the NFR-RES-05 budget (≈ 2 × `max_connections` + `disk_threads` under load), and the pool doc claimed a bound that did not hold.
- B-I1: `begin_drain`'s `Connecting` branch was untested (a mutation passed every test).
- B-I2: the randomized nets never drained or aborted.
- B-I3: the Task 2 ruling still awaited the owner while the spec presented it as settled.
- Minors: `claim_inherited` public; no per-wake accept cap; oversized log entries dropped whole (log evasion); doc drift; duplication; spec gate list; plan amendments lagging the spec; a missing root + systemd + no `user` test; an opaque EACCES when test binaries are unreachable for `nobody`; the `free_port` race; CI hardening (token permissions, checkout pin, root-list completeness); a stale inline status tag.
- Owner decisions requested: the Closing bound, capabilities and gid 0, long log fields, the Task 2 ruling.

## Fix wave

Two parallel implementers on separate branches, combined by rebase onto `stage4-final`:
- Driver (wave A): cancel queued readaheads on `CloseFile` + `disk_threads` in the budget; clear all capabilities after binding (`sys/caps.rs`, verified with `capget`, root tests check every thread); refuse gid 0; cap untrusted log fields at 4 KiB with `<key>_truncated=N`; `claim_inherited` crate-private; `ACCEPT_BATCH = 64`; doc drift; refactors (one `Freed`, `log_register_failed`, `cvt_retry_ssize`, `Readiness` methods).
- Core/sim/docs/CI (wave B): tests for the drain's `Connecting` branch; drain and abort in both randomized nets (`seed % 5 == 0`, plans `Drain | DrainThenAbort | Abort`, witness clients); the Task 2 ruling relabelled an owner decision; NFR-MAINT-11's gate list; plan A4/A11 marked superseded; a root + systemd + no `user` test; a clear error when the test binary is unreachable for `nobody`; the port race documented; CI with a read-only token, `actions/checkout` pinned by commit, and a check that the root-test list stays complete.

**Re-review (round 1): Needs fixes.**
- I-1: a closed file whose readahead had finished but whose result was not yet drained still held its descriptor, so the new bound was not strict.
- I-2: the drain oracles accepted a silent drop of a request the spec says must be answered (two core mutations passed both nets).
- Minors: an open in flight at the drain could be answered 503; no control in the capability tests; overlong lines.
- Owner decision: a head the parser rejects during the drain keeps its own error.

**Round 2.** A closed file is a tombstone; only the running job holds the descriptor, and it closes when the worker drops the job. The capability tests run a control first. The drain oracles allow a silent stop only for spec reasons (the request's bytes not read when the drain began, a closing answer before it, a cut, a reset, the abort, an incomplete head); the witness clients are checked exactly; an open in flight at the drain keeps its tree status; parse errors during the drain keep their own error (FR-PROC-05b). The unanswered check runs on all 1500 static seeds. Both reviewer mutations now fail both nets on oracle checks.

**Re-review (round 2): Approved.** Two overlong spec lines rewrapped by the controller.

Known, accepted gaps (spec-backed or noted): the static net has no parser model; the oracle cannot tell a request read after the drain mark but before the in-flight response ended from a lingering-close read, so it exempts it (strict-but-sound); an incomplete head ended by EOF or cut by the abort may go unanswered (FR-CONN-12, FR-PROC-05c).

## Process notes

- Tests written after the code (teeth shown by mutation instead of RED): the `Date` sim tests (Task 2) and the first late-timer tests (Task 3).
- Container restarts ended several subagents; work was resumed from the ledger, the reports and the pushed branches, and two reports were recovered from transcripts or hand-backs.
- An implementer's local gate once piped clippy through `tail` without `pipefail`; CI caught the lint. Gates now use `set -o pipefail`.

## Deferred to later stages

- Stage 5: "bytes, then reset" backends in the randomized proxy net; IPv6 per-IP grouping by prefix; refused-slot pool fairness; long randomized runs; Landlock and seccomp (NFR-SEC-12).
- Stage 6: the restart target of FR-PROC-05; SIGHUP restart.
- Stage 7: double buffering vs fill-drain-refill; per-request buffer allocation; `next_deadline` O(n); `sendfile` blocking on evicted pages (DRV-09).
- Open: the driver-bug-close log kind is added with its first producer (FR-LOG-07).
- Owner, any time: delete the remote branches contained in `main` (see HANDOFF, "Branches"); `ccr-716d91b0-bff174` stays abandoned until the owner decides.
