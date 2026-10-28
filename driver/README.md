# durchlauferhitzer (driver)

The product binary: the Linux `epoll` driver that runs the sans-IO core in
[`../core`](../core) over real sockets, files, threads and signals (stage 4).
The core decides everything about HTTP; this crate owns all I/O and carries
out the core's actions exactly as the driver contract in
[`../core/src/core/contract.rs`](../core/src/core/contract.rs) says (DRV-01…16
in the [design spec](../docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md)).

The package is `durchlauferhitzer`: a library (so tests can drive its parts)
plus the `durchlauferhitzer` binary.

Rules for this crate (see [`../CLAUDE.md`](../CLAUDE.md)):

- **One dependency besides the core: `libc`** (`default-features = false`),
  pinned in `Cargo.lock`. No other crate, no C. CI checks the tree
  (`scripts/check-product-deps.sh`, NFR-MAINT-04).
- **`unsafe` only in `src/sys/`.** The crate root has
  `#![deny(unsafe_code)]`, and only `mod sys` is `#[allow(unsafe_code)]`.
  Every `unsafe` block carries a `// SAFETY:` comment
  (`clippy::undocumented_unsafe_blocks`) and is kept to the one unsafe
  operation it needs (`clippy::multiple_unsafe_ops_per_block`); both are
  denied in the workspace lint table. Everything `sys` exports is safe to
  call.
- The workspace lints apply (`lints.workspace = true`): no `unwrap`,
  `expect`, `panic!`, indexing or wildcard enum matches outside tests. The
  workspace denies `unsafe_code` rather than forbidding it, so `sys` can
  allow it; core and sim forbid it at their crate roots.
- **Linux only** (NFR-PORT-01): the crate fails to build elsewhere.

## Layout

```
src/
  lib.rs            crate root: lint rules, the Linux and 64-bit checks,
                    the modules, re-exports (`Driver`, `Exit`,
                    `StopHandle`, `LogSink`, `LogEntry`, `DriverLog`, `Pool`)
  main.rs           the binary (a usage stub until the CLI, Task 8)
  event_loop.rs     `Driver`: the epoll loop (wait with the timeout from
                    `next_deadline`, dispatch, `TimerFired`), `deliver`
                    (one `handle` call, its actions in order, then the
                    queued connect answers), `apply` (every `Action`
                    matched), the clock and `set_date`, `Exit`,
                    `StopHandle`
    accept.rs         the accept pause (DRV-16): `classify` (transient or
                      pause), `AcceptGate` (paused until a descriptor
                      closes or the 1 s retry; one log line a second)
    clients.rs        accept, client reads, writes and errors, client
                      actions, resuming the listeners
    upstreams.rs      `Connect` and its later answer, upstream reads and
                      writes by interest, error deferral, upstream actions
    tests.rs          unit tests of the loop's decisions on a driver that
                      is built but not run
  conns.rs          epoll tokens (`Listener | Client | Upstream | Wake |
                    Signal` in a `u64`), the per-client and per-upstream
                    records and the registration each wants
  log.rs            `LogSink` (a channel sink for tests until Task 7),
                    `LogEntry`, `DriverLog` (FR-LOG-07)
  pool.rs           `Pool`: a stub until the disk worker pool (Task 6)
  sys/              the only `unsafe` code: thin, checked syscall wrappers
    mod.rs            module rules
    fd.rs             `Io` (Done / WouldBlock / Closed / Err), the -1/errno
                      checks and EINTR retries the others share
    epoll.rs          `Epoll` (level-triggered), `Readiness` (mirrors the
                      core's `Interest`), `EpollEvent`, `Trouble`
                      (EPOLLERR / EPOLLHUP)
    socket.rs         non-blocking TCP: `tcp_listen` (SO_REUSEADDR; IPv6
                      listeners IPV6_V6ONLY),
                      `accept_nonblocking`, `connect_nonblocking`
                      (`ConnectStart`), `take_error`, `set_nodelay`, `read`,
                      `write` (MSG_NOSIGNAL), `shutdown_write`, `local_addr`,
                      `is_listening_stream`
    file.rs           `openat2_beneath` (RESOLVE_BENEATH |
                      RESOLVE_NO_SYMLINKS, `OPEN_FLAGS`), `openat2_supported`,
                      `open_dir_path` (O_PATH roots), `fstat` → `Stat`,
                      `readahead`, `sendfile`, `file_error` (the contract's
                      errno → `FileError` table)
    eventfd.rs        `EventFd`: workers wake the loop (`notify`, `drain`)
    signal.rs         `SignalFd` for SIGTERM / SIGINT / SIGPIPE (blocked with
                      `pthread_sigmask` first)
    user.rs           `lookup_user` (getpwnam_r), `drop_to`, `euid`
    rlimit.rs         `nofile`, `raise_nofile`, `set_soft_nofile`
                      (RLIMIT_NOFILE)
tests/
  sys.rs            the wrappers on the real kernel, unprivileged
  loop_static_less.rs  the event loop on loopback, no static roots: fixed
                    responses, keep-alive, lingering close, a proxied
                    exchange, an early 413 then a reset (I1), a client
                    reset under interest `None`, the header timeout
  loop_fd_leak.rs   a client reset mid-relay leaks no descriptor; alone in
                    its binary, since the fd count is process-wide
  loop_accept_pause.rs  out of descriptors: accepting pauses without a
                    spin and the 1 s retry resumes it; alone in its binary
                    (it lowers RLIMIT_NOFILE)
  support/ipv6.rs   the IPv6 tests' skip rule (DURCHLAUFERHITZER_REQUIRE_IPV6)
  support/server.rs the loop tests' harness: a `Driver` in a thread on an
                    ephemeral port, a std backend, response readers
```

## The event loop

`Driver::new(core, listeners, roots, pool, log)` registers the listeners
and the wake eventfd (it refuses a core with static roots but no pool,
`InvalidInput`, since every `OpenFile` needs its answer, DRV-09); `Driver::run` loops until a `StopHandle` asks it to
stop (`Exit::Stopped`; the signal-driven endings come with Task 9). It
follows the contract in `core/src/core/contract.rs`; where the contract
leaves a choice, this is what the loop does:

- **Ids and tokens** (DRV-01). Connection ids count from 1 and are never
  reused; an epoll token carries the kind in its low 3 bits and the id
  above, so an event for a socket closed earlier in the same batch finds no
  entry and is dropped (DRV-05).
- **One event at a time** (DRV-02, DRV-04). Every event is one `handle`
  call; its actions are all performed before anything else, and every read
  or write is reported as the very next call. Reads and writes loop until
  the buffer is empty or the socket would block (DRV-03).
- **Timers** (DRV-06). The wait's timeout is the time to `next_deadline`
  (rounded up to whole milliseconds, so the loop never wakes early); after
  the ready descriptors of a wake are handled, `TimerFired` is delivered if
  the deadline has passed. The core's `Instant` is the monotonic clock since
  `Driver::new`.
- **`Date`** (DRV-13). Before each `handle` call the wall-clock second is
  checked and passed to `set_date` when it changed.
- **Clients.** `TCP_NODELAY` on every socket, buffer sizes left to the
  kernel (NFR-PERF-04). A client always stays in the epoll set: after the
  reads or writes its interest allows, an `EPOLLERR`/`EPOLLHUP` still
  pending is reported as `Failed` whatever the interest, with the error from
  `SO_ERROR` (`ECONNRESET`/`EPIPE` are `Reset`; a hang-up without an error
  is `Reset` too). The end of stream is reported once; the socket is not
  read again after it.
- **Accept** (DRV-16). Loops until it would block. `ECONNABORTED`,
  `EPROTO` and `EPERM` are skipped. Any other failure (`EMFILE`, `ENFILE`,
  `ENOBUFS`, `ENOMEM`, ...) is logged (`DriverLog::AcceptFailed`, at most
  once a second) and takes every listener out of the wait, so the loop
  never spins. They come back when the driver closes any descriptor (a
  client, an upstream, a failed connect) or 1 s after the pause, whichever
  is first: the wait's timeout is the earlier of that retry and
  `next_deadline`. A failure on resume pauses again.
- **Upstreams** (DRV-10). `Connect` calls `connect()` at once; an outcome
  known at once (connected, refused, `socket()` failed) is queued and
  delivered after the actions of that call. `socket()` failing with
  `EMFILE`/`ENFILE` is logged (`DriverLog::SocketFailed`). While connecting
  the socket waits for `EPOLLOUT`, then `SO_ERROR` decides. Once open, it is
  registered for its interest and taken out of the epoll set while that is
  `None`, so an error waits until the core reads or writes again. Under
  `ReadWrite` the loop reads before it writes; a write that fails there is
  not reported, the socket stops writing and goes on reading, and the reads
  report bytes, end of stream, then the error (if the reads end with the
  end of stream, the write's error is delivered right after it). Under `Write` alone, or under
  `ReadWrite` once the end of stream was delivered (no read follows), a
  failed write is reported at once. A failed upstream socket is closed when its
  `UpstreamFailed` is delivered; `CloseUpstream` forgets the entry.
- **Registration failures** (DRV-15). A failed `epoll_ctl` for one
  socket fails only that socket and is logged
  (`DriverLog::RegisterFailed`). A client is marked broken (no more I/O)
  and gets `Failed` after the actions of the current call; an open
  upstream is closed and gets `UpstreamFailed` the same way; a pending
  connect is answered `UpstreamFailed`; a socket just accepted is dropped
  before `Accepted`. A queued failure is not delivered if the core closed
  the socket first.
- **Failures of the loop itself** (`epoll_wait`, or `epoll_ctl` on the
  listeners or the wake eventfd) end `run` with the error; a failure of
  one socket never does.

Notes on the wrappers:

- Reads and writes with an empty buffer return `Io::Done(0)` without a
  system call (a 0-byte `read` would look like the end of stream).
  `sendfile` returning 0 (the file ended early) is `Io::Closed`.
- `Epoll::wait` rounds a timeout up to whole milliseconds, so it never wakes
  before a deadline, and reports an interrupted wait as 0 events.
- `SignalFd::new` blocks the signals in the calling thread only: call it on
  the main thread before starting any other thread, so they inherit the mask.
- Tests: the IPv6 tests skip, with a printed reason, where the host has no
  IPv6 (for example a kernel booted with `ipv6.disable=1`). With
  `DURCHLAUFERHITZER_REQUIRE_IPV6=1` a missing IPv6 is a failure instead;
  CI sets it, so CI cannot pass by skipping. The rule lives in one helper,
  `tests/support/ipv6.rs`, which `src/sys/socket.rs` and `tests/sys.rs`
  both include with `#[path]`.
- Tests: the signalfd test is a unit test in `src/sys/signal.rs`, because it
  raises a thread-directed signal (`pthread_kill` on its own thread) — a
  process-directed one could reach another test thread and kill the run.
  `drop_to` changes the whole process, so it is never called by the tests in
  this crate's test process; it is tested in a subprocess by the CLI tests
  (Task 8).

## Commands

```sh
cargo test -p durchlauferhitzer --locked
cargo clippy -p durchlauferhitzer --all-targets --locked -- -D warnings
cargo tree -p durchlauferhitzer -e normal,build   # core + libc, nothing else
```
