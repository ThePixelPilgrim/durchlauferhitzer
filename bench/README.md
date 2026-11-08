# durchlauferhitzer-bench

The benchmark tool (NFR-PERF-08). Test-only tooling (NFR-MAINT-02):
excluded from the workspace, its own `Cargo.lock`, std only, nothing of it
reaches the binary. CI builds, lints and tests it; the nightly workflow
runs it and publishes the rows (`bench` and `bench publish` jobs).

```sh
cargo build --release --locked -p durchlauferhitzer
cargo run --release --locked --manifest-path bench/Cargo.toml -- \
  --binary target/release/durchlauferhitzer --out bench.csv \
  [--runs 5] [--duration 10] [--scenarios static-small,proxy-fresh] \
  [--syscalls-only] [--netem "<netem args>" [--pause S]] [--commit <sha>]
```

`--syscalls-only` skips the measured window: only the strace pass runs,
the row's `requests` are that pass's, `cpu_us_per_req` and `rss_kib` are
empty.

`--netem "<args>"` (root only; `ip` and `tc` from iproute2, a kernel
with `sch_netem`; NFR-PERF-05): the server runs in its own network
namespace (`dlh-bench-srv`, via `ip netns exec`, listening on
10.77.0.2:8080), joined to the bench's namespace (the clients and the
stub backend, on 10.77.0.1) by a veth pair. Both ends carry `tc qdisc
... root netem <args>`, so `delay 50ms` delays each direction (100 ms
round trip) and a proxied request crosses the pair twice (client to
server, server to backend). `--pause S` (at most a third of
`--duration`) drops every packet on both ends (`netem loss 100%`) for S
seconds, starting a third into each measured window; the window then
prints `bench: pause <scenario> run=<n> rate_before=<req/s>
recovery_ms=<ms> gap_ms=<ms>`: from the pause's end to the first 250 ms
with at least half the rate before it (`none` if none), and the longest
time without a completed response. The namespace is removed at exit.
The nightly runs this only when dispatched with `bench_netem`.

Per run a fresh server: a temp static root with a 1 KiB and a 1 MiB file,
one proxy route to the in-process stub backend (keep-alive; the body is
64 KiB for `/medium`, 1 MiB for `/large`, else 1 KiB), and, when
`proxy-file` runs, a second proxy route to it, `/file` with
`internal_files x-accel-redirect /protected/ <files>` (FR-PROXY-32): the
backend answers `/1k` with `200`, `Content-Length: 0` and
`X-Accel-Redirect: /protected/1k.bin`, and the server sends the 1 KiB
`1k.bin` of `<files>` (the route only with that scenario, so
`bisect.sh` still runs every other scenario on a binary from before
stage 8),
default limits except three for `static-small-1k-idle` (one server
serves every scenario of a run): `max_connections 2048` and
`max_connections_per_ip 2048` (every client shares 127.0.0.1), and
`keepalive_timeout 125m`, which outlasts the idle connections' hold at
the longest `--duration` (two windows plus about 8 s). The other
scenarios hold a few connections, closed after each window, so neither
change touches them. Default containment, `user nobody` when run as
root. The server counts as started once `/proc/<pid>/fd` holds its
signalfd (made after the real listeners are bound) and it accepts a
connection: its bind probe listens for a moment first, and a connection
the probe takes is reset when it closes. Per
scenario: a warm-up (at most 1 s), the measured window (`duration`, 8
client threads), then the strace pass (another `duration`).

| Scenario | Traffic |
|---|---|
| `static-small` | 1 KiB file, keep-alive |
| `static-large` | 1 MiB file, keep-alive |
| `static-fresh` | 1 KiB file, a new connection per request |
| `proxy-small` | 1 KiB response, keep-alive client |
| `proxy-fresh` | as `proxy-small`, a new connection per request |
| `conditional-hit` | `If-None-Match` with the file's `ETag`: 304 expected (an `ETag` for every file since FR-STATIC-18; a pre-FR-STATIC-18 binary answers 200, counted correct) |
| `static-small-1k-idle` | as `static-small`, with 1 000 idle keep-alive connections held open (nightly: one run) |
| `proxy-medium` | as `proxy-small`, a 64 KiB response (on demand) |
| `proxy-large` | as `proxy-small`, a 1 MiB response (nightly: one run) |
| `proxy-file` | as `proxy-small`, but the backend names a 1 KiB file (`X-Accel-Redirect`, `Content-Length: 0`) that the server sends; no NFR-PERF target until the owner sets one from its first nightly |

Without `--scenarios` all but `static-small-1k-idle`, `proxy-medium` and
`proxy-large` run. The nightly runs the 1k-idle and `proxy-large`
separately, once (`--scenarios static-small-1k-idle,proxy-large --runs 1`,
their rows appended to the same CSV; NFR-PERF-03, NFR-PERF-05). That one opens its 1 000 connections before the warm-up (each makes
one request, then sends nothing) and holds them through the measured
window and the strace pass (NFR-PERF-03, NFR-PERF-06); its `rss_kib` is
the server with them open. Each idle connection the server closed by
then counts as an error. The bench itself then holds about 1 010
sockets: before such a scenario it raises its soft `RLIMIT_NOFILE` to the
hard limit with util-linux's `prlimit` (the server inherits it), and
fails at startup (`reason=startup`) if that leaves fewer than 1 064.

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
- `commit` and `runner`: the commit `--binary` was built from, from
  `--commit <sha>` (7 to 40 lowercase hex digits; the nightly passes
  the run's commit), else `local`; and `BENCH_RUNNER` (else `local`):
  1 to 64 letters, digits, `.`, `_` or `-`, any other value refused at
  startup (exit 2). The commit label never comes from the checkout the
  bench runs in or from the environment, so a local A/B run of two
  builds must name each build's commit, or its rows say `local`.
  `scripts/bench-chart.py` drops a row whose commit is not hex (`local`
  included).

## Finding the commit

`bisect.sh <scenario> <max-syscalls>` is a `git bisect run` script: it
builds the release binary of the checked-out commit, runs the bench tool
for that scenario (one run, `--syscalls-only`, `BENCH_DURATION` seconds
per window, default 2) and exits 0 if `syscalls_per_req` is at most
`<max-syscalls>`, 1 if above, 125 (skip the commit) if the build or the
bench fails or the row has errors, 2 on bad usage. It needs `strace`
(and passwordless `sudo` unless root), like the bench itself.

```sh
cp bench/bisect.sh /tmp/bisect.sh        # survives the checkouts
git bisect start <bad> <good>            # good: e34cc72 or later
git bisect run /tmp/bisect.sh static-small 25
```

The bench tool is built from `BENCH_DIR` (the crate directory, default
`bench` of the checked-out commit). Commits before `e34cc72` have no
bench tool: for a range reaching back further, point `BENCH_DIR` at a
fixed checkout of it (e.g. a `git worktree` of a later commit).
`scripts/test_bench_bisect.py` tests the exit codes with a stub `cargo`.

A failure prints `bench failed: scenario=<s> reason=<r> (<detail>)` (the
nightly issue's fingerprint is `bench|<s>|<r>`) and exits 1; rows with
errors are still written. `scripts/bench-chart.py` renders the CSV: each
runner label is its own series. The nightly publishes to the `benchmarks`
branch only from the default branch with the default budget (`bench_runs`
5, `bench_seconds` 10); a dispatch from another branch or with another
budget keeps its rows as the run's `bench-results` artifact. The publish takes only
the rows of the run's own commit dated within a day of the run
(`bench-chart.py --append ... --commit <sha>`); any other row is refused.

```
bisect.sh      the `git bisect run` script (tested by scripts/test_bench_bisect.py)
src/
  main.rs      runs, scenarios, the measured window, failures
  cli.rs       the command line (incl. --syscalls-only, --commit)
  fds.rs       the bench's open-files limit (raised for the idle scenario)
  scenario.rs  the scenarios: path, connection reuse, expected response
  load.rs      the client threads and the response check
  backend.rs   the stub backend
  server.rs    the config, the server process, its /proc numbers
  strace.rs    the system-call pass
  measure.rs   /proc and strace parsing, per-request arithmetic
  netem.rs     --netem: the namespace, the veth pair, netem, the pause and its recovery
  row.rs       the CSV row and the date
```
