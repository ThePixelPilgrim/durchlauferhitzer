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
                    `StopHandle`, `LogSink`, `Buffered`, `LogEntry`,
                    `DriverLog`)
  main.rs           the binary: `process::run` with the arguments
  process.rs        the command line (FR-PROC-01: serve, `--check`, the
                    usage), the exit codes (`ExitStatus`, FR-PROC-05a),
                    the startup sequence (A11)
    budget.rs         NFR-RES-05: `needed` (the formula in its docs and
                      the spec), `ensure` (raise the soft limit, or fail
                      naming both numbers)
    privileges.rs     A8: `check_ports` (below 1024 needs root without
                      systemd), `drop_privileges` (root needs `user` and
                      drops to it; non-root keeps, logged)
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
    files.rs          the file contract (DRV-07…09): `Files` (each file id
                      from `OpenFile` to `CloseFile`; `Closing` while a
                      readahead is in flight), file actions to the pool,
                      pool results to events, `sendfile` of the segment
    tests.rs          unit tests of the loop's decisions on a driver that
                      is built but not run
  conns.rs          epoll tokens (`Listener | Client | Upstream | Wake |
                    Signal | Log` in a `u64`), the per-client and per-upstream
                    records and the registration each wants
  log.rs            `LogSink` (`stderr()`, `fd(OwnedFd)`, `channel()` for
                    tests; `write`, `flush`, `buffered` → `Buffered`):
                    O_NONBLOCK with the original mode put back on drop,
                    one stderr sink at a time, whole lines, never blocking (FR-LOG-06); `LogEntry`,
                    `DriverLog` (FR-LOG-07), `KeptReason`
    format.rs         the line format (FR-LOG-05): `format_entry` (a
                      backend by its address in `Core::backends`),
                      `format_driver`, the escaping
    buffer.rs         `EntryBuffer`: 64 KiB of whole entries, drop and
                      count when full, the `dropped` line once there is
                      room, before newer entries
  systemd.rs        socket activation (A7, crate-private): `take_sockets`
                    reads and removes `LISTEN_PID`/`LISTEN_FDS`, claims
                    fds 3.. and checks them against `listen`
                    (`Activation`)
  pool.rs           `Pool` (crate-private): the disk workers (A10):
                    `start`, `submit` (`Job::Open` / `Job::Readahead`),
                    `results` (`JobResult`), `shutdown` (joins)
  sys/              the only `unsafe` code: thin, checked syscall wrappers
    mod.rs            module rules
    fd.rs             `Io` (Done / WouldBlock / Closed / Err), the -1/errno
                      checks and EINTR retries the others share; for the
                      log sink: `write_fd` (write(2)), `status_flags` /
                      `set_status_flags` (`StatusFlags`, `Blocking`:
                      O_NONBLOCK), `pipe` (pipe2, O_CLOEXEC);
                      `claim_inherited` (an inherited descriptor: open
                      without FD_CLOEXEC, then marked)
    env.rs            `remove_vars`: only while the process has one thread
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
    eventfd.rs        `EventFd`: workers wake the loop (`notify`, `drain`,
                      `try_clone` for the pool's duplicate)
    signal.rs         `SignalFd` for SIGTERM / SIGINT / SIGPIPE (blocked with
                      `pthread_sigmask` first)
    user.rs           `lookup_user` (getpwnam_r), `drop_to`, `euid`
    rlimit.rs         `nofile`, `raise_nofile`, `set_soft_nofile`
                      (RLIMIT_NOFILE)
tests/
  cli.rs            the binary in a subprocess: the usage for bad
                    arguments, `--check` (ok, each kind of error with its
                    line:column, a missing file; no root or port touched)
  startup.rs        the startup sequence of the binary, run as non-root
                    (as `nobody` when the tests run as root): it serves; a
                    missing or non-directory root, a port in use and
                    overlapping listen addresses are exit 2 naming them;
                    every thread blocks SIGTERM/SIGINT/SIGPIPE;
                    `disk_threads` workers; fds 0-2 closed at exec are
                    `/dev/null` (the Rust runtime), so no socket lands there
  systemd.rs        socket activation of the binary: a listener passed as
                    fd 3 (`sh -c` sets `LISTEN_PID=$$`) is served; an
                    address not configured, a configured address without a
                    socket, a foreign `LISTEN_PID`, a file and a connected
                    socket are exit 2
  budget.rs         NFR-RES-05 in the binary with RLIMIT_NOFILE lowered
                    in the child: one short is exit 2 with both numbers,
                    exactly the budget starts, a low soft limit is raised
                    (three configs: proxy, static, both)
  privileges.rs     A8 in the binary: as non-root `user` is kept and
                    logged, a port below 1024 is exit 2; as root (else
                    skipped with a printed reason) no `user` and an unknown
                    user are exit 2, `user nobody` drops every id
                    (`drop_to` in the server's process) and serves, and a
                    systemd socket below 1024 serves as nobody
  stderr_sink.rs    `LogSink::stderr()` on the real fd 2, in a re-run of
                    the test binary: non-blocking while it lives, a second
                    one refused (`AlreadyExists`), the mode back after it
  sys.rs            the wrappers on the real kernel, unprivileged
  log.rs            the line format: an exact line per kind, every
                    reason / failure / file error value, the escaping
                    table, a seeded property (any entry is one line with
                    no raw control byte, and parses back to its strings)
  log_sink.rs       the sink on a pipe: O_NONBLOCK set and the flags put
                    back, a stalled reader (writes never block, whole
                    entries dropped and counted, one `dropped` line), a
                    reader that went away
  loop_log.rs       the running loop writes a backed-up log once the
                    reader catches up (EPOLLOUT), then idles without a
                    spin; alone in its binary (CPU time is process-wide)
  loop_static_less.rs  the event loop on loopback, no static roots: fixed
                    responses, keep-alive, lingering close, a proxied
                    exchange, an early 413 then a reset (I1), a client
                    reset under interest `None`, the header timeout
  loop_fd_leak.rs   a client reset mid-relay leaks no descriptor; alone in
                    its binary, since the fd count is process-wide
  loop_accept_pause.rs  out of descriptors: accepting pauses without a
                    spin and the 1 s retry resumes it; alone in its binary
                    (it lowers RLIMIT_NOFILE)
  static_files.rs   the file contract on loopback: a 10 MiB file to a slow
                    reader, directory 301 and index, 404 for a symlink, a
                    dotfile, a FIFO and a missing file, a FIFO that must not
                    hold the only worker, HEAD, a file truncated mid-transfer,
                    64 concurrent downloads through two workers, an empty
                    file, a sparse file over 4 GiB
  static_fd_leak.rs a client reset mid-`sendfile` closes the file and leaks
                    no descriptor; alone in its binary
  support/child.rs  running the binary: temp configs, a free port, the
                    child's setup before exec (`pre_exec`: closed fds,
                    fd 3, RLIMIT_NOFILE; the test crate's only `unsafe`),
                    a guard that kills a running server on drop
  support/ipv6.rs   the IPv6 tests' skip rule (DURCHLAUFERHITZER_REQUIRE_IPV6)
  support/server.rs the loop tests' harness: a `Driver` in a thread on an
                    ephemeral port (`start_static` opens the config's roots
                    and starts the pool), a std backend, response readers
  support/tree.rs   a temp-dir static root: files, symlinks, FIFOs (the
                    `mkfifo` command), deterministic contents
```

## Startup

`process::run` (FR-PROC-01, FR-PROC-04, A11) parses the arguments,
loads the config, then: the `openat2` probe beneath the first root (or
`/`), the roots (`O_PATH | O_DIRECTORY`), the listeners (systemd's,
`systemd.rs`, or bound in config order, the first failure named), the
descriptor budget (`process/budget.rs`), the privilege drop
(`process/privileges.rs`), the signal mask and its `signalfd` (before
any thread, so the disk workers inherit it), the stderr log sink, then
`Driver::new` with `limits.disk_threads` and `Driver::run`. Every
message (the usage, `--check`'s answer, startup errors) is printed
before the sink exists or after the driver holding it is gone; nothing
prints through std while it lives (FR-LOG-06). Exit codes are
`ExitStatus` (0 ok, 1 config or usage, 2 startup), private to
`process.rs`.

## The event loop

`Driver::new(core, listeners, roots, disk_threads, log)` registers the
listeners and the wake eventfd and starts the disk pool with
`disk_threads` workers (`limits.disk_threads`) exactly when the core has
static roots, since every `OpenFile` needs its answer (DRV-09); a
static-less core gets no threads and ignores the count. It spawns the
workers, so `SignalFd::new` must come before it (the mask is
inherited). It refuses (`InvalidInput`) a root list that does not match
`Core::static_roots` one to one, and zero threads for a core with
roots. `Driver::run` loops until a `StopHandle`
asks it to stop (`Exit::Stopped`; the signal-driven endings come with
Task 9), then shuts the pool down (its threads joined). It
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
  (`DriverLog::RegisterFailed`). A client is marked broken (no more I/O, taken out of the epoll set)
  and gets `Failed` after the actions of the current call; an open
  upstream is closed and gets `UpstreamFailed` the same way; a pending
  connect is answered `UpstreamFailed`; a socket just accepted is dropped
  before `Accepted`. A failure queued outside any call (a write that stops
  under `ReadWrite`) is delivered once that ready descriptor is handled. A
  queued failure is not delivered if the core closed the socket first.
- **Files** (DRV-07…09, A10). `OpenFile` and `Readahead` are jobs for the
  pool; each job holds its descriptor (`Arc<OwnedFd>`: the root for an
  open, the file for a readahead) only while it runs. Workers push each
  result to a channel and `notify` a duplicate of the loop's wake eventfd
  (`EventFd::try_clone`, one more descriptor in the NFR-RES-05 budget);
  when the wake fires the loop resets the counter, then turns every
  result in into its event (`FileOpened` with the `fstat` kind and
  stamp, `FileFailed` by the errno table, `ChunkReady` with the stamp at
  read time), in arrival order. An open failing with `EMFILE`/`ENFILE`
  is logged (`DriverLog::OpenFailed`) and is still `FileFailed { Other }`.
  `CloseFile` closes the descriptor at once, or, while a readahead is in
  flight, marks the file `Closing`: the descriptor stays open until that
  result arrives, which is then dropped (no event). Closing a file
  resumes paused listeners (DRV-16). One readahead per file at a time,
  as the core asks: a second one while one is in flight is answered
  `FileFailed { Other }` after the current call, unless a `CloseFile`
  came first (then nothing is delivered), a `Readahead` for a file
  that is not open is ignored (after `CloseFile` nothing may be
  delivered for it), and a `CloseFile` before the open's answer (the core never
  sends one) drops that answer and its descriptor.
- **`sendfile`** (DRV-03, DRV-09). Once `send_buffer` is empty the loop
  `sendfile`s `send_file`'s segment until it is done or the socket would
  block, each call reported as the very next `handle` call: `FileSent`
  with the count; a 0 return is `FileFailed { Truncated }`;
  `EPIPE`/`ECONNRESET` are the socket's `Failed { Reset }`; any other
  error is `FileFailed { Other }`. A segment of a file the driver does not
  hold open (never, from the core) is `FileFailed { Other }` rather than
  a wait for a writability the loop would never use.
- **Pool rules** (`pool.rs`). Every job gets exactly one result; a failure
  is a result, never a panic (R6). The queue is unbounded in the type but
  bounded by the core (one job per connection at a time, orphaned opens
  included: at most `max_connections`). `shutdown` stops the workers and
  joins them: a running job finishes, queued jobs are dropped (the loop
  that wanted them has ended); dropping the pool does the same. A worker
  blocked in the kernel (a hung disk) blocks the join; the bounded wait
  at exit is Task 9's.
- **Logging** (FR-LOG-05/06, DRV-12). Every core `Log` and every
  `DriverLog` goes to the `LogSink` as one `key=value` line. The sink
  writes at once as far as the descriptor takes it and keeps the rest
  (whole entries, 64 KiB); before each wait the loop registers the
  sink's descriptor for `EPOLLOUT` exactly while bytes wait (token
  `Log`) and flushes on readiness. The log never ends the loop: a
  failed registration leaves the bytes for the next entry's write and
  is retried next turn.
- **Failures of the loop itself** (`epoll_wait`, or `epoll_ctl` on the
  listeners or the wake eventfd) end `run` with the error; a failure of
  one socket never does.

Notes on the wrappers:

- Tests: the FIFO test is the guard for `O_NONBLOCK` (Review Focus 2):
  without it the open would block its worker until a writer came.
- Tests: on loopback `sendfile` of a 256 KiB chunk does not return
  `EAGAIN` in practice: `EPOLLOUT` is reported only once a third of the
  (autotuned, MiB-sized) send buffer is free, so each call takes a whole
  chunk. The slow-reader test makes the loop wait for writability again
  and again; the `EAGAIN` and short-count mapping is pinned by the unit
  test in `event_loop/files.rs`. Offsets past 4 GiB are tested on the
  wrappers (`tests/sys.rs`), since the core has no ranges.
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
  this crate's test process; it is tested in the server's own process by
  `tests/privileges.rs` (root only: skipped with a printed reason
  elsewhere, as on GitHub's runners).

## Commands

```sh
cargo test -p durchlauferhitzer --locked
cargo clippy -p durchlauferhitzer --all-targets --locked -- -D warnings
cargo tree -p durchlauferhitzer -e normal,build   # core + libc, nothing else
```
