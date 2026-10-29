# Handoff — where work stands and how to continue

Last updated 2026-10-29, after stage 5 Task 2 landed on `main`.
A new session should start from the latest `main`, read this file first, then `CLAUDE.md` (see its "Branches and main" rules).

## State

| Stage | Status |
|---|---|
| 1 Foundations | done (`ff5480a`) — record: `docs/superpowers/reviews/2026-10-25-stage-1-review.md` |
| 2 Static files | done (`576a77c`) — record: `docs/superpowers/reviews/2026-10-26-stage-2-review.md` |
| 3 Reverse proxy | done (`576a77c..b583d96`) — record: `docs/superpowers/reviews/2026-10-27-stage-3-review.md` |
| 4 epoll driver + binary | done (`7e1b18e..` this handoff) — record: `docs/superpowers/reviews/2026-10-29-stage-4-review.md` |
| 5 Containment | **in progress** — plan `docs/superpowers/plans/2026-10-29-stage-5-containment.md` (owner-approved `bc2f7d3`; decisions of 2026-10-29 in the spec: FR-CFG-25, FR-CONN-13, FR-PROC-06/07, NFR-SEC-13, NFR-PORT-04/07, NFR-TEST-08). Task 1 ✅ (`containment` block, `ipv6_prefix`; `NoEffect` config error); Task 2 ✅ (FR-CONN-13: IPv6 peers grouped by prefix, refused connections capped per group; the static net checks admission against a spec-written model); **next: Task 3** (seccomp allow-list generator and install) |

Requirements live in the spec with stable IDs (FR-…, NFR-…, DRV-…); plans, reviews and ledger lines cite those IDs instead of restating requirements, and new owner decisions go into the spec first. Open owner decisions are listed at the end of the spec.

Key documents: spec `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md` (binding), the driver contract in `core/src/core/contract.rs` (module docs), `README.md` (build, `--check`, running, systemd, signals), crate READMEs `core/README.md`, `sim/README.md`, `driver/README.md`, example config `examples/durchlauferhitzer.conf` (runs on 127.0.0.1:8080 without root; a test keeps it loadable and checks every key is documented).

The stage 4 review record lists every owner decision and ruling of stage 4, the final review, its fix wave and the accepted gaps.

## Where things stand after stage 4 (details in the spec and the stage 4 record)

- The binary `durchlauferhitzer` (crate `driver`, `libc` its only dependency; `unsafe` only in `driver/src/sys/`, pinned by `driver/tests/unsafe_confined.rs`) serves static files and proxies over epoll, follows the contract (DRV-01…16), logs one `key=value` line per entry on a non-blocking stderr (FR-LOG-05/06), starts up in the A11 order with systemd sockets or its own binds, drops to `user` and clears every capability (FR-PROC-03), checks the descriptor budget (NFR-RES-05), and drains on SIGTERM / aborts on a second SIGTERM, SIGINT or `shutdown_timeout` (FR-PROC-05).
- CI: x86_64 and arm64 run the same gate (fmt, clippy, tests with IPv6 required, deps check and self-test, docs with `-D warnings`); a root step runs the 7 server-starting test binaries under `sudo` with `DURCHLAUFERHITZER_REQUIRE_ROOT=1`; a list check keeps that step complete; the workflow token is read-only and `actions/checkout` is pinned by commit.
- Tests: an end-to-end smoke suite on the real binary; randomized sim nets that also drain and abort (`seed % 5 == 0`) and run late timers (`seed % 4 == 0`); the unanswered-request check runs on every static seed.
- This container boots with IPv6 disabled: IPv6 tests skip here and are required in CI. It runs as root, so root tests run here.

## Carry-forwards to later stages

- Stage 5: "bytes, then reset" backends in the randomized proxy net (`backend_reset_after` is used by directed tests only); IPv6 per-IP grouping by prefix; refused-slot pool fairness; long randomized runs; Landlock and seccomp (NFR-SEC-12); a parser model for the static net's drain oracle (today it accepts a 503 for a head the parser would reject).
- Stage 6: the restart target of FR-PROC-05; SIGHUP restart (FR-PROC-07).
- Stage 7: double buffering vs fill-drain-refill measured with `tc netem`; per-request buffer allocation; `next_deadline` O(n); parse/route allocation nits; `sendfile` blocking on evicted pages (DRV-09).
- Open: the driver-bug-close log kind is added with its first producer (FR-LOG-07); an NSS descriptor opened by the user lookup after the budget check is not counted (NFR-RES-05).
- Known test race: `free_port()` (bind, read, release) can collide with a parallel test; documented in `driver/tests/support/child.rs`.

## Branches

- `main` holds all verified work. Each stage 5 task is cut as `stage5-taskN` from `main` and fast-forwarded after review.
- Branch deletion is refused for this session (HTTP 403 from the git proxy): the owner deletes merged branches from their own clone.
- Fully contained in `main`, safe to delete when the owner says so: `claude/magical-dijkstra-2eoh4y`, `unify`, `stage3-task4` … `stage3-task9`, `stage3-final`, `stage4-plan`, `stage4-task1` … `stage4-task10`, `gate-docs`, `stage4-final`, `stage4-final-a`, `stage4-final-b`, `stage4-final-r2a`, `stage4-final-r2b` (the last four were rebased into `stage4-final`; their own commits are superseded, not lost).
- `ccr-716d91b0-bff174`: **abandoned, do not build on it.** A session that missed `claude/magical-dijkstra-2eoh4y` redid stage 3 Tasks 1–4 there; everything worth keeping was ported (stage 3 record, "Unification"); it has 12 commits not on `main` by design. The owner decides on deletion.

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
3. Dispatch an implementer (Opus for interface/security-heavy tasks, Sonnet for small ones) with: brief path, context files, controller rulings, the mandatory process block (TDD, gate `cargo fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked && scripts/check-product-deps.sh && RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`, commit trailers, push to the task branch), and a report file path.
4. `scripts/review-package <plan> BASE HEAD`, dispatch a reviewer (task-reviewer template), adjudicate; fix rounds resume the implementer, then a scoped re-review.
5. Run the gate on a fresh build (touch the sources), update this file, fast-forward `main`, ledger lines for rulings and completion; report to the owner.
