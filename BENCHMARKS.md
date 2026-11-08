# Benchmarks

CPU time and system calls per request of the `durchlauferhitzer` binary, per scenario, measured nightly on GitHub's runners (NFR-PERF-08; data: [`results.csv`](results.csv), tool: `bench/` on `main`).

Method: CPU time is the server process's own utime+stime (`/proc/<pid>/stat`) over the measured window after a warm-up, divided by the complete, correct responses; system calls come from a separate `strace -c -f` pass, divided by that pass's responses.
Each point is the median of a date's runs on one runner (each runner label is its own series); runs with errors are left out (they stay in the CSV). Runner noise is expected: compare trends, not single points.

## Latest

| Scenario | Runner | Date | Commit | CPU µs/req | Syscalls/req | Req/s | RSS KiB |
|---|---|---|---|---:|---:|---:|---:|
| `static-small` | `gha-ubuntu-24.04-x86_64` | 2026-11-08 | `8f68af3` | 70.24 | 17.80 | 21458 | 2984 |
| `static-large` | `gha-ubuntu-24.04-x86_64` | 2026-11-08 | `8f68af3` | 401.07 | 37.07 | 3036 | 2984 |
| `static-fresh` | `gha-ubuntu-24.04-x86_64` | 2026-11-08 | `8f68af3` | 110.20 | 26.35 | 11520 | 3052 |
| `proxy-small` | `gha-ubuntu-24.04-x86_64` | 2026-11-08 | `8f68af3` | 58.29 | 10.25 | 17137 | 3148 |
| `proxy-fresh` | `gha-ubuntu-24.04-x86_64` | 2026-11-08 | `8f68af3` | 114.80 | 18.88 | 8643 | 3148 |
| `proxy-large` | `gha-ubuntu-24.04-x86_64` | 2026-11-08 | `8f68af3` | 836.13 | 74.98 | 1193 | 20284 |
| `conditional-hit` | `gha-ubuntu-24.04-x86_64` | 2026-11-08 | `8f68af3` | 49.38 | 11.51 | 28388 | 3148 |
| `static-small-1k-idle` | `gha-ubuntu-24.04-x86_64` | 2026-11-08 | `8f68af3` | 71.65 | 17.79 | 20788 | 20276 |

## CPU time per request (µs)

### `static-small` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "static-small on gha-ubuntu-24.04-x86_64: CPU µs per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08"]
  y-axis "CPU µs per request" 0 --> 85
  line [45.92, 39.16, 70.24]
```

### `static-large` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "static-large on gha-ubuntu-24.04-x86_64: CPU µs per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08"]
  y-axis "CPU µs per request" 0 --> 482
  line [284.94, 213.45, 401.07]
```

### `static-fresh` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "static-fresh on gha-ubuntu-24.04-x86_64: CPU µs per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08"]
  y-axis "CPU µs per request" 0 --> 133
  line [76.72, 57.05, 110.20]
```

### `proxy-small` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "proxy-small on gha-ubuntu-24.04-x86_64: CPU µs per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08"]
  y-axis "CPU µs per request" 0 --> 102
  line [84.61, 50.09, 58.29]
```

### `proxy-fresh` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "proxy-fresh on gha-ubuntu-24.04-x86_64: CPU µs per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08"]
  y-axis "CPU µs per request" 0 --> 142
  line [117.55, 74.78, 114.80]
```

### `proxy-large` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "proxy-large on gha-ubuntu-24.04-x86_64: CPU µs per request"
  x-axis ["2026-11-07", "2026-11-08"]
  y-axis "CPU µs per request" 0 --> 1004
  line [464.25, 836.13]
```

### `conditional-hit` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "conditional-hit on gha-ubuntu-24.04-x86_64: CPU µs per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08"]
  y-axis "CPU µs per request" 0 --> 60
  line [45.19, 27.01, 49.38]
```

### `static-small-1k-idle` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "static-small-1k-idle on gha-ubuntu-24.04-x86_64: CPU µs per request"
  x-axis ["2026-11-07", "2026-11-08"]
  y-axis "CPU µs per request" 0 --> 86
  line [40.14, 71.65]
```

## System calls per request

### `static-small` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "static-small on gha-ubuntu-24.04-x86_64: system calls per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08"]
  y-axis "system calls per request" 0 --> 23
  line [19.04, 18.44, 17.80]
```

### `static-large` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "static-large on gha-ubuntu-24.04-x86_64: system calls per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08"]
  y-axis "system calls per request" 0 --> 54
  line [44.52, 40.96, 37.07]
```

### `static-fresh` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "static-fresh on gha-ubuntu-24.04-x86_64: system calls per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08"]
  y-axis "system calls per request" 0 --> 33
  line [27.28, 26.75, 26.35]
```

### `proxy-small` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "proxy-small on gha-ubuntu-24.04-x86_64: system calls per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08"]
  y-axis "system calls per request" 0 --> 23
  line [18.84, 14.68, 10.25]
```

### `proxy-fresh` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "proxy-fresh on gha-ubuntu-24.04-x86_64: system calls per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08"]
  y-axis "system calls per request" 0 --> 33
  line [27.25, 23.00, 18.88]
```

### `proxy-large` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "proxy-large on gha-ubuntu-24.04-x86_64: system calls per request"
  x-axis ["2026-11-07", "2026-11-08"]
  y-axis "system calls per request" 0 --> 90
  line [74.97, 74.98]
```

### `conditional-hit` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "conditional-hit on gha-ubuntu-24.04-x86_64: system calls per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08"]
  y-axis "system calls per request" 0 --> 23
  line [19.04, 12.07, 11.51]
```

### `static-small-1k-idle` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "static-small-1k-idle on gha-ubuntu-24.04-x86_64: system calls per request"
  x-axis ["2026-11-07", "2026-11-08"]
  y-axis "system calls per request" 0 --> 23
  line [18.40, 17.79]
```
