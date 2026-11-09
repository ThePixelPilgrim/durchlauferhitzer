# Handoff — where work stands and how to continue

Last updated 2026-11-09, after the stage 8 review.
A new session should start from the latest `main`, read this file first, then `CLAUDE.md` (see its "Branches and main" rules).

## State

| Stage | Status |
|---|---|
| 1 Foundations | done (`ff5480a`) — record: `docs/superpowers/reviews/2026-10-25-stage-1-review.md` |
| 2 Static files | done (`576a77c`) — record: `docs/superpowers/reviews/2026-10-26-stage-2-review.md` |
| 3 Reverse proxy | done (`576a77c..b583d96`) — record: `docs/superpowers/reviews/2026-10-27-stage-3-review.md` |
| 4 epoll driver + binary | done (`7e1b18e..` this handoff) — record: `docs/superpowers/reviews/2026-10-29-stage-4-review.md` |
| 5 Containment | done (`bc2f7d3..` this handoff) — record: `docs/superpowers/reviews/2026-11-02-stage-5-review.md` |
| 6 Restart | done (`22fd0ce..` this handoff) — record: `docs/superpowers/reviews/2026-11-06-stage-6-review.md` |
| 7 Efficiency | done (`71d3867..` this handoff) — record: `docs/superpowers/reviews/2026-11-07-stage-7-review.md`. The Eyra spike: `docs/superpowers/spikes/2026-11-06-eyra.md` — no, revisit later |
| 8 Backend-directed file serving | done (`862e951..` this handoff) — record: `docs/superpowers/reviews/2026-11-09-stage-8-review.md` |
| next | **the owner chooses**: spec Timeline item 9 ("Later, maybe": hung-disk handling) or 10 (caching), or an item from the deferred lists (TLS, Landlock network rules, multipart ranges) |

Requirements live in the spec with stable IDs (FR-…, NFR-…, DRV-…); plans, reviews and ledger lines cite those IDs instead of restating requirements, and new owner decisions go into the spec first. Open owner decisions are listed at the end of the spec.

Key documents: spec `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md` (binding), the driver contract in `core/src/core/contract.rs` (module docs), `README.md` (build, `--check`, running, systemd, signals), crate READMEs `core/README.md`, `sim/README.md`, `driver/README.md`, example config `examples/durchlauferhitzer.conf` (runs on 127.0.0.1:8080 without root; a test keeps it loadable and checks every key is documented).

Each stage's review record lists its owner decisions, rulings, final review, fix wave and accepted gaps.

## Where things stand after stage 8 (details in the spec and the stage records)

- The binary `durchlauferhitzer` (crate `driver`, `libc` its only dependency; `unsafe` only in `driver/src/sys/`, pinned by `driver/tests/unsafe_confined.rs`) serves static files and proxies over epoll, follows the contract (DRV-01…16), logs one `key=value` line per entry on a non-blocking stderr (FR-LOG-05/06), starts up in the A11 order with systemd sockets or its own binds, drops to `user` and clears every capability (FR-PROC-03), checks the descriptor budget (NFR-RES-05), and drains on SIGTERM / aborts on a second SIGTERM, SIGINT or `shutdown_timeout` (FR-PROC-05).
- CI: x86_64 and arm64 run the same gate (fmt, clippy, tests with IPv6 required, deps check and self-test, docs with `-D warnings`); a root step runs the 12 server-starting test binaries (`SERVER_TESTS` in `ci.yml`) under `sudo` with `DURCHLAUFERHITZER_REQUIRE_ROOT=1`; a list check keeps that step complete; the workflow token is read-only and `actions/checkout` is pinned by commit.
- Tests: an end-to-end smoke suite on the real binary; randomized sim nets that also drain and abort (`seed % 5 == 0`) and run late timers (`seed % 4 == 0`); the unanswered-request check runs on every static seed.
- Containment (stage 5): Landlock (read-only beneath the roots; ABI ≥ 6 also scopes signals and abstract unix sockets) and a seccomp allow-list for x86_64 and aarch64 (`driver/src/containment/filter.rs`), each `off | best_effort | required` (FR-CFG-25, FR-PROC-06). The whole driver suite also runs under `seccomp_action kill` (`DURCHLAUFERHITZER_TEST_SECCOMP_KILL=1`) in CI on both arches and in the root step. IPv6 peers are grouped by `ipv6_prefix`, NAT64 peers by their IPv4 (FR-CONN-13).
- Nightly (`.github/workflows/nightly.yml`, 03:17 UTC and by hand): long sim runs, four fuzz targets, the suite under kill on Debian 12; failures file `nightly` issues by fingerprint.
- Restart (stage 6): SIGHUP starts the restart drain (served with `Connection: close`, `restart_timeout`); without systemd `contrib/durchlauferhitzer-reload` (OpenRC `reload`) starts a `--ready-fd` process on shared ports (`SO_REUSEPORT`, steered with a classic-BPF reuseport program), waits for `ready` and a grace, then SIGHUPs the old one; `tcp_migrate_req` moves queued connections. musl builds and tests run on Alpine in CI (x86_64, arm64); a nightly restart soak runs on glibc and musl.
- Efficiency (stage 7): `bench/` (std-only, excluded crate) measures CPU and system calls per request; the nightly publishes rows to the `benchmarks` branch, charted in its `BENCHMARKS.md` (Mermaid), and `bench/bisect.sh` finds a regressing commit. Conditional requests (FR-STATIC-18), `MSG_MORE` before files, a deadline index, backend pooling with a silence check and a safe GET/HEAD retry (FR-PROXY-02), eager I/O with deferred epoll registrations, a 32 KiB upstream buffer. NFR-PERF targets ratchet only towards better; an exception needs the owner's OK; security trumps performance.
- Backend-directed files (stage 8): a proxy route with `internal_files x-sendfile <root>;` or `internal_files x-accel-redirect <prefix> <root>;` serves the file a backend's 200 names (FR-PROXY-32), through the static file path and the same jail; the file headers are stripped both ways and Rack's hints set; single-range requests (206/416, `If-Range`) for static and backend files (FR-STATIC-19); bench `proxy-file`.
- This container boots with IPv6 disabled: IPv6 tests skip here and are required in CI. It runs as root, so root tests run here.

## Carry-forwards to later stages

- After the stage 7 merge: check that the first scheduled nightly publish logs `refused 0`, and compare CPU against the NFR-PERF-01/02 targets over the next two nightlies (bisect if it stays high).
- Stage 8 carry-forwards: the `proxy-file` NFR-PERF target (owner, from its first nightly); multipart ranges (`multipart/byteranges`) still not served (a several-range request gets the whole file); nginx's other `X-Accel-*` features (expiry, buffering, rate limits, named locations) unsupported.
- Efficiency, later: N driver threads only if the loop nears 50 % of a core (they must start before the seccomp filter, musl `membarrier`; steering's 0/1 indices assume one listener per process per port group); answering a silence check without deregistering; reusing buffers without zeroing; a syscall regression alarm; parse/route allocation nits; `sendfile` blocking on evicted pages (DRV-09).
- Later: TLS (pure Rust); Landlock network rules (ABI 4: `CONNECT_TCP` to the backend ports, no `BIND_TCP`); report the reuseport close race to netdev.
- Open: the driver-bug-close log kind is added with its first producer (FR-LOG-07); an NSS descriptor opened by the user lookup after the budget check is not counted (NFR-RES-05).
- Nightly issue reporting (NFR-TEST-08, owner 2026-11-02): `scripts/nightly-report.py` (Python stdlib, tested in CI) files/updates issues labelled `nightly` keyed `[fp-xxxxxxxx]`. Known limits: a panic located in std/`registry` keys on that location alone (no backtraces in the nightly); if one arch's long-run job fails with a record while the other times out, only the recorded failure is filed. Issue #1 was the deliberate end-to-end test (closed, not planned).
- Accepted gaps (stage 5 record): CI runners have Landlock ABI 4, so the ABI ≥ 6 scopes are proven only on newer kernels; Debian 12 under kill is x86_64 only; the disk timeout and a detached worker are not run under kill.
- Maintenance: the fuzz job's nightly toolchain is pinned to a date (`nightly.yml`) and needs a bump by hand now and then; the first full-budget scheduled nightly on `main` is the real check of its durations (estimate: long runs ~8 min, both arches in parallel on 4 cores, plus the build, fuzz ~45 min).

## Branches

- `main` holds all verified work. Each task is cut as a short-lived branch from `main` and fast-forwarded after review.
- Branch deletion is refused for this session (HTTP 403 from the git proxy): the owner deletes merged branches from their own clone.
- Fully contained in `main`, safe to delete when the owner says so: `claude/magical-dijkstra-2eoh4y`, `unify`, `stage3-task4` … `stage3-task9`, `stage3-final`, `stage4-plan`, `stage4-task1` … `stage4-task10`, `gate-docs`, `stage4-final`, `stage4-final-a`, `stage4-final-b`, `stage4-final-r2a`, `stage4-final-r2b` (the last four were rebased into `stage4-final`; their own commits are superseded, not lost), `stage5-task1` … `stage5-task8`, `stage5-final`, `stage5-final-a`, `stage5-final-b`, `stage5-task8-report` (the last three were rebased into `stage5-task8` / `stage5-final`; `git cherry` shows their commits on `main`), `stage6-task1` … `stage6-task6b`, `stage6-design`, `stage6-plan`, `stage6-task6b-plan`, `stage6-closeout`, `stage6-final`, `stage6-final-a`, `stage6-final-b` (the last two were rebased into `stage6-final`). The stage 7 task branches (`stage7-*`) are contained in `main` too, except **`stage7-task8-buf64`**: a scratch measurement of a 64 KiB buffer, never to be merged (its deletion was refused by the proxy); the owner deletes it.
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
