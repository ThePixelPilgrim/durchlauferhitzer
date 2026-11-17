#!/usr/bin/env python3
"""Benchmark results as a chart page (NFR-PERF-08).

Test tooling, Python 3 standard library only. Renders the benchmark CSV
(`bench/`, one row per scenario and run) as `BENCHMARKS.md`: per metric
(CPU time and system calls per request) one Mermaid `xychart-beta` per
scenario, x = date, y = the median of that date's runs, and a table of
the latest values.

    bench-chart.py [--lines product-lines.csv] results.csv BENCHMARKS.md
    bench-chart.py --append bench.csv --commit <sha> [--today YYYY-MM-DD] \
        [--append-lines new-lines.csv --lines product-lines.csv] \
        results.csv BENCHMARKS.md

`--append` first adds the rows of a new CSV (the nightly `bench` job's
artifact) to `results.csv`, created with its header if missing: only
rows of the run's own commit (`--commit`, matched exactly) dated at most
a day from the run (`--today`, default today in UTC) are taken; other
valid rows are refused and counted. Those
rows come from a job that ran code under test, so they are untrusted:
every field is checked (a date, a lowercase hex commit, a plain runner
label, a scenario from the fixed set, numbers that parse, are finite and
not negative) and a row that fails is dropped, before it reaches the CSV
or the page; the page re-checks every row it reads. Rows with errors are
kept in the CSV but never charted. A missing new CSV appends no rows,
so the lines row below is published all the same. `--append` also
writes a README.md beside results.csv whenever the one there is missing
or differs from what this script writes.

The product's source lines (NFR-MAINT-12, `scripts/product-lines.py`)
are a sibling CSV, `--lines` (`date,commit,lines`), charted on the same
page when present. `--append-lines` adds one row of the run's new CSV to
it, under the same checks as `--append`. A missing or broken lines CSV
never fails the publish: the run of an older commit has none, and the
benchmark rows are published all the same.
"""

import argparse
import csv
import io
import math
import os
import re
import statistics
import sys
from datetime import date as _date
from datetime import datetime, timezone

HEADER = [
    "date",
    "commit",
    "runner",
    "scenario",
    "requests",
    "errors",
    "seconds",
    "cpu_us_per_req",
    "syscalls_per_req",
    "rss_kib",
]
SCENARIOS = (
    "static-small",
    "static-large",
    "static-fresh",
    "proxy-small",
    "proxy-fresh",
    "proxy-medium",
    "proxy-large",
    "proxy-file",
    "conditional-hit",
    "static-small-1k-idle",
)
METRICS = (
    ("cpu_us_per_req", "CPU time per request (µs)", "CPU µs per request"),
    ("syscalls_per_req", "System calls per request", "system calls per request"),
)
# Dates shown per chart (the newest).
MAX_DATES = 90
# Rows taken from one artifact.
MAX_NEW_ROWS = 10000
# Longest numeric field.
MAX_NUMBER = 32
LINES_HEADER = ["date", "commit", "lines"]
# Rows taken from one artifact's lines CSV (one count per run).
MAX_NEW_LINES = 1
README = """# Benchmarks

Written by the nightly workflow's `bench publish` job.

| File | What it holds |
|---|---|
| [BENCHMARKS.md](BENCHMARKS.md) | the charts (rendered from the two CSV files) |
| [results.csv](results.csv) | the benchmark rows (NFR-PERF-08) |
| [product-lines.csv](product-lines.csv) | the product's source lines (NFR-MAINT-12) |

The tools are on `main`:

- `bench/`, the benchmark tool;
- `scripts/bench-chart.py`, which renders the page;
- `scripts/product-lines.py`, which counts the source lines.
"""

# Matched whole (fullmatch: `$` would accept a final newline) and in
# ASCII ([0-9], not `\d`, which takes any Unicode digit).
_DATE = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}", re.ASCII)
_COMMIT = re.compile(r"[0-9a-f]{7,40}", re.ASCII)
_RUNNER = re.compile(r"[A-Za-z0-9._-]{1,64}", re.ASCII)
_INT = re.compile(r"[0-9]{1,15}", re.ASCII)
_FLOAT = re.compile(r"[0-9]{1,15}(\.[0-9]{1,9})?", re.ASCII)


def _number(text, optional):
    """A finite, non-negative decimal, or None for an empty optional one;
    raises ValueError otherwise (no exponents, nan or inf)."""
    if text == "" and optional:
        return None
    if len(text) > MAX_NUMBER or not _FLOAT.fullmatch(text):
        raise ValueError(text)
    value = float(text)
    if not math.isfinite(value):
        raise ValueError(text)
    return value


def validate(fields):
    """The typed row of one CSV record (a list of strings), or None."""
    if len(fields) != len(HEADER):
        return None
    if any("\r" in f or "\n" in f for f in fields):
        return None
    raw = dict(zip(HEADER, fields))
    try:
        if not _DATE.fullmatch(raw["date"]):
            return None
        _date.fromisoformat(raw["date"])
        if not _COMMIT.fullmatch(raw["commit"]) or not _RUNNER.fullmatch(raw["runner"]):
            return None
        if raw["scenario"] not in SCENARIOS:
            return None
        if not _INT.fullmatch(raw["requests"]) or not _INT.fullmatch(raw["errors"]):
            return None
        rss = raw["rss_kib"]
        if rss and not _INT.fullmatch(rss):
            return None
        return {
            "date": raw["date"],
            "commit": raw["commit"],
            "runner": raw["runner"],
            "scenario": raw["scenario"],
            "requests": int(raw["requests"]),
            "errors": int(raw["errors"]),
            "seconds": _number(raw["seconds"], False),
            "cpu_us_per_req": _number(raw["cpu_us_per_req"], True),
            "syscalls_per_req": _number(raw["syscalls_per_req"], True),
            "rss_kib": int(rss) if rss else None,
            "fields": list(fields),
        }
    except ValueError:
        return None


def parse(text):
    """(valid rows, number of dropped records) of a CSV text; ValueError
    if its header is not exactly HEADER."""
    reader = csv.reader(io.StringIO(text))
    header = next(reader, None)
    if header != HEADER:
        raise ValueError("not a benchmark CSV (wrong header)")
    rows, dropped = [], 0
    for fields in reader:
        if not fields:
            continue
        r = validate(fields)
        if r is None:
            dropped += 1
        else:
            rows.append(r)
    return rows, dropped


def validate_lines(fields):
    """The typed row of one lines CSV record (a list of strings), or None."""
    if len(fields) != len(LINES_HEADER):
        return None
    if any("\r" in f or "\n" in f for f in fields):
        return None
    day, commit, lines = fields
    if not _DATE.fullmatch(day) or not _COMMIT.fullmatch(commit) or not _INT.fullmatch(lines):
        return None
    try:
        _date.fromisoformat(day)
    except ValueError:
        return None
    return {"date": day, "commit": commit, "lines": int(lines), "fields": list(fields)}


def parse_lines(text):
    """(valid rows, number of dropped records) of a lines CSV text;
    ValueError if its header is not exactly LINES_HEADER."""
    reader = csv.reader(io.StringIO(text))
    if next(reader, None) != LINES_HEADER:
        raise ValueError("not a product lines CSV (wrong header)")
    rows, dropped = [], 0
    for fields in reader:
        if not fields:
            continue
        r = validate_lines(fields)
        if r is None:
            dropped += 1
        else:
            rows.append(r)
    return rows, dropped


def _charted(r, metric):
    return r["errors"] == 0 and r["requests"] > 0 and r[metric] is not None


def medians(rows, metric):
    """{(scenario, runner): [(date, median)]} over the rows without
    errors, dates ascending, the newest MAX_DATES. Each runner is its own
    series: machines are never blended."""
    by = {}
    for r in rows:
        if _charted(r, metric):
            key = (r["scenario"], r["runner"])
            by.setdefault(key, {}).setdefault(r["date"], []).append(r[metric])
    return {
        s: [(d, statistics.median(v)) for d, v in sorted(dates.items())][-MAX_DATES:]
        for s, dates in by.items()
    }


def _fmt(v):
    return "–" if v is None else f"{v:.2f}"


def _chart(title, axis, points, fmt=_fmt):
    top = max(v for _, v in points)
    top = max(1, math.ceil(top * 1.2))
    dates = ", ".join(f'"{d}"' for d, _ in points)
    values = ", ".join(fmt(v) for _, v in points)
    return "\n".join(
        [
            "```mermaid",
            "xychart-beta",
            f'  title "{title}"',
            f"  x-axis [{dates}]",
            f'  y-axis "{axis}" 0 --> {top}',
            f"  line [{values}]",
            "```",
        ]
    )


def _median_of(rows, key):
    vals = [r[key] for r in rows if r[key] is not None]
    return statistics.median(vals) if vals else None


def _runners(rows):
    """The runner labels, in order of first appearance."""
    return list(dict.fromkeys(r["runner"] for r in rows))


def _latest(rows):
    """Per scenario and runner: the newest date with charted runs, its
    medians, and the commit of its last row."""
    out = []
    for s, runner in ((s, k) for s in SCENARIOS for k in _runners(rows)):
        mine = [
            r for r in rows
            if r["scenario"] == s and r["runner"] == runner and r["errors"] == 0 and r["requests"] > 0
        ]
        if not mine:
            continue
        day = max(r["date"] for r in mine)
        today = [r for r in mine if r["date"] == day]
        last = today[-1]
        rates = [r["requests"] / r["seconds"] for r in today if r["seconds"]]
        rate = statistics.median(rates) if rates else None
        rss = _median_of(today, "rss_kib")
        out.append(
            f"| `{s}` | `{runner}` | {day} | `{last['commit']}` | "
            f"{_fmt(_median_of(today, 'cpu_us_per_req'))} | "
            f"{_fmt(_median_of(today, 'syscalls_per_req'))} | "
            f"{'–' if rate is None else f'{rate:.0f}'} | "
            f"{'–' if rss is None else f'{rss:.0f}'} |"
        )
    return out


def _lines_section(lines):
    """The page's product lines section: each date's last record."""
    if not lines:
        return []
    by_date = {r["date"]: r for r in sorted(lines, key=lambda r: r["date"])}
    points = [(d, r["lines"]) for d, r in by_date.items()][-MAX_DATES:]
    last = by_date[points[-1][0]]
    return [
        "## Product source lines",
        "",
        "The product's size, recorded by each nightly (NFR-MAINT-12).",
        "It counts the non-blank, non-comment lines of `core/src` and `driver/src`.",
        "Test code is left out.",
        "",
        "The counting rule is `scripts/product-lines.py` on `main`.",
        "The data is [`product-lines.csv`](product-lines.csv).",
        "",
        "Each point is the last record of its date.",
        "",
        "| Date | Commit | Lines |",
        "|---|---|---:|",
        f"| {last['date']} | `{last['commit']}` | {last['lines']} |",
        "",
        _chart("Product source lines", "source lines", points, lambda v: str(v)),
        "",
    ]


def render(rows, lines=()):
    """The Markdown page for the (valid) benchmark rows and product lines
    rows."""
    page = _render_bench(rows)
    section = _lines_section(lines)
    return page + "\n" + "\n".join(section) if section else page


def _render_bench(rows):
    lines = [
        "# Benchmarks",
        "",
        "CPU time and system calls per request of the `durchlauferhitzer` binary, per "
        "scenario, measured nightly on GitHub's runners (NFR-PERF-08; data: "
        "[`results.csv`](results.csv), tool: `bench/` on `main`).",
        "",
        "Method: CPU time is the server process's own utime+stime (`/proc/<pid>/stat`) over "
        "the measured window after a warm-up, divided by the complete, correct responses; "
        "system calls come from a separate `strace -c -f` pass, divided by that pass's "
        "responses.",
        "Each point is the median of a date's runs on one runner (each runner label is its "
        "own series); runs with errors are left out (they stay in the CSV). Runner noise is "
        "expected: compare trends, not single points.",
        "",
    ]
    if not rows:
        lines += ["No results yet.", ""]
        return "\n".join(lines)
    lines += [
        "## Latest",
        "",
        "| Scenario | Runner | Date | Commit | CPU µs/req | Syscalls/req | Req/s | RSS KiB |",
        "|---|---|---|---|---:|---:|---:|---:|",
    ]
    lines += _latest(rows)
    lines.append("")
    for metric, heading, axis in METRICS:
        lines += [f"## {heading}", ""]
        series = medians(rows, metric)
        for key in ((s, k) for s in SCENARIOS for k in _runners(rows)):
            if key not in series:
                continue
            s, runner = key
            lines += [
                f"### `{s}` on `{runner}`",
                "",
                _chart(f"{s} on {runner}: {axis}", axis, series[key]),
                "",
            ]
    return "\n".join(lines)


def _read_text(path):
    with open(path, encoding="utf-8", errors="replace", newline="") as fh:
        return fh.read()


def _utc_today():
    return datetime.now(timezone.utc).date()


def _the_runs(r, commit, today):
    """Whether a valid row is the run's own: its commit exactly, and a
    date at most one day from the run's."""
    return r["commit"] == commit and abs((_date.fromisoformat(r["date"]) - today).days) <= 1


def append(new_path, results_path, commit, today):
    """Appends the valid rows of `new_path` that are the run's own (its
    `commit`, dated within a day of `today`) to `results_path` (created
    with the header if missing); returns (appended, dropped, refused):
    refused rows are valid but another commit's or date's. A missing
    `new_path` (a `bench` that failed before writing it) appends no rows."""
    rows, dropped = parse(_read_text(new_path)) if os.path.isfile(new_path) else ([], 0)
    mine = [r for r in rows if _the_runs(r, commit, today)]
    refused = len(rows) - len(mine)
    rows = mine
    if len(rows) > MAX_NEW_ROWS:
        dropped += len(rows) - MAX_NEW_ROWS
        rows = rows[:MAX_NEW_ROWS]
    fresh = not os.path.exists(results_path)
    with open(results_path, "a", encoding="utf-8", newline="") as fh:
        out = csv.writer(fh, lineterminator="\n")
        if fresh:
            out.writerow(HEADER)
        for r in rows:
            out.writerow(r["fields"])
    return len(rows), dropped, refused


def append_lines(new_path, lines_path, commit, today):
    """Appends the first valid row of `new_path` that is the run's own to
    `lines_path` (created with the header if missing); returns the line
    to print. Never raises for a missing or broken `new_path`."""
    if not os.path.isfile(new_path):
        return f"product lines: no {new_path}; nothing appended"
    try:
        rows, dropped = parse_lines(_read_text(new_path))
    except ValueError as e:
        return f"product lines: {new_path}: {e}; nothing appended"
    mine = [r for r in rows if _the_runs(r, commit, today)]
    refused = len(rows) - len(mine)
    dropped += max(0, len(mine) - MAX_NEW_LINES)
    mine = mine[:MAX_NEW_LINES]
    fresh = not os.path.exists(lines_path)
    with open(lines_path, "a", encoding="utf-8", newline="") as fh:
        out = csv.writer(fh, lineterminator="\n")
        if fresh:
            out.writerow(LINES_HEADER)
        for r in mine:
            out.writerow(r["fields"])
    return f"product lines: appended {len(mine)} rows, dropped {dropped}, refused {refused}"


def _read_lines(path):
    """The valid rows of the published lines CSV; none (and a note on
    stderr) if it is missing or not a lines CSV."""
    if not os.path.isfile(path):
        return []
    try:
        rows, dropped = parse_lines(_read_text(path))
    except ValueError as e:
        print(f"{path}: {e}; product lines left out", file=sys.stderr)
        return []
    if dropped:
        print(f"{path}: {dropped} invalid rows left out", file=sys.stderr)
    return rows


def _write_readme(results_path):
    """Writes README.md beside `results_path` when it is missing or its
    content differs from `README`."""
    path = os.path.join(os.path.dirname(results_path), "README.md")
    if os.path.isfile(path) and _read_text(path) == README:
        return
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(README)


def _commit_arg(text):
    if not _COMMIT.fullmatch(text):
        raise argparse.ArgumentTypeError(f"{text!r}: 7 to 40 lowercase hex digits")
    return text


def _date_arg(text):
    if not _DATE.fullmatch(text):
        raise argparse.ArgumentTypeError(f"{text!r}: YYYY-MM-DD")
    try:
        return _date.fromisoformat(text)
    except ValueError:
        raise argparse.ArgumentTypeError(f"{text!r}: YYYY-MM-DD")


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--append", metavar="NEW_CSV", help="first append this CSV's valid rows")
    p.add_argument("--commit", type=_commit_arg,
                   help="with --append (required): the run's commit; other rows are refused")
    p.add_argument("--today", type=_date_arg,
                   help="with --append: the run's date (default: today, UTC); rows dated "
                        "more than a day from it are refused")
    p.add_argument("--append-lines", metavar="NEW_LINES_CSV",
                   help="first append this product lines CSV's row of the run to --lines "
                        "(needs --commit); a missing file appends nothing")
    p.add_argument("--lines", metavar="LINES_CSV",
                   help="the product lines CSV charted on the page (if present)")
    p.add_argument("csv")
    p.add_argument("out")
    args = p.parse_args(argv)
    if args.append_lines and args.lines is None:
        p.error("--append-lines needs --lines")
    if args.append_lines and args.commit is None:
        p.error("--append-lines needs --commit")
    if args.append and args.commit is None:
        p.error("--append needs --commit")
    today = args.today or _utc_today()
    if args.append:
        try:
            added, dropped, refused = append(args.append, args.csv, args.commit, today)
        except ValueError as e:
            raise SystemExit(f"bench-chart: {args.append}: {e}")
        if not os.path.isfile(args.append):
            print(f"no bench rows: no {args.append}; nothing appended")
        print(f"appended {added} rows, dropped {dropped}, refused {refused} "
              f"(not commit {args.commit} or not within a day of {today.isoformat()})")
        _write_readme(args.csv)
    if args.append_lines:
        print(append_lines(args.append_lines, args.lines, args.commit, today))
    rows, dropped = parse(_read_text(args.csv))
    if dropped:
        print(f"{args.csv}: {dropped} invalid rows left out", file=sys.stderr)
    lines = _read_lines(args.lines) if args.lines else []
    with open(args.out, "w", encoding="utf-8") as fh:
        fh.write(render(rows, lines))
    return 0


if __name__ == "__main__":
    sys.exit(main())
