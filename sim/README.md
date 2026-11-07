# durchlauferhitzer-sim

**Test-only** (`publish = false`, never part of the product). A simulation
driver for [`durchlauferhitzer-core`](../core): it plays the network, the clock
and the file system, so the whole server can be tested deterministically —
including the failures that are hard to provoke for real.

What it simulates:

- **Clients** — byte delivery in arbitrary chunks, slow or stalled readers
  (`Window`), end of input and resets.
- **Time** — a virtual clock; `advance` fires the core's deadlines in order.
  With `Sim::set_timer_order(TimerOrder::Late)` (NFR-TEST-07) it instead
  wakes once, at the end of the advance, as a real loop that overslept:
  every queued I/O and every disk completion due by then is delivered
  first, then the overdue `TimerFired` (default `TimerOrder::OnTime`).
- **A file system** ([`src/fs.rs`](src/fs.rs)) — files, directories, symlinks,
  denied entries, special files, slow disks, files that shrink or change
  while they are being sent. Each file has a stamp (size, mtime): `add_file`
  gives distinct mtimes of 1, 2, ... ns, `set_modified` sets one, and
  `file_stamp` reads what an open would report (`ETag`, FR-STATIC-18).
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
within their capacities, a client's `Read` interest only with room in its
receive buffer and its `Write` interest only with bytes in its send buffer or
a `send_file` segment (a level-triggered driver would spin otherwise), every file and upstream closed exactly once and never
left open after its connection, at most one upstream per client, `ReadWrite`
interest only on upstreams (and only with bytes to write and room to read),
body reads only into a drained buffer and with its full capacity (fill, drain,
refill; behind a chunked tail of at most 35 bytes, for request and response
bodies), every backend
receiving well-framed requests one at a time and nothing after a body
unless the core pooled the connection since (FR-PROXY-02; an idle pooled
upstream held by no client, with interest `Read`), no
stalled output, no deadline left in the past, and more. A violation fails the test with a replayable trace.

## Layout

```
src/
  lib.rs              Sim: the driver, client-side invariants
  clock.rs            `advance`, `next_due` and `TimerOrder`: on-time
                      and late timers (NFR-TEST-07)
  disk.rs             file ids, disk operations, sendfile, file invariants
  backend.rs          scripted backends, upstream pumping and invariants
  pool.rs             the core's idle pool as the sim sees it: idle
                      hygiene and drain checks, idle closes by cause
  fs.rs               the in-memory file system
  tests.rs            unit tests of the sim's own invariant checks
tests/
  support/
    model_http.rs     the spec's fixed responses, file heads (with `ETag`
                      and `Last-Modified`), 304s, redirects, IMF-fixdates
                      and parse-error statuses, shared by every test binary
  connections.rs      accepting, buffering, backpressure, resets
  date.rs             the `Date` header on every response the core renders,
                      none on `100 Continue`, the backend's kept
  routing.rs          request handling, keep-alive, pipelining
  limits.rs           timeouts, connection caps, lingering close
  shutdown.rs         the graceful drain and the fast path (FR-PROC-05):
                      `Sim::begin_drain(target)`, `abort_all`,
                      `is_drained`
  restart.rs          the restart drain (FR-PROC-07): what the core holds
                      is served with close, nothing refused or closed early
  late_timers.rs      the late-timer mode (NFR-TEST-07): I/O and disk
                      completions before an overdue `TimerFired`, and
                      the outcomes that differ by order (DRV-06)
  static_files/       static routes end to end (one test binary)
    main.rs             shared configs and helpers
    transfer.rs         chunks, slow disks and clients, changing files, resets
    rules.rs            methods, bodies, 404s, redirects, index files, HEAD
    edge.rs             index opens in flight, long redirects, encodings
    conditional.rs      conditional requests: `ETag`, `Last-Modified`,
                        `If-None-Match`, `If-Modified-Since`, 304s
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
    pool.rs             backend connection pooling: reuse across
                        clients, idle hygiene and timeout, the safe retry
                        (the other files' routes say `pool off;`)
  faults/             fault injection and randomized runs (see below)
    main.rs             shared configs and helpers
    split.rs            deterministic split, byte-by-byte and pipelined runs
    generator.rs        random requests and schedule steps
    model.rs            the model of the random file tree
    conditional.rs      the model of conditional requests (RFC 9110), the
                        conditional lines of random requests, coverage
    client.rs           a random or witness client, as the test sees it
    admission.rs        the peers of each seed and the spec's model of
                        what the connection limits admit
    oracle.rs           what each client must receive; coverage
    parse.rs            the parser model of the request templates: which
                        heads the parser rejects (no drain `503` for them);
                        where a parse error's request line comes from
    unanswered.rs       no request the spec says must be answered vanishes
    run.rs              one random schedule; the test over all seeds;
                        seed replay for every net of the binary
    shutdown.rs         the shutdown plans of both nets (drain, drain
                        then abort, abort, restart, restart then abort),
                        where they fell in the log, and the checks they add
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
      drain.rs            the shutdown in this net: when it falls, and
                          the outcomes a drain (shutdown or restart)
                          allows an exchange
      pool.rs             backend connection pooling (FR-PROXY-02, -26):
                          which exchange each upstream serves, dead and
                          poked pooled connections, the model of reuse
                          and of the retry
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
and hostile heads; bytes after the body; resets and ends of stream anywhere,
a reset also queued right behind the bytes before it, `backend_reset_after`;
random windows), on two proxy routes beside a static one, with random body
limits and timeouts. Each exchange is predicted by a model written from the
spec, not from the core: the backend must receive exactly the rewritten
request and the client's body, and the client exactly the rewritten head and
the backend's body bytes, a cut connection with its log, or the predicted
4xx/5xx. A response cut by a reset behind its bytes delivers every body
byte before it, then the cut is logged `BackendFailed { Reset }`
(FR-PROXY-26, DRV-10, NFR-RES-04). Backend connections are pooled
(FR-PROXY-02) on the routes the seed draws pooled (`pool off;` on the
others): the net follows which exchange each upstream serves by the
core's report of who holds it and by the client each forwarded byte was
written for, so a response paired with another request is a mismatch;
every reuse must have been left reusable by the exchange before it, by
the model (a pooled route, an HTTP/1.1 client, the whole request
written, an HTTP/1.1 response without `close`, framed, nothing after
it), and a retry may happen only by FR-PROXY-26's rule (a GET or HEAD
without a body on a reused connection that failed before any response
byte), and then must. Backends kill a reused connection as the request
arrives (reset or end of stream, before or after the first bytes of a
response head), end it right behind a response (the core may reuse it
before it sees that), or write to, close or reset it while idle (which
must close it at once, `Sim::idle_closes`). The coverage gates require
reuse (also across clients), idle closes by a byte, an end of stream,
an error, the idle timeout, the per-backend cap and a drain, a replay
that is relayed, a refused retry and none after a response byte. Timed scenarios (connect, response, body, head and send timeouts)
run alone in their round, so the clock only runs out deadlines the model
knows. To replay one failing seed (of both nets):

```sh
SIM_SEED=1234 cargo test -p durchlauferhitzer-sim --test faults
# one net only:
SIM_SEED=1234 cargo test -p durchlauferhitzer-sim --test faults -- random_proxy_schedules
SIM_SEED=1234 cargo test -p durchlauferhitzer-sim --test faults -- random_fault_schedules
```

`SIM_SEEDS=<from>..<to>` runs both nets over that range instead of their
defaults (`0..1500` static, `0..400` proxy); the nightly workflow's long
runs use it (NFR-TEST-08). `SIM_SEED` wins over it.

In the static net every connection is checked as it is accepted against
a model of the connection limits written from the spec
(`faults/admission.rs`): counted, refused with `503`, or closed at once
when the refused pool is full in all or for the peer's group
(FR-CONN-09 to -13). One seed in three draws IPv4 peers, one IPv6 peers
(two in one /64, one in another; `ipv6_prefix` default, 48 or 128), one
an IPv4 peer and its IPv4-mapped (or, every other such seed, NAT64) form.

One seed in four of either net (`seed % 4 == 0`) runs with late timers
(`TimerOrder::Late`); the order follows from the seed alone, and a failure
prints it beside `SIM_SEED`.

One seed in five of either net (`seed % 5 == 0`) also shuts the server down
(FR-PROC-05, `faults/shutdown.rs`): half of those call `begin_drain` at
a seeded step (then every client reads all it is sent), a third call
`abort_all` some steps after the drain, a sixth call `abort_all` without
a drain. Another one in five (`seed % 5 == 2`) restarts (FR-PROC-07):
two thirds of those call `begin_drain` with `DrainTarget::Restart`, a
third follow it with `abort_all` (`restart_timeout` expired). The plan
follows from the seed alone, by an RNG of its own (the other seeds keep
their schedules), and a failure prints it too; the coverage gates
require each restart plan in the per-push seed range. In the
static net two witness clients join at the drain, kept apart from the
random steps, and their bytes are checked exactly: one with a keep-alive
`404` queued (alone: the `404`, then the shutdown; with a request
pipelined behind it: the `404`, then the `503` with close), one idle (its
`404`, then the close at once). In a restart the pipelined one gets the
`404`, then a `404` with close; the other in-flight one only its `404`
(no shutdown: it is kept until its keep-alive deadline); the idle one
sends a second request just after the drain began and gets a `404` with
close. In the proxy net it falls among the
steps of a round without a timed scenario (or after the last round), the
round runs on, and the seed ends with it; an exchange whose head is
rendered during the drain must match the model's outcome for a client
that closes, or the drain's `503`; a head the parser rejects keeps its
own error (owner decision 2026-10-29, FR-PROC-05b). The static net
holds the same rule against a parser model of its templates
(`faults/parse.rs`, written into the template table, not taken from the
parser): a drain's `503` for a head the model rejects fails the seed.
No request may vanish (`faults/unanswered.rs`, and `silent_stop` in
`faults/proxy/drain.rs`): the drain records what the core had read of
each connection (`Sim::delivered_to_server`, FR-PROC-05b's "buffered"
input) and which file opens were pending (`Sim::opening_file`), the
abort which connections it found open. A request may go unanswered only
for a reason from the spec: none of it was read when the drain began,
the answer before it closes (every head rendered during the drain
does), the abort or the client cut the connection, or it never
completed. A request whose file was being opened when the drain began
gets its own status with close, never the `503`. The static net applies
the same check to every seed, before any shutdown too.
The oracles check what the spec says of the drain: a head logged before
it keeps what it said, one logged after it says `Connection: close` or is
the `503` with close for a request completed during it; a connection
accepted meanwhile is refused (`Rejected { ShuttingDown }`); the abort
logs each exchange it cuts; no upstream opens once the drain began; a
plain drain finishes (no connection left open) within the deadlines it
waits on; the core is drained (`is_drained`) after the wind-down. A
restart's drain differs as FR-PROC-07 says: no `503` answers any request
(every one gets its own answer with close, forwarded requests open their
upstreams), a connection accepted meanwhile is admitted like any other,
`begin_drain` closes no connection, a keep-alive response queued before
it is not followed by a shutdown, a request the core read any byte of
must be answered (no "unread at the drain" exemption), and the plain
drain may wait a keep-alive timeout longer to finish.
One branch is allowed but not reached: a restart's "closed before a
request" (`silent_stop`), which needs a keep-alive deadline to pass
during the drain before the next request is read, while the proxy net's
drain falls only in rounds without a timed scenario. It is not gated;
every proxy run prints its count (`PROXY RARE ...=<n>`, `RARE` in
`faults/proxy/mod.rs`), zero in `0..200000`.

Both nets (the whole `faults` binary) run with a date set (`Sim::set_date`,
the RFC 9110 example `Sun, 06 Nov 1994 08:49:37 GMT`), so every response
they check carries `Date`; the other binaries leave it unset and see none.

The static net's files have random mtimes around that date, and its
clients add conditional lines (`faults/conditional.rs`) to about two in
three `GET`s and `HEAD`s, while the head stays within `max_head`: the
file's real `ETag`, strong, weak, in a list or on two lines, `*`, other
and malformed tags, an empty list, `*` beside a tag, and dates equal to,
before and after the mtime, equal to the clock and after it, in all
three HTTP-date forms (a malformed `If-None-Match` is present and matches
nothing, so the date is ignored). Whether the answer is 200
or 304 comes from a model of RFC 9110 written in the net, never from the
core, and every kind of decision is gated in the coverage.

Every test binary takes the bytes of the responses the core writes itself
(fixed 4xx/5xx, file heads, 304s, redirects) and the status of each parse error
from a model written from the spec (`tests/support/model_http.rs`, included
by `#[path]`), never from the core's renderers.

## Commands

```sh
cargo test -p durchlauferhitzer-sim --locked
```
