# durchlauferhitzer-sim

**Test-only** (`publish = false`, never part of the product). A simulation
driver for [`durchlauferhitzer-core`](../core): it plays the network, the clock
and the file system, so the whole server can be tested deterministically —
including the failures that are hard to provoke for real.

What it simulates:

- **Clients** — byte delivery in arbitrary chunks, slow or stalled readers
  (`Window`), end of input and resets.
- **Time** — a virtual clock; `advance` fires the core's deadlines in order.
- **A file system** ([`src/fs.rs`](src/fs.rs)) — files, directories, symlinks,
  denied entries, special files, slow disks, files that shrink or change
  while they are being sent.
- **Backends** ([`src/backend.rs`](src/backend.rs)) — scripted upstream
  servers: connects accepted, refused or left hanging per backend address
  (and a hanging one let through later, a slow accept);
  the test reads what a backend received and makes it send bytes, close,
  reset (also behind bytes it sent first, `backend_reset_after`; a reset
  is delivered by the next read or write the core asks for, never while
  the upstream's interest is `None`, as the contract demands) or read
  slowly (`Window`).

What it checks: after every step the sim verifies the driver contract
documented in [`../core/src/core/contract.rs`](../core/src/core/contract.rs) from the
core's side — no actions for closed connections, files or upstreams, buffers
within their capacities, every file and upstream closed exactly once and never
left open after its connection, at most one upstream per client, `ReadWrite`
interest only on upstreams (and only with bytes to write and room to read),
body reads only into a drained buffer and with its full capacity (fill, drain,
refill; behind a chunked tail of at most 35 bytes, for request and response
bodies), every backend
receiving exactly one well-framed request and nothing after its body, no
stalled output, no deadline left in the past, and more. A violation fails the test with a replayable trace.

## Layout

```
src/
  lib.rs              Sim: the driver, the clock, client-side invariants
  disk.rs             file ids, disk operations, sendfile, file invariants
  backend.rs          scripted backends, upstream pumping and invariants
  fs.rs               the in-memory file system
  tests.rs            unit tests of the sim's own invariant checks
tests/
  support/
    model_http.rs     the spec's fixed responses, file heads, redirects and
                      parse-error statuses, shared by every test binary
  connections.rs      accepting, buffering, backpressure, resets
  date.rs             the `Date` header on every response the core renders,
                      none on `100 Continue`, the backend's kept
  routing.rs          request handling, keep-alive, pipelining
  limits.rs           timeouts, connection caps, lingering close
  shutdown.rs         the graceful drain and the fast path (FR-PROC-05):
                      `Sim::begin_drain`, `abort_all`, `is_drained`
  static_files/       static routes end to end (one test binary)
    main.rs             shared configs and helpers
    transfer.rs         chunks, slow disks and clients, changing files, resets
    rules.rs            methods, bodies, 404s, redirects, index files, HEAD
    edge.rs             index opens in flight, long redirects, encodings
  proxy/              proxy routes end to end, with scripted backends
    main.rs             heads, failures, timeouts; shared helpers
    bodies.rs           request bodies, `100 Continue`, early responses
    expect.rs           `Expect` on every route
    responses.rs        response bodies: chunked, close-delimited, none,
                        limits, interim heads, cuts
    responses/http10.rs   HTTP/1.0 clients: request line, Transfer-Encoding
    resets.rs           backend resets behind readable bytes (DRV-10)
    timeouts.rs         the backend timeouts as configured `limits`, the
                        client's half-close, the failure log kinds
  faults/             fault injection and randomized runs (see below)
    main.rs             shared configs and helpers
    split.rs            deterministic split, byte-by-byte and pipelined runs
    generator.rs        random requests and schedule steps
    model.rs            the model of the random file tree
    oracle.rs           what each client must receive; coverage
    run.rs              one random schedule; the test over all seeds;
                        seed replay for every net of the binary
    proxy/              the randomized proxy net (see below)
      mod.rs              random configs, coverage, the test over all seeds
      request.rs          random client requests (smuggling attempts among
                          them) and the model of what the backend receives
      chunks.rs           payloads and chunked bodies, with framing defects
      response.rs         random backend responses (hostile ones among
                          them) and the model of what the client receives
      outcome.rs          exchanges, timings and the outcomes they allow
      draw.rs             drawing an exchange: plain ones, timed scenarios
      drive.rs            stories, rounds, backends, timed scenarios
      check.rs            the oracle: each story against the model
```

## Randomized runs

`tests/faults/` runs thousands of seeded random scenarios (requests, chunk
sizes, stalls, resets, clock jumps, file trees, disk faults). Each run checks
every response byte against a model of what the server should do, and the test
fails if any interesting outcome never occurs across all seeds. The
proxy net (`tests/faults/proxy/`) does the same for proxy routes: random
clients (bodies by `Content-Length` and chunked, `Expect`, oversized bodies,
pipelining, half-closes, resets, smuggling attempts) against random backends
(refused, hanging or slow connects; `Content-Length`, chunked,
close-delimited and empty responses of up to 2 MiB; interim heads; malformed
and hostile heads; bytes after the body; resets and ends of stream anywhere;
random windows), on two proxy routes beside a static one, with random body
limits and timeouts. Each exchange is predicted by a model written from the
spec, not from the core: the backend must receive exactly the rewritten
request and the client's body, and the client exactly the rewritten head and
the backend's body bytes, a cut connection with its log, or the predicted
4xx/5xx. Timed scenarios (connect, response, body, head and send timeouts)
run alone in their round, so the clock only runs out deadlines the model
knows. To replay one failing seed (of both nets):

```sh
SIM_SEED=1234 cargo test -p durchlauferhitzer-sim --test faults
# one net only:
SIM_SEED=1234 cargo test -p durchlauferhitzer-sim --test faults -- random_proxy_schedules
SIM_SEED=1234 cargo test -p durchlauferhitzer-sim --test faults -- random_fault_schedules
```

Both nets (the whole `faults` binary) run with a date set (`Sim::set_date`,
the RFC 9110 example `Sun, 06 Nov 1994 08:49:37 GMT`), so every response
they check carries `Date`; the other binaries leave it unset and see none.

Every test binary takes the bytes of the responses the core writes itself
(fixed 4xx/5xx, file heads, redirects) and the status of each parse error
from a model written from the spec (`tests/support/model_http.rs`, included
by `#[path]`), never from the core's renderers.

## Commands

```sh
cargo test -p durchlauferhitzer-sim --locked
```
