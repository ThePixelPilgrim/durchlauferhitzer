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

- **The driver contract** — the module docs of [`src/core/contract.rs`](src/core/contract.rs)
  describe exactly how a driver must talk to the core (buffers, readiness,
  file operations, deadlines, limits). Read this first.
- **The design** — [`../docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md`](../docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md).

## Layout

```
src/
  lib.rs              crate root and re-exports
  core/               the state machine behind `Core::handle`
    mod.rs              Core, buffers API, event dispatch
    contract.rs         the driver contract (module docs); Event, Action,
                        Interest, ConnId, ConnError (client, file and
                        upstream events and actions)
    request.rs          reading request heads, routing, fixed responses,
                        keep-alive, pipelining, lingering close
    static_files.rs     static routes: open, index.html, send, abort
    proxy.rs            proxy routes: plan, connect, `100 Continue`, judge
                        the response head (interim heads dropped), 502/504
                        before it, timeouts (`expire_proxy` decides 408 vs
                        504 `BodyTimeout`, the head cap, send timeouts)
    relay.rs            the response side: queue the rewritten head, relay
                        the body, end the exchange, cut on a failure
    upload.rs           the request side once connected: write the head
                        and body to the backend, read the body from the
                        client, body errors (400, cut), a client end of
                        stream mid-body, the 408 for a stalled body
    limits.rs           connection counting, refusals (503), orphaned opens
    shutdown.rs         begin_drain, abort_all, is_drained; the 503 for
                        requests completed during the drain
    timers.rs           deadlines and TimerFired; closing on a timeout
    test_support.rs     helpers shared by the unit tests
  conn.rs             per-connection state (ConnState) and buffers
  file.rs             file ids, file metadata/stamps, change policy
  upstream.rs         upstream and backend ids, open upstreams, the
                      fixed-size buffers of a proxied exchange
  request_body.rs     a request body in the client's receive buffer:
                      what is due, unsent, kept as a chunked tail or
                      pipelined after it (fill, drain, refill)
  relay_body.rs       a response body in the upstream's receive buffer:
                      what is due (length, chunked, until close), unsent,
                      kept as a chunked tail; its limit (fill, drain, refill)
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
    response_head.rs    strict, resumable backend response-head parser
    head_line.rs        line scanning, limits, header-line, framing and
                        `Connection` rules shared by the two head parsers
    chunked.rs          strict, streaming chunked-body framing validator
    date.rs             the IMF-fixdate of the `Date` header (no time
                        library: days-from-civil arithmetic)
    forward.rs          proxy head rewriting: request head to the backend,
                        response head to the client, hop-by-hop rules
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
