# durchlauferhitzer-core

The product: all HTTP, routing, static-file and (from stage 3) proxy logic of
durchlauferhitzer, written as a **sans-IO core**. It never touches a socket, a
clock or a file. A *driver* owns all I/O, feeds the core `Event`s and carries
out the `Action`s it returns. The simulation in [`../sim`](../sim) is one
driver; the Linux `epoll` driver (stage 4) will be the other.

Rules for this crate (see [`../CLAUDE.md`](../CLAUDE.md)):

- **Zero dependencies** in `[dependencies]` and no build script — CI enforces
  it (`scripts/check-product-deps.sh`). Test-only crates (`proptest`) are fine.
- `#![forbid(unsafe_code)]`; no panics on any input (`unwrap`, `expect`,
  `panic!`, indexing and wildcard enum matches are denied lints).
- Exhaustive enums everywhere; illegal states unrepresentable.
- Bounded memory: every buffer has a fixed capacity derived from the config.

## Where to start

- **The driver contract** — the module docs of [`src/core/mod.rs`](src/core/mod.rs)
  describe exactly how a driver must talk to the core (buffers, readiness,
  file operations, deadlines, limits). Read this first.
- **The design** — [`../docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md`](../docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md).

## Layout

```
src/
  lib.rs              crate root and re-exports
  core/               the state machine behind `Core::handle`
    mod.rs              Core, buffers API, event dispatch; driver contract docs
    contract.rs         Event, Action, Interest, ConnId, ConnError
    request.rs          reading request heads, routing, fixed responses,
                        keep-alive, pipelining, lingering close
    static_files.rs     static routes: open, index.html, send, abort
    limits.rs           connection counting, refusals (503), orphaned opens
    timers.rs           deadlines and TimerFired
    test_support.rs     helpers shared by the unit tests
  conn.rs             per-connection state (ConnState) and buffers
  file.rs             file ids, file metadata/stamps, change policy
  time.rs             Instant: monotonic time supplied by the driver
  log.rs              structured log entries (the driver formats them)
  routing.rs          virtual hosts and segment-based route matching
  config/             the config language
    lexer.rs            tokens
    syntax.rs           nested blocks and settings
    validate/           semantic rules, defaults, line:column errors
    model.rs            the validated Config
    units.rs            durations and sizes
  http/               HTTP building blocks (pure functions, no state)
    request.rs          strict, resumable request-head parser
    target.rs           request-target validation (traversal, encodings)
    static_path.rs      decoding paths to file-system segments
    response.rs         rendering fixed responses, file heads, redirects
    status.rs           status codes
    mime.rs             content types
```

Unit tests live next to the code they test (`tests.rs` / `tests/` beside a
module when they are long). End-to-end tests that drive the whole core over a
simulated network live in [`../sim/tests`](../sim/tests).

## Commands

```sh
cargo test -p durchlauferhitzer-core --locked
cargo clippy -p durchlauferhitzer-core --all-targets --locked -- -D warnings
cargo doc -p durchlauferhitzer-core --no-deps --open   # contract docs
```
