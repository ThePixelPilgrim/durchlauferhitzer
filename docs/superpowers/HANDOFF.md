# Handoff — where work stands and how to continue

Last updated 2026-11-17. Stages 1 to 10 are done. Stage 10 (carry-forwards) closed with its whole-branch review and fix wave. The next work is the kTLS spike and the TLS design (see [TLS](#tls)); no stage 11 plan exists yet.

Read this file first for the state of work. Before any work, follow the "Branches and main" steps in `CLAUDE.md`:

1. `git fetch origin`, then work from the latest `main`.
2. Check the remote branches for commits not on `main`, and report any before planning.

## Next

The next session runs the due checks, then prepares the kTLS spike. The owner chooses what stage 11 holds.

1. **Checks that are due:**
   - After the first scheduled nightly from `main`: check the product-lines chart ([Task 4](#task-4-product-line-count)).
   - The first full-budget scheduled nightly on `main` is the real check of the nightly's durations ([Maintenance](#maintenance)).
   - Close nightly issue #8, with the owner's OK. Its fix is on `main`, and the scheduled nightly of 2026-11-17 was green (run 44076916380 on `b8b13e4`).
2. **Waiting for the owner:**
   - What stage 11 holds. Candidates are in the spec's Timeline and in [Carry-forwards](#carry-forwards-to-later-stages).
   - Open decision 6 in the spec: an OpenRC wrapper ([Task 5](#task-5-openrc-supervision)).
   - Whether the OpenRC pid guard is planned as a task ([Final review](#final-review-and-fix-wave)).
   - The `proxy-file` NFR-PERF target, still to be set from its first nightly ([Stage 8 carry-forwards](#stage-8-carry-forwards)).
3. **Next piece of work:** the kTLS spike, on a GitHub runner ([TLS](#tls)). It precedes the TLS design document.
4. **Standing constraints:**
   - `nightly.yml` is at 899 lines: the next addition must split it first.
   - The fuzz job's pinned nightly toolchain needs a bump by hand now and then ([Maintenance](#maintenance)).

## State

Stages 1 to 9 are done; stage 10 is in progress.

| Stage | Status | Commits | Record |
|---|---|---|---|
| 1 Foundations | done | `ff5480a` | [`docs/superpowers/reviews/2026-10-25-stage-1-review.md`](reviews/2026-10-25-stage-1-review.md) |
| 2 Static files | done | `576a77c` | [`docs/superpowers/reviews/2026-10-26-stage-2-review.md`](reviews/2026-10-26-stage-2-review.md) |
| 3 Reverse proxy | done | `576a77c..b583d96` | [`docs/superpowers/reviews/2026-10-27-stage-3-review.md`](reviews/2026-10-27-stage-3-review.md) |
| 4 epoll driver + binary | done | `7e1b18e..` this handoff | [`docs/superpowers/reviews/2026-10-29-stage-4-review.md`](reviews/2026-10-29-stage-4-review.md) |
| 5 Containment | done | `bc2f7d3..` this handoff | [`docs/superpowers/reviews/2026-11-02-stage-5-review.md`](reviews/2026-11-02-stage-5-review.md) |
| 6 Restart | done | `22fd0ce..` this handoff | [`docs/superpowers/reviews/2026-11-06-stage-6-review.md`](reviews/2026-11-06-stage-6-review.md) |
| 7 Efficiency | done | `71d3867..` this handoff | [`docs/superpowers/reviews/2026-11-07-stage-7-review.md`](reviews/2026-11-07-stage-7-review.md) |
| 8 Backend-directed file serving | done | `862e951..` this handoff | [`docs/superpowers/reviews/2026-11-09-stage-8-review.md`](reviews/2026-11-09-stage-8-review.md) |
| 9 Guarantee hardening | done | see note below | [`docs/superpowers/reviews/2026-11-09-stage-9-review.md`](reviews/2026-11-09-stage-9-review.md) |
| 10 Carry-forwards | done | `462a967..` this handoff; per task, see [Stage 10](#stage-10-in-progress) | [`docs/superpowers/reviews/2026-11-17-stage-10-review.md`](reviews/2026-11-17-stage-10-review.md) |

Notes on the table:

- Stage 7: the Eyra spike (`docs/superpowers/spikes/2026-11-06-eyra.md`) concluded no, revisit later.
- Stage 9 commits, plus this record:
  - `4c999c1..dccd243`;
  - `92b3c36`;
  - `acc3577..dd8f1c6`.
- The stage 8 review's fix wave `92b3c36..acc3577` landed in between.
- Stage 9 brainstorm: `docs/superpowers/reviews/2026-11-09-qmail-lessons.md`.
- Stage 10's owner decisions are in the spec:
  - NFR-TEST-08: the `pressure` and `newest libc` jobs;
  - FR-PROC-03: `user <uid>:<gid>`;
  - NFR-MAINT-12: product size;
  - FR-PROC-07a: OpenRC supervision;
  - the `bodies.rs` split.

Each stage's review record lists its owner decisions, rulings, final review, fix wave and accepted gaps.

Requirements live in the spec with stable IDs (FR-…, NFR-…, DRV-…). Plans, reviews and ledger lines cite those IDs instead of restating requirements. New owner decisions go into the spec first. Open owner decisions are listed at the end of the spec.

Key documents:

- the spec `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md` (binding);
- the driver contract in `core/src/core/contract.rs` (module docs);
- `README.md`: build, `--check`, running, systemd, signals;
- the crate READMEs `core/README.md`, `sim/README.md`, `driver/README.md`;
- the example config `examples/durchlauferhitzer.conf`. It runs on 127.0.0.1:8080 without root. A test keeps it loadable and checks that every key is documented.

## Stage 10 in progress

Stage 10 is done: all six tasks, the whole-branch review and its fix wave. Task 5 ended as a documented gap, not an implementation. The section keeps its heading, because other sections link to it.

### Task 1: nightly `pressure` job

- Done 2026-11-12. Commits: `fe604e4..de557b7`.
- The job runs the driver suite under kill with `2 x nproc` busy loops and asserts their count.
- An `only_job` choice input serves branch runs.
- Failures are filed like older-glibc's.
- Three dispatched runs on the final code were green:
  - 43200502966;
  - 43200808692;
  - 43201063382.
- Earlier failures in 42779607344 and 42781263820 were the survive tests' default timeouts on a starved runner.
  - The fix sets these to 120 s in `every_slot_limits`:
    - `header_timeout`;
    - `send_idle_timeout`;
    - `backend_connect_timeout`;
    - `backend_response_timeout`.
  - The proxy test now reports what a client that never reached the backend was told.
- Nightly issues #6 and #7, filed by the branch runs, were closed against `97e0924`.

### Task 2: nightly `newest-libc` job

- Done 2026-11-12. Commits: `393c78e..d10e4ca`.
- Two legs are started by hand with ci.yml's Alpine `docker run` flags:
  - glibc on `fedora:latest`: glibc 2.44, checked newer than the runner's at run time (`ubuntu-latest` is 26.04 with glibc 2.43 now);
  - musl on `alpine:edge`: 1.2.6.
- Each leg has a `Versions` step and files its failures separately.
- Every driver-suite nightly job uses `--no-fail-fast`. Dispatched pressure and older-glibc runs verified it.
- The job found a real bug on its second run (issue #10).
  - Any bash running the reload helper as a script prints a "Killed" job notice when the new process dies of a signal.
  - Fedora's `/bin/sh` is bash. Dash and busybox ash print nothing.
  - Fix: `disown "$new"` under `BASH_VERSION` in `contrib/durchlauferhitzer-reload`.
  - A restart test runs the helper under `bash` explicitly to pin it.
- Green run: 43216974812.
- `nightly.yml` was at 895 lines then; the next addition must split it.

### Task 3: numeric `user <uid>:<gid>`

- Done 2026-11-17. Commits: `a4531df..2d9c84f`.
- `user <uid>:<gid>` skips the NSS lookup (FR-PROC-03, FR-CFG-11).
- Owner decisions of 2026-11-17:
  - A 0 in the numeric form is a config error: exit 1, and `--check` reports it.
  - Only the startup log shows the form: `kind=privileges action=dropped user=<uid>:<gid> form=name|ids`.
- The uid-0 guard in `drop_to` is defence in depth without a test.
- CI of `main` was green for this landing: run 44131779311 on `5e2c3a8`. Its Alpine jobs are the real musl check.

### Task 4: product line count

- Done 2026-11-17. Commits: `41c0131..3d69657`.
- `scripts/product-lines.py` counts the product's non-blank, non-comment source lines without tests.
- NFR-MAINT-12 holds the rule and its approximations.
- The count was 15407 at `5e2c3a8`: core 9702, driver 5705.
- The nightly `bench` job writes `product-lines.csv` into the `bench-results` artifact.
  - The step may fail on its own, so a counter crash never skips the benchmarks and files no issue.
- The publish appends the row to `product-lines.csv` on `benchmarks` and charts `Product source lines`.
- `bench-chart.py --append` now writes the orphan branch's README.
- Dispatched bench runs on the branch were green: 44138226982 and 44139224075.
- CI of `main` was green for this landing: run 44139782628 on `ae8fa9e`.
- **Still to check** after the first scheduled nightly from `main`: `BENCHMARKS.md` shows the chart, and `product-lines.csv` has one row.

### Task 5: OpenRC supervision

- Done 2026-11-17, as a documented gap. Commits: `cc948bc..55dc088`.
- CI of `main` was green for this landing: run 44157584458 on `02afabc`.
- Stock `supervise-daemon` (OpenRC 0.63.2, Alpine 3.24) cannot carry the reload hand-over, because it:
  - respawns after every exit, including 0;
  - watches only its own fork;
  - writes its own pid into `--pidfile`.
- FR-PROC-07a holds the mechanics with source lines. A reviewer confirmed them against the tag.
- Consequences:
  - The OpenRC script does not restart the server after a crash.
  - The guaranteed install (FR-PROC-08) names systemd only.
- `contrib/test-openrc.sh` pins the operator's recovery in CI's Alpine jobs.
  - After `kill -9`, `status` reports `crashed` with exit status 32, and `restart` serves again.
  - This is tested once after a reload and once without.
  - First observed in run 44154692120.
- **Open owner decision:** a POSIX `sh` wrapper as `supervise-daemon`'s child could give supervision under OpenRC. FR-PROC-07a notes it. The cost:
  - a supervisor of our own;
  - a second pid file;
  - a `reload` that signals the wrapper.

### Task 6: split `bodies.rs`

- Done 2026-11-17 (`0273570..f262033`), NFR-MAINT-10: a move only, no test changed.
- `sim/tests/proxy/bodies.rs` went from 895 to 560 lines.
- New files: `body_timeouts.rs` (235 lines, the six body-timeout tests) and `early_responses.rs` (125 lines, the three early-response tests).
- The `expect_100_continue_*` tests stay in `bodies.rs`, because `expect.rs` (508 lines) would pass 600 with them.
- The proxy test binary lists 135 tests before and after.
- `sim/tests/proxy/main.rs` is at 778 lines.

### Final review and fix wave

The whole-branch review at `8208d59` found no Critical and no Important defect, and seven Minor findings. The record is [`docs/superpowers/reviews/2026-11-17-stage-10-review.md`](reviews/2026-11-17-stage-10-review.md).

- Fix wave: `64817d2`, `06dafbd`, `acfa425`.
- Six findings are fixed:
  - `user` id 4294967295 is an invalid value (FR-CFG-11);
  - a name with uid 0 is refused before the drop with a clear message (FR-PROC-03);
  - nightly fingerprints carry the leg for set-up failures and timeouts (NFR-TEST-08);
  - the `benchmarks` README is rewritten when it differs (NFR-MAINT-12);
  - a night without `bench.csv` still gets its lines row (NFR-MAINT-12);
  - the spec's Timeline and Open decisions match the outcome, with open decision 6 for the OpenRC wrapper.
- The seventh fix was withdrawn. A path-based guard against a reused pid in the OpenRC script did worse than stock OpenRC in two cases: an unreadable executable and a moved binary.
- The OpenRC script, `contrib/test-openrc.sh` and `ci.yml` are byte-identical to `8208d59`.
- The README and FR-PROC-07a state the pid-reuse hazard and the manual way out. That recovery sequence was checked against the OpenRC 0.63.2 source, not run.
- Product size at the end of the stage: 15413 lines (core 9702, driver 5711), 59 more than at `462a967`.

## TLS

The TLS primitives are decided, a process split is proposed, and a kTLS spike comes next. The spike can run beside the remaining stage 10 work and precedes the TLS design document.

Decided, recorded in the spec's open decision 5:

- an own minimal TLS 1.3 handshake crate over kTLS;
- RustCrypto primitives, one suite to start;
- the inline-`asm!` allowance in NFR-MAINT-01;
- Wycheproof and a binary-level constant-time check as requirements;
- the handshake crate in the Rust-to-Lean subset.

The survey is `docs/superpowers/reviews/2026-11-10-tls-primitives-survey.md`.

Still open:

- the no-kTLS behaviour;
- the terminator/signer split and channel;
- uids and namespaces;
- groups.

For the open split, the controller proposed this on 2026-11-17. It is not decided.

- A parser process holds no secrets and turns the ClientHello into one fixed-size struct.
- A key process sees only fixed-size messages. It holds the certificate key, the ECDHE secrets and the key schedule.
- The server relays handshake bytes and installs the kTLS keys.

The kTLS spike:

- It cannot run in the cloud container, whose kernel has `CONFIG_TLS` unset. It runs as a dispatched workflow on a GitHub runner.
- It also checks whether the kernel can carry the handshake records (rekey, about Linux 6.14). If so, `chacha20poly1305` drops out of the product.

## Where things stand after stage 9 (details in the spec and the stage records)

Stage 9 made the default config the guaranteed install (FR-PROC-08).

- Containment defaults are `required`/`required`/`kill` (FR-CFG-25).
- `--check` warns about any explicit softer choice and prints the kernel memory backstop the server will set.
- The README's "Guaranteed install" section names the platform the claims hold for.
- The kernel memory backstop turns an accounting mistake into an abort (NFR-RES-09).
  - It is `RLIMIT_DATA`, soft and hard, set between the descriptor budget and the privilege drop.
  - Its size: `memory_limit` + 4 MiB + `disk_threads` × (2 MiB + 256 KiB) + `max_connections` × (8 KiB + 3 × `max_head`).
  - Every per-connection state must fit inside its allowance. The per-connection term is `8 KiB + 3 × max_head`.
  - The decoded static path is one allocation per connection (`core/src/rel_path.rs`).
  - The final review's live attack found in-flight request state missing from the first allowance. It was fixed, with two survive-worst-case tests.
  - `driver/tests/backstop.rs` holds the worst-case request on every slot at the minimum `memory_limit`.

Other stage 9 results:

- glibc runs with one malloc arena (`driver/src/sys/malloc.rs`), so no worker thread ever opens `/sys/devices/system/cpu/online`. This was found under CPU pressure.
- A body on `GET`/`HEAD` toward a backend is 400 before any connect (FR-PROXY-01).
- The seccomp table allows both epoll waits and both thread clone flag sets for every libc.
- The NFR-SEC-14 framing ledger exists; its review is due 2027-11.
- Stage 9 also covered the CI containers' `nofile` ulimit.

Testing rules that came out of stage 9:

- Every driver server test runs under seccomp `kill`; the test switch equals the default.
- Nightly seeds are replayed at their run's commit, never only at `main`.
- Contention-only libc paths need a run under CPU pressure: the nightly `pressure` job (stage 10, Task 1).
- A survive test that outlasts a default timeout on a starved runner fails for the wrong reason. So the survive tests set every timeout they can hit to 120 s.

## Where things stand after stage 8 (details in the spec and the stage records)

### The binary

- The binary is `durchlauferhitzer`, crate `driver`. `libc` is its only dependency.
- `unsafe` lives only in `driver/src/sys/`, pinned by `driver/tests/unsafe_confined.rs`.
- It serves static files and proxies over epoll, and follows the contract (DRV-01…16).
- It logs one `key=value` line per entry on a non-blocking stderr (FR-LOG-05/06).
- It starts up in the A11 order, with systemd sockets or its own binds.
- It drops to `user` and clears every capability (FR-PROC-03).
- It checks the descriptor budget (NFR-RES-05).
- On SIGTERM it drains (FR-PROC-05). It aborts on any of:
  - a second SIGTERM;
  - SIGINT;
  - `shutdown_timeout`.

### CI

- x86_64 and arm64 run the same gate:
  - fmt and clippy;
  - tests with IPv6 required;
  - the deps check and its self-test;
  - docs with `-D warnings`.
- A root step runs the 12 server-starting test binaries (`SERVER_TESTS` in `ci.yml`) under `sudo` with `DURCHLAUFERHITZER_REQUIRE_ROOT=1`.
- A list check keeps that step complete.
- The workflow token is read-only, and `actions/checkout` is pinned by commit.

### Tests

- An end-to-end smoke suite runs on the real binary.
- Randomized sim nets also drain and abort (`seed % 5 == 0`) and run late timers (`seed % 4 == 0`).
- The unanswered-request check runs on every static seed.

### Containment (stage 5)

- Landlock is read-only beneath the roots. ABI ≥ 6 also scopes signals and abstract unix sockets.
- A seccomp allow-list covers x86_64 and aarch64 (`driver/src/containment/filter.rs`).
- Each is `off | best_effort | required` (FR-CFG-25, FR-PROC-06).
- The whole driver suite also runs under `seccomp_action kill` (`DURCHLAUFERHITZER_TEST_SECCOMP_KILL=1`). This happens in CI on both arches and in the root step.
- IPv6 peers are grouped by `ipv6_prefix`; NAT64 peers are grouped by their IPv4 (FR-CONN-13).

### Nightly

- The workflow is `.github/workflows/nightly.yml`. It runs at 03:17 UTC and by hand.
- It runs:
  - long sim runs;
  - four fuzz targets;
  - the suite under kill on Debian 12.
- Failures file `nightly` issues by fingerprint.

### Restart (stage 6)

- SIGHUP starts the restart drain, served with `Connection: close` and bounded by `restart_timeout`.
- Without systemd, `contrib/durchlauferhitzer-reload` (OpenRC `reload`) runs the hand-over:
  1. It starts a `--ready-fd` process on shared ports (`SO_REUSEPORT`, steered with a classic-BPF reuseport program).
  2. It waits for `ready` and a grace.
  3. It sends SIGHUP to the old process.
- `tcp_migrate_req` moves queued connections.
- musl builds and tests run on Alpine in CI (x86_64, arm64).
- A nightly restart soak runs on glibc and musl.

### Efficiency (stage 7)

- `bench/` (std-only, an excluded crate) measures CPU and system calls per request.
- The nightly publishes rows to the `benchmarks` branch, charted in its `BENCHMARKS.md` (Mermaid).
- `bench/bisect.sh` finds a regressing commit.
- Delivered optimizations:
  - conditional requests (FR-STATIC-18);
  - `MSG_MORE` before files;
  - a deadline index;
  - backend pooling with a silence check and a safe GET/HEAD retry (FR-PROXY-02);
  - eager I/O with deferred epoll registrations;
  - a 32 KiB upstream buffer.
- NFR-PERF targets ratchet only towards better. An exception needs the owner's OK.
- Security trumps performance.

### Backend-directed files (stage 8)

- A proxy route can serve the file a backend's 200 names (FR-PROXY-32). It is configured with one of:
  - `internal_files x-sendfile <root>;`
  - `internal_files x-accel-redirect <prefix> <root>;`
- The file goes through the static file path and the same jail.
- The file headers are stripped both ways, and Rack's hints are set.
- Single-range requests (206/416, `If-Range`) work for static and backend files (FR-STATIC-19).
- The bench `proxy-file` covers it.

### This container

- It boots with IPv6 disabled. IPv6 tests skip here and are required in CI.
- It runs as root, so root tests run here.

## Carry-forwards to later stages

These are the open items and lessons that later stages inherit.

### Nightly issues

Nightly seeds stop reproducing once later commits change the random draws. So a nightly failure is replayed at its run's commit, never only at `main`.

| Issue | Cause | State |
|---|---|---|
| #3 (seed 48287, 2026-11-09) | model bug | fixed by `839e9ac`, pinned |
| #4 (seed 99897, 2026-11-09) | model bug | pinned |
| #5 (2026-11-09) | backstop test in containers | fixed by `3bee322`, closed 2026-11-12 |
| #8 (seed 175664, 2026-11-12) | model bug (FR-PROXY-26) | fixed on `main`, still open: close with the owner's OK |

- **#3:** the stage 8 review's `839e9ac` had already fixed this model bug. It is pinned by two deterministic tests in `sim/tests/proxy/pool.rs` and a FR-PROXY-22 clarification.
- **#4:** the model expected a timeout for any incomplete backend head, where FR-PROXY-14 gives the 502 as soon as `max_head` is exceeded.
  - Pinned by `sim/tests/proxy/head_limits.rs`, a FR-PROXY-14 clarification and the FR-PROXY-17 ruling.
  - The net now draws the case on purpose (`head too large before its end`).
- **#5:** the 65536-connection backstop test in containers.
- **#8:** the proxy net's model failed for the third time.
  - A planned stale kill, or a scripted backend failure, could be outrun by the client's own reset.
  - The model still counted it as a backend failure, so it demanded a retry the core had correctly not made.
  - Fixed by the model's `outrun` flag (`sim/tests/faults/proxy/drive.rs`).
  - Pinned by four tests in `sim/tests/proxy/pool.rs` and one hand-driven story in `sim/tests/faults/proxy/drive/tests.rs`.
  - The 100 000-seed sweep passed.
  - Lesson: a nightly model failure's cousin paths are searched for and guarded in the same fix, with a pin each.

### Nightly issue reporting (NFR-TEST-08, owner 2026-11-02)

- `scripts/nightly-report.py` (Python stdlib, tested in CI) files and updates issues labelled `nightly`, keyed `[fp-xxxxxxxx]`.
- Known limit: a panic located in std/`registry` keys on that location alone. The nightly has no backtraces.
- Known limit: if one arch's long-run job fails with a record while the other times out, only the recorded failure is filed.
- Issue #1 was the deliberate end-to-end test (closed, not planned).

### qmail-review carry-forwards

These are lessons 6–9 and 11 in `docs/superpowers/reviews/2026-11-09-qmail-lessons.md`.

Delivered:

- a nightly run on the newest glibc and musl: stage 10 Task 2. The newest kernel is not covered.
- numeric `uid:gid` for `user` to skip NSS: stage 10 Task 3;
- a product line count beside the nightly benchmarks: stage 10 Task 4.

Open:

- an opener process when TLS arrives;
- naming the supported TLS deployment in the README.

### After the stage 7 merge

- Check that the first scheduled nightly publish logs `refused 0`.
- Compare CPU against the NFR-PERF-01/02 targets over the next two nightlies. Bisect if it stays high.

### Stage 8 carry-forwards

- The `proxy-file` NFR-PERF target is still to be set by the owner, from its first nightly.
- Multipart ranges (`multipart/byteranges`) are still not served. A several-range request gets the whole file.
- nginx's other `X-Accel-*` features are unsupported: expiry, buffering, rate limits and named locations.

### Efficiency, later

- N driver threads, only if the loop nears 50 % of a core. Constraints:
  - They must start before the seccomp filter.
  - musl needs `membarrier`.
  - Steering's 0/1 indices assume one listener per process per port group.
- Answering a silence check without deregistering.
- Reusing buffers without zeroing.
- A syscall regression alarm.
- Parse/route allocation nits.
- `sendfile` blocking on evicted pages (DRV-09).

### Stage 10 carry-forwards

The stage record lists these under "Deferred" and "Accepted gaps".

- Guard the OpenRC main pid by pid plus start time, as `stop_post` does for the draining list. It is a planned task with tests under real OpenRC.
- Unverified: a monitor that runs `restart` while a reload is mid-handover may leave the pid file naming the old process.
- Unverified: the pressure job's false-failure rate rests on four green runs.
- The publish job appends rows again when it is re-run for the same commit and date.
- The publish job may fail when no artifact exists at all.
- After `zap`, processes a reload left draining are not ended.
- `sim/tests/proxy/main.rs` is at 778 lines.

### Later

- TLS (pure Rust).
- Landlock network rules: ABI 4, `CONNECT_TCP` to the backend ports, no `BIND_TCP`.
- Report the reuseport close race to netdev.

### Open

- The driver-bug-close log kind is added with its first producer (FR-LOG-07).
- An NSS descriptor opened by the user lookup after the budget check is not counted (NFR-RES-05).

### Accepted gaps (stage 5 record)

- CI runners have Landlock ABI 4, so the ABI ≥ 6 scopes are proven only on newer kernels.
- Debian 12 under kill is x86_64 only.
- The disk timeout and a detached worker are not run under kill.

### Maintenance

- The fuzz job's nightly toolchain is pinned to a date (`nightly.yml`). It needs a bump by hand now and then.
- The first full-budget scheduled nightly on `main` is the real check of its durations. The estimate:
  - long runs ~8 min, both arches in parallel on 4 cores, plus the build;
  - fuzz ~45 min.

## Branches

`main` holds all verified work. Every other branch below is either contained in `main` or left for the owner to decide.

| Branch | State | What to do |
|---|---|---|
| `claude/magical-dijkstra-2eoh4y`, `unify` | Fully contained in `main`. | Delete when the owner says so. |
| `stage3-task4` … `stage3-task9`, `stage3-final` | Fully contained in `main`. | Delete when the owner says so. |
| `stage4-plan`, `stage4-task1` … `stage4-task10`, `gate-docs`, `stage4-final` | Fully contained in `main`. | Delete when the owner says so. |
| `stage4-final-a`, `stage4-final-b`, `stage4-final-r2a`, `stage4-final-r2b` | Rebased into `stage4-final`. Their own commits are superseded, not lost. | Delete when the owner says so. |
| `stage5-task1` … `stage5-task8`, `stage5-final` | Fully contained in `main`. | Delete when the owner says so. |
| `stage5-final-a`, `stage5-final-b`, `stage5-task8-report` | Rebased into `stage5-task8` / `stage5-final`. `git cherry` shows their commits on `main`. | Delete when the owner says so. |
| `stage6-task1` … `stage6-task6b`, `stage6-design`, `stage6-plan`, `stage6-task6b-plan`, `stage6-closeout`, `stage6-final` | Fully contained in `main`. | Delete when the owner says so. |
| `stage6-final-a`, `stage6-final-b` | Rebased into `stage6-final`. | Delete when the owner says so. |
| `stage7-*` (task branches) | Contained in `main`, except `stage7-task8-buf64`. | Delete when the owner says so. |
| **`stage7-task8-buf64`** | A scratch measurement of a 64 KiB buffer, never to be merged. Its deletion was refused by the proxy. | The owner deletes it. |
| `stage8-final`, `stage8-final-b`, `stage8-final-c` | Seen 2026-11-17, not listed before. Leftovers of the stage 8 fix wave. See below. | The owner decides on deletion. |
| `ccr-716d91b0-bff174` | **Abandoned, do not build on it.** It has 12 commits not on `main` by design. See below. | The owner decides on deletion. |

About the stage 8 leftovers:

- `git cherry` shows one to three commits each as not on `main`.
- All but a `Handoff: date` line have a same-titled commit on `main`, so they look rebased.
- They were not diffed.

About `ccr-716d91b0-bff174`:

- A session that missed `claude/magical-dijkstra-2eoh4y` redid stage 3 Tasks 1–4 there.
- Everything worth keeping was ported (stage 3 record, "Unification").

Each task is cut as a short-lived branch from `main` and fast-forwarded after review.

Merged branches are cleaned up with `scripts/cleanup-merged-branches.sh`:

1. Run it as a dry run first.
2. Then run it with `--delete`. It deletes locally and on the remote.
3. If the git proxy refuses, run it from the owner's clone.

What the script does:

- It skips these branches:
  - `main`;
  - `benchmarks`;
  - checked-out branches.
- It lists unmerged work.
- It reports a refused deletion without stopping, e.g. HTTP 403 from the git proxy.

## Owner's way of working (mandatory, see CLAUDE.md)

These rules bind every session.

- **Subagent-driven development** (`.claude/skills/subagent-driven-development`), one plan task at a time.
  - **Stop and report after each task**, and wait for "Go".
  - Tiny doc-only changes may be done inline.
- **Fix every finding as soon as possible** (owner, 2026-10-26). Minor findings go into the task's own fix loop instead of being deferred.
- Strict red/green TDD:
  - Implementers write the failing test first and paste real RED output.
  - Tests that pin existing behaviour are proven by a temporary mutation. Afterwards, `touch` the sources so cargo rebuilds.
- Commit **and push** after every step.
  - Report the pushed **SHAs**.
  - Report the **GitHub URL of every changed Markdown file**: `https://github.com/ThePixelPilgrim/durchlauferhitzer/blob/<branch>/<path>`, with `main` as the branch once the change has landed.
- Commit trailer lines (from the session's attribution reminder) end every commit message.
- Fast-forward `main` after every task that passes review and the gate (standing permission, 2026-10-26). Never rewrite `main`.
- Dependencies and `unsafe`, as the spec has them:
  - Pure Rust.
  - The core has zero dependencies and no `unsafe`.
  - The driver depends on `libc` only and keeps `unsafe` inside `driver/src/sys/`.
  - The TLS stage adds the primitives of open decision 5.
- Code rules:
  - exhaustive enums (no `_ =>` on enums);
  - enums over bools;
  - no `too_many_arguments` allow in core;
  - bounded memory;
  - illegal states unrepresentable;
  - files under 900 lines.
- Fable must not implement. It delegates to Opus/Sonnet.
- **Reader-facing prose** (owner, 2026-11-12, CLAUDE.md "Reader-facing prose"). Reviewers cite it when they flag prose. Its rules:
  - Three or more parallel items go in a list or table.
  - One idea per paragraph.
  - No nested parentheses.
  - No "the same holds for" cross-references.
- The owner prefers asking before inventing.
  - Give a recommendation.
  - List every decision taken on their behalf ("calls I made") in the report.

## How a task is run

Every plan task runs through these five steps.

1. **Workspace:** `bash .claude/skills/subagent-driven-development/scripts/sdd-workspace <plan>`. It creates the git-ignored `.superpowers/sdd/<plan>/` with its ledger `progress.md`.
2. **Brief:** record `Task N: BASE <sha>`, then `scripts/task-brief <plan> N` writes the brief.
3. **Implement:** dispatch an implementer. Use Opus for interface- or security-heavy tasks and Sonnet for small ones. Give it:
   - the brief path;
   - the context files;
   - the controller rulings;
   - the mandatory process block: TDD, the gate below, commit trailers, push to the task branch;
   - a report file path.

   The gate:

   ```sh
   cargo fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked && scripts/check-product-deps.sh && RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
   ```
4. **Review:** run `scripts/review-package <plan> BASE HEAD` and dispatch a reviewer (task-reviewer template), then adjudicate. Fix rounds resume the implementer, followed by a scoped re-review.
5. **Land:**
   - Run the gate on a fresh build (touch the sources).
   - Update this file.
   - Fast-forward `main`.
   - Write ledger lines for rulings and completion.
   - Report to the owner.
