# durchlauferhitzer-bench

The benchmark tool (NFR-PERF-08). Test-only tooling (NFR-MAINT-02):
excluded from the workspace, its own `Cargo.lock`, std only, nothing of it
reaches the binary. CI builds, lints and tests it; the nightly workflow
runs it and publishes the rows (`bench` and `bench publish` jobs).

```sh
cargo build --release --locked -p durchlauferhitzer
cargo run --release --locked --manifest-path bench/Cargo.toml -- \
  --binary target/release/durchlauferhitzer --out bench.csv \
  [--runs 5] [--duration 10] [--scenarios static-small,proxy-fresh]
```

Per run a fresh server: a temp static root with a 1 KiB and a 1 MiB file,
one proxy route to the in-process stub backend (1 KiB body, keep-alive),
default limits and containment, `user nobody` when run as root. Per
scenario: a warm-up (at most 1 s), the measured window (`duration`, 8
client threads), then the strace pass (another `duration`).

| Scenario | Traffic |
|---|---|
| `static-small` | 1 KiB file, keep-alive |
| `static-large` | 1 MiB file, keep-alive |
| `static-fresh` | 1 KiB file, a new connection per request |
| `proxy-small` | 1 KiB response, keep-alive client |
| `proxy-fresh` | as `proxy-small`, a new connection per request |
| `conditional-hit` | `If-None-Match` with the file's `ETag`: 304 expected; 200 while the server sends none |

CSV, one row per scenario and run:
`date,commit,runner,scenario,requests,errors,seconds,cpu_us_per_req,syscalls_per_req,rss_kib`.

- `requests`: complete responses with the expected status and body length;
  anything else (refused or reset connection, timeout, other status, wrong
  length) is an `error`, in either pass.
- `cpu_us_per_req`: the server's utime + stime (`/proc/<pid>/stat`, all its
  threads) over the measured window, per request. The client and the stub
  backend run in this process and are not counted.
- `syscalls_per_req`: the total of `strace -c -f -p <pid>` (through `sudo
  -n` unless root) over the strace pass, per that pass's requests. Never
  used for CPU. The strace window has about 6 s without load around the
  load (attach and settle); the idle server's few calls there are
  negligible against the requests.
- `rss_kib`: the server's `VmRSS` after the measured window.
- `commit` and `runner`: `BENCH_COMMIT` (else `git rev-parse --short=7
  HEAD`) and `BENCH_RUNNER` (else `local`). Outside a git checkout the
  commit is `unknown`, and `scripts/bench-chart.py` drops such a row (it
  takes hex commits only).

A failure prints `bench failed: scenario=<s> reason=<r> (<detail>)` (the
nightly issue's fingerprint is `bench|<s>|<r>`) and exits 1; rows with
errors are still written. `scripts/bench-chart.py` renders the CSV: each
runner label is its own series. The nightly publishes to the `benchmarks`
branch only from the default branch; a dispatch from another branch keeps
its rows as the run's `bench-results` artifact.

```
src/
  main.rs      runs, scenarios, the measured window, failures
  cli.rs       the command line
  scenario.rs  the scenarios: path, connection reuse, expected response
  load.rs      the client threads and the response check
  backend.rs   the stub backend
  server.rs    the config, the server process, its /proc numbers
  strace.rs    the system-call pass
  measure.rs   /proc and strace parsing, per-request arithmetic
  row.rs       the CSV row and the date
```
