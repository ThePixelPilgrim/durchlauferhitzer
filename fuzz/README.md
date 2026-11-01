# durchlauferhitzer-fuzz

Fuzz targets for the parsers and the config language (NFR-SEC-13), run by
[`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) (libFuzzer, nightly
Rust). Test-only tooling (NFR-MAINT-02): this crate is excluded from the
workspace, has its own `Cargo.lock`, and nothing of it reaches the binary;
the gate never builds it. The nightly workflow (`.github/workflows/nightly.yml`)
runs each target for a fixed time.

```
fuzz_targets/
  request_head.rs   parse_head / HeadParser: no panic; a Complete head is at
                    most the input's length; a prefix gives Incomplete or the
                    whole input's verdict; the parser fed in two steps gives
                    parse_head's verdict
  response_head.rs  ResponseParser: the same for response heads (Interim and
                    Complete), for either request kind
  chunked.rs        ChunkedValidator: no panic; consumed <= given, data <=
                    consumed; at most MAX_UNCONSUMED bytes left before the end;
                    the body in pieces (its calling convention) gives the
                    totals and the error of the body at once
  config.rs         config::load: no panic on any text
  head_input.rs     how the head targets read an input: 8 bytes choose limits
                    the config could set, a cut point and the request kind
http.dict           libFuzzer dictionary of HTTP tokens for the head targets
                    (without it they rarely get past the first line)
```

```sh
cd fuzz
cargo +nightly fuzz run --debug-assertions request_head -- \
  -max_total_time=60 -rss_limit_mb=2048 -malloc_limit_mb=64 -timeout=10 -dict=http.dict
```

A crash input lands in `fuzz/artifacts/<target>/`; replay it with
`cargo +nightly fuzz run --debug-assertions <target> <file>`.
