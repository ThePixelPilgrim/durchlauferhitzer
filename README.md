# durchlauferhitzer
Just another HTTP proxy with very little features. HTTPS is planned for a later stage.

See [`examples/durchlauferhitzer.conf`](examples/durchlauferhitzer.conf) for a documented configuration.

## Repository layout

- [`core/`](core) — the product: a sans-IO HTTP core with zero dependencies (config, parsing, routing, static files, proxying).
- [`sim/`](sim) — test-only simulation driver: simulated network, clock, file system and backends, with contract checks and randomized runs.
- [`examples/`](examples) — documented example configuration.
- [`docs/superpowers/`](docs/superpowers) — design spec, implementation plans per stage, and review records.
- [`scripts/`](scripts) — CI helpers (product dependency check).
