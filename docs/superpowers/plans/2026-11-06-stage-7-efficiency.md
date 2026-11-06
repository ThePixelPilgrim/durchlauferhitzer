# Stage 7: Efficiency — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (mandatory in this repo) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. In this repo, `superpowers:<name>` is the project skill `<name>`.

**Goal:** Less CPU and fewer system calls per request on a small VPS. A nightly benchmark measures this and charts it on a `benchmarks` branch; each optimisation is kept only if it saves work.

**Architecture:**
- **Measure first.** A test-only benchmark tool drives the real binary through fixed scenarios. It records CPU time and system calls per request into CSV, and a stdlib Python script renders the CSV as Mermaid charts.
- **Then save work per request:**
  - conditional requests (core);
  - `MSG_MORE` (driver);
  - an indexed `next_deadline` (core);
  - backend connection pooling with a safe single retry (core, driver, sim).
- **Gated on measurement:**
  - `splice` for proxied bodies;
  - N driver threads (`threads N|auto`, default 1).

  Each is built only after the owner sees the numbers.

**Tech Stack:**
- **Product:** Rust (stable, pinned), `libc` only.
- **Benchmark tool:** an excluded workspace-style crate `bench/`, test-only. It may use crates (NFR-MAINT-02) but prefers std.
- **Charts:** Python stdlib (like `scripts/nightly-report.py`). System calls are counted with `strace -c -f` under sudo.

**Spec:** `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md`, binding. It covers:
- NFR-PERF intro and NFR-PERF-01…08;
- FR-STATIC-18, FR-STATIC-11;
- FR-PROXY-02, FR-PROXY-26;
- FR-CFG-20/21;
- NFR-RES-01/05;
- FR-PROC-06/07;
- NFR-TEST-08.

## Global Constraints

- **No C:** Rust only; the system libc and `libgcc_s` are the only C. The product's dependencies stay `libc` only (`scripts/check-product-deps.sh`). `bench/` is outside the product tree.
- **Sandbox:** strict. A new system call needs an allow-list entry on every arch and libc, plus the suite under `DURCHLAUFERHITZER_TEST_SECCOMP_KILL=1`. Threads start before the filter (FR-PROC-06).
- **Code style:** enums over bools, exhaustive matches, files < 900 lines, READMEs current, docs terse.
- **Spec current:** statuses flip at stage end (Task 9).
- **Gate:** `cargo fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked && scripts/check-product-deps.sh && RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`, the kill-switch run, and `python3 -I -m unittest discover -s scripts -p 'test_*.py'`.
- **Measure, then decide (owner 2026-11-06):** every optimisation task reports before/after numbers from `bench` (local, 5 runs, median) and keeps the change only if CPU time or system calls per request drop in its scenarios. Otherwise it stops and reports.
- **The core stays sans-IO.** The sim models every new core behaviour from the spec.

## Review Focus

1. **Pooled connection hygiene:**
   - A pooled idle backend connection that receives bytes, EOF or a reset while idle must be closed, never reused.
   - A response must never be paired with the wrong client's request.
   - A request may be retried only if no response byte arrived (Task 5).
2. **A conditional request for a file that changed between `If-None-Match` and the open:** the 304 decision uses the stamp of the opened descriptor, and a 200 carries the new ETag (Task 2).
3. **Malformed or odd conditional headers** (several `If-None-Match` lines, `*`, weak tags `W/"…"`, an `If-Modified-Since` in the future or in RFC 850 form) must follow RFC 9110: malformed ones are ignored, and `*` matches any existing file (Task 2).
4. **Benchmark honesty:**
   - Runs that hit errors or refused connections are not recorded as fast.
   - The CPU figure is the server's own `utime+stime`, not the client's.
   - The `strace` pass is never used for CPU numbers (Task 1).
5. **Drains with pooled connections:** on either drain the idle pool closes at once, and a request in flight on a pooled connection finishes or is cut as today (Task 5).

---

### Task 1: Benchmarks: tool, scenarios, nightly job, `benchmarks` branch, charts

**Files:**
- Create: `bench/` (excluded crate like `fuzz/`: own `Cargo.toml` and lockfile, `src/main.rs` plus modules), `scripts/bench-chart.py`, `scripts/test_bench_chart.py`, `bench/README.md`.
- Modify: `.github/workflows/nightly.yml` (jobs `bench` and `bench-publish`), `.github/workflows/ci.yml` (`cargo check --manifest-path bench/Cargo.toml --locked`), `README.md` (one line linking `https://github.com/ThePixelPilgrim/durchlauferhitzer/blob/benchmarks/BENCHMARKS.md`), and the spec's NFR-PERF-08 status stays `[stage 7]`.

**Interfaces:**
- Produces: `bench --binary <path> --out <csv> [--runs N] [--duration S] [--scenarios a,b]`. It starts the server and a stub backend (in-process, std, a fixed small response), warms up, and drives each scenario for `duration` with a fixed concurrency (default 8).
- **Scenarios:**

  | Scenario | Traffic |
  |---|---|
  | `static-small` | 1 KiB file, keep-alive |
  | `static-large` | 1 MiB file, keep-alive |
  | `static-fresh` | 1 KiB file, a new connection per request |
  | `proxy-small` | 1 KiB response, keep-alive client |
  | `proxy-fresh` | as `proxy-small`, a new connection per request |
  | `conditional-hit` | `If-None-Match` that matches; records 200s until Task 2 lands |

- **CSV:** one row per scenario and run, header exactly:
  `date,commit,runner,scenario,requests,errors,seconds,cpu_us_per_req,syscalls_per_req,rss_kib`
  - `cpu_us_per_req`: from the server pid's `/proc/<pid>/stat` utime+stime delta over the measured window, divided by requests.
  - `syscalls_per_req`: from a separate pass under `strace -c -f -p <pid>` (sudo), total calls divided by requests.
  - A run with `errors > 0` is written but marked by its `errors` count; the chart ignores such rows.
- **Chart script:** `scripts/bench-chart.py <csv> <out.md>` writes `BENCHMARKS.md`:
  - one Mermaid `xychart-beta` per metric (`cpu_us_per_req`, `syscalls_per_req`), x = date, one line per scenario, medians per date;
  - plus a short table of the latest values.
- **Nightly:**
  - `bench` (x86_64): builds release, runs `bench`, uploads the CSV rows as an artifact. It runs as root only for `strace`.
  - `bench-publish`: needs `bench`, `permissions: contents: write`, checks out only the `benchmarks` branch (create it if missing), appends the artifact's rows to `results.csv`, and runs `bench-chart.py` from a sparse checkout of `main`'s `scripts/` (no code under test). It commits as `github-actions[bot]` and pushes with the built-in token.
  - Failures report through `nightly-report.py` like other jobs (fingerprint `bench|<scenario>|<reason>`).

- [ ] **Step 1: Failing tests:**
  - `test_bench_chart.py`: a CSV with two dates × two scenarios renders two `xychart-beta` blocks with the scenario names as series and medians as values. Rows with `errors > 0` are excluded. Untrusted CSV fields are sanitised like nightly-report's.
  - In `bench`: a unit test for the per-request arithmetic, and a test that a run against a server answering 503 counts errors.
- [ ] **Step 2: Run them, see them fail.**
- [ ] **Step 3: Implement.** Then dispatch the nightly on the task branch with short budgets and show `bench` and `bench-publish` green. The `benchmarks` branch must hold `results.csv` and a rendered `BENCHMARKS.md` (check that GitHub renders it).
- [ ] **Step 4: Baseline.** Run `bench` locally 5× and in the nightly. Report the medians per scenario in the task report.
- [ ] **Step 5: Gate, commit** "Benchmarks: nightly CPU and syscalls per request, charted on the benchmarks branch (NFR-PERF-08)", push.

**Owner stop after Task 1:** the controller proposes targets for NFR-PERF-01…03 from the baseline; the owner approves before they enter the spec.

### Task 1b: `bench/bisect.sh` (owner 2026-11-06)

A POSIX `sh` script for `git bisect run`: it builds the release binary at the current commit, runs `bench` for one scenario (1 run, short duration, `strace` pass only), and exits 0 if `syscalls_per_req` is at most a given threshold, 1 if above, 125 if the build fails. Usage, documented in `bench/README.md`: `git bisect start <bad> <good>; git bisect run bench/bisect.sh <scenario> <max-syscalls>`. Test: a shell-level test in `scripts/` or `bench/` with a stub bench binary checks the exit codes. Gate, commit, push.

### Task 2: Conditional requests (FR-STATIC-18)

**Files:**
- Modify: `core/src/http/date.rs` (parsing), `core/src/http/request.rs` (`RequestHead` gains the two conditionals), `core/src/core/static_files.rs` (the 304 decision after `FileOpened`), `core/src/http/response.rs` (heads), and the sim static model and oracle (`sim/tests/static_files/*`, `sim/tests/faults/model.rs`).
- Modify the spec: FR-STATIC-11's header list gains `ETag` and `Last-Modified`.

**Interfaces:**
- `http::date::parse_http_date(&[u8]) -> Option<u64>` (unix seconds). It accepts IMF-fixdate, RFC 850 and asctime as RFC 9110 §5.6.7 requires; anything else is `None`.
- `ETag` value: `"<size hex>-<modified_ns hex>"` from `FileStamp` (strong). `Last-Modified` is `imf_fixdate(modified_ns / 1e9)`.
- `If-None-Match` is the list of entity tags or `*`; repeated header lines are combined. It is evaluated with weak comparison (RFC 9110 §13.1.2). A match on GET or HEAD gives `304 Not Modified` with `Date`, `ETag`, `Last-Modified`, no body, and `Connection: close` only when closing. `If-Modified-Since` counts only without `If-None-Match`: it gives 304 when the file's mtime in seconds is ≤ the date. A malformed value, or one in the future, is ignored.
- The file still opens (the stamp comes from the descriptor); a 304 then sends `CloseFile`.

- [ ] **Step 1: Failing tests (core and sim):**
  - Every static 200 carries `ETag` and `Last-Modified` in the documented order.
  - A matching `If-None-Match` gives 304; `*` gives 304; a weak `W/"…"` match gives 304.
  - A non-matching tag gives 200. An `If-Modified-Since` equal to mtime gives 304; one older gives 200.
  - Malformed, future and RFC 850 / asctime dates behave as above.
  - Both headers: `If-None-Match` decides.
  - A file that changed between the client's tag and the open gives 200 with the new ETag (Review Focus 2).
  - HEAD gives 304 the same way.
- [ ] **Step 2: Run, see them fail.**
- [ ] **Step 3: Implement.** `bench` `conditional-hit` now reports 304s. Record before and after in the report.
- [ ] **Step 4: Gate (sim nets incl. `SIM_SEEDS=0..20000`), commit** "Static: conditional requests (FR-STATIC-18)", push.

### Task 3: `MSG_MORE` for heads followed by file data (NFR-PERF-06)

**Files:**
- Modify: `driver/src/sys/socket.rs` (`write` with a `More` flag, as `send(…, MSG_MORE)`), `driver/src/event_loop/clients.rs` and `files.rs` (a head queued with a file to follow is sent with `More::Yes`), and the allow-list only if `sendto` is new (check; prefer `send` on the existing call).

**Interfaces:**
- `sys::socket::write(fd, buf, more: More) -> Io` with `enum More { No, Yes }`. The last write before `sendfile`, or of a head whose body follows at once, uses `Yes`. Anything else uses `No`.

- [ ] **Step 1: Failing test:** a driver test sees a static 1 KiB response arrive in a single segment, by counting `TCP_INFO`'s `tcpi_segs_out` delta or a loopback capture (pick the robust one and explain). A unit test checks the flag mapping.
- [ ] **Step 2: Run, see it fail.**
- [ ] **Step 3: Implement.** Measure `static-small` and `static-fresh` before and after: keep it only if system calls or CPU per request drop, or segments drop without a CPU cost. Otherwise report.
- [ ] **Step 4: Gate (both suites), commit**, push.

### Task 4: An indexed `next_deadline` (NFR-PERF-06)

**Files:**
- Modify: `core/src/core/timers.rs`, `core/src/core/mod.rs` (`next_deadline`), and the call sites that change a connection's or upstream's deadline.

**Interfaces:**
- `next_deadline()` keeps its contract and result. Internally it uses a `BTreeSet<(Instant, Owner)>` (or similar, std only) kept in step with every deadline change.
- A debug-only check compares it with the old scan.

- [ ] **Step 1: Failing test:** a property-style test over random deadline updates asserts that the index equals the scan. A benchmark in `bench` or a criterion-free micro-timing in the report shows the gain at `max_connections`.
- [ ] **Step 2: Run, see it fail** (the index type does not exist yet).
- [ ] **Step 3: Implement; measure.** Keep it if CPU per request drops in any scenario at 1 000 open connections; otherwise report.
- [ ] **Step 4: Gate (sim nets incl. long seeds), commit**, push.

### Task 5: Backend connection pooling and the safe retry (FR-PROXY-02, FR-PROXY-26)

**Files:**
- Core: `core/src/upstream.rs` (an idle state), `core/src/core/proxy.rs` and `proxy/*` (pick an idle connection by `BackendId`, return it after a complete, reusable response), `core/src/http/forward.rs` (keep-alive toward the backend: no `Connection: close`), `core/src/core/relay.rs` (end of response), `core/src/core/limits.rs` (budgets), `core/src/core/shutdown.rs` (drains close the idle pool).
- Config: `core/src/config/*`:
  - `limits { backend_idle_timeout 2s; backend_idle_max 8; }` with bounds 1 ms–24 h and 1–`max_connections`;
  - per proxy route `pool off;`;
  - `examples/durchlauferhitzer.conf`.
- Driver: `driver/src/event_loop/upstreams.rs` (idle sockets watched for read readiness), `driver/src/process/budget.rs` (pooled sockets counted).
- Sim: the proxy net model (`sim/tests/faults/proxy/*`) learns pooling and the retry rule; `sim/tests/proxy/*`.
- Spec: the FR-PROXY-26 failure matrix gains the retry row; FR-CFG-20 gains the two keys.

**Interfaces and rules:**
- **When a connection is reused:**
  - An upstream becomes idle after a response that is complete and keep-alive on both sides. That means an HTTP/1.1 backend response with no `Connection: close`, a known length or chunked framing, and nothing unread.
  - It is reused for the next request to the same `BackendId` on a pooled route, from any client.
  - Idle connections are kept LIFO, at most `backend_idle_max` per backend, and closed after `backend_idle_timeout`.
- **While idle:** any readable byte, EOF or error closes it (Review Focus 1).
- **Retry:** a request sent on a reused connection that fails before any response byte (reset, EOF, connect error not possible) is replayed once on a fresh connection. Only GET or HEAD without a body qualify; the core keeps the forwarded head bytes until the first response byte. Anything else is 502 as today (FR-PROXY-26). Fresh connections are never retried.
- **Budgets:** every upstream, active or idle, counts against `max_connections` (one per counted connection), `memory_limit` (no buffers held while idle) and the descriptor formula (unchanged bound).
- **Drains:** both drains close the idle pool at once.

- [ ] **Step 1: Failing tests:**
  - **core:**
    - a second request reuses the idle upstream (no `Connect`);
    - another client reuses it;
    - `Connection: close` from the backend prevents reuse;
    - an unread or chunked-unfinished response prevents reuse;
    - the idle timeout and the per-backend cap close connections;
    - a byte or EOF while idle closes it;
    - a GET on a dead reused connection is retried once and succeeds;
    - a POST on a dead reused connection gives 502;
    - a failure after a response byte gives no retry;
    - `pool off` never reuses;
    - drains close the pool;
    - budgets.
  - **sim:** both proxy nets with pooling. The oracle is written from the spec: no response is ever paired with another request, and the retry happens only by the rule. Mutations: reuse after `Connection: close`, retry of a POST, and retry after a response byte each fail.
  - **driver:**
    - a binary test with a keep-alive stub backend counts one backend connection for N requests;
    - an idle connection the backend closes is not reused (the next request opens a new one);
    - the kill switch stays green.
- [ ] **Step 2: Run, see them fail.**
- [ ] **Step 3: Implement.** Measure `proxy-small` and `proxy-fresh` before and after.
- [ ] **Step 4: Gate (both suites, sim nets incl. long seeds), commit(s)**, push.

**Owner stop after Task 5:** the controller shows the numbers so far, plus a measurement of where CPU goes in `proxy-*` with large bodies and in `static-*` at high concurrency. The owner decides whether Tasks 6 and 7 are built.

### Task 6 (gated): `splice` for proxied bodies with a known `Content-Length`

Built only if the owner keeps it after Task 5. It needs:
- a contract change (an upstream-to-client body move the driver performs with a pipe pair, reported as progress events);
- a sim model for it;
- `pipe2` and `splice` allow-list entries on every arch and libc;
- the descriptor formula, which gains two per proxied connection.

The plan for it is written at that stop, from the measurement.

### Task 7 (gated): `threads N|auto`, default 1 (NFR-PERF-07)

Built only if the owner keeps it after Task 5. Its spec rules:
- process-wide limits through shared counters;
- every thread starts before the seccomp filter;
- systemd's socket is shared by the threads;
- the restart steering is redesigned for N listeners per process.

The plan for it is written at that stop.

### Task 8: `tc netem` measurements (NFR-PERF-05)

**Files:** `bench/` (an `--netem <delay,loss>` option that sets up a network namespace with a veth pair and netem, as root), `bench/README.md`.

- [ ] **Step 1:** Add netem scenarios: 50 ms delay, 1 % loss, and a 2 s pause, for `static-large` and `proxy-small`.
- [ ] **Step 2:** Record throughput, CPU per request and recovery. Measure fill-drain-refill against double buffering and two body buffer sizes, the latter on a scratch branch only.
- [ ] **Step 3:** Report the numbers. The controller proposes the NFR-PERF-05 decision to the owner: keep fill-drain-refill unless the measurement shows a clear gain. Any code change becomes a follow-up task.
- [ ] **Step 4: Commit** the bench option, push.

### Task 9: Stage close-out (controller, inline)

- [ ] Spec statuses for the built items `[done]`, and the targets approved after Task 1 in NFR-PERF-01…03 and -05. Dropped items are marked as decided by measurement.
- [ ] HANDOFF, then the stage 7 whole-branch review.
