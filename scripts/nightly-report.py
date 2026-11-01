#!/usr/bin/env python3
"""Nightly failures file or update GitHub issues (NFR-TEST-08).

Test tooling, Python 3 standard library only. Two modes:

record  In a failing nightly job: read its logs, write a small failure
        record (JSON) for the reporting job to download.

            nightly-report.py record --job long-runs --arch x86_64 \\
                --run-url URL --out failure.json long-runs.log
            nightly-report.py record --job fuzz ... logs/request_head.log ...
            nightly-report.py record --job older-glibc ... driver-tests.log

report  In the reporting job: read every record, group the failures by
        fingerprint and, with `gh` (GH_TOKEN, GH_REPO), comment on the open
        issue with that fingerprint, reopen a closed one with a
        "regressed" comment, or file a new one labelled `nightly` (at most
        three per run; the rest are listed in the job summary).

            nightly-report.py report --records DIR --needs needs.json --run-url URL

The fingerprint is `fp-` + the first 8 hex digits of the SHA-256 of a
normalized text, which names the failure, not the seed or input:
  long runs    job, panic location (path:line), first line of the panic
               message with run-varying numbers replaced by N;
  fuzz         job, target, and the same panic location and message, or
               the kind (oom, timeout, leak, crash);
  older glibc  job and the failing test's name;
  a job that timed out or was cancelled (no record): `<job>|timeout-or-cancelled`.
Log text is untrusted: it only ever lands in record fields and issue
bodies (fenced), never in a command line other than as one argv item.
"""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
from datetime import datetime, timezone

LABEL = "nightly"
LABEL_COLOR = "B60205"
LABEL_DESCRIPTION = "A failing nightly job (NFR-TEST-08)"
MAX_NEW_ISSUES = 3
MAX_TEXT = 4000
MAX_REASON = 150

JOBS = {"long-runs": "long runs", "fuzz": "fuzz", "older-glibc": "older glibc"}

# Replay prefixes: the environment each job runs its tests with.
LONG_RUNS_ENV = (
    "CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=true"
)
LONG_RUNS_CMD = "cargo test --release --locked -p durchlauferhitzer-sim"
OLDER_GLIBC_ENV = (
    "DURCHLAUFERHITZER_REQUIRE_ROOT=1 DURCHLAUFERHITZER_REQUIRE_LANDLOCK=1 "
    "DURCHLAUFERHITZER_TEST_SECCOMP_KILL=1"
)
OLDER_GLIBC_CMD = "cargo test -p durchlauferhitzer --locked"
OLDER_GLIBC_WHERE = "as root in debian:12, as the older glibc job runs it"

_PANIC = re.compile(
    r"^thread '(?P<thread>[^']*)'(?: \(\d+\))? panicked at (?P<loc>.+:\d+:\d+):$"
)
_MESSAGE_END = re.compile(
    r"^(note: run with|stack backtrace:|thread '[^']*'(?: \(\d+\))? panicked at |"
    r"test \S+ \.\.\. |failures:$|---- |==\d+== |error: test failed|test result: |"
    r"\s+Running )"
)
_RUNNING = re.compile(r"^\s+Running (?:unittests )?(?P<src>\S+)")
_FAILED = re.compile(r"^test (?P<name>\S+) \.\.\. FAILED")
_SIM_SEED = re.compile(r"^SIM_SEED=(?P<seed>\d+) failed \((?P<plan>.*?)\): (?P<rest>.*)$")
_TEST_UNIT = re.compile(r"Test unit written to (?P<path>\S+)")
_ASAN = re.compile(r"ERROR: AddressSanitizer: (?P<kind>[\w-]+)")


# --- normalization and fingerprint ---------------------------------------


def normalize_message(text):
    """Strip run-varying numbers from one line of a panic message: hex
    numbers become 0xN and every digit run not preceded by a letter or
    underscore becomes N (so `u64`, `h2`, `sha256` keep theirs, `12ms`
    becomes `Nms`); whitespace runs collapse to one space."""
    text = re.sub(
        r"0x[0-9a-fA-F]+|(?<![A-Za-z_\d])\d+",
        lambda m: "0xN" if m.group(0).startswith("0x") else "N",
        text,
    )
    return " ".join(text.split())


def normalize_location(loc, workspace):
    """`path:line:col` -> `path:line`, the path relative to the checkout
    (absolute for path dependencies outside the workspace, such as the fuzz
    crate's), the registry and rustc source prefixes shortened."""
    path, line = loc, ""
    m = re.match(r"^(?P<path>.+?):(?P<line>\d+)(?::\d+)?$", loc)
    if m:
        path, line = m.group("path"), m.group("line")
    if workspace:
        prefix = workspace.rstrip("/") + "/"
        if path.startswith(prefix):
            path = path[len(prefix):]
    path = re.sub(r"^.*/\.cargo/registry/src/[^/]+/", "registry/", path)
    path = re.sub(r"^/rustc/[0-9a-f]+/", "rustc/", path)
    return f"{path}:{line}" if line else path


def fingerprint(text):
    return "fp-" + hashlib.sha256(text.encode("utf-8")).hexdigest()[:8]


def _clean(text, limit):
    """One line, no control characters, at most `limit` characters."""
    text = "".join(c if c.isprintable() else " " for c in text)
    text = " ".join(text.split())
    return text if len(text) <= limit else text[: limit - 1] + "…"


def _cap(text):
    return text if len(text) <= MAX_TEXT else text[: MAX_TEXT - 2] + "\n…"


def describe(job, f):
    """(fingerprint, title) of one failure of `job`."""
    label = JOBS.get(job, job)
    kind = f.get("kind", "")
    test = f.get("test", "")
    if job == "fuzz" and test:
        label = f"fuzz {test}"
    if kind == "unrecognized":
        key = f"{job}|unrecognized"
        reason = "failed without a recognized failure"
    elif kind == "timeout-or-cancelled":
        key = f"{job}|timeout-or-cancelled"
        reason = "timed out or was cancelled, no failure record"
    elif job == "older-glibc":
        key = f"{job}|{test}"
        reason = f"{test} failed"
    elif kind == "panic":
        target = test if job == "fuzz" else ""
        key = f"{job}|{target}|{f.get('location', '')}|{normalize_message(f.get('message', ''))}"
        reason = f"panic at {f.get('location', '')}"
    elif kind in ("oom", "timeout", "leak"):
        key = f"{job}|{test}|{kind}"
        reason = {"oom": "out of memory", "timeout": "timeout", "leak": "memory leak"}[kind]
    elif kind == "crash":
        detail = f.get("detail", "")
        key = f"{job}|{test}|crash|{detail}"
        reason = f"crash ({detail})" if detail else "crash"
    else:
        key = f"{job}|{test}|failed"
        reason = f"{test} failed"
    fp = fingerprint(key)
    title = f"nightly: {_clean(label, MAX_REASON)} — {_clean(reason, MAX_REASON)} [{fp}]"
    return fp, title


# --- log parsing ---------------------------------------------------------


def parse_panics(text):
    """Every `thread '..' panicked at ..` in the log, in order, with the
    message lines that follow it."""
    panics = []
    current = None
    for line in text.splitlines():
        m = _PANIC.match(line)
        if m:
            current = {"thread": m.group("thread"), "loc": m.group("loc"), "lines": [], "header": line}
            panics.append(current)
            continue
        if current is not None:
            if _MESSAGE_END.match(line):
                current = None
            else:
                current["lines"].append(line)
    return panics


def parse_failed_tests(text):
    """[(binary source, test name)] of every `test .. ... FAILED`."""
    failed, src = [], ""
    for line in text.splitlines():
        m = _RUNNING.match(line)
        if m:
            src = m.group("src")
            continue
        m = _FAILED.match(line)
        if m:
            failed.append((src, m.group("name")))
    return failed


def _target_args(src):
    """(label, cargo target args) of a test binary's source path."""
    m = re.match(r"^tests/(?P<name>[^/]+?)(?:\.rs|/main\.rs)$", src)
    if m:
        return m.group("name"), f"--test {m.group('name')}"
    if src.endswith("src/lib.rs"):
        return "lib", "--lib"
    if src.endswith(".rs") and src.startswith("src/"):
        return "bin", "--bins"
    return "", ""


def _unrecognized(why):
    return {
        "kind": "unrecognized",
        "test": "",
        "location": "",
        "message": "",
        "panic_text": why,
        "seed": "",
        "input": "",
        "replay": "",
    }


def _failure(**fields):
    f = _unrecognized("")
    f.update(fields)
    return f


def record_long_runs(text, workspace, seeds=None):
    """The failures of a long-runs log: per failed test, its last panic;
    a `SIM_SEED=..` panic of run_reporting takes the location and
    message of the panic it wraps (the one before it on that thread)."""
    if text is None:
        return [_unrecognized("The job left no log; see the run.")]
    panics = parse_panics(text)
    failures = []
    for src, test in parse_failed_tests(text):
        _, target = _target_args(src)
        mine = [p for p in panics if p["thread"] == test]
        replay = f"{LONG_RUNS_ENV} {{env}}{LONG_RUNS_CMD} {target} -- {test} --exact --nocapture"
        replay = " ".join(replay.split())
        if not mine:
            failures.append(_failure(kind="test", test=test, replay=replay.format(env="")))
            continue
        last = mine[-1]
        first = last["lines"][0] if last["lines"] else ""
        loc, message, seed = last["loc"], first, ""
        m = _SIM_SEED.match(first)
        if m:
            seed, message = m.group("seed"), m.group("rest")
            if len(mine) > 1:
                loc = mine[-2]["loc"]
        if seed:
            env = f"SIM_SEED={seed} "
        elif seeds:
            env = f"SIM_SEEDS={seeds} "
        else:
            env = ""
        header = f"panicked at {loc}:" if seed else last["header"]
        failures.append(
            _failure(
                kind="panic",
                test=test,
                location=normalize_location(loc, workspace),
                message=message,
                panic_text=_cap("\n".join([header] + last["lines"])),
                seed=seed,
                replay=replay.format(env=env),
            )
        )
    return failures or [_unrecognized("No failed test found in the log; see the run.")]


def _fuzz_one(target, text, workspace, toolchain):
    unit = _TEST_UNIT.search(text)
    input_name = os.path.basename(unit.group("path")) if unit else ""
    replay = f"cd fuzz && cargo +{toolchain} fuzz run --debug-assertions {target}"
    if input_name:
        replay += f" artifacts/{target}/{input_name}"
    base = {"test": target, "input": input_name, "replay": replay}
    panics = parse_panics(text)
    if panics:
        p = panics[-1]
        return _failure(
            kind="panic",
            location=normalize_location(p["loc"], workspace),
            message=p["lines"][0] if p["lines"] else "",
            panic_text=_cap("\n".join([p["header"]] + p["lines"])),
            **base,
        )
    lines = text.splitlines()

    def line_with(needle):
        return next((ln for ln in lines if needle in ln), "")

    for needle, kind in (
        ("ERROR: libFuzzer: out-of-memory", "oom"),
        ("ERROR: libFuzzer: timeout", "timeout"),
        ("LeakSanitizer", "leak"),
    ):
        hit = line_with(needle)
        if hit:
            return _failure(kind=kind, panic_text=_cap(hit), **base)
    asan = _ASAN.search(text)
    if asan:
        return _failure(kind="crash", detail=asan.group("kind"), panic_text=_cap(line_with("ERROR: AddressSanitizer")), **base)
    if "has overflowed its stack" in text:
        return _failure(kind="crash", detail="stack overflow", panic_text=_cap(line_with("has overflowed its stack")), **base)
    hit = line_with("ERROR: libFuzzer: deadly signal")
    if hit or unit:
        return _failure(kind="crash", detail="", panic_text=_cap(hit or unit.group(0)), **base)
    return None


def record_fuzz(logs, workspace, toolchain):
    """The failures of the fuzz job: `logs` maps each target to its log
    text (None if missing); targets that ran clean are skipped."""
    failures = []
    for target, text in logs.items():
        if text is None:
            continue
        f = _fuzz_one(target, text, workspace, toolchain)
        if f is not None:
            failures.append(f)
    return failures


def record_older_glibc(text, workspace):
    """The failures of the older-glibc job: each failed test by name."""
    if text is None:
        return [_unrecognized("The job left no log; see the run.")]
    panics = parse_panics(text)
    failures = []
    for src, test in parse_failed_tests(text):
        label, target = _target_args(src)
        mine = [p for p in panics if p["thread"] == test]
        panic_text = ""
        if mine:
            panic_text = _cap("\n".join([mine[-1]["header"]] + mine[-1]["lines"]))
        replay = f"{OLDER_GLIBC_ENV} {OLDER_GLIBC_CMD} {target} -- {test} --exact --nocapture"
        failures.append(
            _failure(
                kind="test",
                test=f"{label}::{test}" if label else test,
                panic_text=panic_text,
                replay=" ".join(replay.split()) + f"  # {OLDER_GLIBC_WHERE}",
            )
        )
    return failures or [_unrecognized("No failed test found in the log; see the run.")]


# --- records -------------------------------------------------------------


def make_record(job, arch, run_url, failures):
    return {"version": 1, "job": job, "arch": arch, "run_url": run_url, "failures": failures}


def _valid(rec):
    if not isinstance(rec, dict) or rec.get("version") != 1:
        return False
    if not all(isinstance(rec.get(k), str) for k in ("job", "arch", "run_url")):
        return False
    fs = rec.get("failures")
    return isinstance(fs, list) and all(
        isinstance(f, dict) and all(isinstance(v, str) for v in f.values()) for f in fs
    )


def load_records(directory):
    """Every valid record (*.json) under `directory`, in path order."""
    records = []
    if not os.path.isdir(directory):
        return records
    paths = []
    for root, _, files in os.walk(directory):
        paths += [os.path.join(root, n) for n in files if n.endswith(".json")]
    for path in sorted(paths):
        try:
            with open(path, encoding="utf-8") as fh:
                rec = json.load(fh)
        except (OSError, ValueError) as e:
            print(f"skipping {path}: {e}", file=sys.stderr)
            continue
        if _valid(rec):
            records.append(rec)
        else:
            print(f"skipping {path}: not a failure record", file=sys.stderr)
    return records


def add_missing(records, needs, run_url):
    """A record for each failed or cancelled job (by `needs`) that left
    none. A job ended by `timeout-minutes` concludes `cancelled` and runs
    no `if: failure()` step, so it never writes a record; a job cancelled
    by hand looks the same and is reported too (controller ruling
    2026-11-02)."""
    have = {r["job"] for r in records}
    out = list(records)
    for job, info in needs.items():
        if not isinstance(info, dict) or job in have:
            continue
        result = info.get("result")
        if result == "failure":
            why = "The job failed but left no failure record; see the run."
            out.append(make_record(job, "", run_url, [_unrecognized(why)]))
        elif result == "cancelled":
            why = "The job timed out (timeout-minutes) or was cancelled; see the run."
            f = _failure(kind="timeout-or-cancelled", panic_text=why)
            out.append(make_record(job, "", run_url, [f]))
    return out


# --- decision ------------------------------------------------------------


def group(records):
    """One group per fingerprint, in order of first appearance."""
    groups, by_fp = [], {}
    for rec in records:
        for f in rec["failures"]:
            fp, title = describe(rec["job"], f)
            if fp not in by_fp:
                by_fp[fp] = {"fp": fp, "title": title, "occurrences": []}
                groups.append(by_fp[fp])
            by_fp[fp]["occurrences"].append(
                {"job": rec["job"], "arch": rec["arch"], "run_url": rec["run_url"], "failure": f}
            )
    return groups


def decide(groups, issues, limit=MAX_NEW_ISSUES):
    """Per group: comment on the open issue with its fingerprint, else
    reopen the latest closed one, else create (up to `limit`, then skip)."""
    actions, created = [], 0
    for g in groups:
        tag = f"[{g['fp']}]"
        mine = [i for i in issues if tag in i.get("title", "")]
        opened = [i for i in mine if str(i.get("state", "")).upper() == "OPEN"]
        if opened:
            op, number = "comment", max(i["number"] for i in opened)
        elif mine:
            op, number = "reopen", max(i["number"] for i in mine)
        elif created < limit:
            op, number = "create", None
            created += 1
        else:
            op, number = "skip", None
        actions.append({"op": op, "number": number, "group": g})
    return actions


# --- issue bodies --------------------------------------------------------


def fenced(text):
    """`text` in a code fence longer than any backtick run inside it."""
    longest = max((len(m) for m in re.findall(r"`+", text)), default=0)
    fence = "`" * max(3, longest + 1)
    return f"{fence}\n{text}\n{fence}"


def _occurrence(o):
    f = o["failure"]
    name = JOBS.get(o["job"], o["job"])
    if o["job"] == "fuzz" and f.get("test"):
        name = f"fuzz {f['test']}"
    head = f"{name} ({o['arch']})" if o["arch"] else name
    lines = [f"#### {_clean(head, 200)}", "", f"Run: {o['run_url']}"]
    if f.get("seed"):
        lines.append(f"Seed: {_clean(f['seed'], 40)}")
    if f.get("input"):
        lines.append(f"Input: `{_clean(f['input'], 200).replace('`', '')}` (in the run's artifacts)")
    if f.get("replay"):
        lines += ["", "Replay:", "", fenced(f["replay"])]
    return "\n".join(lines)


def _blocks(g):
    return "\n\n".join(_occurrence(o) for o in g["occurrences"])


def new_issue_body(g):
    first = g["occurrences"][0]["failure"]
    text = first.get("panic_text") or "(no panic text)"
    return (
        f"A nightly job failed (NFR-TEST-08). Fingerprint `{g['fp']}`: later failures with it "
        "are added here as comments.\n\n"
        f"{_blocks(g)}\n\n#### Panic text\n\n{fenced(text)}\n"
    )


def comment_body(g, date):
    return f"Failed again on {date}.\n\n{_blocks(g)}\n"


def regressed_body(g, date):
    return f"Regressed on {date}: this failure is back.\n\n{_blocks(g)}\n"


# --- report --------------------------------------------------------------


def report(records, gh, date):
    """Apply the decision with `gh(argv, body=None) -> stdout`; return a
    Markdown summary for the job summary."""
    issues = json.loads(
        gh(["issue", "list", "--label", LABEL, "--state", "all", "--limit", "1000",
            "--json", "number,title,state"]) or "[]"
    )
    actions = decide(group(records), issues)
    if any(a["op"] == "create" for a in actions):
        labels = json.loads(gh(["label", "list", "--limit", "1000", "--json", "name"]) or "[]")
        if LABEL not in {lab.get("name") for lab in labels}:
            gh(["label", "create", LABEL, "--color", LABEL_COLOR, "--description", LABEL_DESCRIPTION])
    lines = ["## Nightly failures", ""]
    for a in actions:
        g, n = a["group"], a["number"]
        if a["op"] == "create":
            url = gh(["issue", "create", "--title", g["title"], "--label", LABEL, "--body-file", "-"],
                     body=new_issue_body(g)).strip()
            lines.append(f"- created {url}: {g['title']}")
        elif a["op"] == "comment":
            gh(["issue", "comment", str(n), "--body-file", "-"], body=comment_body(g, date))
            lines.append(f"- commented on #{n}: {g['title']}")
        elif a["op"] == "reopen":
            gh(["issue", "reopen", str(n)])
            gh(["issue", "comment", str(n), "--body-file", "-"], body=regressed_body(g, date))
            lines.append(f"- reopened #{n} (regressed): {g['title']}")
        else:
            runs = ", ".join(sorted({o["run_url"] for o in g["occurrences"]}))
            lines.append(
                f"- not filed (at most {MAX_NEW_ISSUES} new issues per run): {g['title']} ({runs})"
            )
    if not actions:
        lines.append("- no failure records")
    return "\n".join(lines) + "\n"


def _gh(args, body=None):
    done = subprocess.run(["gh", *args], input=body, text=True, capture_output=True)
    if done.returncode != 0:
        sys.stderr.write(done.stderr)
        raise SystemExit(f"gh {args[0]} {args[1]} failed ({done.returncode})")
    return done.stdout


# --- command line --------------------------------------------------------


def _read(path):
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            return fh.read()
    except OSError:
        return None


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = p.add_subparsers(dest="mode", required=True)
    r = sub.add_parser("record", help="write a failing job's failure record")
    r.add_argument("--job", required=True, choices=sorted(JOBS))
    r.add_argument("--arch", required=True)
    r.add_argument("--run-url", required=True)
    r.add_argument("--out", required=True)
    r.add_argument("--workspace", default=os.environ.get("GITHUB_WORKSPACE", os.getcwd()))
    r.add_argument("--toolchain", default="nightly", help="fuzz: the nightly toolchain")
    r.add_argument("--seeds", default=os.environ.get("SIM_SEEDS"), help="long runs: SIM_SEEDS")
    r.add_argument("logs", nargs="*")
    s = sub.add_parser("report", help="file or update issues from the records")
    s.add_argument("--records", required=True)
    s.add_argument("--needs", help="JSON of the workflow's `needs` context")
    s.add_argument("--run-url", required=True)
    s.add_argument("--summary", default=os.environ.get("GITHUB_STEP_SUMMARY"))
    args = p.parse_args(argv)

    if args.mode == "record":
        if args.job == "fuzz":
            logs = {os.path.basename(x).removesuffix(".log"): _read(x) for x in args.logs}
            failures = record_fuzz(logs, args.workspace, args.toolchain)
            failures = failures or [_unrecognized("No crash, OOM or timeout found in the logs; see the run.")]
        else:
            text = _read(args.logs[0]) if args.logs else None
            if args.job == "long-runs":
                failures = record_long_runs(text, args.workspace, args.seeds)
            else:
                failures = record_older_glibc(text, args.workspace)
        with open(args.out, "w", encoding="utf-8") as fh:
            json.dump(make_record(args.job, args.arch, args.run_url, failures), fh, indent=1)
        for f in failures:
            print(describe(args.job, f)[1])
        return 0

    records = load_records(args.records)
    if args.needs:
        with open(args.needs, encoding="utf-8") as fh:
            records = add_missing(records, json.load(fh), args.run_url)
    date = datetime.now(timezone.utc).date().isoformat()
    summary = report(records, _gh, date)
    print(summary)
    if args.summary:
        with open(args.summary, "a", encoding="utf-8") as fh:
            fh.write(summary)
    return 0


if __name__ == "__main__":
    sys.exit(main())
