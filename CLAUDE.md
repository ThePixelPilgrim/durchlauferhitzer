# durchlauferhitzer

## Model roles (mandatory)

- **Fable never does implementation work itself.** When the session runs on
  Fable, it may brainstorm, write specs and plans, coordinate, and review —
  but it must not write or edit product code, tests, build files, or
  dependencies. All implementation is delegated to subagents running on
  Opus or Sonnet (Agent tool with `model: "opus"` or `model: "sonnet"`).
- This applies to every execution path, including
  `subagent-driven-development` and `executing-plans`: under Fable, "Native"
  execution is not allowed; use subagent-driven execution with Opus/Sonnet
  implementers.

## Execution (mandatory)

- Implementation plans are executed subagent-driven
  (`subagent-driven-development`), for every model. The only exception: a
  change so small that doing it inline is cheaper than dispatching an
  implementer and reviewer.
- Commit and push after every step that changes the repository (each
  implementer commit, each fix round, each doc or plan edit), unless told
  otherwise. Implementer subagents push their own commits. A plan's
  red/green pair (failing test, then the code that makes it pass) lands as
  one commit, so the pushed branch never carries a deliberately failing
  test.
- Work one plan task at a time (or two tightly linked tasks, if that is
  cheaper), then stop and report before starting the next.

## Rust only (mandatory)

- No C code in this project, ever — neither in the repo nor in the
  dependency tree. The project is pure Rust; the **only** exception is
  linking against the system glibc.
- That rules out any crate that compiles or links C/C++/assembly: no `cc`
  or `cmake` build scripts, no `*-sys` crates other than `libc`, no
  OpenSSL, `ring`, or `aws-lc-rs`. If TLS crypto is ever needed, use a
  pure-Rust provider (e.g. `rustls` with a RustCrypto-based provider).
- Scope: the shipped product. Test-only tooling (e.g. `cargo-fuzz` /
  libFuzzer) may contain C/C++, as long as none of it reaches the binary.
  An automated check (CI) verifies the product's dependency tree.
- Before adding a dependency, check its full transitive tree
  (`cargo tree`) for build scripts or bundled C sources; reject it if any
  are found.

## Minimal dependencies (mandatory)

- This applies to the shipped product (`[dependencies]` of anything that
  ends up in the binary). Test-only tooling (`[dev-dependencies]`, fuzz
  targets, test harness crates) may use established helper crates freely.
- Use external crates as little as possible in the product, to shrink the
  supply-chain attack surface. Default to the Rust standard library and hand-rolled
  code; a new dependency needs a concrete reason the std library cannot
  reasonably cover.
- Every dependency that is added must be justified in its commit message,
  pinned via `Cargo.lock` (committed), and kept as small as possible
  (disable default features, prefer crates with few transitive deps).

## Skills

The superpowers skills are vendored as project skills in `.claude/skills/`
(see `.claude/skills/SUPERPOWERS-SOURCE.md`). Where a skill refers to
`superpowers:<name>`, use the project skill `<name>`.

## Reporting (mandatory)

- Whenever a Markdown file is written and pushed, post its public GitHub URL
  in the chat, so the owner can open it in a browser:
  `https://github.com/ThePixelPilgrim/durchlauferhitzer/blob/<branch>/<path>`
- After every push, state in the chat the SHA(s) of the commit(s) that were
  committed and pushed.

## Development

- Strict red/green TDD: write the failing test, run it and see it fail, then
  implement.
- Before every commit run `cargo fmt --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace` and `scripts/check-product-deps.sh`.
