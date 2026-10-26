# Handoff — where work stands and how to continue

Last updated 2026-10-26, after stage 3 Task 4 landed on `main`.
A new session should start from the latest `main`, read this file first, then `CLAUDE.md` (see its "Branches and main" rules).

## State

| Stage | Status |
|---|---|
| 1 Foundations | done, merged to `main` (`ff5480a`) — record: `docs/superpowers/reviews/2026-10-25-stage-1-review.md` |
| 2 Static files | done, merged to `main` (`576a77c`) — record: `docs/superpowers/reviews/2026-10-26-stage-2-review.md` |
| 3 Reverse proxy | **in progress** on `main` (Tasks 1–4 landed) — plan: `docs/superpowers/plans/2026-10-26-stage-3-reverse-proxy.md` |

Stage 3 tasks: **1 split core.rs ✅, 2 response-head parser ✅, 3 chunked validator ✅**, 4 head rewriting ✅, 5 upstream interface + scripted backends ← **next**, 6 request bodies, 7 response bodies, 8 timeouts/failure matrix, 9 randomized sim. Each task lands on `main` once reviewed. Then the stage 3 whole-branch review over the stage's range on `main`, its fixes, and the review record.

Requirements live in the spec with stable IDs (FR-…, NFR-…, DRV-…, consolidated 2026-10-25); plans, reviews and ledger lines cite those IDs instead of restating requirements, and new owner decisions go into the spec first. Open owner decisions are listed at the end of the spec.

Key documents: spec `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md` (binding), crate READMEs `core/README.md`, `sim/README.md`, example config `examples/durchlauferhitzer.conf` (a test keeps it loadable).

## Decided before Task 4

Obs-text in header values is rejected (FR-HTTP-05, FR-PROXY-13); `%3F` in paths → 400 (FR-HTTP-15a); response `Content-Length` passes through (FR-PROXY-12); a client stalling its body → 408 (FR-PROXY-26). Task 4 implements the first three (see the note in Task 4 of the stage 3 plan). Also decided since: half-close keeps the upstream (Task 8), `Expect` outside proxy routes (Task 6), shutdown modes / exit codes / `Date` (stage 4), interim cap 8 and empty reason kept (Task 7) — all in the spec.

To start: `git fetch origin && git checkout -B <work-branch> origin/main`, run the gate once, recreate the SDD workspace and its `progress.md` from the ledger snapshot below, then run Task 4 as described in "How a task is run". Remaining open questions: the spec's "Open decisions (owner)" section.

## Unification (2026-10-26)

The abandoned branch `ccr-716d91b0-bff174` had redone Tasks 1–4. A comparison (keep `main` in all three parts) ported its stronger tests and one fix onto `main`:
- U1: teardown order Log → CloseFile → Close for abort and socket reset (stage-2 M7), with tests and contract docs.
- U3–U8: response-head tests (interim-head checks, first bad line wins, byte-by-byte interim + final, early limit errors, linear scan); U11–U15: chunked tests (exact size values, CR/LF arms, largest sizes, deep-state proptest, spent verdicts).
- U17: FR-HTTP-09a (new) — `Connection` listing `content-length`, `transfer-encoding` or `host` → 400 / 502, shared in `head_line.rs`.
- Owner decisions D1–D5 written into the spec: FR-HTTP-09a; FR-PROXY-08 (more than one `X-Forwarded-For` line → 400); FR-PROXY-12 (one hop-by-hop list both ways, `Expect` included; empty reason for a code with no standard reason); FR-PROXY-25 (duplicate `Expect` or a list → 417); FR-PROXY-16 states the response check order.
- MIT license declared in the manifests and README.

Task 4 (done, `e05a468`..`4b8a681`): obs-text rejected in both parsers (FR-HTTP-05, FR-PROXY-13), `%3F` in paths → 400 (FR-HTTP-15a), `core/src/http/forward.rs` (seeded from the abandoned branch, adapted): `render_backend_request` / `render_client_response` with exact `*_len`, `is_hop_by_hop`, and `check_forwardable` for FR-PROXY-08's duplicate-`X-Forwarded-For` rule. Calls accepted in review: request line always HTTP/1.1; empty client XFF counts as absent; `Connection: x-forwarded-for` drops the client value; header names go out lowercased (values byte-exact); standard reasons = RFC 9110 + 103/428/429/431/511, else empty.

Still to carry into later tasks (`.superpowers/` notes are not in git, so listed here):
- Task 5: call `forward::check_forwardable` before `Connect` (its `ForwardError::status()` is 400); map every `ResponseError` (incl. `BadHeaderValue` for obs-text, the FR-HTTP-09a variant) to 502.
- Task 5: the abandoned branch's `Exchange { persistence, buffered }` struct shared by the responding states (removes repeated fields); statuses 417/502/504 and a 502 mapping for every `ResponseError`.
- Task 6: the abandoned branch's `Expect` parsing (with D3) as a starting point.

## Branches

- `main` holds all verified work; from now on each task is fast-forwarded to `main` once it passes review (CLAUDE.md, "Branches and main").
- `claude/magical-dijkstra-2eoh4y`: the branch stage 3 Tasks 1–3 were built on; fully contained in `main`.
- `unify`: the unification work; fully contained in `main`.
- `ccr-716d91b0-bff174`: **abandoned, do not build on it.** A later session (2026-10-25/26) missed this branch and wrote a second stage 3 plan (`2026-10-25-stage-3-reverse-proxy.md`) from the pre-consolidation spec, then redid Tasks 1–4 there. It contradicts owner decisions (allows obs-text, resume-at-half backpressure, no interim cap). Task 4 has used its `forward.rs`; Tasks 5–6 still take the `Exchange` struct and `Expect` parsing from it, then it can go (owner decides).

## Owner's way of working (mandatory, see CLAUDE.md)

- **Subagent-driven development** (`.claude/skills/subagent-driven-development`), one plan task at a time; **stop and report after each task**, wait for "Go". Tiny doc-only changes may be done inline.
- Strict red/green TDD; implementers write the failing test first and paste real RED output.
- Commit **and push** after every step; report the pushed **SHAs** and the **GitHub URL of every changed Markdown file** (`https://github.com/ThePixelPilgrim/durchlauferhitzer/blob/main/<path>`).
- Commit trailer lines (from the session's attribution reminder) end every commit message.
- Fast-forward `main` after every task that passes review and the gate (standing permission, 2026-10-26); never rewrite `main`.
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
- Stage 4 (epoll driver): open-file limit for 2 × max_connections sockets + files + roots; follow the file contract in `core/src/core/mod.rs` docs exactly.
- Stage 5: IPv6 per-IP grouping by prefix; refused-slot pool fairness.
- Stage 7: double buffering vs fill-drain-refill measured with `tc netem`; body buffer size; parse/route allocation nits.
