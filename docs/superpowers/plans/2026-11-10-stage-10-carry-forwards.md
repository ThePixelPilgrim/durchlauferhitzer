# Stage 10: Carry-forwards — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (mandatory in this repo) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. In this repo, `superpowers:<name>` is the project skill `<name>`.

**Goal:** Close the known gaps the stage 9 record and the qmail review left: the gate sees contention and the newest libcs, a hardened install needs no NSS, product size is measured, OpenRC installs are supervised, and the oversized test file is split.

**Architecture:** Five independent, small tasks; none changes the request path. Two add nightly jobs modelled on the existing `older-glibc` job; one extends the config's `user` value and the privilege drop; one adds a counting script and a chart series beside the benchmarks; one touches `contrib/openrc`; one splits a test file.

**Tech Stack:** Rust (stable, pinned), `libc` only; bash and Python 3 stdlib for scripts; GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md` (binding): NFR-TEST-08 (the `pressure` and `newest libc` jobs), FR-PROC-03 (`user <uid>:<gid>`), NFR-MAINT-12 (product size), FR-PROC-07a (OpenRC supervision), NFR-MAINT-10 (files under 900 lines); Timeline item 10. Owner decisions all dated 2026-11-10.

## Global Constraints

- **No C; `libc` only; no `unsafe` outside `driver/src/sys/`.** Task 3 touches `driver/src/sys/user.rs` only if a new wrapper is needed (it should not be: parsing two decimals needs none).
- **Enums over bools; exhaustive matches; files under 900 lines; crate READMEs current; the spec's status flipped to `[done]` with *Planner calls (stage 10 plan)* in each task's commit.**
- **Strict red/green TDD** for code; scripts get a test script in the house style (`scripts/test-check-product-deps.sh`); workflow changes are verified by `workflow_dispatch` runs or, where the nightly cannot be dispatched for one job, by reading the run after the next scheduled nightly (the task is done when that run is green).
- **Gate before every commit:** `cargo fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked && scripts/check-product-deps.sh && RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`; for driver changes also `DURCHLAUFERHITZER_TEST_SECCOMP_KILL=1 cargo test -p durchlauferhitzer --locked` and the static musl target.
- **CI of `main` is part of done** (stage 9 lesson): a task is complete when the CI run of its landing is green, and for nightly jobs when the job has run green once.

## Review Focus

1. **The pressure job's false-failure rate** (Task 1): the backstop tests under pressure passed 12 of 12 locally; on a 4-core runner with 8 busy loops a test that needs more than its timeouts fails for the wrong reason. The job must use the long timeouts the survive tests already have and report which test timed out; three green dispatched runs before it is declared done.
2. **`user 0:0` and `user 65534:0`** (Task 3) are exit 2 with the same messages as the name forms; `user 65534:65534` drops exactly as the name would (the privilege tests' assertions on uids and gids, run as root).
3. **A line count that counts tests** (Task 4): the rule must exclude `#[cfg(test)]` modules inside product files, not only `tests.rs` files and `tests/` directories; the test pins a file with an inline test module.
4. **Supervision versus the reload hand-over** (Task 5): under `supervise-daemon`, the old process exits after its drain and the new one must not be "respawned" twice; the restart test under OpenRC (`contrib/test-openrc.sh`) runs with supervision on.
5. **The split keeps every test** (Task 6): the test count before and after is equal, and every test keeps its name.

---

### Task 1: The `pressure` nightly job (NFR-TEST-08)

**Files:**
- Modify: `.github/workflows/nightly.yml` (a new job after `older-glibc`), `scripts/nightly-report.py` only if a job name needs registering (check `record --job`), `docs/superpowers/HANDOFF.md` nightly notes, the spec (NFR-TEST-08 status note).

**Interfaces:**
- Produces: job `pressure` (name `pressure (x86_64)`), `runs-on: ubuntu-24.04`, the same checkout and toolchain steps as `long-runs`; a step that starts `2 × nproc` busy loops (`sh -c 'while :; do :; done'` each, started with `&`, killed in an `if: always()` step) and then runs `DURCHLAUFERHITZER_TEST_SECCOMP_KILL=1 cargo test -p durchlauferhitzer --locked -- --nocapture 2>&1 | tee pressure.log` with `set -o pipefail`; the failure record step as in `older-glibc` with `--job pressure`; `timeout-minutes` 40.
- Root: the job runs as the runner user with `sudo` like ci.yml's root step only if the implementer finds the backstop's 65536-connection test needs it; otherwise not root (the test skips without `REQUIRE_ROOT`, and the pressure job's purpose is contention, not root paths).

- [ ] **Step 1:** Write the job; validate the YAML (`python3 -I -c 'import yaml,sys; yaml.safe_load(open(sys.argv[1]))' .github/workflows/nightly.yml`).
- [ ] **Step 2:** Commit, push the task branch, and dispatch the nightly on that branch with `workflow_dispatch` restricted to the new job if the workflow supports an input for it; if it does not, add an input `only_job` handled by `if:` on every job (small, keep it), and dispatch with `only_job=pressure`. Three green runs; paste their links and the busy-loop count the log shows.
- [ ] **Step 3:** Spec: NFR-TEST-08's 2026-11-10 sentence gets `[done for pressure]`; handoff nightly notes.
- [ ] **Step 4:** Gate (unchanged code; run it anyway), commit, push.

### Task 2: The `newest libc` nightly legs (NFR-TEST-08)

**Files:**
- Modify: `.github/workflows/nightly.yml` (job `newest-libc`, matrix `glibc` and `musl`), the spec, the handoff.

**Interfaces:**
- Produces: a matrix job `newest libc (glibc)` on a rolling glibc image (builder picks, e.g. `fedora:latest`; must have a glibc newer than the runner's 2.39) built like `older-glibc` (container with the same `options`, `--ulimit nofile=1048576:1048576`, rustup install, `DURCHLAUFERHITZER_REQUIRE_ROOT=1 … TEST_SECCOMP_KILL=1 cargo test -p durchlauferhitzer`), and `newest libc (musl)` on `alpine:edge` built like ci.yml's Alpine job (a `docker run` with the same flags). The `Versions` step prints the libc version; the failure record uses `--job newest-libc --arch x86_64`.

- [ ] **Step 1:** Write both legs; validate the YAML.
- [ ] **Step 2:** Dispatch on the task branch (as Task 1 arranged); one green run per leg; paste the libc versions printed.
- [ ] **Step 3:** Spec and handoff; gate; commit; push.

### Task 3: `user <uid>:<gid>` (FR-PROC-03)

**Files:**
- Modify: `core/src/config/model.rs` (`Config.user: Option<String>` → `Option<User>` with `enum User { Name(String), Ids { uid: u32, gid: u32 } }`), `core/src/config/validate/mod.rs` (~line 553: parse `<decimal>:<decimal>` into `Ids`, else `Name`; `0` in either place is a config error at the setting: `user 0:0: the server never runs as root after binding`), `core/src/config/validate/tests/mod.rs` (new cases), `driver/src/process/privileges.rs` (`drop_privileges(user: Option<&User>)`: `Ids` skips `lookup_user`, keeps the gid-0 check, and logs `kind=privileges action=dropped user=<uid>:<gid> form=ids` vs `form=name`), `driver/tests/privileges.rs` (root-only: `user 65534:65534` drops exactly as `user nobody` does; `user 65534:0` and `user 0:65534` are exit 2 with the messages), `driver/tests/cli.rs` (`--check` of a config with `user 1000:1000` prints `ok`), `README.md` "Privileges" and "Guaranteed install" (recommend the numeric form for a hardened install: no NSS module runs as root), `examples/durchlauferhitzer.conf` (the comment on `user`), the spec (FR-PROC-03 status, FR-LOG-05 if the log line gains a token).

**Interfaces:**
- Consumes: `lookup_user(name) -> io::Result<Option<(u32, u32)>>` (`driver/src/sys/user.rs`); the existing privilege-drop sequence.
- Produces: `pub enum User { Name(String), Ids { uid: u32, gid: u32 } }` in `core/src/config/model.rs`, `Display` giving the written form.

- [ ] **Step 1:** Failing core tests: `user_accepts_numeric_ids` (`user 1000:1000;` → `Ids{1000,1000}`), `user_refuses_root_ids` (`0:1000`, `1000:0`, `0:0` → the error at the setting), `user_name_still_a_name` (`user nobody;` → `Name`), `user_rejects_malformed_ids` (`1000:`, `:1000`, `1000:1000:1`, `+1:1`, `99999999999:1` → invalid value).
- [ ] **Step 2:** RED; implement; GREEN.
- [ ] **Step 3:** Failing driver tests as listed (root-only ones run here: this container is root); RED by expecting the new log token; implement; GREEN; kill-mode and musl runs.
- [ ] **Step 4:** README, example config, spec (FR-PROC-03 `[done]`, planner calls: the enum, the log token, the exact error messages); gate; commit; push.

### Task 4: Product size beside the benchmarks (NFR-MAINT-12)

**Files:**
- Create: `scripts/product-lines.py` (Python 3 stdlib: walks `core/src` and `driver/src`, skips any `tests.rs`, any path with a `tests/` or `test_support` component, and inside remaining `.rs` files drops `#[cfg(test)]` modules (from the attribute line to the matching closing brace of the `mod` that follows), then counts non-blank lines that are not `//` or `///`/`//!` comments; prints `<count>` or, with `--csv --commit <sha> --date <d>`, one row), `scripts/test_product_lines.py` (fixtures with an inline test module, a `tests.rs`, comments, blank lines), CI step next to the other script tests.
- Modify: `.github/workflows/nightly.yml` `bench` job (a step that writes `product-lines.csv` next to `bench.csv` and uploads it in the same artifact), the publish job and `scripts/bench-chart.py` (a new series `product-lines` in `results.csv`'s schema or a sibling CSV; a chart `Product source lines` on `BENCHMARKS.md`; the chart script's tests `scripts/test_bench_chart.py` extended), `bench/README.md` if it documents the rows, the spec (NFR-MAINT-12 `[done]` with the counting rule as implemented), the stage 9 record is not touched (its delta is stated from the next record on).

**Interfaces:**
- Consumes: `scripts/bench-chart.py`'s row schema and `--append` flow; the `bench-results` artifact.
- Produces: the `product-lines` series; the counting rule, pinned by `scripts/test_product_lines.py`.

- [ ] **Step 1:** Failing Python tests for the counter (fixtures above; expected counts computed by hand in the test).
- [ ] **Step 2:** RED; implement; GREEN; run the counter on the repo and paste the number.
- [ ] **Step 3:** Failing chart-script tests for the new series; RED; implement; GREEN.
- [ ] **Step 4:** Workflow steps; YAML validated; a dispatched bench run on the task branch shows the CSV in the artifact (publishing happens only from `main`, so the chart appears after the landing's first scheduled nightly: note it in the report).
- [ ] **Step 5:** Spec, READMEs, CI step; gate; commit; push.

### Task 5: OpenRC supervision (FR-PROC-07a)

**Files:**
- Modify: `contrib/openrc/durchlauferhitzer`, `contrib/openrc/durchlauferhitzer.confd`, `contrib/README.md`, `contrib/test-openrc.sh`, `README.md` "Restart without systemd" and "Guaranteed install", the spec (FR-PROC-07a `[done]` or the documented gap, planner calls).

**Interfaces:**
- Consumes: the reload helper's contract (`contrib/durchlauferhitzer-reload`, `--ready-fd`, the draining list in `$pidfile.draining`, `stop_post`).

- [ ] **Step 1:** Investigate on this container (OpenRC is not installed here; `contrib/test-openrc.sh` runs in CI's Alpine job — read how): whether `supervisor=supervise-daemon` with `respawn_delay`/`respawn_max` can coexist with the reload helper's take-over, where the helper starts a new process outside the supervisor. Write the finding into the report with the exact OpenRC mechanics (what `supervise-daemon` does when its child exits after a reload drain; whether `reload` can hand the supervisor the new pid).
- [ ] **Step 2:** If compatible: implement supervision in the script and confd (respawn on abnormal exit only; a clean `stop` must not respawn), extend `contrib/test-openrc.sh` with a kill-and-respawn case and a reload-under-supervision case, run it in a dispatched CI run of the Alpine job on the task branch (it runs there), paste the result. If not compatible: document the gap in the README and contrib README, mark FR-PROC-07a with the documented gap and FR-PROC-08's guaranteed install as systemd-only, and say so in the report.
- [ ] **Step 3:** Spec; gate; commit; push.

### Task 6: Split `sim/tests/proxy/bodies.rs` (NFR-MAINT-10)

**Files:**
- Modify: `sim/tests/proxy/bodies.rs` (817 lines, 27 tests) → `bodies.rs` (uploads and limits: `post_with_content_length_body` … `chunked_over_limit_cuts_both`, `chunk_extension_from_client_is_400`, `chunked_over_limit_with_the_head_is_cut_before_connect`, the GET/HEAD 400 tests) and a new `sim/tests/proxy/expect.rs` (`expect_100_continue_*`, `early_backend_response_*`, `early_response_*`) and `sim/tests/proxy/body_timeouts.rs` (`stalled_request_body_is_408`, `backend_not_reading_the_body_is_504`, `response_timeout_runs_once_the_body_is_written`, `client_eof_mid_body_closes_both`, `backend_failure_after_the_whole_body_keeps_the_client`, `pipelined_request_after_body_is_not_forwarded`); shared helpers (`forwarded`, `answer_ok`, `client_reads`, `answer_ok_after`, `upload_body`) move to the proxy tests' support module if one exists (`sim/tests/proxy/main.rs` declares the modules; check for a `support` module) or stay in `bodies.rs` as `pub(crate)`; `sim/tests/proxy/main.rs` gains the module lines; `sim/README.md`.

- [ ] **Step 1:** Record the test count (`cargo test -p durchlauferhitzer-sim --locked --test proxy -- --list | grep -c ': test'`).
- [ ] **Step 2:** Move tests without changing a line of their bodies; keep the names; run the binary; same count, all pass; every file under 600 lines.
- [ ] **Step 3:** README, spec note under NFR-MAINT-10 (none needed unless a rule is clarified); gate; commit; push.
