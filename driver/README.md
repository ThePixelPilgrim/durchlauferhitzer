# durchlauferhitzer (driver)

The product binary: the Linux `epoll` driver that runs the sans-IO core in
[`../core`](../core) over real sockets, files, threads and signals (stage 4).
The core decides everything about HTTP; this crate owns all I/O and carries
out the core's actions exactly as the driver contract in
[`../core/src/core/contract.rs`](../core/src/core/contract.rs) says (DRV-01…14
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
                    `mod sys`
  main.rs           the binary (a usage stub until the CLI, Task 8)
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
    rlimit.rs         `nofile`, `raise_nofile` (RLIMIT_NOFILE)
tests/
  sys.rs            the wrappers on the real kernel, unprivileged
```

Notes on the wrappers:

- Reads and writes with an empty buffer return `Io::Done(0)` without a
  system call (a 0-byte `read` would look like the end of stream).
  `sendfile` returning 0 (the file ended early) is `Io::Closed`.
- `Epoll::wait` rounds a timeout up to whole milliseconds, so it never wakes
  before a deadline, and reports an interrupted wait as 0 events.
- `SignalFd::new` blocks the signals in the calling thread only: call it on
  the main thread before starting any other thread, so they inherit the mask.
- Tests: the IPv6 tests skip where the host has no IPv6 (for example a
  kernel booted with `ipv6.disable=1`); CI runs them.
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
