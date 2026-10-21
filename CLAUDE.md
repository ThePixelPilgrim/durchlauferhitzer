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

## Rust only (mandatory)

- No C code in this project, ever — neither in the repo nor in the
  dependency tree. The project is pure Rust; the **only** exception is
  linking against the system glibc.
- That rules out any crate that compiles or links C/C++/assembly: no `cc`
  or `cmake` build scripts, no `*-sys` crates other than `libc`, no
  OpenSSL, `ring`, or `aws-lc-rs`. If TLS crypto is ever needed, use a
  pure-Rust provider (e.g. `rustls` with a RustCrypto-based provider).
- Before adding a dependency, check its full transitive tree
  (`cargo tree`) for build scripts or bundled C sources; reject it if any
  are found.

## Minimal dependencies (mandatory)

- Use external crates as little as possible, to shrink the supply-chain
  attack surface. Default to the Rust standard library and hand-rolled
  code; a new dependency needs a concrete reason the std library cannot
  reasonably cover.
- Every dependency that is added must be justified in its commit message,
  pinned via `Cargo.lock` (committed), and kept as small as possible
  (disable default features, prefer crates with few transitive deps).

## Skills

The superpowers skills are vendored as project skills in `.claude/skills/`
(see `.claude/skills/SUPERPOWERS-SOURCE.md`). Where a skill refers to
`superpowers:<name>`, use the project skill `<name>`.
