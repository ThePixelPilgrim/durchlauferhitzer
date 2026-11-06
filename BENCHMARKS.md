# Benchmarks

CPU time and system calls per request of the `durchlauferhitzer` binary, per scenario, measured nightly on GitHub's runners (NFR-PERF-08; data: [`results.csv`](results.csv), tool: `bench/` on `main`).

Method: CPU time is the server process's own utime+stime (`/proc/<pid>/stat`) over the measured window after a warm-up, divided by the complete, correct responses; system calls come from a separate `strace -c -f` pass, divided by that pass's responses.
Each point is the median of a date's runs; runs with errors are left out (they stay in the CSV). Runner noise is expected: compare trends, not single points.

## Latest

| Scenario | Date | Commit | Runner | CPU µs/req | Syscalls/req | Req/s | RSS KiB |
|---|---|---|---|---:|---:|---:|---:|
| `static-small` | 2026-11-06 | `71a95f3` | `gha-ubuntu-24.04-x86_64` | 45.92 | 19.04 | 32211 | 2992 |
| `static-large` | 2026-11-06 | `71a95f3` | `gha-ubuntu-24.04-x86_64` | 284.94 | 44.52 | 4186 | 2992 |
| `static-fresh` | 2026-11-06 | `71a95f3` | `gha-ubuntu-24.04-x86_64` | 76.72 | 27.28 | 15078 | 3024 |
| `proxy-small` | 2026-11-06 | `71a95f3` | `gha-ubuntu-24.04-x86_64` | 84.61 | 18.84 | 11826 | 3130 |
| `proxy-fresh` | 2026-11-06 | `71a95f3` | `gha-ubuntu-24.04-x86_64` | 117.55 | 27.25 | 8479 | 3120 |
| `conditional-hit` | 2026-11-06 | `71a95f3` | `gha-ubuntu-24.04-x86_64` | 45.19 | 19.04 | 32679 | 3120 |

## CPU time per request (µs)

### `static-small`

```mermaid
xychart-beta
  title "static-small: CPU µs per request"
  x-axis ["2026-11-06"]
  y-axis "CPU µs per request" 0 --> 56
  line [45.92]
```

### `static-large`

```mermaid
xychart-beta
  title "static-large: CPU µs per request"
  x-axis ["2026-11-06"]
  y-axis "CPU µs per request" 0 --> 342
  line [284.94]
```

### `static-fresh`

```mermaid
xychart-beta
  title "static-fresh: CPU µs per request"
  x-axis ["2026-11-06"]
  y-axis "CPU µs per request" 0 --> 93
  line [76.72]
```

### `proxy-small`

```mermaid
xychart-beta
  title "proxy-small: CPU µs per request"
  x-axis ["2026-11-06"]
  y-axis "CPU µs per request" 0 --> 102
  line [84.61]
```

### `proxy-fresh`

```mermaid
xychart-beta
  title "proxy-fresh: CPU µs per request"
  x-axis ["2026-11-06"]
  y-axis "CPU µs per request" 0 --> 142
  line [117.55]
```

### `conditional-hit`

```mermaid
xychart-beta
  title "conditional-hit: CPU µs per request"
  x-axis ["2026-11-06"]
  y-axis "CPU µs per request" 0 --> 55
  line [45.19]
```

## System calls per request

### `static-small`

```mermaid
xychart-beta
  title "static-small: system calls per request"
  x-axis ["2026-11-06"]
  y-axis "system calls per request" 0 --> 23
  line [19.04]
```

### `static-large`

```mermaid
xychart-beta
  title "static-large: system calls per request"
  x-axis ["2026-11-06"]
  y-axis "system calls per request" 0 --> 54
  line [44.52]
```

### `static-fresh`

```mermaid
xychart-beta
  title "static-fresh: system calls per request"
  x-axis ["2026-11-06"]
  y-axis "system calls per request" 0 --> 33
  line [27.28]
```

### `proxy-small`

```mermaid
xychart-beta
  title "proxy-small: system calls per request"
  x-axis ["2026-11-06"]
  y-axis "system calls per request" 0 --> 23
  line [18.84]
```

### `proxy-fresh`

```mermaid
xychart-beta
  title "proxy-fresh: system calls per request"
  x-axis ["2026-11-06"]
  y-axis "system calls per request" 0 --> 33
  line [27.25]
```

### `conditional-hit`

```mermaid
xychart-beta
  title "conditional-hit: system calls per request"
  x-axis ["2026-11-06"]
  y-axis "system calls per request" 0 --> 23
  line [19.04]
```
