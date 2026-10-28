# durchlauferhitzer

New session? Start from the latest `main` and read `docs/superpowers/HANDOFF.md` first — it says where work stands and what is next. See "Branches and main" below before doing anything else.

Requirements live in the design spec (`docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md`) with stable IDs (FR-…, NFR-…, DRV-…); plans and reviews cite those IDs, and a new owner decision is written into the spec before it is cited.

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

## Branches and main (mandatory)

- `main` is the single source of truth. Every session begins with
  `git fetch origin` and works from the latest `origin/main`, whatever
  branch the session was assigned.
- Before any other work, list the remote branches
  (`git ls-remote --heads origin`). If any branch carries commits that are
  not on `main`, stop and report it to the owner before planning or
  implementing anything: it may be unmerged work from an earlier session.
- Work on one short-lived branch cut from `main`. As soon as a plan task
  has passed its task review and the full gate, fast-forward `main` to it
  and push `main` (standing owner permission, 2026-10-26). Doc-only
  commits (plans, spec, handoff) go to `main` the same way. Never rewrite
  `main`; if a fast-forward is impossible, stop and report.
- Update `docs/superpowers/HANDOFF.md` and land it on `main` at the end of
  every task, so the next session finds the current state there.
- The stage's whole-branch review still runs at the end of each stage, over
  the stage's range on `main`; its fixes land on `main` the same way.

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

## Requirements stay current (mandatory)

- Whenever a new owner decision or mandate changes the product's behaviour or
  constraints, add or update the matching requirement (with its ID and status)
  in the design spec in the same commit as the change — before any plan,
  brief or code relies on it. Retired requirements are marked as such, not
  silently deleted. Process-only rules (like this file's) stay here.

## Reporting (mandatory)

- Whenever a Markdown file is written and pushed, post its public GitHub URL
  in the chat, so the owner can open it in a browser:
  `https://github.com/ThePixelPilgrim/durchlauferhitzer/blob/<branch>/<path>`
- After every push, state in the chat the SHA(s) of the commit(s) that were
  committed and pushed.

## Development

- Strict red/green TDD: write the failing test, run it and see it fail, then
  implement.
- Each crate has a README describing its purpose and layout; update it in
  the same commit when modules are added, moved or removed.
- Before every commit run `cargo fmt --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, `scripts/check-product-deps.sh` and
  `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`.
  CI runs all of these plus `scripts/test-check-product-deps.sh`; a green
  CI run is enough for that self-test (owner, 2026-10-28).
