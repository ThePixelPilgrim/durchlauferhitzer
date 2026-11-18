# Benchmarks

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
