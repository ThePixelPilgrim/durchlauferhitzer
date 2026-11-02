# Stage 5 (containment, long runs, fuzzing) — review record

Plan: `docs/superpowers/plans/2026-10-29-stage-5-containment.md` (owner-approved `bc2f7d3`; the spec, `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md`, is binding where they differ). Range `bc2f7d3..` on `main`; the eight tasks landed one by one after their task reviews (`bc2f7d3..bea8f33`), the final review's fix wave on top. Every task was implemented by an Opus subagent (Task 1: Sonnet), reviewed by a separate Opus reviewer, fixed until approved (every finding, Minor included), gated on a fresh build and fast-forwarded onto `main` with CI green.

Gate at the end of the fix wave: 853 Rust tests (280 of them again under the seccomp kill switch), 42 Python tests, fmt, clippy `-D warnings`, the product-deps check and its self-test, the docs build with `-D warnings`; CI green on x86_64 and arm64, the root step included (both kill-switch settings).

## The tasks

| Task | What | Range on `main` |
|---|---|---|
| 1 | `containment` block, `ipv6_prefix` (FR-CFG-25, FR-CFG-20) | `bc2f7d3..21b80db` |
| 2 | IPv6 peers grouped by prefix, refused pool capped per group (FR-CONN-13) | `21b80db..9720d8d` |
| 3 | seccomp allow-list generator and install (FR-PROC-06) | `9720d8d..f9ee94e` |
| 4 | Landlock wrapper: read-only beneath the roots | `f9ee94e..a1b8393` |
| 5 | containment at startup (7a Landlock, 10a seccomp), `kind=containment` lines | `a1b8393..855b6ba` |
| 6 | the driver suite under `seccomp_action kill`; rare paths through the binary | `2bd6cc0..004408a` |
| 7 | nightly: long sim runs, four fuzz targets, Debian 12 under kill (NFR-TEST-08, NFR-SEC-13) | `004408a..a49097c` |
| 8 | bytes-then-reset backends, parser model for the drain oracle, nightly issues by fingerprint | `a49097c..bea8f33` |

Between tasks: `2bd6cc0` (`free_port` below the ephemeral range, the arm64 port race).

## Owner decisions taken during stage 5 (all in the spec)

- 2026-10-29 (brainstorm, builder check): FR-CFG-25 `off | best_effort | required` per layer, `seccomp_action fail | kill`; FR-CONN-13 settable prefix, default /64; Landlock ABI 1 counts as supported, gaps named with what covers them; seccomp checks numbers plus stable key arguments; strict sandbox now, stage 6 adapts (FR-PROC-06/07); nightly plus on demand (NFR-TEST-08).
- 2026-11-01 (Task 3): truncation before Landlock ABI 3 is a documented gap; `READ_IMPLIES_EXEC` makes seccomp partial.
- 2026-11-01 (Task 5): a layer the kernel offers but refuses to apply exits 2 in every mode.
- 2026-11-02 (Task 8): nightly failures file or update issues by a stable fingerprint.
- 2026-11-02 (final review): `setsockopt`/`getsockopt`/`futex` argument checks, plus `pthread_join`'s shared futex wait; Landlock ABI ≥ 6 scopes signals and abstract unix sockets; NAT64 `64:ff9b::/96` peers grouped by their embedded IPv4; the `LD_PRELOAD` clock shim is the second test-only `unsafe` site (NFR-MAINT-05); test-scope rulings and accepted gaps live in this record, not the spec.

## Rulings made on the owner's behalf

- Task 3: x86_64 table from `strace`, aarch64 written from it (proven in Task 6); startup-only calls left out; `clone3` answered `ENOSYS`; `newfstatat` replaced by a direct `SYS_fstat`.
- Task 4: raw syscalls and `repr(C)` structs per ABI; no network rules (ABI 4) in stage 5.
- Task 5: no seccomp user-notification supervisor for CI-skipped paths (its test-only `unsafe` costs more than it proves); the ABI < 5 `device_ioctl` and `READ_IMPLIES_EXEC` exit-2 paths are unit-tested only. "`READ_IMPLIES_EXEC` required on arm64" was reversed in `4f10659`: exec clears the flag on both arches.
- Task 6: the kill switch adds `containment { required, required, kill }` unless a test writes its own block.
- Task 7: actions bumped to Node 24 releases pinned by SHA; `REQUIRE_IPV6` stays off in the Debian container.
- Task 8: a reset right behind a queued head may cut the head (FR-PROXY-26 known behaviour); timed-out or cancelled nightly jobs file as `<job>|timeout-or-cancelled`, a manual cancel too (NFR-TEST-08).
- Final review: only the well-known NAT64 prefix is mapped (not `64:ff9b:1::/48`); older Landlock ABIs log no gap for the scopes (seccomp covers both).

## Final whole-stage review (at `bea8f33`)

Two Opus reviewers in parallel, read-only: (A) product code in `core/src` and `driver/src`, the sandbox as a whole; (B) tests, sim, fuzz, CI, nightly, scripts, docs and traceability. A traced a release build under `strace -f` and forced the clock onto its system call; B ran the full nightly long run (0..200000, 471 s) and every fuzz target for 120 s.

**Strengths.** Startup order exact (drop, `capset`, Landlock, no_new_privs, then threads; nothing opened by path after Landlock but `openat2` beneath the roots); TSYNC covers every thread; every listed escape route closed; BPF offsets, call tables and low-32-bit argument reads correct on both arches; decision logic matches FR-PROC-06 in every mode; `unsafe` sound; FR-CONN-13 accounting releases each count exactly once. Sim oracles still spec-written; the report job never runs code under test; every requirement pinned by a test.

**Findings.**
- A-1 (Critical): `clock_gettime` missing from both allow-lists — on hosts without a vDSO clock the default config crashes (`fail`) or is killed (`kill`) at startup.
- A-2 (Important): `setsockopt`/`getsockopt` allowed with any arguments.
- B-1 (Important): the timed-out-job ruling was in code, not the spec. B-2 (Important): the refuse-to-apply decision had no test. B-3 (Important): a record's `run_url` reached issue bodies unchecked. B-4 (Important): HANDOFF stale.
- Minors: the personality read even with seccomp off; the `O_PATH` metadata gap and unsymbolized backtraces undocumented; containment not self-contained; duplicated `cvt` helpers; 32-bit variadic arguments; the ledger's reversed arm64 ruling; test-scope rulings unrecorded; stale fuzz README and run estimates; `free_port` holding the whole ephemeral range; the empty-artifacts report path untested.
- Owner decisions requested: socket-option and futex checks, NAT64 grouping, Landlock scopes, where test-scope rulings go.

## Fix wave

Spec first (`d7ad1aa`). Two parallel implementers, combined by rebase onto `stage5-final`:
- Driver and core (wave A): `clock_gettime` with a real-binary regression test (a test-only Rust cdylib forces the system call under `kill`); the argument checks; Landlock scopes from ABI 6; NAT64 grouping in core and the sim model; the personality read on the apply path; refuse-to-apply tests for both layers; `containment::Error` and `containment/line.rs`; `fd::cvt_long`, `c_long` arguments; README gaps.
- Scripts and docs (wave B): issues link the report job's own run, record fields cleaned or code-spanned; the empty-artifacts path tested; `free_port` checks a sample; fuzz README and estimates.

**Re-reviews.** Wave B round 1: Needs fixes (bare URLs from records autolinked in headings; no size cap on panic text). Round 2: Approved. Wave A: Approved, with the shared `pthread_join` futex wait raised for the owner (ratified, spec `06ad85d`) and Minors fixed (the shim proven loaded via `/proc/<pid>/maps`; `unsafe_confined.rs` also checks `driver/tests`; README wording). CI then caught the root-test list check matching the new test's string; it now matches the `#[path]` include only.

## Accepted gaps

- Not run under `kill`: the disk timeout and a detached worker; `sched_yield`.
- `SECCOMP_FILTER_FLAG_LOG` is not checked by a test.
- The ABI < 5 `device_ioctl` and `READ_IMPLIES_EXEC` exit-2 paths are unit-tested only.
- The older glibc (Debian 12) is proven on x86_64 only; aarch64 not covered.
- CI runners have Landlock ABI 4: the ABI ≥ 6 scopes are proven only on newer kernels (this container, ABI 7).
- `RUST_BACKTRACE` under `kill` ends a panicking process by `SIGSYS`; `O_PATH` metadata stays visible to the server process (documented).
- No Landlock network rules (ABI 4): a compromised process may connect anywhere over TCP.
- Nit left as is: titles may carry inert text such as `GH-1` or a bare SHA (titles do not autolink or notify).

## Process notes

- Container restarts ended several subagents; work resumed from the ledger and the pushed branches.
- The worktree guard kept isolated implementers from writing reports into the main checkout; the controller saved them from the hand-backs.
- Branch deletion stays refused (HTTP 403); the owner deletes merged branches.

## Deferred to later stages

- Stage 6: restart under the strict sandbox (FR-PROC-07: no `execve`, no reads outside the roots); SIGHUP; every new system call needs a table entry on both arches and a binary test under the kill switch.
- Later: Landlock network rules (`CONNECT_TCP` to the backend ports, no `BIND_TCP`).
- Stage 7 and open items: as in the stage 4 record.
