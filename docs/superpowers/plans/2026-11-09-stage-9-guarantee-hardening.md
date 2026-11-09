# Stage 9: Guarantee hardening — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (mandatory in this repo) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. In this repo, `superpowers:<name>` is the project skill `<name>`.

**Goal:** The default config is the guaranteed install, the kernel holds the memory budget the config promises, `--check` says when a config leaves a guarantee to the environment, and a body on `GET`/`HEAD` never reaches a backend.

**Architecture:**
- **Defaults flip in the core's config model** (`Containment::default()`), nowhere else: the parser, the driver and the tests read the defaults from there. The example config and the README show the guaranteed values.
- **The memory backstop is a driver startup step** next to the descriptor budget: a pure `limit(&Limits)` computes `memory_limit + allowance`, a `sys/rlimit.rs` wrapper sets `RLIMIT_DATA` soft and hard, and the pool's threads get a pinned stack size so the allowance is arithmetic over measured constants, not a guess. The allowance has three terms: a base, one stack per disk thread, and one small constant per connection (the refused-connection send buffer and the per-entry state NFR-RES-01 leaves outside `memory_limit`). Startup runs before the seccomp filter, so no new system call is allowed after it.
- **`--check` warnings come from a pure core function** on `Containment` (`softenings()`), shared by `--check` (stderr lines) and the startup log (a `mode=` token on the existing `containment` line).
- **The `GET`/`HEAD` body rule is one arm in `proxy::plan`**, before the `max_request_body` check, answered like the existing 413: `Respond(BadRequest, Close)`, body unread, no connect, no `100`. The randomized proxy net's verdict and coverage learn the new bucket.

**Tech Stack:** Rust (stable, pinned), `libc` only. Test-only: the sim (`fastrand`), the driver's subprocess helpers (`driver/tests/support/child.rs`).

**Spec:** `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md`, binding. This plan covers:
- FR-CFG-25 (the defaults, owner decision 2026-11-09) and the FR-PROC-06 sentence on them;
- NFR-RES-09, all of it;
- FR-PROC-08, all of it;
- FR-PROXY-01's 2026-11-09 amendment, and the TE.0 row of NFR-SEC-14;
- Timeline item 9.

It touches FR-LOG-02/-05 (a new kind, a new token) and the README. The new text is `git diff 34faa70..HEAD -- docs/superpowers/specs/`.

## Global Constraints

- **No C:** Rust only; the system libc and `libgcc_s` are the only C. The product's dependencies stay `libc` only (`scripts/check-product-deps.sh`).
- **`unsafe`:** only in `driver/src/sys/` and the two documented test sites (NFR-MAINT-05). Task 2 adds `setrlimit`/`getrlimit` calls for `RLIMIT_DATA` in `driver/src/sys/rlimit.rs` (already the `unsafe` home of `RLIMIT_NOFILE`) and one `pre_exec` helper in `driver/tests/support/child.rs` (the documented test site).
- **Sandbox stays strict.** Every new system call runs during startup, before `containment::restrict_calls` installs the filter (`driver/src/process.rs`, `run_loop`). The allow-list (`driver/src/containment/filter.rs`) does not change. If an implementer finds otherwise, stop and report.
- **Code style:** enums over bools; exhaustive matches (no `_ =>` on enums); files < 900 lines (`driver/src/process.rs` is at 603: new startup logic goes into `driver/src/process/backstop.rs`, not into it); crate READMEs current in the same commit; docs terse.
- **Strict red/green TDD.** The failing test and the code that passes it land as one commit. Tests that pin existing behaviour are proven by a temporary mutation.
- **Spec current:** each task writes its "Planner calls" into the spec under the requirement, as *Planner calls (stage 9 plan):*, and flips its requirement's status to [done], in the same commit. New log names go into FR-LOG-02/-05 with the code.
- **Gate:** before every commit, `cargo fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked && scripts/check-product-deps.sh && RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`. For driver changes also `DURCHLAUFERHITZER_TEST_SECCOMP_KILL=1 cargo test -p durchlauferhitzer --locked`; for sim net changes also `SIM_SEEDS=0..20000 cargo test -p durchlauferhitzer-sim --locked --test faults`.
- **New server-starting test files** go into CI's `SERVER_TESTS` (`.github/workflows/ci.yml`, line 29).
- **NFR-PERF ratchet:** nothing here touches the request path except Task 4's one match arm; no bench run is required, but a `bench` static and proxy median before and after Task 4 is reported if the implementer has the time.

## Review Focus

1. **The allowance at the extremes** (Task 2): `disk_threads 64` with `memory_limit 1MiB` must start and serve, and so must `max_connections 65536` with `memory_limit` at its minimum for that count; `disk_threads 1` at `max_connections 1` must not leave slack large enough to hide a leak. Tests `starts_and_serves_with_64_disk_threads_under_the_backstop`, `starts_with_65536_connections_under_the_backstop` and the unit test `limit_is_memory_limit_plus_the_three_allowance_terms`.
2. **An inherited `RLIMIT_DATA`** that is `RLIM_INFINITY`, finite above the backstop, or finite below it (Task 2): the first two are lowered to exactly the backstop, soft and hard; the third is exit 2 naming both numbers. Test `an_inherited_hard_limit_below_the_backstop_is_exit_2`, and `startup_sets_rlimit_data_to_the_backstop` run with both an unlimited and a finite inherited limit.
3. **`--check` stays side-effect free** (Task 3): it computes the backstop and prints it, and sets no limit, binds nothing, opens no root. Test `check_prints_the_backstop_and_sets_no_limit` reads the child's exit and its own `/proc/self/limits` unchanged.
4. **`Content-Length: 0` on `GET`** (Task 4) is no body (FR-HTTP-08) and is still forwarded; `HEAD` with a chunked body and `Expect: 100-continue` gets a 400 and never a `100`. Tests `get_with_content_length_zero_is_forwarded` and `head_with_a_chunked_body_and_expect_gets_400_not_100`.
5. **An absent `containment` block on a kernel without a layer** (Task 1) is exit 2 with FR-PROC-06's `landlock is required but the kernel has none`, not a served request. Test `an_absent_block_requires_landlock` with `Filter::LandlockEnosys`.

---

### Task 1: `required` and `kill` are the defaults (FR-CFG-25)

**Files:**
- Modify: `core/src/config/model.rs:58-66` (`impl Default for Containment`)
- Modify: `core/src/config/validate/tests/containment.rs:33-45` (`defaults_without_the_block`), and lines 67–79 where a half-written block falls back to the default
- Modify: `driver/tests/containment.rs` (one new test; existing `*_under_best_effort` tests write their mode explicitly and stay)
- Modify: `examples/durchlauferhitzer.conf:95-111` (the `containment` block and its comment)
- Modify: `README.md` "Containment" (lines 109–136) and "Try it" (lines 58–96)
- Modify: `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md` (FR-CFG-25: status `[done]`, drop the "until it lands" parenthesis; FR-PROC-06's sentence)

**Interfaces:**
- Consumes: `Layer::{Off, BestEffort, Required}`, `SeccompAction::{Fail, Kill}` (`core/src/config/model.rs`).
- Produces: `Containment::default() == Containment { landlock: Layer::Required, seccomp: Layer::Required, seccomp_action: SeccompAction::Kill }`. Tasks 3 and 5 rely on it.

- [ ] **Step 1: Change the failing test** `defaults_without_the_block` in `core/src/config/validate/tests/containment.rs` to assert `Layer::Required`, `Layer::Required`, `SeccompAction::Kill`; at lines 67–79, a block with only `seccomp required;` leaves `landlock` at `Layer::Required` and a block with only `landlock best_effort;` leaves `seccomp` at `Layer::Required`.

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p durchlauferhitzer-core --locked defaults_without_the_block`
Expected: FAIL, `left: Containment { landlock: BestEffort, ... }`.

- [ ] **Step 3: Flip `impl Default for Containment`** in `core/src/config/model.rs` to `Required`, `Required`, `Kill`; update its doc comment and `Layer`'s doc comments (`Required` is the default).

- [ ] **Step 4: Run the core tests**

Run: `cargo test -p durchlauferhitzer-core --locked`
Expected: PASS.

- [ ] **Step 5: Write the driver test** `an_absent_block_requires_landlock` in `driver/tests/containment.rs` (it pins Step 3's flip, so it passes at once and is proven by mutation in Step 6): a config without a `containment` block, run with `Setup { filter: Filter::LandlockEnosys, ..Setup::default() }` (the helper that makes `landlock_create_ruleset` fail with `ENOSYS`, `driver/tests/support/child.rs:256`); assert exit 2 and stderr containing `landlock is required but the kernel has none`. Check how the file's existing tests build a config without the block (`child::config` with `Containment::Switch` writes one; the test needs the variant, or a literal config, that writes none).

- [ ] **Step 6: Prove it by mutation**

Run: `cargo test -p durchlauferhitzer --locked --test containment an_absent_block_requires_landlock`
Expected: PASS. Then set `Default` back to `BestEffort` for a moment (`touch` the sources so cargo rebuilds): the test must FAIL (the server starts and logs `state=missing`). Restore the flip, run again: PASS. Paste both outputs in the report.

- [ ] **Step 7: Run the whole driver suite twice**

Run: `cargo test -p durchlauferhitzer --locked` and `DURCHLAUFERHITZER_TEST_SECCOMP_KILL=1 cargo test -p durchlauferhitzer --locked`
Expected: PASS. A test that fails because it relied on the old default (a config without a `containment` block on a container without Landlock) names itself here: give it an explicit `best_effort` block only if the test is about something else; if it is about containment, it is wrong and the plan needs a ruling.

- [ ] **Step 8: Example config and README.** `examples/durchlauferhitzer.conf`: the block reads `landlock required;`, `seccomp required;`, `seccomp_action kill;` with the softer values and their meaning in the comments ("the values shown are the defaults" stays true). README "Containment": `required` (the default: missing or partial is exit 2), `best_effort` (used where the kernel has it, otherwise logged as missing and the server serves), `off`; `seccomp_action kill` the default. README "Try it": one sentence that on a kernel without Landlock or seccomp the example needs `containment { landlock best_effort; seccomp best_effort; }` to start. Run the example-config test (`cargo test -p durchlauferhitzer-core --locked example`) and `cargo run -p durchlauferhitzer --locked -- --check examples/durchlauferhitzer.conf` → `ok`.

- [ ] **Step 9: Spec.** FR-CFG-25 → `[done]`, remove "(`best_effort`, `best_effort`, `fail` until it lands)"; FR-PROC-06's "(the default since the owner decision 2026-11-09, stage 9; `best_effort` before)" → "(the default; owner decision 2026-11-09)".

- [ ] **Step 10: Gate and commit**

```bash
git add core/src/config examples README.md driver/tests/containment.rs docs/superpowers/specs
git commit -m "Containment defaults: required, required, kill (FR-CFG-25, owner decision 2026-11-09)"
```

---

### Task 2: The kernel memory backstop (NFR-RES-09)

**Files:**
- Create: `driver/src/process/backstop.rs`
- Modify: `driver/src/sys/rlimit.rs` (generalise `get` over the resource; add `data()` and `cap_data(to)`)
- Modify: `driver/src/process.rs:251-258` (`start_up`: after `budget::ensure`, before `drop_privileges`), `driver/src/process.rs` `Ready`/`notes` (the new log note)
- Modify: `driver/src/pool.rs:200` (`thread::Builder::new().name(..).stack_size(DISK_THREAD_STACK)`)
- Modify: `driver/src/log.rs` (`DriverLog::MemoryBackstop { bytes: u64 }` and its `kind=memory_backstop bytes=<n>` line)
- Create: `driver/tests/backstop.rs` (server-starting: add `backstop` to `SERVER_TESTS` in `.github/workflows/ci.yml`)
- Modify: `driver/tests/support/child.rs` (`Setup.data: Option<(u64, u64)>`, `pre_exec::limit_data`, next to `limit_nofile`)
- Modify: `driver/README.md` (module list), the spec (NFR-RES-09 → `[done]`, planner calls; FR-LOG-02/-05 the new kind)

**Interfaces:**
- Consumes: `Limits { memory_limit: u64, disk_threads: u32, .. }` (`core/src/config/model.rs`); `Failure(String)` and `report` in `driver/src/process.rs`; `cvt` in `driver/src/sys/fd.rs`.
- Produces:
  - `pub const DISK_THREAD_STACK: usize = 2 * 1024 * 1024;` in `driver/src/pool.rs`, passed to `Builder::stack_size` so the stacks are this size whatever `RUST_MIN_STACK` says.
  - `pub(crate) const BASE_ALLOWANCE: u64` and `pub(crate) const PER_CONNECTION_ALLOWANCE: u64` in `driver/src/process/backstop.rs`: the measured constants (Step 5). `PER_CONNECTION_ALLOWANCE` covers the refused-connection send buffer (`SEND_CAPACITY`, 1 KiB) plus the per-entry state of a connection in the core and the driver, rounded up to a KiB.
  - `pub(crate) fn limit(limits: &Limits) -> u64`: `memory_limit + BASE_ALLOWANCE + disk_threads × DISK_THREAD_STACK + max_connections × PER_CONNECTION_ALLOWANCE`, every operation saturating (`u64`). Task 3 prints it.
  - `pub(super) fn apply(limits: &Limits) -> Result<DriverLog, Failure>`: reads `rlimit::data()`; a hard limit below `limit` → `Failure("memory backstop: the server needs <needed> bytes of RLIMIT_DATA, <hard> are available")`; else `rlimit::cap_data(limit)` and `Ok(DriverLog::MemoryBackstop { bytes: limit })`.
  - `pub fn data() -> io::Result<(u64, u64)>` and `pub fn cap_data(to: u64) -> io::Result<()>` in `driver/src/sys/rlimit.rs`: `cap_data` sets `rlim_cur` and `rlim_max` both to `to` (`RLIM_INFINITY` is `u64::MAX`; lowering needs no privilege).
  - Log line: `kind=memory_backstop bytes=<n>`, written with the startup notes before the `containment` lines.

- [ ] **Step 1: Write the failing unit tests** in `driver/src/process/backstop.rs` (`#[cfg(test)] mod tests`): `limit_is_memory_limit_plus_the_three_allowance_terms` (`memory_limit` 1 MiB, `disk_threads` 4, `max_connections` 16 → `1 MiB + BASE_ALLOWANCE + 8 MiB + 16 × PER_CONNECTION_ALLOWANCE`); `limit_saturates` (`memory_limit: u64::MAX` → `u64::MAX`).

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p durchlauferhitzer --locked backstop::`
Expected: FAIL to compile (`backstop` does not exist).

- [ ] **Step 3: Implement** `limit` with placeholders `BASE_ALLOWANCE` of 64 MiB and `PER_CONNECTION_ALLOWANCE` of 4 KiB (both replaced in Step 5), `DISK_THREAD_STACK` in `pool.rs` with `Builder::stack_size`, `data`/`cap_data` in `rlimit.rs`, `apply`, the `DriverLog` variant and its line, and the call in `start_up` between `budget::ensure` and `drop_privileges`, pushing the note onto `notes`.

- [ ] **Step 4: Run the unit tests**

Run: `cargo test -p durchlauferhitzer --locked backstop::`
Expected: PASS.

- [ ] **Step 5: Measure the allowance.** Start the binary with `max_connections 16`, `memory_limit 1MiB`, `disk_threads 4`, a static root and a proxy route (the `both` config of `driver/tests/budget.rs`), with the backstop call temporarily disabled; read `VmData` from `/proc/<pid>/status` after the `ready` line and again after the smoke-test load (`driver/tests/smoke.rs`'s requests, or 16 parallel `GET`s of a 1 MiB file plus 16 proxied `POST`s); do this on glibc (this container) and on musl (`cargo build --target x86_64-unknown-linux-musl` if the toolchain has it; otherwise on the Alpine CI job, by reading the number from a temporary log line, then remove it). Then start the same config with `max_connections 4096` (and a `memory_limit` that fits it) and read `VmData` after `ready`: `PER_CONNECTION_ALLOWANCE` = (`VmData` at 4096 minus `VmData` at 16) / 4080, rounded up to a KiB, and at least `SEND_CAPACITY` + that (the refused-connection send buffers are allocated on demand, so they are not in the startup reading). `BASE_ALLOWANCE` = the larger loaded `VmData` at 16 connections minus `16 × worst_case_per_connection` minus `4 × DISK_THREAD_STACK` minus `16 × PER_CONNECTION_ALLOWANCE`, rounded up to a MiB, times 2. Write every number and the formula into the module docs. If musl cannot be measured in this task, say so in the report and the commit; the Alpine CI run of Step 9 is then the check.

- [ ] **Step 6: Write the failing driver tests** in `driver/tests/backstop.rs` (header like `driver/tests/budget.rs`; a `both` config at `max_connections 16`, `memory_limit 1MiB`, `disk_threads 4`, an explicit `containment { landlock best_effort; seccomp best_effort; }` so the test runs on any kernel, like the other server tests do):
  - `startup_sets_rlimit_data_to_the_backstop`: run twice, with `Setup::default()` (inherited `RLIM_INFINITY`) and with `Setup { data: Some((backstop + 1 MiB, backstop + 1 MiB)), .. }`; after `ready`, `/proc/<pid>/limits` line `Max data size` shows soft and hard both equal to `backstop::limit` (compute the expected number in the test from the same constants, exported `pub` for the test); the log has `kind=memory_backstop bytes=<that number>`.
  - `an_inherited_hard_limit_below_the_backstop_is_exit_2`: `Setup { data: Some((limit - 1, limit - 1)), .. }`, run as non-root (`Setup::non_root()` pattern of `budget.rs`): exit 2, stderr `durchlauferhitzer: memory backstop: the server needs <limit> bytes of RLIMIT_DATA, <limit - 1> are available`.
  - `starts_and_serves_with_64_disk_threads_under_the_backstop`: `disk_threads 64`, `memory_limit 1MiB`; after `ready`, one static `GET` and one proxied `POST` succeed; the process is still alive after them (no abort).
  - `starts_with_65536_connections_under_the_backstop`: `max_connections 65536`, `memory_limit` the smallest value FR-CFG-21 accepts for it (compute from `worst_case_per_connection`, or read the number from the `MemoryBudget` error of a too-small value), `RLIMIT_NOFILE` raised as `budget.rs` does; `ready` is reached and one request succeeds.
  - `serves_the_smoke_load_under_the_smallest_budget`: the `both` config above; 16 parallel clients each fetch a 1 MiB static file while 16 others `POST` 64 KiB through the proxy; every response complete; the process alive afterwards.

- [ ] **Step 7: Run them to see them fail**

Run: `cargo test -p durchlauferhitzer --locked --test backstop`
Expected: the first two FAIL (`Setup.data` and `limit_data` do not exist; the limits line is unlimited); the last two are pinned by mutation: with `BASE_ALLOWANCE = 0` the 64-thread start fails (thread spawn refused: `driver: ...` exit 2) or the load aborts. Paste the outputs.

- [ ] **Step 8: Implement** `Setup.data` and `pre_exec::limit_data(cmd, soft, hard)` in `child.rs` (same shape as `limit_nofile`, `RLIMIT_DATA`), and whatever the tests still need.

- [ ] **Step 9: Run the suites**

Run: `cargo test -p durchlauferhitzer --locked --test backstop`, then the gate, then `DURCHLAUFERHITZER_TEST_SECCOMP_KILL=1 cargo test -p durchlauferhitzer --locked`
Expected: PASS. Add `backstop` to `SERVER_TESTS`.

- [ ] **Step 10: Spec and READMEs.** NFR-RES-09 → `[done]` with *Planner calls (stage 9 plan):* `RLIMIT_DATA`; the allowance is three terms, `BASE_ALLOWANCE + disk_threads × DISK_THREAD_STACK + max_connections × PER_CONNECTION_ALLOWANCE` (the stack 2 MiB, pinned with `Builder::stack_size`), with the measured numbers, replacing the requirement's "a fixed allowance" by "an allowance of measured constants"; the log kind `memory_backstop` (also in FR-LOG-02/-05); the exit-2 message; set between the descriptor budget and the privilege drop. `driver/README.md`: the new module.

- [ ] **Step 11: Gate and commit**

```bash
git add driver core/src/config docs .github/workflows/ci.yml
git commit -m "Memory backstop: RLIMIT_DATA set to memory_limit plus a measured allowance (NFR-RES-09)"
```

---

### Task 3: `--check` warnings and the guaranteed install (FR-PROC-08)

**Files:**
- Create: `core/src/config/softening.rs` (`Softening` enum, `Containment::softenings`, `Display`)
- Modify: `core/src/config/mod.rs` (module and re-export), `core/README.md`
- Modify: `driver/src/process.rs:134-144` (`check`)
- Modify: `driver/src/containment/line.rs` (`mode=` token) and `driver/src/containment.rs` (the `Outcome` carries its configured `Layer`)
- Modify: `driver/tests/cli.rs` (`check_on_a_valid_config_prints_ok` and new tests), `driver/tests/containment.rs` or `driver/tests/log.rs` (the `mode=` token)
- Modify: `README.md` (new section "Guaranteed install" after "Containment"; "Check a config" mentions the warnings and the backstop line)
- Modify: the spec (FR-PROC-08 → `[done]`, planner calls; FR-LOG-05 the `mode=` token)

**Interfaces:**
- Consumes: `Containment`, `Layer`, `SeccompAction` (`core/src/config/model.rs`); `backstop::limit(&Limits)` (Task 2); `Outcome::write_line` (`driver/src/containment/line.rs`).
- Produces:
  - `pub enum Softening { LandlockBestEffort, LandlockOff, SeccompBestEffort, SeccompOff, SeccompActionFail }` and `impl Containment { pub fn softenings(&self) -> Vec<Softening> }`, in config order (landlock, seccomp, seccomp_action); `Display` gives exactly:
    - `containment landlock best_effort: serves on a kernel without Landlock`
    - `containment landlock off: Landlock is not used`
    - `containment seccomp best_effort: serves on a kernel without seccomp`
    - `containment seccomp off: the system-call filter is not used`
    - `containment seccomp_action fail: an unexpected system call is answered, not fatal`
  - `--check` stderr: one `<config-path>: warning: <Display>` line per softening, then `<config-path>: memory backstop: <bytes> bytes`; stdout `ok`; exit 0. The warning lines come first so a reader sees them above the number.
  - The startup `containment` line: `kind=containment layer=landlock mode=required state=active abi=4 ...` (`mode=` right after `layer=`; values `off`, `best_effort`, `required`).

- [ ] **Step 1: Write the failing core tests** in `core/src/config/softening.rs`: `the_default_has_no_softening` (`Containment::default().softenings()` is empty); `every_softening_in_config_order` (`Off`, `BestEffort`, `Fail` → `[LandlockOff, SeccompBestEffort, SeccompActionFail]`); `display_is_the_exact_line` (each variant's string above).

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p durchlauferhitzer-core --locked softening`
Expected: FAIL to compile.

- [ ] **Step 3: Implement** the module, with `match` arms over `Layer` and `SeccompAction` (no `_ =>`).

- [ ] **Step 4: Run the core tests** — Expected: PASS.

- [ ] **Step 5: Change and write the failing driver tests** in `driver/tests/cli.rs`: `check_on_a_valid_config_prints_ok` now expects stderr `== format!("{path}: memory backstop: {n} bytes\n")` with `n` from `durchlauferhitzer::process::backstop::limit` (export the function `pub` for the test, or recompute from the exported constants); `check_warns_about_each_softening` (a config with `landlock off; seccomp best_effort; seccomp_action fail;` → three warning lines in that order, then the backstop line, stdout `ok`, exit 0); `check_prints_the_backstop_and_sets_no_limit` (after the child exits, the test process's own `/proc/self/limits` `Max data size` is unchanged; `--check` must not bind: the config's port is held by a `TcpListener` in the test and `--check` still exits 0, as `check_on_a_valid_config_prints_ok` already arranges). In `driver/tests/containment.rs` (or `log.rs`, wherever the `kind=containment` line is asserted today): the line carries `mode=best_effort` for an explicit `best_effort` and `mode=required` for the default.

- [ ] **Step 6: Run them to see them fail** — Expected: the stderr assertions fail (no backstop line, no warnings, no `mode=`).

- [ ] **Step 7: Implement.** `check` in `process.rs`: after `load_config`, `for s in config.containment.softenings() { say_err(format_args!("{}: warning: {s}", path.display())) }`, then the backstop line with `backstop::limit(&config.limits)`, then `ok`. `Outcome` gains the configured `Layer` and `write_line` emits ` mode=<value>` after the layer name.

- [ ] **Step 8: Run the suites** — Expected: PASS, also under `DURCHLAUFERHITZER_TEST_SECCOMP_KILL=1`.

- [ ] **Step 9: README.** New section "Guaranteed install" after "Containment": the config and platform the security requirements are claimed for — the default `containment` block (`required`, `required`, `kill`), `user` set, Linux ≥ 6.2 (Landlock ABI 3; 6.10 for every right), `memory_limit` with the backstop the startup log shows, a supervisor that restarts the process (systemd `Restart=on-abnormal` or the OpenRC script); and the sentence that outside it NFR-SEC-12 and NFR-RES-09 hold only as far as the kernel allows. "Check a config": the warning lines and the backstop line, with one example of each.

- [ ] **Step 10: Spec.** FR-PROC-08 → `[done]` with *Planner calls (stage 9 plan):* the five exact warning strings, the order (warnings, then the backstop line), the `mode=` token (also in FR-LOG-05's line description).

- [ ] **Step 11: Gate and commit**

```bash
git add core driver README.md docs
git commit -m "--check warnings for softened containment, the backstop line, mode= in the containment log line, the guaranteed install (FR-PROC-08)"
```

---

### Task 4: A body on `GET`/`HEAD` toward a backend is 400 (FR-PROXY-01)

**Files:**
- Modify: `core/src/core/proxy.rs:61-77` (`plan`)
- Modify: `core/src/core/proxy/tests.rs` (two unit tests)
- Modify: `sim/tests/proxy/bodies.rs` (four tests)
- Modify: `sim/tests/faults/proxy/request.rs:697-716` (the verdict), `sim/tests/faults/proxy/outcome.rs:176-180` (the tag), `sim/tests/faults/proxy/mod.rs:277-290` (`REQUIRED` bucket `400 body on GET or HEAD`)
- Modify: the spec (FR-PROXY-01's amendment → `[done]`; NFR-SEC-14's TE.0 row: verdict without the stage tag, tests as named)

**Interfaces:**
- Consumes: `Method::{Get, Head, Other(_)}`, `BodyFraming::{None, Length(_), Chunked}` (`core/src/http/request.rs`); `ProxyPlan::Respond(Status, Persistence)` and the existing 413 arm in `plan`.
- Produces: in `plan`, before the `max_request_body` match: `(Method::Get | Method::Head, BodyFraming::Length(_) | BodyFraming::Chunked) => return ProxyPlan::Respond(Status::BadRequest, Persistence::Close)`; `(Method::Get | Method::Head, BodyFraming::None) | (Method::Other(_), _) => {}` written out in full. The response is logged as today's 413 is (FR-LOG-03 `Response`), the body unread (`UnreadBody::Pending` → lingering close, FR-CONN-05), no upstream is opened and no `100` is written (`plan` runs before FR-PROXY-25's answer).

- [ ] **Step 1: Write the failing unit tests** in `core/src/core/proxy/tests.rs`, next to `plan_takes_the_routes_internal_files_or_fails_closed`: `plan_rejects_a_body_on_get_and_head` (`Get` + `Length(1)`, `Get` + `Chunked`, `Head` + `Chunked` → `Respond(BadRequest, Close)`); `plan_keeps_other_methods_with_bodies_and_get_without` (`Other("POST")` + `Chunked` passes the arm; `Get` + `None` passes).

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p durchlauferhitzer-core --locked plan_rejects_a_body_on_get_and_head`
Expected: FAIL (the plan connects).

- [ ] **Step 3: Implement the arm** in `plan`.

- [ ] **Step 4: Run the core tests** — Expected: PASS.

- [ ] **Step 5: Write the failing sim tests** in `sim/tests/proxy/bodies.rs`, in the style of `content_length_over_limit_is_413_before_connect`: `get_with_a_body_is_400_without_connect` (`GET` with `Content-Length: 5` and five body bytes in the same read: a 400 with `Connection: close`, no `Action::ConnectUpstream`, the connection lingers and closes); `head_with_a_chunked_body_is_400_without_connect` (head-only 400, FR-HTTP-21); `get_with_content_length_zero_is_forwarded` (a connect happens; the forwarded head has no `Content-Length` line beyond what FR-PROXY-10 renders for `None`); `head_with_a_chunked_body_and_expect_gets_400_not_100` (no `100 Continue` bytes precede the 400).

- [ ] **Step 6: Run them to see them fail** — Expected: the first, second and fourth FAIL (a connect is attempted); the third passes and is pinned by a temporary mutation that rejects `Length(0)` too, if the parser ever produced it (it does not: `Content-Length: 0` is `None`; note that in the test's doc comment and skip the mutation).

- [ ] **Step 7: Teach the randomized proxy net.** In `sim/tests/faults/proxy/request.rs`, after the static-route verdicts (line ~712) and before the route lookup: `if j.target < 2 && matches!(j.method, "GET" | "HEAD") && j.has_body { return Verdict::Answer(Status::BadRequest, Persistence::Close); }`. Check the drawing at lines 285–288: `GET`/`HEAD` draw a body one time in six already, so the bucket fills without a change there. In `outcome.rs:176`, tag it `400 body on GET or HEAD` (the existing `400 duplicate X-Forwarded-For` tag must not claim it: distinguish by the request's method and body). Add the bucket to `REQUIRED` in `mod.rs`.

- [ ] **Step 8: Run the nets**

Run: `cargo test -p durchlauferhitzer-sim --locked --test faults` then `SIM_SEEDS=0..20000 cargo test -p durchlauferhitzer-sim --locked --test faults`
Expected: PASS with the new bucket above zero. A seed that fails names a case the core and the model disagree on: the test is right if it matches the spec; otherwise the plan needs a ruling.

- [ ] **Step 9: Spec.** FR-PROXY-01's amendment → `[done]`; NFR-SEC-14's TE.0 row: verdict `C 400 on proxy routes (static: 413)`, pinned by the four `bodies.rs` tests and the net's bucket.

- [ ] **Step 10: Gate and commit**

```bash
git add core sim docs
git commit -m "A body on GET or HEAD toward a backend is 400 (FR-PROXY-01, NFR-SEC-14 TE.0)"
```

---

### Task 5: Close-out

**Files:**
- Modify: the spec (Timeline item 9 → `[done]`; the status legend's "stages 1–8" → "1–9"), `docs/superpowers/HANDOFF.md` (stage 9 done, next the stage 9 whole-branch review)

- [ ] **Step 1: Check every status** named in this plan is `[done]` and every "Planner calls" is in place: `grep -n "stage 9" docs/superpowers/specs/*.md` lists only timeline and decision history.
- [ ] **Step 2: Update the handoff** (stage table row, "Where things stand" paragraph for stage 9, carry-forwards unchanged).
- [ ] **Step 3: Commit**

```bash
git add docs
git commit -m "Stage 9 close-out: statuses done, handoff"
```
