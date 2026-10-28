# Handoff — where work stands and how to continue

Last updated 2026-10-28, after stage 4 Task 5 landed on `main`.
A new session should start from the latest `main`, read this file first, then `CLAUDE.md` (see its "Branches and main" rules).

## State

| Stage | Status |
|---|---|
| 1 Foundations | done (`ff5480a`) — record: `docs/superpowers/reviews/2026-10-25-stage-1-review.md` |
| 2 Static files | done (`576a77c`) — record: `docs/superpowers/reviews/2026-10-26-stage-2-review.md` |
| 3 Reverse proxy | done (`576a77c..b583d96` + this handoff) — record: `docs/superpowers/reviews/2026-10-27-stage-3-review.md` |
| 4 epoll driver + binary | **in progress** — plan `docs/superpowers/plans/2026-10-27-stage-4-epoll-driver.md` (owner-approved, amendments A1–A11). Task 1 ✅ (config bounds, memory budget, `disk_threads`, `shutdown_timeout`, `memory_limit`); Task 2 ✅ (core: `Date`, drain/abort API, FR-LOG-07 fields); Task 3 ✅ (sim late-timer mode, `TimerOrder`); Task 4 ✅ (`driver` crate, package `durchlauferhitzer`, safe `sys` wrappers; `libc` only); Task 5 ✅ (epoll event loop for clients and upstreams; `Driver::run -> Exit`, `StopHandle`); **next: Task 6** (disk worker pool and `sendfile`) |

Requirements live in the spec with stable IDs (FR-…, NFR-…, DRV-…); plans, reviews and ledger lines cite those IDs instead of restating requirements, and new owner decisions go into the spec first. Open owner decisions are listed at the end of the spec.

Key documents: spec `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md` (binding), the driver contract in `core/src/core/contract.rs` (module docs; the stage 4 driver follows it exactly), crate READMEs `core/README.md`, `sim/README.md`, example config `examples/durchlauferhitzer.conf` (a test keeps it loadable and checks every key is documented).

The stage 3 review record lists every owner decision, every ruling taken on the owner's behalf, the final review and its fix wave.

## Before or during the stage 4 plan

- FR-CFG-21 is decided (owner, 2026-10-27): per-key caps plus a `memory_limit` budget over each connection's own buffers; an unset `max_connections_per_ip` defaults to min(64, `max_connections`).
- The driver must implement the contract in `core/src/core/contract.rs` exactly: client and upstream readiness, `Interest::ReadWrite` (upstreams only), reading an upstream's readable bytes before reporting its socket error, deferring errors while the interest is `None`, connect answers always as a later event, the file contract, and the descriptor budget (NFR-RES-05: startup check of `RLIMIT_NOFILE`).
- Backend addresses are IP literals only (no DNS); unspecified, broadcast and multicast are rejected at config load.
- Drain and abort (owner, 2026-10-27; spec FR-PROC-05b–05d): during a drain a complete pipelined request behind a response whose head was queued keep-alive before the drain gets 503 + close; `abort_all` logs `Rejected { ShuttingDown }` per connection with an exchange in flight. Controller ruling (awaiting owner confirmation, then relabel in FR-PROC-05b): a request pipelined behind a head rendered during the drain (file opening, backend connecting/awaited) is not answered, since that head says close (A4).
- Late wakes (owner, 2026-10-27; DRV-06): delivery order decides — an event handled before `TimerFired` counts even past its deadline; every deadline may be overrun by as long as the loop was late. The sim's `TimerOrder::Late` pins both orders (`sim/tests/late_timers.rs`); the randomized nets run seeds with `seed % 4 == 0` late.
- IPv6 listeners always set `IPV6_V6ONLY` (owner, 2026-10-27; FR-CFG-10 note, A6). This container boots with IPv6 disabled: the IPv6 tests skip here and are required in CI (`DURCHLAUFERHITZER_REQUIRE_IPV6=1`).
- Lints: one workspace table (`unsafe_code = "deny"`, `undocumented_unsafe_blocks`, `multiple_unsafe_ops_per_block`); core, sim and the driver's `main.rs` forbid unsafe at their roots; `unsafe` only in `driver/src/sys/`.
- Deps check (NFR-MAINT-04 done): workspace crates by path, all targets, allow-listed names only from the registry; offline self-test `scripts/test-check-product-deps.sh` runs in CI.
- Accept pause (owner, 2026-10-28; DRV-16): any non-transient accept failure pauses all listeners until the driver closes any descriptor or 1 s passes; at most one log line per second.
- Per-socket `epoll_ctl` failure (owner, 2026-10-28; DRV-15): fails only that socket (`Failed`/`UpstreamFailed` + `DriverLog::RegisterFailed`); only `epoll_wait` or the loop's own descriptors end `run`.
- Carry to Task 6: the pool gets the loop's wake eventfd via `EventFd::try_clone` (std `OwnedFd::try_clone`, no new unsafe; one descriptor in the NFR-RES-05 budget); drop `Driver::new`'s "static roots need a pool" guard once the pool exists; the sim checks only `Read` (not `Write`) with an empty buffer for clients — relevant once `send_file` is driven. Task 8's process exit enum is `ExitStatus` (plan amended, e40c7e3). Carry to Task 8: the A11 probe opens `/` with `O_PATH` when no root is configured; `SignalFd::new` before worker threads spawn; `drop_to` tested in a subprocess.
- Process notes: the `Date` sim tests (Task 2, 66a8f77) and the first late-timer tests (Task 3, ddc859f) were written after the code; their teeth were shown by mutation.
- Open owner questions: should the pre-commit gate in CLAUDE.md also run `scripts/test-check-product-deps.sh` (CI runs it)? Add `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` to the gate? (A private doc link from Task 1 slipped through; fixed in 1c8257f.)

## Carry-forwards to later stages

- Stage 4/5: draw "bytes, then reset" backends in the randomized proxy net (`backend_reset_after` is used by directed tests only).
- Stage 5: IPv6 per-IP grouping by prefix; refused-slot pool fairness; long randomized runs.
- Stage 7: double buffering vs fill-drain-refill measured with `tc netem`; per-request buffer allocation; `next_deadline` O(n); parse/route allocation nits.

## Branches

- `main` holds all verified work. Each plan task is fast-forwarded to `main` once it passes its review and the gate (CLAUDE.md, "Branches and main").
- Fully contained in `main`, safe to delete when the owner says so: `claude/magical-dijkstra-2eoh4y`, `unify`, `stage3-task4` … `stage3-task9`, `stage3-final`.
- `ccr-716d91b0-bff174`: **abandoned, do not build on it.** A session that missed `claude/magical-dijkstra-2eoh4y` redid stage 3 Tasks 1–4 there from the pre-consolidation spec. Everything worth keeping was ported (see the stage 3 review record, "Unification"); it has 12 commits not on `main` by design. The owner decides on deletion.

## Owner's way of working (mandatory, see CLAUDE.md)

- **Subagent-driven development** (`.claude/skills/subagent-driven-development`), one plan task at a time; **stop and report after each task**, wait for "Go". Tiny doc-only changes may be done inline.
- **Fix every finding as soon as possible** (owner, 2026-10-26): Minor findings go into the task's own fix loop instead of being deferred.
- Strict red/green TDD; implementers write the failing test first and paste real RED output; tests that pin existing behaviour are proven by a temporary mutation (then `touch` the sources so cargo rebuilds).
- Commit **and push** after every step; report the pushed **SHAs** and the **GitHub URL of every changed Markdown file** (`https://github.com/ThePixelPilgrim/durchlauferhitzer/blob/main/<path>`).
- Commit trailer lines (from the session's attribution reminder) end every commit message.
- Fast-forward `main` after every task that passes review and the gate (standing permission, 2026-10-26); never rewrite `main`.
- Pure Rust, zero product dependencies, no `unsafe`, exhaustive enums (no `_ =>` on enums), enums over bools, no `too_many_arguments` allow in core, bounded memory, illegal states unrepresentable, files under 900 lines. Fable must not implement (delegate to Opus/Sonnet).
- The owner prefers asking before inventing; give a recommendation, list every decision taken on their behalf ("calls I made") in the report.

## How a task is run

1. Workspace: `bash .claude/skills/subagent-driven-development/scripts/sdd-workspace <plan>` (git-ignored `.superpowers/sdd/<plan>/`), with its ledger `progress.md`.
2. Record `Task N: BASE <sha>`; `scripts/task-brief <plan> N` writes the brief.
3. Dispatch an implementer (Opus for interface/security-heavy tasks, Sonnet for small ones) with: brief path, context files, controller rulings, the mandatory process block (TDD, gate `cargo fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked && scripts/check-product-deps.sh`, commit trailers, push to the task branch), and a report file path.
4. `scripts/review-package <plan> BASE HEAD`, dispatch a reviewer (task-reviewer template), adjudicate; fix rounds resume the implementer, then a scoped re-review.
5. Run the gate on a fresh build (touch the sources), update this file, fast-forward `main`, ledger lines for rulings and completion; report to the owner.
