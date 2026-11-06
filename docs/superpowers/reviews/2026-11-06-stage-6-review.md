# Stage 6 (zero-downtime restart, musl) — review record

Plan: `docs/superpowers/plans/2026-11-04-stage-6-restart.md` (owner-approved `22fd0ce`; the spec, `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md`, is binding where they differ). Range `22fd0ce..` on `main`; the tasks landed one by one after their task reviews (`22fd0ce..778ba3f`), the final review's fix wave on top (`b210e76..a8e1305`). Every task was implemented by an Opus subagent (Task 1: Sonnet), reviewed by a separate Opus reviewer, fixed until approved (every finding, Minor included), gated on a fresh build and fast-forwarded with CI green.

Gate at the end of the fix wave: 928 Rust tests (338 of them again under the seccomp kill switch), 53 Python tests, fmt, clippy `-D warnings`, the product-deps check, the docs build with `-D warnings`; CI green on glibc x86_64 and arm64 and on Alpine (musl) x86_64 and arm64, the root step included; a dispatched nightly (restart soak on all four, Debian 12 with the restart tests) green.

## The tasks

| Task | What | Range on `main` |
|---|---|---|
| 1 | `restart_timeout` (FR-CFG-20) | `22fd0ce..ef30f04` |
| 2 | core restart drain (`DrainTarget`), sim restart plans | `ef30f04..0581a28` |
| 3 | SIGHUP starts the restart drain | `0581a28..d32d5db` |
| 4 | `SO_REUSEPORT` after a probe, overlap check, `--ready-fd` | `d32d5db..54a0d43` |
| 5 | reload helper, OpenRC script, lossless handover proven | `54a0d43..a02fd11` |
| 6 | musl on Alpine in CI, real OpenRC end to end | `a02fd11..14384e1` |
| 6b | steering against a kernel race | `c813268..3165d66` |
| 7 | statuses | `3165d66..778ba3f` |

## Owner decisions taken during stage 6 (all in the spec)

- 2026-11-04 (brainstorm): no connection may be lost; the HAProxy model (shared ports, kernel queue migration with `tcp_migrate_req`) over a supervisor or socket handoff; musl allowed (Alpine/OpenRC); requests during a restart drain served with `Connection: close`; its own `restart_timeout` (10m); a restart may change anything.
- 2026-11-05 (Task 6): musl's plain shared futex wait/wake; `tkill` only to the main thread (a worker's abort ending by SIGSEGV/SIGSYS is the one accepted exception); `libgcc_s` counts as system runtime (also CLAUDE.md).
- 2026-11-05 (Task 6 CI): fix the kernel race by steering (Task 6b); the Eyra spike after stage 6.
- 2026-11-06 (final review): SIGTERM during a restart drain turns it into the SIGTERM drain; `--ready-fd` is the join-and-take-over mode (documented); a non-root bind logs `kind=shared_ports`; a nightly restart soak on glibc and musl; the Debian 12 nightly runs the restart tests.

## Rulings made on the owner's behalf

- Brainstorm/plan: readiness comes from the new process on `--ready-fd` (after seccomp), the script signals the old one (the sandbox forbids signalling); the script, not the server, manages processes; a POSIX `sh` helper wrapped by OpenRC; dynamic musl linking (`-crt-static`); idle keep-alive connections kept during a restart drain.
- Task 2: a shutdown drain is final (a later restart call is ignored); a connection whose keep-alive response was queued before the drain becomes idle once written.
- Task 4: spec status tags flipped at stage end, not per task.
- Task 6: `socket2` as a test-only dependency for the dual-stack test socket.
- Task 6b: steer a starting process's ports to the oldest member (the running server), to itself right before `ready`, back on a late failure.
- Final review: a changed `listen` address is documented, not refused; the helper SIGKILLs a new process only when its SIGHUP is still pending after 10 s (it never reached its loop, accepted nothing; its queue migrates); the lock is a file created with noclobber.

## Final whole-stage review (at `778ba3f`)

Two Opus reviewers in parallel, read-only: (A) product code and the shipped scripts, with strace on live reloads and hand-made crashes; (B) tests, sim, CI, docs and traceability, with mutations, reruns and a 20 000-seed sim run.

**Strengths.** The running server gains no system call for restarts; steering before the filter and `ready` after it (strace); no descriptor leaks; the musl entries are the narrowest; the core drain is exhaustive and final; every owner decision has a pinning test; the storm and queued-connection tests and a deterministic migration-off control; the root-run scripts resist local unprivileged users.

**Findings.** 0 Critical.
- A-I1 (Important, reproduced): a new process dying during the grace still got the old one SIGHUPed — a total outage reported as success.
- B-I1 (Important): nothing caught `ready` written before the seccomp filter. B-I2 (Important): the musl thread-start invariant (`membarrier`) lived only in a test comment.
- Minors: a blunt stop of a new process that may serve; orphans after a killed helper; a changed `listen` losing queued connections; a stale lock; an early give-up in the listener wait; tick-counted timeouts; `RUSTFLAGS` overriding the musl setting; the grace and a lone keep-alive twin unpinned; a skip that should fail under `REQUIRE_ROOT`; an added address and a changed `user` untested; no `check` job timeout; the nightly Debian job skipping the restart tests; doc drift.
- Owner decisions requested: SIGTERM in a restart drain, `--ready-fd` under supervisors, same-uid exposure, a nightly soak, restart tests on Debian 12.

## Fix wave

Two parallel implementers, combined by rebase onto `stage6-final`:
- Product (wave A): the helper re-checks the new process before the SIGHUP (and after), stops an unhanded new one with SIGHUP, lists every process in `<pidfile>.draining` from its start, takes over stale locks, measures time; SIGTERM turns a restart drain into the SIGTERM drain; `kind=shared_ports`; tests for `ready` after seccomp, the grace, a lone keep-alive, an added address and a changed `user`.
- CI/docs (wave B): the restart soak job (x86_64/arm64 × glibc/musl) filing issues per test; restart tests in the Debian 12 job; a `check` timeout; per-matrix-leg reporting of timed-out legs; README updates; a sim counter for the never-reached restart branch.

**Re-reviews.** Wave B: Approved, then four Minors fixed (strict soak shells, per-leg reporting, fingerprint tests, a stale comment). Wave A: Needs fixes — the guarded stale-lock takeover let two helpers hold the lock (32/200). Root cause found on CI: the runner's `mkdir` (uutils coreutils 0.10) is not atomic, so neither a directory lock nor a directory guard is exclusive there; the lock is now a file created with noclobber (`O_EXCL`), proven by a race test with a deliberately non-atomic `mkdir` on PATH. Plus six Minors (exit 1 on every path that leaves nothing serving, list writes checked, the holder's start time, docs).

## Root causes found on the way

- **Kernel race (Task 6 CI):** a handshake completing on a listener at the instant it closes is reset (`inet_child_forget`) instead of migrated; `tcp_migrate_req` does not cover it. Reproduced and traced; fixed by steering plus a grace (storm test 5/53 lost before, 0/100 after). Residual window documented in FR-PROC-07.
- **Test ports (Task 6b):** "refused" ports inside the ephemeral range let a connect reach itself; a port probe inherited by a child forked by another test thread kept listening until that child's `exec`. Ports are now picked below the ephemeral range from `/proc/net/tcp{,6}` without any probe socket.
- **Nightly issue #2:** `ldd --version | head -n1` under `pipefail` (SIGPIPE, exit 141); fixed and the issue closed.

## Accepted gaps

- The storm test is probabilistic (about 1 run in 10 caught the race before steering); the deterministic guard is `ready_steers_…`; the nightly soak raises the odds.
- The grace has no loopback test of its purpose (its timing is pinned).
- The helper's SIGKILL path (SIGHUP still pending after 10 s) has no test: no deterministic way to stick a process after its signal mask.
- A new process the helper must stop while it ignores SIGHUP (helper run under `nohup`) is left to OpenRC `stop`.
- The wait for the old listeners needs root with `CAP_SYS_PTRACE`; without it, a warning and no wait.
- The helper cannot close descriptors 10 and up (POSIX `sh`).
- A changed (not exactly kept) `listen` address can lose queued connections if the new process fails.
- The proxy net's "restart: closed before a request" branch never occurs in 200 000 seeds (reported, not gated).
- Under musl, a worker's abort ends by SIGSEGV/SIGSYS (owner-accepted).

## Process notes

- Worktree isolation kept implementers from writing reports into the main checkout; the controller saved them from the hand-backs.
- Several CI failures were root-caused rather than rerun: the kernel race, the self-connect, two fork races, the non-atomic `mkdir`, the SIGPIPE flake.
- A transient GitHub push error ("fatal error in commit_refs") was retried with backoff.

## Deferred

- Stage 7: N driver threads must start before the seccomp filter (musl `membarrier`); steering assumes one listener per process per port group — redesign the 0/1 indices for N reuseport threads; plus the existing stage 7 list.
- After stage 6 (owner): the Eyra spike — the musl allow-list entries, the `libgcc_s` NEEDED pins, `-crt-static` and the clock shim all assume a system libc.
- Later: Landlock network rules; reporting the kernel race to netdev.
