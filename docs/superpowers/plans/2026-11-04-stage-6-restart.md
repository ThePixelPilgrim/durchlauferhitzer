# Stage 6: Zero-downtime restart — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (mandatory in this repo) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. In this repo, `superpowers:<name>` is the project skill `<name>`.

**Goal:** A restart loses no connection. Without systemd an init script starts a new process beside the old one on shared ports, waits for its readiness, and sends SIGHUP to the old process. The old process closes its listeners, so the kernel migrates their queues to the new process, and serves what it holds with `Connection: close`. The server also builds and passes its suite on Alpine (musl), where the OpenRC script is at home.

**Architecture:**
- **Core:** the drain gains a target, `Shutdown` (today's behaviour) or `Restart`. In a restart drain, requests are served with `Connection: close` instead of 503, idle connections stay until their next request, and late-accepted connections are served. The sim model checks it from the spec.
- **Driver:** the shutdown gate handles SIGHUP as a restart drain bounded by `restart_timeout`. Self-bound listeners set `SO_REUSEPORT` after a plain-bind probe. `--ready-fd <n>` reports readiness after the seccomp filter.
- **Outside the server:** a POSIX `sh` reload helper does the process management and the HAProxy-style handover. The OpenRC script wraps it. The server stays a foreground process with no pid file.
- **Portability:** a musl build on Alpine, x86_64 and arm64 in CI, with seccomp entries per libc where they differ.

**Tech Stack:** Rust (stable, pinned) and `libc` only. The new script is POSIX `sh` (dash, busybox ash) and OpenRC's `openrc-run`. Alpine builds use rustup's `*-unknown-linux-musl` host, dynamically linked against the system musl.

**Spec:** `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md` (binding). Requirements: FR-PROC-07 (all of it), FR-PROC-05 (target restart), FR-PROC-01 (`--ready-fd`), FR-CFG-10 (`SO_REUSEPORT`), FR-CFG-20/21 (`restart_timeout`), NFR-PORT-02 (musl), NFR-MAINT-01. Carry-forwards: HANDOFF "Carry-forwards", stage 5 record "Deferred".

## Global Constraints

- Rust only; the system libc (glibc or musl) is the only C. The product's dependencies stay `libc` only, checked by `scripts/check-product-deps.sh`.
- The stage 5 sandbox stays strict: no new system call in the allow-list for restarts. Neither the running server nor the new process signals another process.
- `unsafe` only in `driver/src/sys/` and the two documented test sites (NFR-MAINT-05).
- Enums over bools, exhaustive matches, files < 900 lines, crate READMEs current, spec statuses updated with the behaviour, docs terse.
- The full gate before every commit: `cargo fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked && scripts/check-product-deps.sh && RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`. Also `DURCHLAUFERHITZER_TEST_SECCOMP_KILL=1 cargo test -p durchlauferhitzer --locked` for driver changes.
- A new server-starting test file goes into CI's `SERVER_TESTS` list.
- If a needed system call or futex op contradicts FR-PROC-06 (for example a requeue futex on musl), stop and report. Do not add it.

## Review Focus

1. **Requests at the moment of SIGHUP:** a pipelined request behind an in-flight response, a head half received, or a request on an idle keep-alive connection must each get a real answer with `Connection: close`, never a 503 or a reset (Tasks 2, 3).
2. **Readiness misuse:**
   - `--ready-fd` naming fd 0–2, a closed descriptor, or a systemd socket must be exit 2 before anything is bound.
   - A startup that fails after binding must close the descriptor without writing (Task 4).
3. **Two reloads in quick succession:** the second handover happens while the first old process still drains. No request may be lost, and the pid file must name the newest process (Task 5).
4. **A handover that cannot be lossless must not start:**
   - With `tcp_migrate_req` unset and not settable, or the new process failing, the helper exits non-zero and the old process keeps serving, untouched (Task 5).
   - An accidental second normal start still fails with `EADDRINUSE` (Task 4).
5. **musl differences:** `clone` flags, futex operations, `membarrier`/`madvise`, and `ppoll` vs `poll` are proven by the whole driver suite under the kill switch on Alpine, not only the happy path (Task 6).

---

### Task 1: Config: `restart_timeout`

**Files:**
- Modify: `core/src/config/model.rs` (`Limits`), `core/src/config/validate/mod.rs` (key table, parse), validate tests, `examples/durchlauferhitzer.conf`, `core/README.md` if it lists keys.

**Interfaces:**
- Produces: `Limits.restart_timeout: Duration`, default `Duration::from_secs(600)`, bounds 1 ms – 24 h like `shutdown_timeout` (FR-CFG-20/21).

- [ ] **Step 1: Failing tests:**
  - the default is 600 s;
  - `limits { restart_timeout 2m; }` parses to 120 s;
  - `0s` and `1441m` are errors at the value;
  - the example config documents the key (the existing key-check test).
- [ ] **Step 2: Run, see them fail.**
- [ ] **Step 3: Implement.** Add the key next to `shutdown_timeout` in the example, with one line on its purpose.
- [ ] **Step 4: Gate, commit** "Config: restart_timeout (FR-CFG-20, FR-PROC-07)", push.

### Task 2: Core: the restart drain, and the sim checks it

**Files:**
- Modify: `core/src/core/shutdown.rs`, `core/src/core/mod.rs` (`Mode`), `core/src/core/request.rs`, `core/src/core/limits.rs` (the refusal during the drain), `core/src/core/contract.rs` (the drain text), `core/README.md`.
- Modify: `sim/src/lib.rs` (`begin_drain`), `sim/tests/faults/shutdown.rs`, `sim/tests/faults/oracle.rs` and/or `model.rs` (the drain oracle), `sim/tests/faults/generator.rs` (the drain plans), `sim/README.md`.
- Modify: `driver/src/event_loop.rs` (the call passes `DrainTarget::Shutdown` until Task 3).

**Interfaces:**
- Produces:
  - `pub enum DrainTarget { Shutdown, Restart }` in `core::core`, re-exported where `Core` is;
  - `Core::begin_drain(&mut self, now: Instant, target: DrainTarget, actions: &mut Vec<Action>)`;
  - `Mode::Draining(DrainTarget)`;
  - `Sim::begin_drain(&mut self, target: DrainTarget)`.
- With `Shutdown`, every behaviour is unchanged (FR-PROC-05b–d). With `Restart` (FR-PROC-05 target restart, FR-PROC-07 restart drain):
  - a request completed during the drain (a partly received head, a pipelined one, one on an idle keep-alive connection) is routed and answered normally, its head saying `Connection: close`;
  - idle keep-alive connections are not closed by `begin_drain`; they end by their next request or their keep-alive timeout;
  - an `Accepted` connection is served like any other (`Connection: close`), not rejected;
  - `is_drained` and `abort_all` are unchanged.

- [ ] **Step 1: Failing core tests** in `core/src/core/` tests, one per bullet above, each with the `Shutdown` twin asserting today's behaviour:
  - `restart_drain_serves_a_pipelined_request_with_close`: a 200 with `Connection: close`, and no 503;
  - `restart_drain_keeps_idle_connections`: no `Close` from `begin_drain`; a later request gets a 200 with close; idle until the keep-alive deadline means a normal close;
  - `restart_drain_serves_an_accepted_connection`;
  - `restart_drain_answers_a_partial_head_once_complete`.
- [ ] **Step 2: Run, see them fail.**
- [ ] **Step 3: Implement.** Thread the target through `Mode`. Where `Mode::Draining` chooses the 503 (`request.rs` `refuse_during_drain`, `limits.rs`), the restart target takes the serving path and forces `Persistence::Close`.
- [ ] **Step 4: Sim, failing first.**
  - The randomized drain plans (`seed % 5 == 0`, `Drain | DrainThenAbort | Abort`) gain `Restart` and `RestartThenAbort`, drawn so that each still appears in the per-push seed range (add a coverage gate like the existing ones).
  - The oracle is written from the spec, not from the core. In a restart drain, a request whose bytes were read must get its normal answer with close, never 503. Idle connections may stay until their keep-alive deadline. The unanswered check applies as for `Drain`.
  - Show a mutation that answers 503 in a restart drain failing both nets.
- [ ] **Step 5: Gate** (the per-push nets plus `SIM_SEEDS=0..20000` once locally), **commit** "Core: restart drain serves with Connection: close (FR-PROC-05, FR-PROC-07)", push.

### Task 3: Driver: SIGHUP starts the restart drain

**Files:**
- Modify: `driver/src/event_loop/shutdown.rs` (the gate), `driver/src/event_loop.rs` (`watch_signals`, `begin_drain`), `driver/src/process.rs` (passes both timeouts), `driver/src/log.rs` and `driver/src/log/format.rs` (`phase=restarting`), the spec's FR-LOG-05 shutdown phases, `driver/README.md`.
- Test: `driver/src/event_loop/shutdown.rs` units; `driver/tests/shutdown.rs` (binary).

**Interfaces:**
- Consumes: `DrainTarget`, `Core::begin_drain(now, target, actions)` (Task 2); `Limits.restart_timeout` (Task 1).
- Produces:
  - `ShutdownGate::new(shutdown_timeout: Duration, restart_timeout: Duration)`;
  - `Phase::Draining { deadline: Instant, target: DrainTarget }`;
  - `Step::Drain(DrainTarget)`;
  - `watch_signals(fd, shutdown_timeout, restart_timeout)`;
  - the log entry `kind=shutdown phase=restarting`.

  The gate's transitions (FR-PROC-07):

  | Phase | Signal | Result |
  |---|---|---|
  | Running | SIGHUP | `Drain(Restart)`, deadline `restart_timeout` |
  | Restart drain | SIGHUP | ignored |
  | Restart drain | SIGTERM or SIGINT | `Abort` |
  | SIGTERM drain | SIGHUP | ignored |
  | any | SIGPIPE | ignored |

- [ ] **Step 1: Failing unit tests** for every row of that table, plus the restart deadline aborting at `restart_timeout`.
- [ ] **Step 2: Failing binary tests** in `driver/tests/shutdown.rs`:
  - `sighup_serves_in_flight_and_pipelined_requests_with_close_then_exits_0`;
  - `sighup_closes_the_listeners` (a new connect is refused when no other process shares the port);
  - `sighup_keeps_an_idle_connection_until_its_next_request` (it gets a 200 with `Connection: close`);
  - `sighup_then_sigterm_aborts`;
  - `restart_timeout_cuts_a_stalled_download` (a 1 s `restart_timeout`, a client that stops reading; exit 0 and a `phase=aborting` line);
  - `sighup_logs_phase_restarting`;
  - `sighup_with_systemd_sockets_drains_the_same` (in `driver/tests/systemd.rs`).

  All of them also pass under the kill switch.
- [ ] **Step 3: Run, see them fail.**
- [ ] **Step 4: Implement.** The restart drain closes the listeners exactly like the SIGTERM drain (systemd's descriptors included, which only closes this process's copies). Update the module doc ("SIGHUP: stage 4" goes).
- [ ] **Step 5: Gate (both suites), commit** "Driver: SIGHUP starts the restart drain (FR-PROC-07)", push.

### Task 4: Driver: shared ports and `--ready-fd`

**Files:**
- Modify: `driver/src/sys/socket.rs` (`tcp_listen`, the unit test `listeners_set_reuseaddr_and_not_reuseport` becomes `..._and_reuseport`), `driver/src/process.rs` (CLI, `bind`, readiness), `driver/README.md`, `README.md` (usage), the spec's FR-PROC-01/FR-CFG-10 status tags.
- Test: `driver/tests/cli.rs`, `driver/tests/startup.rs`.

**Interfaces:**
- Produces:
  - `durchlauferhitzer --ready-fd <n> <config-path>`. The usage text gains the line `       durchlauferhitzer --ready-fd <n> <config-path>`.
  - `Command::Serve { path: PathBuf, ready: Option<ReadyFd> }`.
  - `sys::socket::tcp_listen(addr, backlog, probe: Probe)` with `enum Probe { First, Skip }`. `First` binds a plain `SO_REUSEADDR` socket and closes it (an existing group or user gives `EADDRINUSE`, the same startup error as today), then binds the listener with `SO_REUSEADDR` and `SO_REUSEPORT`. A start with `--ready-fd` uses `Skip`.
- Readiness (FR-PROC-07 step 3):
  - **Checked early:** `n` is checked right after the arguments (before the config's effects). It must be ≥ 3 and open, not a listening socket, and not one of systemd's `LISTEN_FDS` range. Otherwise exit 2: `durchlauferhitzer: --ready-fd <n>: <reason>`. A non-number is a usage error (exit 1).
  - **Kept out of the way:** the descriptor is counted in the budget check and is not a listener.
  - **Written last:** after step 10a, before the loop, the server writes `ready\n` (one `write`, retried on `EINTR`/partial) and closes it. A write error is logged as a driver log entry and the server keeps serving.
  - **A failed start writes nothing:** any exit before that closes it unwritten (process exit is enough).

- [ ] **Step 1: Failing tests:**
  - CLI: `--ready-fd` with no number, `x`, `2`, or a closed fd; the usage text.
  - Startup:
    - `ready_fd_gets_ready_then_eof`: the test passes one end of a pipe via the `pre_exec` helper's descriptor setup; the read gives `ready\n` then EOF; a request after it is served;
    - `ready_fd_is_closed_unwritten_when_startup_fails` (a missing static root): EOF with no bytes, exit 2;
    - `a_second_normal_start_fails_with_eaddrinuse` (the probe; FR-CFG-10's message);
    - `a_ready_fd_start_shares_the_port_with_a_running_server`: both up; connections are answered.
  - All under the kill switch too.
- [ ] **Step 2: Run, see them fail.**
- [ ] **Step 3: Implement.** No allow-list change is expected: the write and close happen with calls already listed. If the kill switch says otherwise, stop and report.
- [ ] **Step 4: Gate (both suites), commit** "Driver: SO_REUSEPORT listeners and --ready-fd (FR-PROC-07, FR-PROC-01)", push.

### Task 5: The reload helper, the OpenRC script, and the lossless handover proven

**Files:**
- Create: `contrib/durchlauferhitzer-reload` (POSIX `sh`, executable), `contrib/openrc/durchlauferhitzer` (init script), `contrib/openrc/durchlauferhitzer.confd`, `contrib/README.md`.
- Create: `driver/tests/restart.rs` (server-starting; add to CI `SERVER_TESTS`).
- Modify: `.github/workflows/ci.yml` (the root step sets `sysctl -w net.ipv4.tcp_migrate_req=1` and exports `DURCHLAUFERHITZER_REQUIRE_MIGRATE=1`), `README.md` ("Restart without systemd": the steps, `tcp_migrate_req`, the stage 5 upgrade needing one full stop and start, binding as root then `user`, a dropped `listen` address losing its queue), `driver/README.md`.

**Interfaces:**
- `durchlauferhitzer-reload <binary> <config> <pidfile> <logfile> [ready-timeout-seconds, default 30]`. In order:
  1. Run `<binary> --check <config>`; on failure, print its output and exit 1.
  2. Ensure `/proc/sys/net/ipv4/tcp_migrate_req` is 1, writing it if it is 0. If it is missing or not writable, exit 1 naming it.
  3. Read the old pid from `<pidfile>` and check that `readlink /proc/<pid>/exe`, minus a trailing ` (deleted)` (an upgrade replaces the file), is `<binary>`'s real path. If not, exit 1 and start nothing.
  4. Start `<binary> --ready-fd 3 <config>` in the background with fd 3 the write end of a FIFO in a fresh `mktemp -d`. Its stdout and stderr are appended to `<logfile>`, and it runs in its own session (`setsid`).
  5. Wait for `ready` on the FIFO, bounded by the timeout. On EOF or timeout, kill the new process if it is still alive, exit 1, and leave the old process untouched.
  6. Write the new pid to `<pidfile>` (via a temp file, then rename), send `SIGHUP` to the old pid, and exit 0.
- The OpenRC script (`#!/sbin/openrc-run`, modelled on Alpine's haproxy script):
  - `command`, `command_args`, `pidfile`, `command_background=yes`, `output_log`/`error_log` from conf.d;
  - `checkconfig` runs `--check`;
  - `start_pre` runs `checkconfig`;
  - `reload` (in `extra_started_commands`) calls the helper with the script's variables;
  - `stop` uses OpenRC's default SIGTERM.

- [ ] **Step 1: Failing end-to-end tests** in `driver/tests/restart.rs`. They run as root with `tcp_migrate_req = 1`; they skip with a reason otherwise, and fail when `DURCHLAUFERHITZER_REQUIRE_MIGRATE=1`. They drive the helper with `sh` (dash on Ubuntu):
  - `reload_under_load_loses_nothing`:
    - the test starts the server itself and writes its pid file;
    - run clients that issue keep-alive and fresh requests in a loop; also queue connections in the accept backlog by holding the old process stopped (`SIGSTOP`) across the reload's SIGHUP, then `SIGCONT`;
    - reload with a config whose route answers differently, and with the binary replaced by a copy (so the old `/proc/<pid>/exe` reads ` (deleted)`);
    - assert that every request got a complete response (old or new answer), no connection was reset or refused, the old pid exited 0, the pid file names the new process, and later requests see the new route.
  - `two_reloads_back_to_back_lose_nothing`.
  - `a_bad_config_reload_changes_nothing`: the helper exits 1, the same pid serves.
  - `a_new_process_that_fails_leaves_the_old_untouched` (a missing root in the new config, with `--check` passing): exit 1, the old serves, no stray process.
  - `a_stale_pidfile_is_refused` (a pid that is not the binary).
  - `a_dropped_listen_address_is_closed` (documented behaviour).
  - Control: the same load with `tcp_migrate_req = 0` set by the test shows resets. If this cannot be made deterministic, say so in the report and keep the test out.
- [ ] **Step 2: Run, see them fail.**
- [ ] **Step 3: Implement** the helper and the OpenRC files. Run `shellcheck` if it is available locally (it is test-only tooling) and say so in the report. Docs stay terse.
- [ ] **Step 4: Gate.** Run the restart tests as root locally (this container runs as root; set the sysctl). **Commit** "Restart: reload helper, OpenRC script, lossless handover proven (FR-PROC-07)", push.

### Task 6: musl on Alpine

**Files:**
- Modify: `driver/src/containment/filter.rs` (per-libc entries via `cfg(target_env = "musl")`, only where musl's calls differ, each with a comment naming the musl function), `driver/src/sys/*` only if a `libc` item differs on musl, `.cargo/config.toml` (create if absent: `-C target-feature=-crt-static` for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`, so the binary links the system musl dynamically), `.github/workflows/ci.yml`, `README.md` (requirements: glibc or musl), `driver/README.md`, the spec's NFR-PORT-02 status.

**Interfaces:**
- CI gets a job `alpine (x86_64)` and `alpine (arm64)`:
  - Container `alpine` pinned by digest, on `ubuntu-latest` / `ubuntu-24.04-arm`. Install rustup's toolchain from `rust-toolchain.toml`, plus `openrc`, `dash` or busybox `sh`, `curl`.
  - Run the gate's tests and clippy, `scripts/check-product-deps.sh`, and the driver suite with and without the kill switch, as root, with `DURCHLAUFERHITZER_REQUIRE_ROOT=1`, `DURCHLAUFERHITZER_REQUIRE_LANDLOCK=1` and `DURCHLAUFERHITZER_REQUIRE_MIGRATE=1`.
  - Run the OpenRC script end to end: put the script in `/etc/init.d`, initialise OpenRC in the container (`openrc` with `/run/openrc/softlevel`), `rc-service durchlauferhitzer start`, a request, `reload`, a request, `stop`.
  - The container options follow the nightly Debian job (no Docker seccomp profile, `CAP_SYS_PTRACE` if needed).
- The cdylib clock shim (`driver/tests/support/clock_syscall.rs`) must work on musl. If `LD_PRELOAD` cannot work there, the test skips on musl with the reason, and the report says so.

- [ ] **Step 1: Red first.** Add the CI jobs and push to the task branch. The expected first failures (SIGSYS under the kill switch, build differences) are the red. Record each failing call and its musl caller.
- [ ] **Step 2: Fix each:**
  - an allow-list entry under `cfg(target_env = "musl")` with a BPF-interpreter unit test (in `filter/tests.rs`, compiled on musl);
  - or a code change if the call is avoidable.
  - Anything contradicting FR-PROC-06's argument checks: stop and report.
- [ ] **Step 3: CI green on all four jobs** (two glibc, two Alpine). Run the gate locally on glibc.
- [ ] **Step 4: Commit(s)** "Portability: musl builds on Alpine (NFR-PORT-02)" (one per fix group is fine), push.

### Task 6b: Steer new connections before the old listener closes (owner decision 2026-11-05)

Root cause and evidence: `.superpowers/sdd/2026-11-04-stage-6-restart/reset-investigation.md` (git-ignored; the controller passes it to the implementer). Spec: FR-PROC-07 "Steering".

**Files:**
- Modify: `driver/src/sys/socket.rs`, `driver/src/process.rs`, `contrib/durchlauferhitzer-reload`, `driver/tests/restart.rs` (stay < 900 lines; split helpers into `support/` if needed), `driver/README.md`, `contrib/README.md`, `README.md` ("Restart without systemd": one line on the grace).

**Interfaces:**
- `sys::socket::steer_reuseport(listener: BorrowedFd<'_>, index: u32) -> io::Result<()>`: attaches a one-instruction classic BPF program returning `index` (`SO_ATTACH_REUSEPORT_CBPF`, defined locally as 51, asm-generic; check aarch64 and x86_64).
- In a `--ready-fd` start:
  - right after binding: `steer_reuseport(fd, u32::MAX)` (neutral: an out-of-range index falls back to the hash);
  - as the last step before the seccomp filter: `steer_reuseport(fd, 1)`.
  - A failure is a driver log entry (name the kind) and startup goes on.
- The helper:
  - waits a grace after `ready` before `kill -HUP` (default 1 s; an optional sixth argument);
  - after SIGHUP, waits (bounded, default 10 s) until the old pid holds no listening socket, comparing `/proc/<old>/fd` socket inodes with the LISTEN entries of `/proc/net/tcp` and `/proc/net/tcp6`;
  - releases its lock only then. On timeout it releases with a warning on stderr; the exit status stays 0, since the handover itself succeeded.

- [ ] **Step 1: Failing tests:**
  - unit (`sys/socket.rs`): listeners A and B share a port; after `steer_reuseport(B, 1)`, 64 connects all reach B and A's accept gives `EAGAIN`; after A closes, connects still reach B.
  - `restart.rs` `ready_steers_new_connections_to_the_new_process`: after `ready`, before any SIGHUP, 50 fresh requests all answer from the new process.
  - `restart.rs` `reload_under_fresh_storm_loses_nothing`: about 12 fresh-connection clients across several back-to-back reloads, zero failures.
  - The helper waiting for the old listeners: lock released only after they closed.
- [ ] **Step 2: Run, see them fail.** The storm test should fail on `main` within a few runs (about 1 in 80 per reload locally). Record the evidence.
- [ ] **Step 3: Implement.** No allow-list change (both calls run before the filter); the kill switch confirms it.
- [ ] **Step 4: Gate (both suites), CI green on all four jobs. Commit** "Restart: steer new connections before the old listener closes (FR-PROC-07)", push.

### Task 7: Stage close-out (controller, inline)

- [ ] Spec statuses: FR-PROC-07, FR-PROC-05, FR-PROC-01, FR-CFG-10, FR-CFG-20/21 (`restart_timeout`), NFR-PORT-02 → `[done]`; Timeline 6 `[done]`; the legend "stages 1–6".
- [ ] HANDOFF: stage 6 done, next the stage 6 whole-branch review.
- [ ] Gate, push, fast-forward `main`.
