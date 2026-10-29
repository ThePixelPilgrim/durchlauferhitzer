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

Linux 5.14 or later on x86_64 or aarch64 (glibc), and the stable Rust
toolchain pinned in `rust-toolchain.toml`:

```sh
cargo build --release --locked
# the binary: target/release/durchlauferhitzer
```

## Check a config

```sh
durchlauferhitzer --check examples/durchlauferhitzer.conf
```

prints `ok` (exit 0), or the first error as `<path>:<line>:<column>:
<message>` on stderr (exit 1). It only reads the file: no socket is bound and
no static root is opened.

## Try it

The example config listens on `127.0.0.1:8080` and runs as any normal user
(no root). Its static roots must exist:

```sh
mkdir -p /tmp/durchlauferhitzer/www /tmp/durchlauferhitzer/well-known
echo '<p>hello</p>' > /tmp/durchlauferhitzer/www/index.html
target/release/durchlauferhitzer examples/durchlauferhitzer.conf
```

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
<message>` line and exit 2: `openat2` support, every static root (it must be
a directory), binding every `listen` address in config order, the descriptor
budget (`RLIMIT_NOFILE`: a low soft limit is raised to what is needed when
the hard limit allows, else the error names both numbers; raise the limit or
lower `max_connections`), and the privileges (below).

## Privileges

- Started as a normal user, the server keeps that user (a `user` setting is
  then ignored, and that is logged once). Ports below 1024 need root or
  socket activation; a refused bind is a startup error naming the address.
- Started as root (any of the real, effective or saved uid 0), `user` is
  required: after binding and opening the static roots the server drops to
  that user and group for good, and refuses to run as root otherwise
  (exit 2).

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

## Signals and exit codes

- `SIGTERM`: graceful shutdown. The listeners close (new connections are
  refused), idle connections close, requests in flight finish (a request
  that arrives on an open connection gets `503` with `Connection: close`),
  then the server exits. It waits at most `shutdown_timeout` (default 30s),
  then cuts what is left. systemd's `systemctl stop` sends `SIGTERM`; keep
  `TimeoutStopSec` above `shutdown_timeout` plus 2 s.
- A second `SIGTERM`, or `SIGINT` (Ctrl-C): fast shutdown; every connection is
  cut at once.
- `SIGHUP` and `SIGPIPE` are ignored.
- Exit codes: 0 after any shutdown (and `--check` on a valid config), 1 for
  an invalid config or bad arguments, 2 for a startup failure.

## Repository layout

- [`core/`](core) — the product: a sans-IO HTTP core with zero dependencies (config, parsing, routing, static files, proxying).
- [`driver/`](driver) — the `durchlauferhitzer` binary: the Linux `epoll` driver that runs the core over real sockets, files, threads and signals.
- [`sim/`](sim) — test-only simulation driver: simulated network, clock, file system and backends, with contract checks and randomized runs.
- [`examples/`](examples) — documented example configuration.
- [`docs/superpowers/`](docs/superpowers) — design spec, implementation plans per stage, and review records.
- [`scripts/`](scripts) — CI helpers (product dependency check).

## License

MIT — see [`LICENSE`](LICENSE).
