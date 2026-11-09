# durchlauferhitzer
Just another HTTP proxy with very little features. HTTPS is planned for a later stage.

It serves static files and forwards requests to backends, routed by virtual
host and path prefix, on Linux (`epoll`, `openat2`, `sendfile`). It is pure
Rust; its one dependency is `libc`. See
[`examples/durchlauferhitzer.conf`](examples/durchlauferhitzer.conf) for a
documented configuration, and the
[design spec](docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md)
for every rule.

## Build

Linux 5.14 or later on x86_64 or aarch64, with glibc or musl (CI builds
and tests all four, musl on Alpine), and the stable Rust toolchain pinned
in `rust-toolchain.toml`. The binary links the system libc dynamically
(`.cargo/config.toml` turns off musl's default static linking); on Alpine
the linker needs `gcc` and `musl-dev`:

```sh
cargo build --release --locked
# the binary: target/release/durchlauferhitzer
```

Build the musl binary on Alpine: on a glibc host, `--target *-musl` with
`-crt-static` links the host's glibc dynamically; for a static musl binary
there, override it with `RUSTFLAGS="-C target-feature=+crt-static"`.
Packagers: `RUSTFLAGS` in the environment replaces `.cargo/config.toml`'s
`rustflags`, so a musl build with `RUSTFLAGS` set must add
`-C target-feature=-crt-static` itself to link the system musl.

## Command line

```
usage: durchlauferhitzer <config-path>
       durchlauferhitzer --check <config-path>
       durchlauferhitzer --ready-fd <n> <config-path>
```

`--ready-fd <n>` serves like the first form, beside a running server on
the same ports, and writes `ready\n` to descriptor `n` (then closes it)
once it accepts connections; a failed start closes it unwritten (exit
2). It is the join-and-take-over mode of a restart: do not point a
supervisor's readiness descriptor (e.g. s6's `notification-fd`) at it
for an ordinary start. See `SIGHUP` below.

## Check a config

```sh
durchlauferhitzer --check examples/durchlauferhitzer.conf
```

prints `ok` (exit 0), or the first error as `<path>:<line>:<column>:
<message>` on stderr (exit 1); a file that cannot be read is
`durchlauferhitzer: <path>: <reason>` (exit 1). It only reads the file: no socket is bound and
no static root is opened.

## Try it

The example config listens on `127.0.0.1:8080` and runs as any normal user
(no root). Its static and `internal_files` roots must exist. They are
under `/tmp`, which every user can write to: on a shared machine another
user could create `/tmp/durchlauferhitzer` first (or a symlink there), so
use roots you own instead and change them in the config. On your own
machine:

```sh
mkdir -p /tmp/durchlauferhitzer/www /tmp/durchlauferhitzer/well-known \
         /tmp/durchlauferhitzer/protected
echo '<p>hello</p>' > /tmp/durchlauferhitzer/www/index.html
target/release/durchlauferhitzer examples/durchlauferhitzer.conf
```

On a kernel without Landlock or seccomp the example needs
`containment { landlock best_effort; seccomp best_effort; }` to start.

and in another terminal:

```sh
curl -i -H 'Host: example.com' http://127.0.0.1:8080/      # 200, the file
curl -i -H 'Host: example.com' http://127.0.0.1:8080/api/x # 502: no backend on 127.0.0.1:3000
curl -i http://127.0.0.1:8080/                             # 404: unknown host
```

The server logs one `key=value` line per request and event on stderr, with no
timestamp (journald or a collector adds one), e.g.
`kind=response conn=2 peer=127.0.0.1 status=200 method=GET host=example.com path=/`.
Logging never blocks: lines stderr cannot take are dropped and counted
(`kind=dropped count=N`).

Startup checks, in this order, and fails with one `durchlauferhitzer:
<message>` line and exit 2: `openat2` support, every static and
`internal_files` root (it must be a directory), binding every `listen`
address in config order, the descriptor budget (`RLIMIT_NOFILE`: a low soft
limit is raised to what is needed when the hard limit allows, else the
error names both numbers; raise the limit or lower `max_connections`), the
privileges (below), and a containment layer set `required` that the kernel
cannot apply (below).

## Privileges

- Started as a normal user, the server keeps that user (a `user` setting is
  then ignored, and that is logged once). Ports below 1024 need root,
  `CAP_NET_BIND_SERVICE`, a lowered `net.ipv4.ip_unprivileged_port_start`,
  or socket activation; a refused bind is a startup error naming the
  address.
- Started as root (any of the real, effective or saved uid 0), `user` is
  required: after binding and opening the static and `internal_files`
  roots the server drops to that user and group for good, and refuses to
  run as root otherwise (exit 2).

## Containment

After startup the server confines itself (config block `containment`,
see the example config): Landlock lets it only read beneath its static and
`internal_files` roots, and a seccomp filter allows only the system calls it
needs. Each layer is `required` (the default: missing or partial is exit 2),
`best_effort` (used where the kernel has it, otherwise logged as missing and
the server serves) or `off`; the startup log says what applies
(`kind=containment ...`, one line per layer). A blocked call ends the
process (`seccomp_action kill`, the default) or, with `seccomp_action fail`,
gets `EPERM`; either way the kernel logs it:

```sh
journalctl -k | grep seccomp    # or, without systemd: dmesg | grep seccomp
```

Use Linux 6.2 or later (6.10 or later restricts every right with
Landlock alone; before 6.2 truncation is not restricted), and static and
`internal_files` roots the server's user cannot write. Landlock never
restricts `O_PATH` opens: the existence and metadata (size, owner, times)
of paths outside the roots stay visible to the server process itself, not
their contents. Clients cannot see them: requests resolve beneath the roots
(`openat2` `RESOLVE_BENEATH`).

Under containment a panic's backtrace (`RUST_BACKTRACE`) is not
symbolized; with `seccomp_action kill` the attempt ends the process by
SIGSYS.

## systemd socket activation

systemd can bind the sockets, so the service never needs root. The config's
`listen` addresses must be exactly the passed sockets (`listen 0.0.0.0:80
[::]:80;` here), and an IPv6 socket must accept IPv6 only:

```ini
# /etc/systemd/system/durchlauferhitzer.socket
[Socket]
ListenStream=0.0.0.0:80
ListenStream=[::]:80
BindIPv6Only=ipv6-only

[Install]
WantedBy=sockets.target
```

```ini
# /etc/systemd/system/durchlauferhitzer.service
[Service]
ExecStart=/usr/local/bin/durchlauferhitzer /etc/durchlauferhitzer.conf
User=www-data
```

Leave `user` out of the config here: the service already runs as `User=`.
`systemctl enable --now durchlauferhitzer.socket` starts it on the first
connection. Without `LISTEN_PID`/`LISTEN_FDS` the server binds itself; a
socket that does not match the config, or a dual-stack IPv6 socket, is
exit 2.

## Restart without systemd

`contrib/durchlauferhitzer-reload <binary> <config> <pidfile> <logfile>
[ready-timeout-seconds [grace-seconds]]` (POSIX `sh`) restarts a running server without
losing a connection; the OpenRC script's `reload` calls it
([`contrib/`](contrib)). In order, it

1. takes the lock `<pidfile>.lock` (a second reload meanwhile is refused;
   one whose helper no longer runs is taken over, with a note), checks
   the config (`--check`) and stops on an error;
2. sets `net.ipv4.tcp_migrate_req` to 1 (Linux 5.14 or later; left set),
   so connections queued on the old process move to the new one when it
   closes its listeners;
3. checks that the pid in `<pidfile>` runs `<binary>`: replace the binary
   in place to upgrade (a symlink re-pointed to another version is
   refused);
4. starts `<binary> --ready-fd 3 <config>` beside it, in its own session,
   output appended to `<logfile>`, and lists its pid in
   `<pidfile>.draining` at once (so `stop` ends it if the helper dies);
5. waits for `ready` (default 30 s); if it does not come, it sends the
   new process `SIGHUP` (one already serving drains, its queue moving to
   the old process; one stuck before its event loop is killed after
   10 s) and leaves the old one alone. Until it is ready, every new
   connection goes to the old process; once ready, the new process steers
   them all to itself;
6. adds the old pid to `<pidfile>.draining`, writes the new pid to
   `<pidfile>`, waits the grace (default 1 s), checks that the new process
   still runs (if not, `<pidfile>` names the old one again and the helper
   exits 1) and sends the old one `SIGHUP` (it drains, then exits 0); if
   the new process is gone right after, the helper exits 1: start the
   server again. The OpenRC script's `stop` also
   stops the processes still draining (each checked by binary and start
   time, so a reused pid is left alone);
7. waits (at most 10 s, then a warning) until the old process has closed
   its listeners, then releases the lock.

It exits 0, or 1 with the reason. Notes:

- Only a process with the same effective uid at bind time can share the
  ports: start the server as root with `user` set (it binds, then
  drops), and run the helper as root.
- Only a `listen` address kept exactly is shared. One the new config
  drops closes with the old process, and its queued connections are
  reset; a changed one (`0.0.0.0:80` to `127.0.0.1:80`) is separate, and
  its queued connections are reset if the new process fails. Change
  addresses with a full stop and start if nothing may be lost.
- Do not run the helper under `nohup`: the new process would ignore
  `SIGHUP` too, so one the helper must stop is left to `stop`.
- Upgrading from a stage 5 binary (listeners without `SO_REUSEPORT`)
  needs one full stop and start.
- The grace lets handshakes under way on the old listeners finish before
  they close. A Linux race resets a handshake whose last ACK reaches a
  listener exactly as it closes, so one begun before the steering and
  still unfinished after the grace (a client round trip above it, or a
  retransmitted SYN-ACK) can still be reset.
- The wait for the old listeners reads `/proc/<pid>/fd` of a server
  running as `user`: that needs root with `CAP_SYS_PTRACE` (as root
  normally has on a host; not in a container that drops it). Without it
  the helper warns and releases the lock without waiting.

## Signals and exit codes

- `SIGTERM`: graceful shutdown. The listeners close (new connections are
  refused), idle connections close, requests in flight finish (a request
  that arrives on an open connection gets `503` with `Connection: close`),
  then the server exits. It waits at most `shutdown_timeout` (default 30s),
  then cuts what is left. systemd's `systemctl stop` sends `SIGTERM`; keep
  `TimeoutStopSec` above `shutdown_timeout` plus 2 s.
- A second `SIGTERM` during the graceful shutdown, or `SIGINT` (Ctrl-C): fast shutdown; every connection is
  cut at once.
- `SIGHUP`: restart drain — the listeners close, the connections held are
  served with `Connection: close`, bounded by `restart_timeout` (default
  10m). A second `SIGHUP` is ignored; `SIGTERM` during it becomes the
  graceful `SIGTERM` drain, bounded by `shutdown_timeout` from then;
  `SIGINT` or a second `SIGTERM` cuts every connection. The server's own
  listeners set `SO_REUSEPORT`: a `--ready-fd` start shares the ports; a
  second normal start still fails ("Address already in use"). A server
  binding as a non-root user logs so at startup (`kind=shared_ports`):
  any process of that user could join its ports.
  `SIGPIPE` is ignored.
- Exit codes: 0 after any shutdown (and `--check` on a valid config), 1 for
  an invalid or unreadable config or bad arguments, 2 for a startup
  failure or a failure of the event loop itself (printed as
  `durchlauferhitzer: event loop: <reason>`).

## Fuzzing and long runs

The nightly workflow (`.github/workflows/nightly.yml`, also started by
hand) runs the randomized sim nets over many seeds and fuzzes the parsers.
Locally:

```sh
SIM_SEEDS=0..20000 cargo test --release -p durchlauferhitzer-sim --test faults -- random_
cd fuzz && cargo +nightly fuzz run --debug-assertions request_head -- -max_total_time=60 -dict=http.dict
```

A failing seed prints its `SIM_SEED=<seed>` replay line; the targets and
crash replay are in [`fuzz/README.md`](fuzz/README.md).
A failing (or timed-out) nightly job files or updates a GitHub issue labelled `nightly`,
keyed by a fingerprint of the failure (`scripts/nightly-report.py`).

## Benchmarks

CPU time and system calls per request, measured nightly: [BENCHMARKS.md](https://github.com/ThePixelPilgrim/durchlauferhitzer/blob/benchmarks/BENCHMARKS.md) (on the `benchmarks` branch; the tool is [`bench/`](bench)).

## Repository layout

- [`core/`](core) — the product: a sans-IO HTTP core with zero dependencies (config, parsing, routing, static files, proxying).
- [`driver/`](driver) — the `durchlauferhitzer` binary: the Linux `epoll` driver that runs the core over real sockets, files, threads and signals.
- [`sim/`](sim) — test-only simulation driver: simulated network, clock, file system and backends, with contract checks and randomized runs.
- [`fuzz/`](fuzz) — test-only fuzz targets for the parsers and the config (outside the workspace).
- [`bench/`](bench) — test-only benchmark tool: CPU time and system calls per request (outside the workspace).
- [`examples/`](examples) — documented example configuration.
- [`contrib/`](contrib) — the reload helper and the OpenRC script.
- [`docs/superpowers/`](docs/superpowers) — design spec, implementation plans per stage, and review records.
- [`scripts/`](scripts) — CI helpers (product dependency check, nightly failure issues, the benchmark page).

## License

MIT — see [`LICENSE`](LICENSE).
