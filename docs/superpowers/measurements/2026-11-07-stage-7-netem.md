# Stage 7 Task 8: epoll re-arms, body buffer size, `tc netem` (NFR-PERF-05)

2026-11-07, branch `stage7-task8`.

**Method.**
- **Local:** `bench`, release build, root, 4-core Xeon VM (kernel 6.18), loopback, 8 clients. Each cell is the median of 5 runs × 5 s. CPU is the server's utime+stime; syscalls come from the strace pass. The 32/64 KiB builds change only `UPSTREAM_RECV_MIN`, on a scratch worktree.
- **netem:** this kernel has no `sch_netem` and no veth, so netem ran on GitHub's `ubuntu-24.04` runners: Nightly dispatch `bench_netem=true`, `bench --netem`, 3 runs × 10 s.
  - Run 42380082090 used 16 KiB (`dacb1ae`).
  - Run 42380141379 used 64 KiB (scratch branch `stage7-task8-buf64`).
  - The two runs are on different runner hosts. Compare system calls freely; compare CPU only roughly.

## A. Eager I/O, deferred registrations (kept, `3bcbafb`)

Why a chunk cost about 4 `epoll_ctl`: per 16 KiB chunk the core sets upstream `None`, and the driver removes the socket from the set (DEL). It sets the client `Write` (MOD), then client `None` (MOD) and upstream `Read` (ADD).

The change has three parts:
- Interests are only noted.
- Before the wait, each noted socket is tried at once if its hint says the I/O will likely go through. At most 64 attempts per turn.
- Then each socket's registration is synced to what it wants at that point.

The unit test counts `epoll_ctl` per proxied MiB: 265 before, 1–7 after.

| Scenario | syscalls/req before | after | CPU µs/req before | after |
|---|---|---|---|---|
| static-small | 18.90 | 17.96 | 55.5 | 54.5 |
| static-large | 44.95 | 37.18 | 268 | 278 (runs 190–330 both) |
| proxy-small | 13.51 | **10.26** | 46.3 | **43.1** |
| proxy-medium (64 KiB) | 38.53 | **18.27** | 113.4 | **98.6** |
| proxy-large (1 MiB) | 421.8 | **140.0** | 1326 | **1039** |

## B. Upstream body buffer: 16 vs 32 vs 64 KiB (owner chose 32 KiB, `ad86dc3`)

All three are measured with A in place.

| Scenario | 16 KiB sys / CPU | 32 KiB sys / CPU | 64 KiB sys / CPU |
|---|---|---|---|
| proxy-small | 10.26 / 43.1 | 10.26 / 47.3 | 10.26 / 50.3 |
| proxy-medium | 18.27 / 98.6 | 14.26 / 83.0 | 12.26 / 87.3 |
| proxy-large | 140.0 / 1039 | 75.0 / **626** | 43.0 / **514** |
| static-small, static-large | unchanged (no upstream buffer) | | |

The block above ran each build in sequence. A second block interleaved the builds (5 rounds, 1 run each):

| Scenario | 16 KiB | 32 KiB | 64 KiB |
|---|---|---|---|
| proxy-small CPU µs | 48.9 | 47.7 | **51.6** |
| static-small CPU µs (control) | 59.7 | 60.2 | 60.0 |

- **proxy-small, 32 KiB:** within noise.
- **proxy-small, 64 KiB:** about 5–10 % more CPU. The cause is the zeroed `vec![0; cap]` that every exchange allocates (`ExchangeBuffers::new`), even for a 1 KiB response.

Memory, with default limits:

| Buffer | worst_case_per_connection (proxied) | connections in `memory_limit` 1 GiB | 1024 (default `max_connections`) × worst case |
|---|---|---|---|
| 16 KiB | 50 404 B (49.2 KiB) | 21 302 | 49.2 MiB |
| 32 KiB | 66 788 B (65.2 KiB) | 16 076 | 65.2 MiB |
| 64 KiB | 99 556 B (97.2 KiB) | 10 785 | 97.2 MiB |

- The worst case is client recv 16 KiB + send 1 KiB + upstream recv + head buffer 16 612 B.
- **Idle connections do not hold the buffer.** It exists only during an exchange, in the connection's proxy state. A keep-alive client and a pooled backend connection hold none, so NFR-PERF-03 (~20 KiB per idle connection, `static-small-1k-idle`) is unaffected.

### Final: 32 KiB in the product (`ad86dc3`, owner decision 2026-11-07)

Measured interleaved with the 16 KiB build (A in place in both), 5 rounds of one 5 s run each, medians. The host was a little busier than for the tables above.

| Scenario | syscalls/req 16 KiB | 32 KiB | CPU µs/req 16 KiB | 32 KiB |
|---|---|---|---|---|
| static-small (control) | 17.93 | 17.93 | 62.2 | 60.7 |
| proxy-small | 10.26 | 10.26 | 51.3 | 49.2 |
| proxy-medium | 18.27 | **14.26** | 120.8 | **94.6** |
| proxy-large | 140.0 | **75.0** | 1116 | **651** |

With default limits, `worst_case_per_connection` (proxied) is now 66 788 B (was 50 404 B). 1024 × 66 788 = 68 390 912 B (about 65 MiB), and 16 076 such connections fit the 1 GiB `memory_limit` (was 21 302).

From A and B together, against the stage 7 stop baseline (16 KiB, before A): proxy-large goes from 422 to 75 syscalls per request (5.6× fewer) and from about 1 330 to about 650 µs of CPU.

## C. `tc netem` (CI; netem on both veth ends)

| Setting | Scenario | req/s 16 KiB | 64 KiB | syscalls/req 16 KiB | 64 KiB | CPU µs/req 16 KiB | 64 KiB |
|---|---|---|---|---|---|---|---|
| delay 50 ms each way | static-large | 74 | 74 | 43.2 | 41.8 | 215 | 204 |
| | proxy-small | 39 | 39 | 11.9 | 11.9 | 50* | 50* |
| | proxy-large | 36 | 36 | 144.5 | 44.6 | 516 | 496 |
| loss 1 % | static-large | 1 096 | 975 | 41.8 | 40.6 | 311 | 283 |
| | proxy-small | 1 184 | 1 236 | 11.4 | 11.3 | 43 | 55 |
| | proxy-large | 486 | 491 | 143.2 | 49.7 | 894 | 567 |
| 2 s pause (loss 100 %) | static-large | 2 488 | 3 144 | 37.4 | 37.2 | 321 | 264 |
| | proxy-small | 18 894 | 16 849 | 10.3 | 10.5 | 35 | 40 |
| | proxy-large | 744 | 1 556 | 139.9 | 42.5 | 920 | 438 |

\* About 400 requests per window, so CPU is quantized to whole 10 ms ticks.

Recovery after the 2 s pause (3 runs each; from the pause's end to the first 250 ms at half the earlier rate / longest gap without a response):

| Scenario | 16 KiB | 64 KiB |
|---|---|---|
| static-large | 1000, 1000, 1000 ms / 3.09 s | 1050 ×3 ms / 3.14 s |
| proxy-small | 1200 ×3 ms / 3.32 s | 1200 ×3 ms / 3.29 s |
| proxy-large | 1100, 1050, 1000 ms / 3.14 s | 1050, 1000, 1050 ms / 3.12 s |

No run had errors.

- **Throughput under delay and loss is set by TCP, not by our buffer.** It is the same for 16 and 64 KiB: 36 req/s for proxy-large at a 100 ms round trip, and about 490 req/s at 1 % loss.
- **Recovery is TCP's retransmit backoff.** Retransmits come at about 0.2, 0.6, 1.4 and 3.0 s after the first loss, so a 2 s pause ends with about 1 s more to wait. Static and proxied traffic match, and buffer size makes no difference.
- **What the larger buffer changes is work per request:** proxied syscalls fall about 3× under every setting.

### NFR-PERF-05 check at the 32 KiB product build (`999724c`)

Nightly dispatch on `main` with `bench_netem=true`, run 42398454271 (job `bench netem`, success), same method as above: 3 runs × 10 s per setting, medians.

| Setting | Scenario | req/s | syscalls/req | CPU µs/req |
|---|---|---|---|---|
| delay 50 ms each way | static-large | 73 | 41.6 | 229 |
| | proxy-small | 39 | 12.0 | 75* |
| | proxy-large | 36 | 78.3 | 549 |
| loss 1 % | static-large | 940 | 41.1 | 279 |
| | proxy-small | 1 272 | 11.4 | 56 |
| | proxy-large | 533 | 77.2 | 685 |
| 2 s pause (loss 100 %) | static-large | 2 949 | 37.5 | 281 |
| | proxy-small | 17 096 | 10.3 | 39 |
| | proxy-large | 1 078 | 75.0 | 639 |

\* About 400 requests per window: CPU quantized to 10 ms ticks (runs 50, 75, 100).

Recovery after the 2 s pause: static-large 1000 ms in all 3 runs (longest gap 3.11 s), proxy-small 1250 ms in all 3 runs (3.33 s), proxy-large 1000, 1050, 1050 ms (3.15 s).

**The target holds:** no run had errors under any setting, and every recovery is within 1.5 s (at most 1.25 s). Throughput matches the 16 and 64 KiB runs (TCP-bound), and proxy-large syscalls sit between them (78 under delay, against 144.5 and 44.6), as the local 32 KiB numbers predicted.

## Recommendations

1. **A (kept):** eager I/O and deferred registrations. This drops per-request work in every proxy scenario and in static-large, with no contract change.
2. **B: 32 KiB, as the owner decided 2026-11-07 (implemented in `ad86dc3`; final numbers above).** The recommendation was:
   - Gain: proxy-large −40 % CPU and −46 % syscalls; proxy-medium −15 % CPU and −22 % syscalls; proxy-small unchanged.
   - Cost: +16 KiB per *active* proxied exchange. The worst case goes from 49.2 to 65.2 KiB, and the connections that fit 1 GiB from 21 302 to 16 076. Idle connections are unaffected.
   - Against 64 KiB: 64 KiB gains a little more on 1 MiB bodies (−51 %), but costs proxy-small about 5–10 % CPU, because the zeroed allocation happens on every request.
   - Changing it meant editing the NFR-RES-01 builder call ("`max(16 KiB, max_head)`"), which needed the owner's OK; the owner gave it on 2026-11-07, and `ad86dc3` made the change.
   - A possible later follow-up: reuse the buffer, without zeroing it again, across exchanges (pooled with the backend connection). That would make 64 KiB free for small responses.
3. **NFR-PERF-05: fill-drain-refill stays (owner decision 2026-11-07).** Under delay, loss and a pause, throughput and recovery are set by TCP. Even a 4× larger user buffer changed neither, and double buffering adds at most one more buffer in flight. Double buffering was not built: it is not cheap, because it rewrites the core relay and adds a sim model.
   - NFR-PERF-05 target (owner decision 2026-11-07, in the spec): no errors under netem 50 ms, 1 % loss and a 2 s pause; traffic back within 1.5 s after a 2 s pause (the kernel's RTO backoff).
