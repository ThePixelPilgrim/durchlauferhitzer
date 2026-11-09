# Benchmarks

CPU time and system calls per request of the `durchlauferhitzer` binary, per scenario, measured nightly on GitHub's runners (NFR-PERF-08; data: [`results.csv`](results.csv), tool: `bench/` on `main`).

Method: CPU time is the server process's own utime+stime (`/proc/<pid>/stat`) over the measured window after a warm-up, divided by the complete, correct responses; system calls come from a separate `strace -c -f` pass, divided by that pass's responses.
Each point is the median of a date's runs on one runner (each runner label is its own series); runs with errors are left out (they stay in the CSV). Runner noise is expected: compare trends, not single points.

## Latest

| Scenario | Runner | Date | Commit | CPU µs/req | Syscalls/req | Req/s | RSS KiB |
|---|---|---|---|---:|---:|---:|---:|
| `static-small` | `gha-ubuntu-24.04-x86_64` | 2026-11-09 | `92b3c36` | 70.98 | 17.82 | 21099 | 3000 |
| `static-large` | `gha-ubuntu-24.04-x86_64` | 2026-11-09 | `92b3c36` | 412.38 | 37.09 | 2942 | 3000 |
| `static-fresh` | `gha-ubuntu-24.04-x86_64` | 2026-11-09 | `92b3c36` | 111.47 | 26.36 | 11383 | 3044 |
| `proxy-small` | `gha-ubuntu-24.04-x86_64` | 2026-11-09 | `92b3c36` | 58.68 | 10.25 | 17039 | 3136 |
| `proxy-fresh` | `gha-ubuntu-24.04-x86_64` | 2026-11-09 | `92b3c36` | 114.37 | 19.00 | 8690 | 3184 |
| `proxy-large` | `gha-ubuntu-24.04-x86_64` | 2026-11-09 | `92b3c36` | 944.91 | 74.98 | 1058 | 20392 |
| `proxy-file` | `gha-ubuntu-24.04-x86_64` | 2026-11-09 | `92b3c36` | 101.47 | 22.18 | 12889 | 3324 |
| `conditional-hit` | `gha-ubuntu-24.04-x86_64` | 2026-11-09 | `92b3c36` | 50.10 | 11.51 | 27821 | 3136 |
| `static-small-1k-idle` | `gha-ubuntu-24.04-x86_64` | 2026-11-09 | `92b3c36` | 72.17 | 17.80 | 20584 | 20388 |

## CPU time per request (µs)

### `static-small` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "static-small on gha-ubuntu-24.04-x86_64: CPU µs per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08", "2026-11-09"]
  y-axis "CPU µs per request" 0 --> 86
  line [45.92, 39.16, 70.24, 70.98]
```

### `static-large` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "static-large on gha-ubuntu-24.04-x86_64: CPU µs per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08", "2026-11-09"]
  y-axis "CPU µs per request" 0 --> 495
  line [284.94, 213.45, 401.07, 412.38]
```

### `static-fresh` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "static-fresh on gha-ubuntu-24.04-x86_64: CPU µs per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08", "2026-11-09"]
  y-axis "CPU µs per request" 0 --> 134
  line [76.72, 57.05, 110.20, 111.47]
```

### `proxy-small` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "proxy-small on gha-ubuntu-24.04-x86_64: CPU µs per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08", "2026-11-09"]
  y-axis "CPU µs per request" 0 --> 102
  line [84.61, 50.09, 58.29, 58.68]
```

### `proxy-fresh` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "proxy-fresh on gha-ubuntu-24.04-x86_64: CPU µs per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08", "2026-11-09"]
  y-axis "CPU µs per request" 0 --> 142
  line [117.55, 74.78, 114.80, 114.37]
```

### `proxy-large` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "proxy-large on gha-ubuntu-24.04-x86_64: CPU µs per request"
  x-axis ["2026-11-07", "2026-11-08", "2026-11-09"]
  y-axis "CPU µs per request" 0 --> 1134
  line [464.25, 836.13, 944.91]
```

### `proxy-file` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "proxy-file on gha-ubuntu-24.04-x86_64: CPU µs per request"
  x-axis ["2026-11-09"]
  y-axis "CPU µs per request" 0 --> 122
  line [101.47]
```

### `conditional-hit` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "conditional-hit on gha-ubuntu-24.04-x86_64: CPU µs per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08", "2026-11-09"]
  y-axis "CPU µs per request" 0 --> 61
  line [45.19, 27.01, 49.38, 50.10]
```

### `static-small-1k-idle` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "static-small-1k-idle on gha-ubuntu-24.04-x86_64: CPU µs per request"
  x-axis ["2026-11-07", "2026-11-08", "2026-11-09"]
  y-axis "CPU µs per request" 0 --> 87
  line [40.14, 71.65, 72.17]
```

## System calls per request

### `static-small` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "static-small on gha-ubuntu-24.04-x86_64: system calls per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08", "2026-11-09"]
  y-axis "system calls per request" 0 --> 23
  line [19.04, 18.44, 17.80, 17.82]
```

### `static-large` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "static-large on gha-ubuntu-24.04-x86_64: system calls per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08", "2026-11-09"]
  y-axis "system calls per request" 0 --> 54
  line [44.52, 40.96, 37.07, 37.09]
```

### `static-fresh` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "static-fresh on gha-ubuntu-24.04-x86_64: system calls per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08", "2026-11-09"]
  y-axis "system calls per request" 0 --> 33
  line [27.28, 26.75, 26.35, 26.36]
```

### `proxy-small` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "proxy-small on gha-ubuntu-24.04-x86_64: system calls per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08", "2026-11-09"]
  y-axis "system calls per request" 0 --> 23
  line [18.84, 14.68, 10.25, 10.25]
```

### `proxy-fresh` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "proxy-fresh on gha-ubuntu-24.04-x86_64: system calls per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08", "2026-11-09"]
  y-axis "system calls per request" 0 --> 33
  line [27.25, 23.00, 18.88, 19.00]
```

### `proxy-large` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "proxy-large on gha-ubuntu-24.04-x86_64: system calls per request"
  x-axis ["2026-11-07", "2026-11-08", "2026-11-09"]
  y-axis "system calls per request" 0 --> 90
  line [74.97, 74.98, 74.98]
```

### `proxy-file` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "proxy-file on gha-ubuntu-24.04-x86_64: system calls per request"
  x-axis ["2026-11-09"]
  y-axis "system calls per request" 0 --> 27
  line [22.18]
```

### `conditional-hit` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "conditional-hit on gha-ubuntu-24.04-x86_64: system calls per request"
  x-axis ["2026-11-06", "2026-11-07", "2026-11-08", "2026-11-09"]
  y-axis "system calls per request" 0 --> 23
  line [19.04, 12.07, 11.51, 11.51]
```

### `static-small-1k-idle` on `gha-ubuntu-24.04-x86_64`

```mermaid
xychart-beta
  title "static-small-1k-idle on gha-ubuntu-24.04-x86_64: system calls per request"
  x-axis ["2026-11-07", "2026-11-08", "2026-11-09"]
  y-axis "system calls per request" 0 --> 23
  line [18.40, 17.79, 17.80]
```
