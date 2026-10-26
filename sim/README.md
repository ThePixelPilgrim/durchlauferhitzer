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
  servers: connects accepted, refused or left hanging per backend address;
  the test reads what a backend received and makes it send bytes, close,
  reset or read slowly (`Window`).

What it checks: after every step the sim verifies the driver contract
documented in [`../core/src/core/mod.rs`](../core/src/core/mod.rs) from the
core's side — no actions for closed connections, files or upstreams, buffers
within their capacities, every file and upstream closed exactly once and never
left open after its connection, at most one upstream per client, body reads
only into a drained buffer and with its full capacity (fill, drain, refill), no
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
  connections.rs      accepting, buffering, backpressure, resets
  routing.rs          request handling, keep-alive, pipelining
  limits.rs           timeouts, connection caps, lingering close
  static_files.rs     static routes end to end
  proxy.rs            proxy routes end to end, with scripted backends
  faults.rs           fault injection and randomized runs (see below)
```

## Randomized runs

`tests/faults.rs` runs thousands of seeded random scenarios (requests, chunk
sizes, stalls, resets, clock jumps, file trees, disk faults). Each run checks
every response byte against a model of what the server should do, and the test
fails if any interesting outcome never occurs across all seeds. To replay one
failing seed:

```sh
SIM_SEED=1234 cargo test -p durchlauferhitzer-sim --test faults
```

## Commands

```sh
cargo test -p durchlauferhitzer-sim --locked
```
