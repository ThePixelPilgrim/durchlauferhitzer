# Stage 10 (carry-forwards) — review record

Stage 10 is done. Five of its six tasks delivered what the plan asked;
Task 5 (OpenRC supervision) ended as a documented gap. The whole-branch
review found no Critical and no Important defect in the six tasks. Its
fix wave fixed six Minor findings and withdrew the seventh fix.

- Plan: `docs/superpowers/plans/2026-11-10-stage-10-carry-forwards.md`
- Spec: `docs/superpowers/specs/2026-10-21-durchlauferhitzer-design.md`
- Range on `main`: `462a967..` this record

## The tasks

| Task | What | Range on `main` |
|---|---|---|
| 1 | The nightly `pressure` job (NFR-TEST-08) | `fe604e4..de557b7` |
| 2 | The nightly `newest-libc` job (NFR-TEST-08) | `393c78e..d10e4ca` |
| 3 | `user <uid>:<gid>` skips the NSS lookup (FR-PROC-03, FR-CFG-11) | `a4531df..2d9c84f` |
| 4 | Product source lines beside the benchmarks (NFR-MAINT-12) | `41c0131..3d69657` |
| 5 | OpenRC supervision: a documented gap (FR-PROC-07a) | `cc948bc..55dc088` |
| 6 | `sim/tests/proxy/bodies.rs` split, a move only (NFR-MAINT-10) | `0273570..f262033` |

Outside the plan, in the same range:

- `CLAUDE.md` gained the pyramid rule for reader-facing prose (owner,
  2026-11-17), `b9b6c25`.
- The spec's narrative sections and the handoff were rewritten to that
  rule (`40ae042`, `1aa21a2`, `8e13880`, `60fb434`). The requirement
  entries were not touched.

## Product size (NFR-MAINT-12)

The product grew by 59 lines in stage 10.

| Crate | At `462a967` | At the end of the stage | Delta |
|---|---|---|---|
| `core` | 9668 | 9702 | +34 |
| `driver` | 5686 | 5711 | +25 |
| Total | 15354 | 15413 | +59 |

The growth is Task 3 and its review fixes: the `User` enum, the id
parser, the root-id config error, the drop's log entry and the uid-0
refusal.

## Owner decisions taken during stage 10 (all in the spec)

- 2026-11-17, FR-PROC-03: a 0 in `user <uid>:<gid>` is a config error
  (exit 1), so `--check` reports it.
- 2026-11-17, FR-PROC-03: only the startup log shows which form of
  `user` was used.
- 2026-11-17, `CLAUDE.md`: reader-facing prose follows the pyramid
  order.

## Rulings made on the owner's behalf

Each can be overturned; the cost of a wrong ruling is stated.

1. **`user` id 4294967295 is an invalid value** (FR-CFG-11). It is the
   kernel's "no change" id and can never start. Same class as the
   owner's decision for id 0. If wrong: one line and one test.
2. **A name that resolves to uid 0 is refused before the drop** with a
   clear message, for both forms (FR-PROC-03). If wrong: the old
   refusal after the drop still holds.
3. **`nightly.yml` stays under 900 lines** by moving logic into
   `scripts/bench-chart.py`. It is at 899 lines. If wrong: the next
   addition splits the file anyway.
4. **Out-of-line test modules are not product lines** (NFR-MAINT-12),
   although the plan's file rule would have counted them.
5. **A crash of the line counter is silent**: it stops no benchmark and
   files no issue (NFR-MAINT-12).
6. **Nightly fingerprints carry the leg** for set-up failures and
   timeouts of `newest libc` and `restart soak` (NFR-TEST-08). Four old
   fingerprints change; their issues are closed.
7. **The OpenRC pid-file guard was withdrawn** (see below).
8. **The OpenRC wrapper is open decision 6** in the spec, for the
   owner.

## Final whole-stage review (at `8208d59`, Fable)

The review found seven Minor findings and nothing above that. It
checked several claims by running them:

- With `user 65534:65534` the binary opens neither `/etc/nsswitch.conf`
  nor `/etc/passwd` (strace of the built binary).
- Every nightly job runs or skips as intended for the scheduled run and
  for each dispatch input.
- The line counter's brace matching holds on every product file.
- The publish path works on a copy of the live `benchmarks` branch.

| # | Finding | Outcome |
|---|---|---|
| 1 | OpenRC: after a crash a reused pid is taken for the server | Documented as a caveat; the guard was withdrawn |
| 2 | `user 4294967295:…` passes `--check` but cannot start | Fixed |
| 3 | Both `newest libc` legs share a set-up fingerprint | Fixed |
| 4 | The new `benchmarks` README never replaces the old one | Fixed |
| 5 | A night without `bench.csv` loses its lines row | Fixed |
| 6 | A name with uid 0 gets a cryptic refusal | Fixed |
| 7 | Spec Timeline and Open decisions behind the outcome | Fixed |

## Fix wave (`64817d2`, `06dafbd`, `acfa425`, one Opus implementer)

The wave took three commits, because the fix for finding 1 failed its
re-review twice.

1. `64817d2` fixed all seven findings. Its OpenRC guard overrode `stop`
   and `status` and compared the pid's executable path with `command`.
2. The re-review found that an unreadable executable read as "not the
   server". As root without `CAP_SYS_PTRACE`, `stop` dropped the pid
   file without signalling, and `restart` started a second server.
3. `06dafbd` made the guard three-way: server, stranger, unknown.
4. The second re-review found that a healthy server whose binary was
   moved aside still read as a stranger. That is worse than stock
   OpenRC.
5. `acfa425` withdrew the guard. The OpenRC script, its test and
   `ci.yml` are byte-identical to `8208d59`. The README and FR-PROC-07a
   state the pid-reuse hazard and the manual way out.

Gate at the end of the wave, on a fresh build: fmt, clippy `-D
warnings`, the workspace tests, the driver suite under the seccomp kill
switch, the product-dependency check, docs with `-D warnings`, and
every script test.

## Root causes found on the way

- **A path does not identify a running process.** The executable path
  changes when a binary is moved, and it is unreadable without
  `CAP_SYS_PTRACE`. The script's draining list already uses pid plus
  start time, which has neither problem.
- **CI hid a capability dependency.** The Alpine container runs with
  `SYS_PTRACE`, so a check that needs it passed there and would have
  failed in a plain container.
- **A plan goes stale in a week.** Task 6's plan named a file size and
  a target file that had both changed. The task needed rulings before
  it could start.
- **A bash-only job notice** broke the reload helper on Fedora, where
  `/bin/sh` is bash (Task 2, issue #10).

## Accepted gaps

- **OpenRC does not restart the server after a crash** (FR-PROC-07a).
  The guaranteed install names systemd only (FR-PROC-08).
- **OpenRC knows the server only by its pid.** After an unnoticed crash
  `status` can report started, and `restart` can signal an unrelated
  process. The README gives the check and the way out. That recovery
  sequence was verified against the OpenRC 0.63.2 source, not run.
- **The uid-0 refusal for a name** is tested through the numeric path
  only; a name with uid 0 would need a passwd entry.
- **`#[cfg(all(test, ..))]` counts as product code** in the line count.
  The repo has no such case.
- **The newest-libc job covers the newest glibc and musl, not the
  newest kernel.**

## Process notes

- A fix that changes behaviour beyond the finding needs its own design
  step. The OpenRC guard was a new feature inside a review fix wave,
  and it cost three commits to learn that.
- A re-review that reproduces the failure, here with stubbed OpenRC
  functions, finds what reading alone does not.
- A subagent's refused push is not worked around by the subagent. The
  controller pushes its own branch as its regular step and says so.

## Deferred

- **Guard the OpenRC main pid by pid plus start time**, as `stop_post`
  does for the draining list. A planned task with tests under real
  OpenRC.
- **Open decision 6:** a POSIX `sh` wrapper as `supervise-daemon`'s
  child (owner).
- **Unverified suspicion:** a monitor that runs `restart` while a
  reload is mid-handover may leave the pid file naming the old process.
  A test with a polling loop around the helper's grace would settle it.
- **Unverified suspicion:** the pressure job's false-failure rate rests
  on four green runs.
- **The publish job** appends rows again when it is re-run for the same
  commit and date. It may fail when no artifact exists at all. Both
  predate the stage.
- **After `zap`**, processes a reload left draining are not ended,
  because `zap` runs no script function.
- **The first scheduled nightly from `main`** must show the
  product-lines chart and one row in `product-lines.csv`.
- **`nightly.yml` is at 899 lines**: the next addition splits it first.
- **`sim/tests/proxy/main.rs` is at 778 lines.**
