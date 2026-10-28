# Stage 4: epoll Driver and First Runnable Binary — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (mandatory in this repo) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. In this repo, `superpowers:<name>` is the project skill `<name>`.

**Goal:** A runnable `durchlauferhitzer` binary. It serves static files and proxies over real sockets through the unchanged sans-IO core. The production epoll driver follows the driver contract exactly. The binary adds a disk worker pool, non-blocking logging, the CLI, startup checks, binding or systemd socket activation, privilege drop and signal-driven shutdown.

**Architecture:**
- **Crate.** A new crate, `driver/` (package `durchlauferhitzer`, a binary plus a library for tests), owns all I/O, threads and process setup. `libc` is its only dependency.
- **Unsafe code.** Every `unsafe` block sits in one documented module, `driver/src/sys/`, of thin, checked wrappers. The rest of the crate is safe Rust.
- **The core.** It gains only what the spec assigns to it in this stage: a wall-clock `Date`, a shutdown API, the remaining config bounds and keys, and the FR-LOG-07 fields.
- **The sim.** It gains a late-timer mode.
- **Smoke tests.** They run the real binary or driver over loopback.

**Tech Stack:** Rust 2024; core std only; the driver uses `libc` (pinned in `Cargo.lock`; no build script and no C, checked with `cargo tree`); the sim is test-only.

**Spec:** `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md`. It is binding, and the plan argues from it. Relevant requirements:
- every `[stage 4]` and `[done in core; driver stage 4]` requirement: FR-PROC-01…05a, FR-LOG-05…07, FR-HTTP-22a, FR-CFG-10 (bind-time overlap), FR-CFG-15 (root exists);
- NFR-SEC-04/07/11/12, NFR-RES-05, NFR-PORT-01…05, NFR-MAINT-04/05, NFR-PERF-04, NFR-TEST-07;
- DRV-01…12.

The binding driver text is the module docs of `core/src/core/contract.rs`. Carry-forwards: `docs/superpowers/reviews/2026-10-27-stage-3-review.md` ("Deferred to later stages"), `docs/superpowers/HANDOFF.md`.

## Owner decisions (2026-10-27, before planning)

1. **FR-CFG-21 bounds: a memory budget.**
   - Fixed caps per key, plus a new `limits` key `memory_limit` (a size, default 1 GiB).
   - At config load the core computes the worst-case memory per connection from the limits and checks `max_connections × worst case ≤ memory_limit`. A config that fails is a config error naming the `limits` block (exit 1).
2. **Packaging.** The driver is the new crate `driver/` (package `durchlauferhitzer`). It has a binary and a library, its only dependency is `libc`, and its `unsafe` sits in `driver/src/sys/`. `core` stays free of dependencies and of `unsafe`.
3. **Disk worker pool size.** A `limits` key `disk_threads`, default 4, at least 1.
4. **Log lines** (FR-LOG-05). One `key=value` line per entry on stderr, with no timestamp; journald and collectors add their own.

## Spec amendments (decided while planning; listed for the owner's review; applied to the spec in the task that implements them, before code relies on them)

- **A1, per-key caps (FR-CFG-21).**

  | Key | Bound |
  |---|---|
  | `max_connections` | 1–65536 |
  | `max_connections_per_ip` | 1–`max_connections` |
  | `max_request_line` | ≤ `max_head`, as now |
  | `max_head` | ≤ 1 MiB, as now |
  | `max_headers` | 1–1000 |
  | each duration | 1 ms – 24 h |
  | `disk_threads` | 1–64 |
  | `memory_limit` | 1 MiB – 1 TiB |
  | `shutdown_timeout` | 1 ms – 24 h |

  Worst case per connection = client receive capacity + send buffer + the proxy exchange's upstream receive buffer + head buffer, using the NFR-RES-01 formulas. Static-only configs leave out the proxy terms.
- **A2, `shutdown_timeout` (FR-PROC-05).** It becomes a `limits` key, default 30s. It is documented in the example config and listed in `LimitKey::ALL`.
- **A3, Date (FR-HTTP-22a).**
  - The driver tells the core the wall-clock second with `Core::set_date(unix_secs: u64)` whenever the second changes, before the next `handle` call. The core keeps the rendered IMF-fixdate (29 bytes) and adds `Date: <IMF-fixdate>` to every response it generates: fixed responses, 301/414, file heads, 100 Continue is excluded.
  - Proxied responses keep the backend's `Date` as received (FR-PROXY-12). If the backend sent none, the core adds its own. This makes FR-HTTP-22a's "core-generated" explicit.
  - Before the first `set_date` (the sim's default), no `Date` header is sent. The sim sets a fixed date in tests that check it.
  - The send-buffer bound (1 KiB) and the head-buffer slack are rechecked for the 37 added bytes.
- **A4, shutdown API in the core (FR-PROC-05, core half).**
  - `Core::begin_drain(now, &mut actions)` starts a graceful drain:
    - idle keep-alive connections are closed at once;
    - a connection in `ReadingHead` with no byte yet is closed;
    - a request that completes during the drain is answered 503 with `Connection: close` (FR-PROC-05 "target shutdown");
    - every in-flight response finishes with `Connection: close` semantics, ending the connection after it.
  - `Core::is_drained(&self) -> bool` is true once no connection, upstream, file or orphaned open is left.
  - `Core::abort_all(&mut actions)` is the fast path. It closes every connection, upstream and open file at once (`Log` entries as for a reset), and leaves orphaned opens to be answered later as usual.
  - *Amended by owner decisions 2026-10-27 (Task 2 review), written into the spec:* a request pipelined behind an in-flight response during the drain gets 503 with `Connection: close` (FR-PROC-05b); `abort_all` logs `Rejected { ShuttingDown }` per cut exchange instead of reset-like entries, and the driver exits after it without waiting for orphaned opens (FR-PROC-05c, DRV-14); a connection delivered during the drain is logged `Rejected { ShuttingDown }` (FR-PROC-05d).
  - The driver closes the listeners itself before `begin_drain`.
- **A5, FR-LOG-07 (core half).** `Rejected` for a parse error found after a valid request line carries the method and the raw path: `RejectReason::Http` gains an optional `RequestSummary`-like `method + path`. Driver-bug closes and `EMFILE`/`ENFILE` get their own driver log kinds (`DriverLog`, Task 7). They are not core `Log`s.
- **A6, bind overlap (FR-CFG-10).** Binding is attempted for every `listen` address in config order. The first failure (`EADDRINUSE`, for example `0.0.0.0:80` plus `127.0.0.1:80`, or `EACCES`) is a startup error naming the address (exit 2). `SO_REUSEADDR` is set on listeners and `SO_REUSEPORT` is not (stage 7). *Amended by owner decision 2026-10-27 (Task 4 review), written into the spec (FR-CFG-10):* every IPv6 listener sets `IPV6_V6ONLY = 1`, so an IPv6 address accepts IPv6 only and `0.0.0.0:80` + `[::]:80` bind together on every host; IPv4 needs its own `listen` line.
- **A7, systemd sockets (FR-PROC-02).**
  - `LISTEN_PID` must equal our pid and `LISTEN_FDS` ≥ 1. Descriptors start at 3.
  - Each must be a listening `AF_INET`/`AF_INET6` stream socket (`SO_ACCEPTCONN`).
  - Each one's local address must be one of the configured `listen` addresses, and every configured address must be covered, or startup fails (exit 2).
  - `LISTEN_FDNAMES` is ignored.
  - The variables are cleared after use.
  - *Owner decision 2026-10-28 (Task 8 review, O2), written into the spec (FR-PROC-02):* the activation environment is read strictly: one variable without the other, a non-number, a foreign `LISTEN_PID`, more sockets than addresses or two sockets on one address are exit 2.
- **A8, privilege drop (FR-PROC-03).**
  - ~~Without systemd sockets: if any `listen` port is < 1024 and euid ≠ 0, startup fails (exit 2).~~ *Retired by owner decision 2026-10-28 (Task 8 review, O7), written into the spec (FR-PROC-03):* no up-front port check; a bind refused with `EACCES` is the startup error naming the address (A6).
  - After binding and opening the static roots, if `user` is set, the driver calls `setgroups(1, [gid])`, `setgid` and `setuid` (looked up with `getpwnam_r`). It then checks that `setuid(0)` fails.
  - If euid is 0 and `user` is unset, startup fails (exit 2): never root after binding.
  - With systemd sockets and no root, nothing is dropped.
  - *Owner decision 2026-10-28 (Task 8 review, O3), written into the spec (FR-PROC-03):* root with systemd sockets and no `user` is exit 2 too.
- **A9, signals.**
  - SIGTERM, SIGINT and SIGPIPE are handled via `signalfd` in the epoll set. SIGPIPE is blocked; writes use `MSG_NOSIGNAL` or `send`.
  - First SIGTERM: graceful drain. A second SIGTERM, a SIGINT, or `shutdown_timeout` expiry: `abort_all`, then exit 0.
  - SIGHUP is ignored in stage 4. The restart signal is stage 6.
- **A10, worker pool contract.**
  - `disk_threads` OS threads take jobs (open, readahead) from one `Mutex<VecDeque>` with a `Condvar`, and post results to a second queue plus one `eventfd` write.
  - The loop drains all results on each eventfd readiness, in any order (DRV-08). A `CloseFile` while a readahead is in flight marks the file "closing". Its descriptor is closed only when that worker's result arrives, which is then dropped (DRV-09).
  - Opens use `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` with `O_RDONLY | O_NONBLOCK | O_NOCTTY | O_CLOEXEC`, then `fstat`. Readahead uses `readahead(2)`.
  - Errno maps to `FileError` by the contract's table.
- **A11, startup order (FR-PROC-04).**
  1. Parse args.
  2. Load the config (exit 1).
  3. Check the kernel for `openat2` with a probe open of `"."` beneath the first root, or of `/` if there is none (exit 2).
  4. Open every static root (`O_PATH | O_DIRECTORY | O_CLOEXEC`; a missing root, exit 2).
  5. Take the systemd sockets or bind (exit 2).
  6. Check `RLIMIT_NOFILE` against NFR-RES-05: try to raise the soft limit up to the hard limit first; exit 2 if it is still too low.
  7. Drop privileges.
  8. Start the workers.
  9. Run the loop.
  - *Owner decision 2026-10-28 (Task 8 review, O1), written into the spec (FR-PROC-04):* no check of fds 0–2 before step 1: the Rust runtime opens closed fds 0–2 on `/dev/null` before `main`, so nothing lands on them and a server with stderr closed runs with its log discarded (without `/dev/null` the runtime aborts).

## Global Constraints

- Everything from stages 1–3 still holds:
  - zero product dependencies in `core`; no `unsafe` in `core`;
  - the denied clippy lints; no `_ =>` on enums; enums over bools;
  - no `too_many_arguments` allow;
  - illegal states unrepresentable; bounded memory;
  - files under 900 lines; crate READMEs current;
  - strict red/green TDD with real RED output, and every behaviour decision in the spec.
- **`driver` crate:**
  - `#![deny(unsafe_code)]` at the crate root;
  - `#[allow(unsafe_code)]` only on `mod sys`;
  - every `unsafe` block has a `// SAFETY:` comment;
  - every wrapper returns `io::Result` and checks `-1`/errno;
  - no `unwrap`/`expect`/`panic`/indexing outside tests (the workspace lints apply).
- **Product dependency tree.** `durchlauferhitzer-core` has no dependencies, and `durchlauferhitzer` depends only on `libc`. `scripts/check-product-deps.sh` checks both packages, and CI runs it (NFR-MAINT-04).
- **The driver follows `core/src/core/contract.rs` to the letter.** Where the contract leaves a choice, the task decides it, documents it in `driver/README.md`, and writes it into the spec when the choice is visible behaviour.
- **Exhaustive matching.** Every `Action` is matched exhaustively in the driver, so a new action is a compile error.
- **Linux only.** The crate states `#[cfg(target_os = "linux")]` at the root and fails to build elsewhere with a clear message (NFR-PORT-01).
- **Gate before every commit:** `cargo fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked && scripts/check-product-deps.sh`. Smoke tests must pass in the container (unprivileged; root-only steps are skipped with a printed reason).

## Review Focus

1. **A client that resets mid-`sendfile`, or a backend that resets mid-relay.**
   - Expect: no panic, no fd leak; `EPIPE`/`ECONNRESET` are reported as `Failed`/`UpstreamFailed`.
   - Tests: Tasks 5 and 6 (fd count before and after, via `/proc/self/fd`).
2. **A blocked disk** (a FIFO in a static root, or a slow `openat2`).
   - Expect: other connections keep being served; the FIFO answers 404 because of `O_NONBLOCK`.
   - Test: Task 6.
3. **Log injection.** A header value or path with control or non-ASCII bytes, or a `Host` with odd bytes.
   - Expect: one escaped line per entry, never a split line.
   - Test: Task 7.
4. **SIGTERM during an upload, and during a slow download.**
   - Expect: the in-flight response finishes, new requests on open connections get 503 with close, and the listener refuses new connections.
   - Test: Task 9.
5. **`RLIMIT_NOFILE` too low, a missing root, or a port in use.**
   - Expect: a clear message and exit 2, with no partial start.
   - Test: Task 8.

---

### Task 1: Config bounds, memory budget and the new limit keys

**Files:**
- Modify: `core/src/config/model.rs`, `core/src/config/validate/mod.rs`, `core/src/config/validate/tests/limits.rs`, `examples/durchlauferhitzer.conf`, spec (FR-CFG-20, -21; NFR-RES-01; Open decisions item 1 → decided)

**Interfaces:**
- Produces:
  - `Limits.disk_threads: u32`, default 4;
  - `Limits.shutdown_timeout: Duration`, default 30s;
  - `Limits.memory_limit: u64` (bytes), default 1 GiB;
  - all three in `LimitKey::ALL`;
  - `pub fn worst_case_per_connection(limits: &Limits, proxied: bool) -> u64` in `core/src/config/model.rs`, the documented NFR-RES-01 formula;
  - `ConfigErrorKind::MemoryBudget { needed: u64, limit: u64 }`, reported at the `limits` block, or at the first `host` if there is no `limits` block.

- [ ] **Step 1: Write the failing tests.**
  - Each A1 bound: the lowest and highest accepted values, and one past each, rejected with line:column.
  - Defaults for the new keys.
  - `max_connections_per_ip` greater than `max_connections` is rejected.
  - `worst_case_per_connection` matches a hand-computed value for defaults with and without a proxy route.
  - The budget is exceeded with `max_connections 65536` plus `max_head 1MiB`, giving a `MemoryBudget` error with exact `needed`.
  - The example config still loads and documents every key.
- [ ] **Step 2: Run** `cargo test -p durchlauferhitzer-core config` and see them fail.
- [ ] **Step 3: Implement.** Write A1 and decision 1 into the spec, move Open decision 1 to decided, and document the keys in the example config with the arithmetic.
- [ ] **Step 4: Run the gate.**
- [ ] **Step 5: Commit** "Config bounds, memory budget, disk_threads and shutdown_timeout" and push.

### Task 2: Core: `Date`, drain/abort API, FR-LOG-07 fields

**Files:**
- Modify: `core/src/core/{mod.rs, contract.rs, request.rs, proxy.rs, relay.rs, limits.rs}`, `core/src/http/{response.rs, forward.rs}`, `core/src/log.rs`, core tests, `sim/src/lib.rs` (`Sim::set_date`, `Sim::begin_drain`, `Sim::abort_all` pass-throughs), `sim/tests/support/model_http.rs` (the model learns the `Date` line), and the spec (FR-HTTP-22a, FR-PROC-05, FR-LOG-07, DRV text).

**Interfaces:**
- Produces:
  - `Core::set_date(&mut self, unix_secs: u64)`;
  - `Core::begin_drain(&mut self, now: Instant, actions: &mut Vec<Action>)`;
  - `Core::is_drained(&self) -> bool`;
  - `Core::abort_all(&mut self, actions: &mut Vec<Action>)`;
  - `RejectReason::Http` carries `Option<(Method, String /*raw path*/)>` (or a small named struct). A request-line error has none.
- Rules: A3, A4 and A5 as above. Add contract docs for each new call: when the driver calls it, and what it may emit. The IMF-fixdate is rendered by a pure function `http::date::imf_fixdate(unix_secs) -> [u8; 29]`, with no time crate (days-from-civil arithmetic).

- [ ] **Step 1: Write the failing tests.**
  - `imf_fixdate` table: 0 gives `Thu, 01 Jan 1970 00:00:00 GMT`; also 951782400 (2000-02-29), 4102444799 and 253402300799.
  - Every core-generated response (each status, 301/414, file head) carries `Date` after `set_date`, and the head bounds still hold, so `every_rendered_head_fits_the_send_buffer` extends to the date.
  - A proxied response keeps the backend's `Date` byte-exact, and gets ours only when the backend sent none.
  - Drain cases in the sim:
    - an idle keep-alive is closed;
    - an in-flight static download completes, then closes;
    - a request completed during the drain gets a 503 with `connection: close`;
    - a proxied exchange completes;
    - `is_drained` flips exactly when the last resource goes;
    - `abort_all` closes everything, each upstream once, each file once, and orphans stay until their answer arrives.
  - FR-LOG-07: `HEAD / HTTP/1.1` with a bad header logs `Rejected` carrying `HEAD` and `/`, while a bad request line carries none.
  - The randomized nets still pass; their model adds `Date` where the spec says.
- [ ] **Step 2: Run them and see them fail.**
- [ ] **Step 3: Implement** (spec first, then code).
- [ ] **Step 4: Run the gate.**
- [ ] **Step 5: Commit** "Core: Date header, drain and abort, method and path on late parse errors" and push.

### Task 3: Sim: late-timer mode (NFR-TEST-07, sim half)

**Files:**
- Modify: `sim/src/lib.rs`, `sim/tests/faults/` (run a share of the seeds in late-timer mode), `sim/README.md`, spec NFR-TEST-07.

**Interfaces:**
- Produces: `Sim::set_late_timers(&mut self, on: bool)` (*changed in execution, controller ruling R1: enums over bools, so it is `Sim::set_timer_order(TimerOrder::OnTime | TimerOrder::Late)`, default `OnTime`*). When it is on, `advance` delivers every queued I/O and disk completion that is due before an overdue `TimerFired`. This is how a real loop wakes with both ready at once. The core's handling must be correct either way.

- [ ] **Step 1:** Write directed tests where the order matters:
  - the client's last body byte and its body-idle deadline arrive together;
  - a backend's head byte and the response timeout arrive together;
  - a `FileOpened` and its disk timeout arrive together.

  Each test asserts the spec's outcome in late mode, and that the outcome is consistent with on-time mode, which may differ only where the spec allows. Mark the randomized nets to run one seed in four with late timers.
- [ ] **Step 2: Run them.** A failure is a core bug; fix it red/green with a pinned test. If the spec is ambiguous, stop and report it to the controller.
- [ ] **Step 3: Run the gate**, then **commit** "Sim: late-timer mode" and push.

### Task 4: `driver` crate skeleton and the `sys` wrapper module

**Files:**
- Create:
  - `driver/Cargo.toml` (package `durchlauferhitzer`; `[lib]` plus `[[bin]] name = "durchlauferhitzer"`; depends on `libc` with `default-features = false` if it builds, otherwise the defaults, justified in the commit, and on `durchlauferhitzer-core`);
  - `driver/src/lib.rs`, `driver/src/main.rs` (a stub that prints usage);
  - `driver/src/sys/mod.rs` with `epoll.rs`, `socket.rs`, `fd.rs`, `file.rs`, `eventfd.rs`, `signal.rs`, `user.rs`, `rlimit.rs`;
  - `driver/README.md`.
- Modify: root `Cargo.toml` (workspace member), `scripts/check-product-deps.sh` (also check `-p durchlauferhitzer`, allowing only `libc`), `.github/workflows/ci.yml` if needed, `Cargo.lock`.

**Interfaces:**
- Produces (all safe to call, each returning `io::Result`; `OwnedFd` from std wherever possible):
  - `epoll`: `Epoll::new()`, `add/modify(fd, token: u64, Readiness)`, `delete(fd)`, `wait(&mut [EpollEvent], timeout: Option<Duration>) -> usize`; `Readiness { read, write }` as an enum `None | Read | Write | ReadWrite` mirroring `Interest`; level-triggered.
  - `socket`: `tcp_listen(addr, backlog)` (with `SO_REUSEADDR`), `accept_nonblocking(listener) -> Option<(OwnedFd, SocketAddr)>` (`accept4` with `SOCK_NONBLOCK | SOCK_CLOEXEC`; `None` on `EAGAIN`), `connect_nonblocking(addr) -> Connecting { fd, immediate: Result<bool> }`, `take_error(fd)` (`SO_ERROR`), `set_nodelay`, `read(fd, &mut [u8])`, `write(fd, &[u8])` (`send` with `MSG_NOSIGNAL`), `shutdown_write`, `local_addr`, `is_listening_stream(fd)`. Reads and writes return `Io::Done(n) | Io::WouldBlock | Io::Closed | Io::Err(io::Error)` (an enum, not a bare `isize`).
  - `file`: `openat2_beneath(dirfd, path_segments) -> io::Result<OwnedFd>` (it joins the segments with `/`, and an empty list means `"."`; the flags are always the contract's, DRV-08, so no caller can open an inheritable descriptor), `open_dir_path(path)`, `fstat(fd) -> Stat { kind, size, mtime_ns }` (pre-1970 clamps to 0), `readahead(fd, offset, len)`, `sendfile(out, in, offset, len) -> Io`, and `openat2_supported() -> bool`.
  - `eventfd`: `EventFd::new()`, `notify()`, `drain()`.
  - `signal`: `SignalFd::new(&[Signal::Term, Signal::Int, Signal::Pipe])` (blocks them with `pthread_sigmask` first), `read() -> Option<Signal>`.
  - `user`: `lookup_user(name) -> Option<(uid, gid)>`, `drop_to(uid, gid)` (setgroups, then setgid, then setuid, then a check that `setuid(0)` fails), `euid()`.
  - `rlimit`: `nofile() -> (soft, hard)`, `raise_nofile(to)`.
  - Errno helpers: `FileError` mapping per the contract table.

- [ ] **Step 1: Write the failing tests** in `driver/tests/sys.rs`, on the real kernel and unprivileged:
  - an epoll round trip over a loopback pair, covering readable, writable, both and `EPOLLHUP` after the peer closes;
  - `accept` returns `WouldBlock` when none are pending;
  - a non-blocking connect to a closed port fails via `take_error`, and a connect to an open listener completes;
  - `openat2_beneath` refuses `..`, a symlink and an absolute path, and opens a regular file and `"."`;
  - a FIFO opened with `O_NONBLOCK` returns at once;
  - `fstat` reports the kind and size;
  - `sendfile` from a temp file to a socket delivers the exact bytes;
  - `eventfd` notify and drain;
  - signalfd receives a `kill(getpid(), SIGTERM)`;
  - `rlimit` reads.
- [ ] **Step 2: Run them and see them fail** (compile failure).
- [ ] **Step 3: Implement** with minimal, documented wrappers. Check the libc tree with `cargo tree -p durchlauferhitzer -e normal,build` and put the output in the report.
- [ ] **Step 4: Run the gate, including the extended deps script.**
- [ ] **Step 5: Commit** "Driver crate and safe syscall wrappers" and push. The commit message justifies `libc` (the only product dependency, allowed by CLAUDE.md).

### Task 5: Event loop for clients and upstreams

**Files:**
- Create: `driver/src/event_loop.rs` (the loop and dispatch), `driver/src/conns.rs` (fd ↔ `ConnId`/`UpstreamId` tables and token encoding), `driver/tests/loop_static_less.rs`.
- Modify: `driver/src/lib.rs`, `driver/README.md`.

**Interfaces:**
- Consumes: `sys` (Task 4) and the core API.
- Produces: `pub struct Driver` with `Driver::new(core: Core, listeners: Vec<OwnedFd>, roots: Vec<OwnedFd>, pool: Option<Pool>, log: LogSink) -> io::Result<Driver>` and `Driver::run(&mut self) -> io::Result<Exit>`. In this task the pool is a stub that answers nothing and the log is a `Vec` sink for tests; Tasks 6 and 7 replace them. *(Amended in Task 6, review M1:)* the signature is `Driver::new(core, listeners, roots, disk_threads: u32, log)`; `new` starts the disk pool itself, exactly when the core has static roots (with a duplicate of its wake eventfd), refuses zero threads for a core with roots and a root list that does not match `Core::static_roots`; `run` shuts the pool down when it ends. The pool is crate-private.

  Epoll tokens encode `Listener(i) | Client(ConnId) | Upstream(UpstreamId) | Wake | Signal` in a `u64`. Ids are allocated by the driver from 1 and never reused (DRV-01).
- Rules, implemented exactly as the contract says:
  - **DRV-02, actions in order:** perform every action of one `handle` call in order before the next event.
  - **DRV-03, level-triggered I/O:** read until `recv_buffer` is empty or `WouldBlock`. Write `send_buffer`, then `send_file`, until done or `WouldBlock`.
  - **DRV-04, report at once:** each read or write is reported as the very next `handle` call.
  - **DRV-05:** after `Close`, close the fd and deliver nothing more for it.
  - **DRV-06, timers:** derive the epoll timeout from `next_deadline` after every `handle` call; deliver `TimerFired` once that instant has passed. Use the monotonic clock (`std::time::Instant` against a fixed origin, converted to `core::Instant`).
  - **`set_date`:** call it when the wall second changes.
  - **Upstreams:**
    - `Connect` happens immediately with `connect_nonblocking`, but its result is always delivered as a later event, even when the connect is immediate (DRV-10).
    - While connecting, wait for `EPOLLOUT` with interest `None`, then read `SO_ERROR`.
    - `ReadWrite` maps to `EPOLLIN | EPOLLOUT`.
    - A socket error is reported only when a read or write hits it. Readable bytes are drained first.
    - While the interest is `None`, errors are deferred: the socket is removed from the epoll set, and `EPOLLERR`/`EPOLLHUP` are ignored until an interest returns.
    - Clients report errors whatever their interest.
  - **`TCP_NODELAY`** on every client and upstream socket (NFR-PERF-04). Buffer sizes are never set.
  - **Accept:** loop until `WouldBlock`. `EMFILE`/`ENFILE`/`ENOBUFS`/`ENOMEM` are logged as driver logs (FR-LOG-07). After an `EMFILE`, stop accepting until a connection closes, so the loop never spins.

- [ ] **Step 1: Write the failing tests** (real loopback, a driver in a thread, a test config with no static roots; proxy routes to a std `TcpListener` test backend):
  - A 404 for an unknown host with keep-alive: two requests on one connection.
  - A 400 for a bad request with a lingering close.
  - A proxied GET to the test backend gives the exact relayed bytes, plus the exact forwarded head as the backend saw it.
  - A backend that answers 413 early and then resets during a 4 MiB upload: the client gets the 413. This is the stage-3 I1 rule on a real kernel.
  - A client reset mid-relay: no panic, the upstream is closed, and the fd count returns to its start value (Review Focus 1).
  - `next_deadline` timers: a header timeout gives a 408 at about `header_timeout` (tolerance ±200 ms).
- [ ] **Step 2: Run them and see them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the gate.**
- [ ] **Step 5: Commit** "epoll event loop for clients and upstreams" and push.

### Task 6: Disk worker pool and the file contract

**Files:**
- Create: `driver/src/pool.rs`, `driver/tests/static_files.rs`.
- Modify: `driver/src/event_loop.rs`, `driver/README.md`.

**Interfaces:**
- Produces:
  - `Pool::start(threads: u32, wake: EventFd) -> io::Result<Pool>`;
  - `Pool::submit(Job::Open { file, root_fd, segments } | Job::Readahead { file, fd, offset, len })`;
  - `Pool::results() -> impl Iterator<Item = JobResult>`;
  - `Pool::shutdown()`, which joins the threads.
- Rules (A10, DRV-07…09):
  - Every job gets exactly one result. The loop turns results into `FileOpened`/`FileFailed`/`ChunkReady` events.
  - On `CloseFile`, the file's state becomes `Closing` while a readahead is in flight. The fd is closed only when that result arrives, and the result is then dropped.
  - `sendfile` runs on the loop, and its outcome is reported as `FileSent`/`FileFailed`. A return of 0 in the middle of a segment is `Truncated`. `EPIPE`/`ECONNRESET` are reported as the socket's `Failed`.
  - The stamp is `st_size` plus `st_mtime` in nanoseconds.

- [ ] **Step 1: Write the failing tests** (temp-dir roots):
  - serve a 10 MiB file byte-exact to a slow reader;
  - a directory without a trailing `/` gives a 301, and with `/` gives its `index.html`;
  - a symlink, a dotfile, a FIFO (`mkfifo` via `libc` in the test) and a missing file each give 404. The FIFO must not hang a worker (Review Focus 2): a concurrent request still gets its answer within 1 s;
  - HEAD gives the head only, with the full `Content-Length`;
  - a file truncated mid-transfer gives a cut connection, never a complete-looking short body;
  - a client reset mid-`sendfile` leaks no fd (`/proc/self/fd` count) (Review Focus 1);
  - 64 concurrent downloads under `disk_threads 2` are all byte-exact.
- [ ] **Step 2: Run them and see them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the gate.**
- [ ] **Step 5: Commit** "Disk worker pool and sendfile" and push.

### Task 7: Logging: `key=value` lines, escaping, non-blocking stderr

**Files:**
- Create: `driver/src/log.rs`, `driver/tests/log.rs`.
- Modify: `driver/src/event_loop.rs`; spec FR-LOG-05…07 (the exact field names and escaping, recorded as the stage 4 format).

**Interfaces:**
- Produces:
  - `pub fn format_entry(entry: &Log, out: &mut Vec<u8>)`, a pure function, and `pub fn format_driver(entry: &DriverLog, out: &mut Vec<u8>)`;
  - `pub enum DriverLog { AcceptFailed { errno }, SocketFailed { errno }, DriverBugClose { conn }, Dropped { count }, Startup { .. }, Shutdown { phase } }`;
  - `LogSink::stderr()`: stderr is set to `O_NONBLOCK`, writes go to a bounded 64 KiB buffer, and `EAGAIN` keeps bytes buffered. When the buffer is full, whole entries are dropped and counted, and `kind=dropped count=N` is written once output catches up (FR-LOG-06). The writer is flushed on `EPOLLOUT` of fd 2 within the loop.
- Rules:
  - One line per entry: `kind=<snake_case> key=value ...`, with keys in a fixed order per kind.
  - Values that are not plain tokens (`[A-Za-z0-9._:/@%+-]+`) are quoted, with `"` → `\"` and `\` → `\\`. Control bytes, bytes ≥ 0x80 and `=` or space in a value are written as `\xHH`.
  - Untrusted fields (host, path, method) always go through escaping. The decoded path is never logged (NFR-SEC-07).

- [ ] **Step 1: Write the failing tests:**
  - an exact line for every `Log` variant and every `DriverLog` variant;
  - an escaping table with CR, LF, NUL, `"`, `\`, `=`, space, 0x7F and 0xFF;
  - a fuzz-style property: any `Log` with arbitrary strings formats to exactly one line with no raw control byte (Review Focus 3);
  - the sink against a pipe whose reader stalls: writes never block, entries are dropped and counted, and after draining the `dropped` line appears once.
- [ ] **Step 2: Run them and see them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the gate.**
- [ ] **Step 5: Commit** "Non-blocking key=value logging" and push.

### Task 8: Process: CLI, startup checks, binding and systemd sockets, privilege drop

**Files:**
- Create: `driver/src/process.rs` (startup sequence A11), `driver/src/systemd.rs` (A7), `driver/tests/cli.rs` (runs the built binary via `env!("CARGO_BIN_EXE_durchlauferhitzer")`).
- Modify: `driver/src/main.rs`; spec FR-PROC-01…04, -05a, FR-CFG-10, FR-CFG-15, NFR-RES-05, NFR-PORT-*, and A6–A8, A11.

**Interfaces:**
- Produces: `pub fn run(args: impl Iterator<Item = OsString>) -> ExitCode`, with `ExitStatus { Ok = 0, Config = 1, Startup = 2 }` (private to `process.rs`, not re-exported; renamed from `Exit` in Task 5's review, since `Exit` is the event loop's ending, `Driver::run -> io::Result<Exit>`). The usage text goes to stderr with exit 1 for bad arguments; that is a builder call recorded in the spec.
- Rules: A6, A7, A8 and A11. A11 step 8 ("Start the workers") is `Driver::new`, which spawns the disk workers (Task 6): `SignalFd::new` comes before `Driver::new`, so the workers inherit the blocked signal mask. Pass `limits.disk_threads` to it, and count the pool's duplicate wake eventfd (one, whatever the thread count) in the NFR-RES-05 budget. `--check` loads the config and prints `ok` or the error with line:column, then exits 0 or 1 without touching the network or the roots.

- [ ] **Step 1: Write the failing tests:**
  - `--check` on a valid config gives 0, and on each invalid case 1 with the message.
  - No args, or an unknown flag, gives usage and exit 1.
  - A missing static root gives exit 2 naming it.
  - A port in use gives exit 2 naming the address (bind a listener first).
  - The `RLIMIT_NOFILE` check: run with `prlimit` / `setrlimit` in a `pre_exec` lowering the limit, and expect exit 2 with the needed and available numbers (Review Focus 5).
  - systemd: pass a pre-bound listener as fd 3 with `LISTEN_PID`/`LISTEN_FDS` set (via `pre_exec` and `dup2`). The server serves on it, and a mismatching address gives exit 2.
  - Running as non-root with `user` set and a port ≥ 1024: no drop is attempted, the run proceeds, and this is logged.
  - The root-only drop test is `#[ignore]`d unless euid is 0, with a printed reason; CI runs it if it runs as root.
  - A port < 1024 as non-root without systemd sockets gives exit 2.
- [ ] **Step 2: Run them and see them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the gate.**
- [ ] **Step 5: Commit** "CLI, startup checks, socket activation and privilege drop" and push.

### Task 9: Signals and graceful shutdown

**Files:**
- Modify: `driver/src/event_loop.rs`, `driver/src/process.rs`; create `driver/tests/shutdown.rs`; spec FR-PROC-05 and A9.

**Interfaces:**
- Consumes: `Core::begin_drain`, `Core::is_drained` and `Core::abort_all` (Task 2), and `SignalFd` (Task 4).
- Rules:
  - First SIGTERM: close the listeners, call `begin_drain`, and set the shutdown deadline to now plus `shutdown_timeout`.
  - Once `is_drained`, shut down the pool, flush the log and exit 0.
  - A second SIGTERM, a SIGINT, or the deadline: `abort_all`, then the same exit, waiting at most 1 s for pool threads stuck in the kernel. Threads still blocked are detached and reported in the log; the exit is 0.

- [ ] **Step 1: Write the failing tests** (the real binary on loopback):
  - SIGTERM during a slow 50 MiB download: the download completes byte-exact, and a new connect is refused.
  - On another keep-alive connection, a request sent after the SIGTERM gets 503 with `connection: close`.
  - The process exits 0 after the download (Review Focus 4).
  - SIGTERM during a proxied upload: the upload completes and the response is relayed.
  - A second SIGTERM mid-download: the connection is cut and the exit follows within 1 s.
  - `shutdown_timeout 500ms` with a stalled client: the exit comes about 500 ms after SIGTERM.
  - SIGINT: an immediate exit.
- [ ] **Step 2: Run them and see them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the gate.**
- [ ] **Step 5: Commit** "SIGTERM drain and fast shutdown" and push.

### Task 10: End-to-end smoke suite, documentation, spec status

**Files:**
- Create: `driver/tests/smoke.rs`.
- Modify: `README.md` (how to build and run), `driver/README.md`, `examples/durchlauferhitzer.conf` (a runnable loopback example), and the spec (status markers: every requirement this stage completes → `[done]`; Timeline: stage 4 done; DRV-11/12 done).

- [ ] **Step 1: Write the smoke tests.** One binary run serves a static host and a proxy host at once, against a test backend thread:
  - a parallel mix of 200 requests over 32 keep-alive connections: static 200/404/301, a proxied GET and POST of 1 MiB, chunked both ways, an early 413, and a backend refused (502) and hanging (504, with short timeouts);
  - each response is checked byte-exact against the sim's spec-written model where it applies (`sim/tests/support/model_http.rs` via `#[path]`), and against expected status and bytes otherwise;
  - the log is captured from stderr, and one line per expected entry is asserted;
  - at the end, SIGTERM gives exit 0, and the fd count goes back to its baseline during the run (sampled from `/proc/<pid>/fd`).
- [ ] **Step 2: Run them.** Any failure is a driver or core bug: fix it red/green with a pinned directed test in the matching task's test file.
- [ ] **Step 3:** Update the docs and the spec status markers.
- [ ] **Step 4: Run the gate.**
- [ ] **Step 5: Commit** "End-to-end smoke tests; stage 4 docs" and push.
