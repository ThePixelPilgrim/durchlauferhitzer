# Stage 5: Containment — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (mandatory in this repo) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. In this repo, `superpowers:<name>` is the project skill `<name>`.

**Goal:** The binary confines itself after startup — Landlock limits file access to reading beneath the static roots, a seccomp filter allow-lists its system calls — each best effort unless the config requires it; IPv6 peers share per-IP limits by prefix and the refused pool is fair; the parsers and the config language are fuzzed, and long randomized runs plus fuzzing run nightly and on demand.

**Architecture:**
- **Config and core.** The core gains the `containment` block (a model only; the core enforces nothing of it) and `ipv6_prefix`, and the per-IP accounting groups IPv6 peers by prefix and caps refused connections per group. Still std-only, sans-IO.
- **Containment in the driver.** One module, `driver/src/containment.rs`, behind one function, decides and applies both layers at startup and returns their outcomes. Raw Landlock and seccomp calls sit in `driver/src/sys/landlock.rs` and `driver/src/sys/seccomp.rs`; the BPF program is built by a pure, testable generator in `driver/src/containment/filter.rs`. A build without containment (NFR-PORT-07) leaves out this one module.
- **Startup order.** Containment is applied after the privilege drop and capability clear (A11 step 7). Landlock comes right after it, before any further file access; seccomp is installed last, right before the loop, with `SECCOMP_FILTER_FLAG_TSYNC` so the disk workers already started by `Driver::new` are covered too.
- **Testing.** A test switch makes every server-starting test run under `seccomp_action kill`, which proves the allow-list end to end; a nightly workflow runs long sim runs and the fuzz targets.

**Tech Stack:** Rust 2024 (stable, pinned) for the product; the driver still depends on `libc` only (`landlock_*` and `seccomp` via `libc::syscall`, structs hand-declared `#[repr(C)]` where libc lacks them). Fuzzing: `cargo-fuzz` (libFuzzer, nightly Rust) in a separate `fuzz/` crate excluded from the workspace (test-only, NFR-MAINT-02).

**Spec:** `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md` (binding; the plan argues from it). Requirements: FR-CFG-10 (the `containment` block), FR-CFG-20/21 (`ipv6_prefix`), FR-CFG-25, FR-CONN-13, FR-PROC-06, FR-PROC-07 note, NFR-SEC-12/13, NFR-PORT-04/07, NFR-TEST-08. Carry-forwards: `docs/superpowers/HANDOFF.md` "Carry-forwards to later stages" (stage 5 items) and `docs/superpowers/reviews/2026-10-29-stage-4-review.md` "Deferred".

## Owner decisions (2026-10-29, written into the spec)

All in the spec under FR-CFG-25, FR-CONN-13, FR-PROC-06/07, NFR-SEC-13, NFR-PORT-04/07, NFR-TEST-08: every layer `off | best_effort | required` (default `best_effort`); `seccomp_action fail | kill` (default `fail` = `EPERM`, keep running, a crash it causes is a server bug); blocked calls logged by the kernel (`SECCOMP_FILTER_FLAG_LOG`); Landlock ABI 1 counts as supported, its gaps (truncation before ABI 3, device `ioctl` before ABI 5) covered by seccomp and named in the startup log; seccomp checks numbers plus key arguments; the sandbox is strict now (no `execve`), stage 6 adapts; `ipv6_prefix` default 64, settable; nightly + on-demand long runs and fuzzing.

## Global Constraints

- Rust only; glibc the only C; the product's dependencies stay `libc` only, checked by `scripts/check-product-deps.sh` (NFR-MAINT-01…04). The `fuzz/` crate is outside the workspace and the product tree.
- `unsafe` only in `driver/src/sys/` (product) and the existing `pre_exec` module of `driver/tests/support/child.rs` (tests); `driver/tests/unsafe_confined.rs` keeps that.
- Enums over bools, exhaustive matches (no `_ =>` on enums), files < 900 lines, crate READMEs current, spec updated in the same commit as the behaviour.
- Linux ≥ 5.14, x86_64 and aarch64 (NFR-PORT-01); both CI jobs green; root tests via the existing root step.
- The gate before every commit: `cargo fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked && scripts/check-product-deps.sh && RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`.
- No change to the core's behaviour beyond FR-CONN-13; the sim's model stays spec-written.

## Review Focus

1. **A system call used only on a rare path** (log flush on `EAGAIN`, accept pause and retry, `EMFILE`, a disk timeout, shutdown and worker detach, the 1 s pool join, a signal while draining) must be in the allow-list: the whole driver test suite runs under `seccomp_action kill` (Task 6), not only the happy path.
2. **glibc differences** (`clone3` vs `clone`, `rseq`, `statx`/`newfstatat`, `getrandom`, `futex` variants, `exit_group`, aarch64 having no `open`/`poll`/`epoll_wait`): the filter lists what both glibc builds use and the Debian 12 container job proves an older glibc (Task 7).
3. **Landlock and the roots:** rules are added from the already opened `O_PATH` root descriptors (no second path lookup, so a root replaced by a symlink after startup changes nothing), and a file opened through a root still works while `/etc/passwd` read through any path does not (Task 4).
4. **Peers at the prefix boundaries:** `/128` (one address per group), `/32`, an IPv4-mapped IPv6 peer (stays its IPv4 address), two addresses in the same /64 sharing a slot, and orphans counted per group (Task 2).
5. **Startup order mistakes:** containment after the drop and capability clear and after every root/listener is open; seccomp after `Driver::new` (threads exist, so TSYNC is mandatory) — a required layer that fails to apply exits 2 before serving, and nothing is printed through std while the log sink lives (Task 5).

---

### Task 1: Config: the `containment` block and `ipv6_prefix`

**Files:**
- Modify: `core/src/config/model.rs` (types, `Config`, `Limits`), `core/src/config/validate/*` (parse and check), `core/src/config/validate/tests/*`, `examples/durchlauferhitzer.conf`, `core/README.md`, the spec (FR-CFG-10/20/21/25 statuses stay `[stage 5]` until Task 8).

**Interfaces:**
- Produces (in `core::config::model`, re-exported like the other model types):
  - `pub enum Layer { Off, BestEffort, Required }` (config words `off`, `best_effort`, `required`);
  - `pub enum SeccompAction { Fail, Kill }` (`fail`, `kill`);
  - `pub struct Containment { pub landlock: Layer, pub seccomp: Layer, pub seccomp_action: SeccompAction }` with `Default` = `BestEffort`, `BestEffort`, `Fail`;
  - `Config.containment: Containment`;
  - `Limits.ipv6_prefix: u8` (default 64).

- [ ] **Step 1: Failing tests** in the validate tests: a config without the block gives the defaults; `containment { landlock required; seccomp off; }` parses; `seccomp_action kill;` parses; `seccomp off; seccomp_action kill;` is an error at `seccomp_action` (FR-CFG-25 builder call); an unknown value, a duplicate key, a second `containment` block, and the block nested inside `host` are errors with positions; `limits { ipv6_prefix 48; }` parses; `ipv6_prefix 31`, `129`, `64x` are errors; the example config still loads and documents every key (the existing key-check test covers the new keys once the example names them).
- [ ] **Step 2: Run them, see them fail.**
- [ ] **Step 3: Implement** the types and validation; document the block and key in `examples/durchlauferhitzer.conf` with defaults and behaviour.
- [ ] **Step 4: Gate, commit** "Config: containment block and ipv6_prefix (FR-CFG-25, FR-CFG-20)", push.

### Task 2: Core: per-IP grouping by IPv6 prefix and a fair refused pool (FR-CONN-13)

**Files:**
- Modify: `core/src/core/limits.rs` (accounting), `core/src/core/mod.rs` (state: `refused_per_group`), `sim/tests/` (a directed test file `limits.rs` or the existing one), the spec FR-CONN-13 text if the code needs a wording fix.

**Interfaces:**
- Consumes: `Limits.ipv6_prefix` (Task 1).
- Produces: `pub(crate) fn peer_group(peer: IpAddr, ipv6_prefix: u8) -> IpAddr` in `core/src/core/limits.rs`: canonical IPv4 (and IPv4-mapped) peers unchanged; IPv6 peers with the bits past `ipv6_prefix` zeroed. Every per-IP map (`counted_per_ip`, orphans) is keyed by it; a new `refused_per_group: BTreeMap<IpAddr, u32>` caps refused connections per group at `max_connections_per_ip` (FR-CONN-13), on top of the global `max_connections` refused cap (FR-CONN-10); every close path releases it exactly once.

- [ ] **Step 1: Failing unit tests** in `limits.rs`: `peer_group` for `2001:db8::1` and `2001:db8::ffff:1` at /64 equal; at /128 different; `::ffff:192.0.2.1` → `192.0.2.1`; /32 masks to `2001:db8::`; two peers in one /64 share `max_connections_per_ip` (the second beyond the limit gets the 503); an orphan counts against its group; refused connections from one group stop at `max_connections_per_ip` while another group still gets its 503 (the extra one is `Close` with nothing written and logged rejected, as FR-CONN-10's overflow); `limit_counts_return_to_zero_on_every_close_path` extended to the per-group refused count.
- [ ] **Step 2: Run, see them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: A sim test** (directed) with IPv6 peers in one /64 and in two different /64s, asserting exact bytes (503 via the model) and logs; and the randomized static net draws some peers from a shared /64 (seeded), so the oracle's per-IP model (spec-written, grouping by the configured prefix) is exercised. Mutation proof: key the map by the full address → the directed test fails.
- [ ] **Step 5: Gate, commit** "Core: per-IP limits group IPv6 peers by prefix; fair refused pool (FR-CONN-13)", push.

### Task 3: Seccomp filter generator and its system-call wrapper

**Files:**
- Create: `driver/src/containment/filter.rs` (pure: allow-list tables per architecture and the BPF program), `driver/src/sys/seccomp.rs` (install), tests in both.
- Modify: `driver/src/sys/mod.rs`, `driver/README.md`.

**Interfaces:**
- Produces:
  - `pub(crate) fn program(arch: Arch, action: SeccompAction) -> Vec<libc::sock_filter>` in `filter.rs`; `pub(crate) enum Arch { X86_64, Aarch64 }` with `Arch::current()`; the program checks `seccomp_data.arch` first (a foreign arch gets the action), allows the listed call numbers, applies the argument checks of FR-PROC-06 (`socket` domain `AF_INET`/`AF_INET6` and type `SOCK_STREAM` with flags masked; `mmap`/`mprotect` without `PROT_EXEC`; `clone` only with the thread flags glibc's `pthread_create` passes, `clone3` answered `ENOSYS` so glibc falls back to `clone`; `prctl`, `fcntl`, `ioctl` only for the operations the server uses), and returns `SECCOMP_RET_ERRNO | EPERM` (`Fail`) or `SECCOMP_RET_KILL_PROCESS` (`Kill`) for the rest. Truncation calls (`truncate`, `ftruncate`) and device `ioctl` beyond the listed requests are never allowed (they cover the Landlock ABI gaps, FR-PROC-06).
  - `pub fn install(program: &[libc::sock_filter]) -> io::Result<()>` in `sys/seccomp.rs`: `prctl(PR_SET_NO_NEW_PRIVS, 1)`, then `seccomp(SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_TSYNC | SECCOMP_FILTER_FLAG_LOG, &prog)`; `pub fn supported() -> Support` (`Supported | Missing`) probing `seccomp(SECCOMP_GET_ACTION_AVAIL, …)`.

- [ ] **Step 1: Failing tests** for the generator: a tiny BPF interpreter in the test module (load, jump, return over `seccomp_data`) runs the program on synthetic calls: an allowed call → ALLOW; an unlisted one → the action; `socket(AF_UNIX)` → the action; `mmap` with `PROT_EXEC` → the action; a foreign arch → the action; `clone3` → `ERRNO|ENOSYS`; `ftruncate` → the action. Program length stays within `BPF_MAXINSNS`.
- [ ] **Step 2: Run, see them fail.**
- [ ] **Step 3: Implement** the tables from the calls the driver makes (derive them by running the driver tests under `strace -f -c` on both architectures, then list them explicitly with a comment per group; the list lives in code, not generated at runtime).
- [ ] **Step 4: `install` test** in a single-test, `harness = false` binary (`driver/tests/seccomp_self.rs`): install `Fail` with a list that omits `getppid`, call it, see `EPERM`, then read `/proc/self/status` `Seccomp: 2`; mutation proof: drop the TSYNC flag and show a second thread is unfiltered (`/proc/self/task/*/status`).
- [ ] **Step 5: Gate, commit** "Seccomp: allow-list generator and install (FR-PROC-06)", push.

### Task 4: Landlock wrapper

**Files:**
- Create: `driver/src/sys/landlock.rs`, `driver/tests/landlock_self.rs` (`harness = false`, one process restricting itself).
- Modify: `driver/src/sys/mod.rs`, `driver/README.md`.

**Interfaces:**
- Produces: `pub fn abi() -> Abi` (`Abi::Missing | Abi::Version(u32)`, from `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`; `ENOSYS`/`EOPNOTSUPP` → `Missing`); `pub fn restrict_to_reading(roots: &[BorrowedFd<'_>], abi: u32) -> io::Result<()>`: a ruleset handling every filesystem access right the ABI knows, one `LANDLOCK_RULE_PATH_BENEATH` rule per root fd allowing only `READ_FILE | READ_DIR`, then `prctl(PR_SET_NO_NEW_PRIVS)` and `landlock_restrict_self`; `pub fn gaps(abi: u32) -> Vec<Gap>` with `Gap::Truncate` (ABI < 3) and `Gap::DeviceIoctl` (ABI < 5).

- [ ] **Step 1: Failing test** in `landlock_self.rs` (skips with a printed reason when `abi()` is `Missing`, fails instead when `DURCHLAUFERHITZER_REQUIRE_LANDLOCK=1`, set in CI): open a temp root `O_PATH`, restrict, then `openat2` a file beneath the root works, `std::fs::read("/etc/hostname")` fails with `EACCES`, writing a file beneath the root fails. Unit tests for `gaps`.
- [ ] **Step 2: Run, see it fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Gate, commit** "Landlock: restrict file access to reading beneath the roots (FR-PROC-06)", push; CI sets `DURCHLAUFERHITZER_REQUIRE_LANDLOCK=1` on both jobs.

### Task 5: The containment module and startup

**Files:**
- Create: `driver/src/containment.rs` (plus `containment/filter.rs` from Task 3 as its submodule).
- Modify: `driver/src/process.rs` (startup order), `driver/src/log.rs` and `log/format.rs` (`DriverLog::Containment`), `driver/tests/containment.rs` (new, real binary), the spec FR-PROC-04 (the steps) and FR-LOG-05/07 (the new kind).

**Interfaces:**
- Consumes: `Containment` (Task 1), `sys::landlock` (Task 4), `filter::program` and `sys::seccomp` (Task 3).
- Produces:
  - `pub(crate) fn restrict_files(config: &Containment, roots: &[OwnedFd]) -> Result<Outcome, Failure>` (Landlock, called in `start_up` right after `drop_privileges`) and `pub(crate) fn restrict_calls(config: &Containment, landlock: &Outcome) -> Result<Outcome, Failure>` (seccomp, called in `run_loop` after `Driver::new`, before `driver.run()`);
  - `pub enum State { Active, Missing, Partial, Off }`; `Outcome { layer: LayerName, state: State, gaps: Vec<Gap> }`;
  - `DriverLog::Containment { layer, state, gaps, action }`, one line per layer, e.g. `kind=containment layer=landlock state=active abi=4 gaps=device_ioctl covered_by=seccomp`, `kind=containment layer=seccomp state=active action=fail log=kernel` (the README names the one command to see blocked calls: `journalctl -k | grep seccomp`, or `dmesg` without systemd); exact lines pinned in `driver/tests/log.rs`.
  - Rules: `best_effort` + missing → `Missing`, logged, serve; `required` + missing → exit 2 `landlock is required but the kernel has none` (or seccomp likewise); a Landlock gap with seccomp `off` or missing → Landlock `Partial` (required → exit 2 naming the gap); `off` → `Off`, logged.

- [ ] **Step 1: Failing tests** (`driver/tests/containment.rs`, real binary): default config → both `state=active` lines on this CI kernel (6.8) and `/proc/<pid>/status` shows `Seccomp: 2` and `NoNewPrivs: 1`; `landlock off` → its `state=off` line; `seccomp off` + `landlock required` on an ABI < 5 kernel → exit 2 naming `device_ioctl` (skip with reason when the kernel is ABI ≥ 5); `seccomp required` with seccomp hidden by the existing test seccomp filter (`Filter` gains a variant answering `seccomp(2)` with `ENOSYS`) → exit 2; with `best_effort` the same → `state=missing` and serving; a request for a file outside the roots through any route still 404s and a file inside 200s under Landlock.
- [ ] **Step 2: Run, see them fail.**
- [ ] **Step 3: Implement** and update FR-PROC-04's step list (step 7a Landlock, step 9a seccomp).
- [ ] **Step 4: Gate, commit** "Containment at startup: Landlock after the drop, seccomp before the loop (FR-PROC-06)", push.

### Task 6: The whole driver suite under `seccomp_action kill`

**Files:**
- Modify: `driver/tests/support/child.rs` (config writer), `.github/workflows/ci.yml`, `driver/README.md`, and the filter tables (Task 3) for every call this finds.

**Interfaces:**
- Produces: `DURCHLAUFERHITZER_TEST_SECCOMP_KILL=1` makes every config the test helper writes add `containment { seccomp required; seccomp_action kill; landlock required; }` (tests that set their own `containment` keep theirs).

- [ ] **Step 1:** Run the driver tests with the switch on; every killed server (exit by `SIGSYS`) is a missing allow-list entry or an argument check that is too tight. Record each in the report.
- [ ] **Step 2: Fix the tables** (one commit per group of calls with the test that hit it named in the message).
- [ ] **Step 3: CI:** a step "Driver tests under seccomp kill" on both jobs, including the root step's binaries (the switch passed through `sudo env`).
- [ ] **Step 4: Gate, commit** "Tests: the driver suite runs under seccomp kill", push; both jobs green.

### Task 7: Nightly workflow: long sim runs, fuzzing, an older glibc

**Files:**
- Create: `.github/workflows/nightly.yml`, `fuzz/Cargo.toml`, `fuzz/fuzz_targets/{request_head,chunked,response_head,config}.rs`, `fuzz/README.md`.
- Modify: `Cargo.toml` (workspace `exclude = ["fuzz"]`), `sim/tests/faults/run.rs` and `generator.rs` (seed range from `SIM_SEEDS=<from>..<to>`; default unchanged), `scripts/check-product-deps.sh` only if needed so it ignores `fuzz/`, the spec NFR-TEST-08 (exact workflow names), `README.md` (how to run fuzzing and long runs locally).

**Interfaces:**
- Consumes: `core::http::request::parse_head`, `core::http::chunked` (its validator `new` + feed), `core::http::response_head` (its parser), `core::config::load`.
- Produces: fuzz targets that assert no panic and, for the parsers, the documented bounds (never reads past the input; a `Complete` head's length ≤ input length); `nightly.yml` with `schedule` (daily) and `workflow_dispatch` (inputs: seed range, fuzz seconds per target, default 600):
  - job "long runs" (x86_64 and arm64): the faults nets with `SIM_SEEDS=0..200000` (or the input), the late-timer and drain plans as usual; on failure the seed line from the panic is uploaded as an artifact;
  - job "fuzz" (x86_64): nightly toolchain installed in the job only, `cargo fuzz run <target> -- -max_total_time=<s>` per target; crash inputs uploaded as artifacts; the corpus cached between runs;
  - job "older glibc" (x86_64): in a `debian:12` container, build and run the driver tests with `DURCHLAUFERHITZER_TEST_SECCOMP_KILL=1`.

- [ ] **Step 1: Failing test** for `SIM_SEEDS` parsing (a range is accepted, garbage panics with a clear message).
- [ ] **Step 2: Implement** the range, the fuzz crate (each target runs once locally with `cargo +nightly fuzz run <t> -- -runs=1000` if nightly is available here; otherwise note it) and the workflow.
- [ ] **Step 3: Trigger** the workflow by hand (`workflow_dispatch` via the GitHub API) with short times, and quote the run's result in the report; fix what it finds (a fuzz crash is a core bug: stop, fix red/green with a directed test, report it).
- [ ] **Step 4: Gate, commit** "Nightly: long randomized runs, fuzzing, glibc 2.36 under seccomp kill (NFR-TEST-08, NFR-SEC-13)", push.

### Task 8: Net gaps, docs and spec status

**Files:**
- Modify: `sim/tests/faults/proxy/*` (draw "bytes, then reset" backends with `backend_reset_after`, with model outcomes from the spec: a cut relayed response, logged as a backend failure), `sim/tests/faults/oracle.rs` and `shutdown.rs` (a parser model for templates, so a 503 during the drain for a head the parser rejects fails — derive "rejects" from the template table, not from the parser), `README.md` (a short containment section: the block, best effort vs required, the one command that shows blocked calls; owner, 2026-10-29: docs stay terse, less is more), `driver/README.md`, `sim/README.md`, `docs/superpowers/HANDOFF.md` is the controller's.
- Spec: flip FR-CFG-25, FR-CONN-13, FR-PROC-06, NFR-SEC-12, NFR-SEC-13, NFR-PORT-04, NFR-TEST-08 and the `ipv6_prefix` rows to `[done]` only with a pinning test named in the report; Timeline 5 `[done]`.

- [ ] **Step 1:** Mutation proofs for the two net additions (a core that relays a cut response as complete fails the proxy net; a core that answers 503 to a malformed head during the drain fails the static net).
- [ ] **Step 2:** Docs and status.
- [ ] **Step 3: Gate, commit** "Stage 5 nets, docs and spec status", push.
