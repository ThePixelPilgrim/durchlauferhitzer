# Handoff — where work stands and how to continue

Last updated after commit `1db3e36` on branch `claude/magical-dijkstra-2eoh4y`.
A new session should read this file first, then `CLAUDE.md`.

## State

| Stage | Status |
|---|---|
| 1 Foundations | done, merged to `main` (`ff5480a`) — record: `docs/superpowers/reviews/2026-10-25-stage-1-review.md` |
| 2 Static files | done, merged to `main` (`576a77c`) — record: `docs/superpowers/reviews/2026-10-26-stage-2-review.md` |
| 3 Reverse proxy | **in progress** on `claude/magical-dijkstra-2eoh4y` — plan: `docs/superpowers/plans/2026-10-26-stage-3-reverse-proxy.md` |

Stage 3 tasks: **1 split core.rs ✅, 2 response-head parser ✅, 3 chunked validator ✅**, 4 head rewriting ← **next**, 5 upstream interface + scripted backends, 6 request bodies, 7 response bodies, 8 timeouts/failure matrix, 9 randomized sim. Then the stage 3 whole-branch review, fixes, review record, merge to `main` (owner approves merges and pushes to `main`).

Key documents: spec `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md` (binding), crate READMEs `core/README.md`, `sim/README.md`, example config `examples/durchlauferhitzer.conf` (a test keeps it loadable).

## Open decision before Task 4

From the Task 2 review: header values may contain obs-text (bytes ≥ 0x80), which RFC 9110 allows and both parsers accept. Task 4 forwards header values (backend → client and client → backend). Decide with the owner whether to forward them unchanged or reject/strip them. Recommendation to offer: forward unchanged (RFC-conformant; values are already free of CR/LF/NUL/CTL, so no splitting risk), and say so in the spec.

## Owner's way of working (mandatory, see CLAUDE.md)

- **Subagent-driven development** (`.claude/skills/subagent-driven-development`), one plan task at a time; **stop and report after each task**, wait for "Go". Tiny doc-only changes may be done inline.
- Strict red/green TDD; implementers write the failing test first and paste real RED output.
- Commit **and push** after every step; report the pushed **SHAs** and the **GitHub URL of every changed Markdown file** (`https://github.com/ThePixelPilgrim/durchlauferhitzer/blob/claude/magical-dijkstra-2eoh4y/<path>`).
- Commit trailer lines (from the session's attribution reminder) end every commit message.
- Never push to `main` without explicit permission; merges to `main` are fast-forward after the stage's final review.
- Pure Rust, zero product dependencies, no `unsafe`, exhaustive enums (no `_ =>` on enums), enums over bools, bounded memory, illegal states unrepresentable. Fable must not implement (delegate to Opus/Sonnet).
- The owner prefers asking before inventing; give a recommendation, list every decision taken on their behalf ("calls I made") in the report.

## How a task is run (as in this session)

1. Workspace: `bash .claude/skills/subagent-driven-development/scripts/sdd-workspace <plan>` (git-ignored `.superpowers/sdd/<plan>/`); recreate `progress.md` from the ledger snapshot below if missing.
2. Record `Task N: BASE <sha>`; `scripts/task-brief <plan> N` writes the brief.
3. Dispatch an implementer (Opus for interface/security-heavy tasks, Sonnet for small ones) with: brief path, context files, controller rulings, the mandatory process block (TDD, gate `cargo fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked && scripts/check-product-deps.sh`, commit trailers, push), and a report file path.
4. `scripts/review-package <plan> BASE HEAD`, dispatch a reviewer (task-reviewer template), adjudicate; fix rounds resume the implementer, then a scoped re-review.
5. Ledger lines for rulings, deferred minors, completion; report to the owner.

## Ledger snapshot (stage 3, as of `1db3e36`)

```
Ruling: T5–T7 may hard-code the default timeout values as named constants; T8 replaces them with Limits keys.
Task 1: complete (a9d03aa..78d4e87). Rulings: request.rs tests moved too (900-line rule); orphan invariant as debug_assert.
  deferred: take_opening not #[must_use]; route-kind dispatch (Proxy → 501) lives in static_files::plan — move to request.rs::respond in Task 5.
Task 2: complete (0abe3c1..a5dfa89). Rulings: shared header-line module (http/head_line.rs) used by both parsers;
  status line needs SP before (possibly empty) reason; HTTP/1.0 + Transfer-Encoding → UnsupportedTransferEncoding;
  Content-Length: 0 → ResponseBody::None (header kept); framing headers checked on 1xx/204/304/HEAD too.
  → Task 7: cap consecutive interim 1xx heads (e.g. 8) → 502.
  deferred: ResponseBody::None doc mentions 1xx; response tests lack `Content-Length : 5` and obs-text rows;
  → Task 4: obs-text in header values (open decision above).
Task 3: complete (a5dfa89..c7973d4). Rulings: hex case-insensitive, ≤16 significant digits, ≤32 size chars;
  calling convention = caller re-passes the unconsumed tail + new bytes; fill-drain-refill refined: a ≤35-byte
  framing tail (MAX_UNCONSUMED) may remain and is moved to the front before the next full read (plan/spec updated, 1db3e36).
  → Task 6: doc "stay valid" → "already forwarded; cut both"; test stickiness after mid-data BadLineEnd;
  accumulate `data` with saturating arithmetic for the body limit.
```

## Carry-forwards from earlier stages

- Stage 3: send backends a normalised `Content-Length`; empty stripped path → `/` (`strip_segments` already does it).
- Stage 4 (epoll driver): driver-side open timeout answering `FileFailed`; open-file limit for 2 × max_connections sockets + files + roots; follow the file contract in `core/src/core/mod.rs` docs exactly.
- Stage 5: IPv6 per-IP grouping by prefix; refused-slot pool fairness.
- Stage 7: double buffering vs fill-drain-refill measured with `tc netem`; body buffer size; parse/route allocation nits.
