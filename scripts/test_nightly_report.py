"""Tests of scripts/nightly-report.py (NFR-TEST-08): failure records from
canned logs, the fingerprint, and the comment / reopen / create decision.

Run with `python3 -m unittest discover -s scripts -p 'test_*.py'`.
"""

import contextlib
import importlib.util
import io
import json
import os
import re
import tempfile
import unittest

_HERE = os.path.dirname(os.path.abspath(__file__))
_SPEC = importlib.util.spec_from_file_location(
    "nightly_report", os.path.join(_HERE, "nightly-report.py")
)
nr = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(nr)

RUN = "https://github.com/o/r/actions/runs/111"
WS = "/home/runner/work/durchlauferhitzer/durchlauferhitzer"

# The long runs' output (`--nocapture`): the inner panic, then
# run_reporting's `SIM_SEED=..` panic, as this toolchain prints them.
SIM_LOG = """\
     Running tests/faults/main.rs (target/release/deps/faults-e4979b665c20f239)

running 2 tests

thread 'random_fault_schedules' (1047) panicked at sim/tests/faults/static_net.rs:412:13:
conn 17 got 503 after 12ms, expected 200 (step 88)
trace:
 step 1: accept conn 17
 step 2: read 4096
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

thread 'random_fault_schedules' (1047) panicked at sim/tests/faults/run.rs:336:9:
SIM_SEED=48213 failed (timer order OnTime, shutdown Drain { drain: 31 }): conn 17 got 503 after 12ms, expected 200 (step 88)
trace:
 step 1: accept conn 17
 step 2: read 4096
test random_fault_schedules ... FAILED
COVERAGE statuses=[1, 2] http=3
test random_proxy_schedules ... ok

failures:

failures:
    random_fault_schedules

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 3 filtered out; finished in 291.20s
"""

# The same bug on another seed (other numbers, other arch).
SIM_LOG_OTHER_SEED = (
    SIM_LOG.replace("48213", "190007")
    .replace("conn 17", "conn 4")
    .replace("12ms", "7ms")
    .replace("step 88", "step 3")
    .replace("drain: 31", "drain: 2")
    .replace("OnTime", "Late")
)

# A panic that is not wrapped by run_reporting (the coverage check).
SIM_COVERAGE_LOG = """\
running 1 test

thread 'random_proxy_schedules' (77) panicked at sim/tests/faults/proxy.rs:930:9:
coverage: only 3 of 40 reset cases reached
test random_proxy_schedules ... FAILED

failures:
    random_proxy_schedules
"""

FUZZ_PANIC_LOG = """\
INFO: Running with entropic power schedule (0xFF, 100).
INFO: Seed: 3405691582
#2\tINITED cov: 1234 ft: 1234 corp: 12/345b exec/s: 0 rss: 40Mb

thread '<unnamed>' (2817) panicked at /home/runner/work/durchlauferhitzer/durchlauferhitzer/core/src/http/request.rs:312:17:
index out of bounds: the len is 4 but the index is 9
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
==2817== ERROR: libFuzzer: deadly signal
    #0 0x55d1a2 in __sanitizer_print_stack_trace
SUMMARY: libFuzzer: deadly signal
MS: 3 InsertByte-ChangeBit-CopyPart-; base unit: 5f1b
artifact_prefix='/home/runner/work/durchlauferhitzer/durchlauferhitzer/fuzz/artifacts/request_head/'; Test unit written to /home/runner/work/durchlauferhitzer/durchlauferhitzer/fuzz/artifacts/request_head/crash-6b1f0c1f0e5a3cbbd0f3a0e4a7b2c9d8e1f2a3b4
Base64: R0UB
Error: Fuzz target exited with exit status: 77
"""

FUZZ_OOM_LOG = """\
INFO: Seed: 12
==3011== ERROR: libFuzzer: out-of-memory (malloc(67108865))
   To change the out-of-memory limit use -rss_limit_mb=<N>
SUMMARY: libFuzzer: out-of-memory
artifact_prefix='/w/fuzz/artifacts/chunked/'; Test unit written to /w/fuzz/artifacts/chunked/oom-a1b2c3
Base64: AAAA
"""

FUZZ_TIMEOUT_LOG = """\
ALARM: working on the last Unit for 11 seconds
       and the timeout value is 10 (use -timeout=N to change)
==3120== ERROR: libFuzzer: timeout after 11 seconds
SUMMARY: libFuzzer: timeout
artifact_prefix='/w/fuzz/artifacts/config/'; Test unit written to /w/fuzz/artifacts/config/timeout-c0ffee
"""

FUZZ_CLEAN_LOG = """\
INFO: Seed: 99
Done 123456 runs in 601 second(s)
"""

DEBIAN_LOG = """\
     Running unittests src/main.rs (/tmp/target/debug/deps/durchlauferhitzer-1a2b)

running 3 tests
test config::tests::parses ... ok

test result: ok. 3 passed; 0 failed
     Running tests/containment.rs (/tmp/target/debug/deps/containment-0f3a9c2b1d4e5f60)

running 2 tests
test landlock_denies_outside_root ... ok

thread 'seccomp_kill_on_unlisted_call' (4242) panicked at driver/tests/containment.rs:211:5:
server exited with signal 31 (SIGSYS) after 37 ms
test seccomp_kill_on_unlisted_call ... FAILED

failures:
    seccomp_kill_on_unlisted_call

test result: FAILED. 1 passed; 1 failed
error: test failed, to rerun pass `-p durchlauferhitzer --test containment`
"""


class Normalization(unittest.TestCase):
    def test_numbers_become_n(self):
        self.assertEqual(
            nr.normalize_message("conn 17 got 503 after 12ms (step 88)"),
            "conn N got N after Nms (step N)",
        )

    def test_identifiers_keep_their_digits(self):
        self.assertEqual(
            nr.normalize_message("u64 h2 sha256 at 0x7ffd1234 x[3]"),
            "u64 h2 sha256 at 0xN x[N]",
        )

    def test_whitespace_collapses(self):
        self.assertEqual(nr.normalize_message("  a \t b  "), "a b")

    def test_location_drops_column_and_workspace(self):
        self.assertEqual(
            nr.normalize_location(WS + "/core/src/http/request.rs:312:17", WS),
            "core/src/http/request.rs:312",
        )
        self.assertEqual(
            nr.normalize_location("sim/tests/faults/run.rs:336:9", WS),
            "sim/tests/faults/run.rs:336",
        )

    def test_location_strips_registry_and_rustc_prefixes(self):
        self.assertEqual(
            nr.normalize_location(
                "/home/runner/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/x-1.0/src/a.rs:5:1",
                WS,
            ),
            "registry/x-1.0/src/a.rs:5",
        )
        self.assertEqual(
            nr.normalize_location("/rustc/8bab26f4f68e/library/core/src/str.rs:9:2", WS),
            "rustc/library/core/src/str.rs:9",
        )

    def test_fingerprint_is_fp_and_8_hex_of_sha256(self):
        import hashlib

        want = "fp-" + hashlib.sha256(b"abc").hexdigest()[:8]
        self.assertEqual(nr.fingerprint("abc"), want)


class LongRuns(unittest.TestCase):
    def test_sim_panic_takes_the_inner_location_and_the_seed(self):
        (f,) = nr.record_long_runs(SIM_LOG, WS)
        self.assertEqual(f["kind"], "panic")
        self.assertEqual(f["test"], "random_fault_schedules")
        self.assertEqual(f["location"], "sim/tests/faults/static_net.rs:412")
        self.assertEqual(f["message"], "conn 17 got 503 after 12ms, expected 200 (step 88)")
        self.assertEqual(f["seed"], "48213")
        self.assertIn("timer order OnTime, shutdown Drain { drain: 31 }", f["panic_text"])
        self.assertIn("SIM_SEED=48213 ", f["replay"])
        self.assertIn("--test faults -- random_fault_schedules --exact", f["replay"])

    def test_seeds_and_ids_normalize_away(self):
        (a,) = nr.record_long_runs(SIM_LOG, WS)
        (b,) = nr.record_long_runs(SIM_LOG_OTHER_SEED, WS)
        self.assertNotEqual(a["seed"], b["seed"])
        self.assertEqual(nr.describe("long-runs", a), nr.describe("long-runs", b))

    def test_title(self):
        (f,) = nr.record_long_runs(SIM_LOG, WS)
        fp, title = nr.describe("long-runs", f)
        self.assertRegex(fp, r"^fp-[0-9a-f]{8}$")
        self.assertEqual(
            title, f"nightly: long runs — panic at sim/tests/faults/static_net.rs:412 [{fp}]"
        )

    def test_an_unwrapped_panic_is_its_own_failure(self):
        (f,) = nr.record_long_runs(SIM_COVERAGE_LOG, WS)
        self.assertEqual(f["location"], "sim/tests/faults/proxy.rs:930")
        self.assertEqual(f["seed"], "")
        self.assertNotIn("SIM_SEED=", f["replay"])

    def test_a_log_without_a_failure_is_unrecognized(self):
        (f,) = nr.record_long_runs("error[E0425]: cannot find value `x`\n", WS)
        self.assertEqual(f["kind"], "unrecognized")
        fp, title = nr.describe("long-runs", f)
        self.assertEqual(title, f"nightly: long runs — failed without a recognized failure [{fp}]")


class Fuzz(unittest.TestCase):
    def test_panic(self):
        (f,) = nr.record_fuzz({"request_head": FUZZ_PANIC_LOG}, WS, "nightly-2026-10-31")
        self.assertEqual(f["kind"], "panic")
        self.assertEqual(f["test"], "request_head")
        self.assertEqual(f["location"], "core/src/http/request.rs:312")
        self.assertEqual(f["input"], "crash-6b1f0c1f0e5a3cbbd0f3a0e4a7b2c9d8e1f2a3b4")
        self.assertEqual(
            f["replay"],
            "cd fuzz && cargo +nightly-2026-10-31 fuzz run --debug-assertions request_head "
            "artifacts/request_head/crash-6b1f0c1f0e5a3cbbd0f3a0e4a7b2c9d8e1f2a3b4",
        )
        fp, title = nr.describe("fuzz", f)
        self.assertEqual(
            title, f"nightly: fuzz request_head — panic at core/src/http/request.rs:312 [{fp}]"
        )

    def test_panic_numbers_normalize_away(self):
        other = FUZZ_PANIC_LOG.replace("len is 4", "len is 0").replace("index is 9", "index is 1")
        other = other.replace("crash-6b1f", "crash-0000")
        (a,) = nr.record_fuzz({"request_head": FUZZ_PANIC_LOG}, WS, "n")
        (b,) = nr.record_fuzz({"request_head": other}, WS, "n")
        self.assertEqual(nr.describe("fuzz", a)[0], nr.describe("fuzz", b)[0])

    def test_the_target_is_part_of_the_fingerprint(self):
        (a,) = nr.record_fuzz({"request_head": FUZZ_PANIC_LOG}, WS, "n")
        (b,) = nr.record_fuzz({"response_head": FUZZ_PANIC_LOG}, WS, "n")
        self.assertNotEqual(nr.describe("fuzz", a)[0], nr.describe("fuzz", b)[0])

    def test_oom(self):
        (f,) = nr.record_fuzz({"chunked": FUZZ_OOM_LOG}, WS, "n")
        self.assertEqual((f["kind"], f["input"]), ("oom", "oom-a1b2c3"))
        self.assertTrue(nr.describe("fuzz", f)[1].startswith("nightly: fuzz chunked — out of memory [fp-"))

    def test_timeout(self):
        (f,) = nr.record_fuzz({"config": FUZZ_TIMEOUT_LOG}, WS, "n")
        self.assertEqual((f["kind"], f["input"]), ("timeout", "timeout-c0ffee"))
        self.assertTrue(nr.describe("fuzz", f)[1].startswith("nightly: fuzz config — timeout [fp-"))

    def test_clean_targets_are_skipped(self):
        failures = nr.record_fuzz(
            {"config": FUZZ_CLEAN_LOG, "chunked": FUZZ_OOM_LOG}, WS, "n"
        )
        self.assertEqual([f["test"] for f in failures], ["chunked"])


class OlderGlibc(unittest.TestCase):
    def test_the_failing_test_name(self):
        (f,) = nr.record_driver_tests(DEBIAN_LOG, WS)
        self.assertEqual(f["kind"], "test")
        self.assertEqual(f["test"], "containment::seccomp_kill_on_unlisted_call")
        self.assertIn("server exited with signal 31", f["panic_text"])
        self.assertIn(
            "cargo test -p durchlauferhitzer --locked --test containment -- "
            "seccomp_kill_on_unlisted_call --exact",
            f["replay"],
        )
        self.assertIn("DURCHLAUFERHITZER_REQUIRE_MIGRATE=1", f["replay"])
        fp, title = nr.describe("older-glibc", f)
        self.assertEqual(
            title,
            f"nightly: older glibc — containment::seccomp_kill_on_unlisted_call failed [{fp}]",
        )

    def test_only_the_name_counts(self):
        (a,) = nr.record_driver_tests(DEBIAN_LOG, WS)
        (b,) = nr.record_driver_tests(DEBIAN_LOG.replace("211:5", "300:1").replace("37 ms", "5 ms"), WS)
        self.assertEqual(nr.describe("older-glibc", a), nr.describe("older-glibc", b))


class Pressure(unittest.TestCase):
    def test_filed_as_pressure_with_its_own_replay(self):
        (f,) = nr.record_driver_tests(DEBIAN_LOG, WS, nr.PRESSURE_ENV, nr.PRESSURE_WHERE)
        self.assertIn("DURCHLAUFERHITZER_TEST_SECCOMP_KILL=1 cargo test", f["replay"])
        self.assertNotIn("REQUIRE_ROOT", f["replay"])
        fp, title = nr.describe("pressure", f)
        self.assertTrue(title.startswith("nightly: pressure — containment::"))
        self.assertNotEqual(fp, nr.describe("older-glibc", f)[0])
        self.assertEqual(nr.parse_leg("pressure (x86_64)"), ("pressure", "x86_64", ""))


# The restart soak's log: the loop's run markers, then each run of the
# restart test binary (`--nocapture`); one test fails in two of three runs.
SOAK_LOG = """\
== restart soak run 1 of 3
running 2 tests
test reload_under_load_loses_nothing ... ok
test the_scripts_parse ... ok

test result: ok. 2 passed; 0 failed
== restart soak run 2 of 3
running 2 tests

thread 'reload_under_load_loses_nothing' (811) panicked at driver/tests/restart.rs:140:5:
lost 1 of 4000 connections (conn 2817: reset)
test reload_under_load_loses_nothing ... FAILED
test the_scripts_parse ... ok

failures:
    reload_under_load_loses_nothing

test result: FAILED. 1 passed; 1 failed
== restart soak run 3 of 3
running 2 tests

thread 'reload_under_load_loses_nothing' (902) panicked at driver/tests/restart.rs:140:5:
lost 2 of 4000 connections (conn 12: reset)
test reload_under_load_loses_nothing ... FAILED
test the_scripts_parse ... ok

failures:
    reload_under_load_loses_nothing

test result: FAILED. 1 passed; 1 failed
"""


class RestartSoak(unittest.TestCase):
    def test_one_failure_per_test_with_its_count(self):
        (f,) = nr.record_restart_soak(SOAK_LOG, WS, "arm64", "musl")
        self.assertEqual(f["kind"], "test")
        self.assertEqual(f["test"], "restart::reload_under_load_loses_nothing")
        self.assertEqual((f["arch"], f["libc"]), ("arm64", "musl"))
        self.assertIn("failed in 2 of 3 runs", f["panic_text"])
        self.assertIn("lost 2 of 4000 connections", f["panic_text"])
        self.assertIn(
            "cargo test -p durchlauferhitzer --locked --test restart -- "
            "reload_under_load_loses_nothing --exact",
            f["replay"],
        )
        self.assertIn("DURCHLAUFERHITZER_REQUIRE_MIGRATE=1", f["replay"])
        self.assertIn("alpine", f["replay"])
        fp, title = nr.describe("restart-soak", f)
        self.assertEqual(
            fp, nr.fingerprint("restart-soak|arm64|musl|restart::reload_under_load_loses_nothing")
        )
        self.assertEqual(
            title,
            f"nightly: restart soak (arm64, musl) — restart::reload_under_load_loses_nothing "
            f"failed [{fp}]",
        )

    def test_the_fingerprint_keys_on_arch_libc_and_name_only(self):
        (a,) = nr.record_restart_soak(SOAK_LOG, WS, "x86_64", "glibc")
        (b,) = nr.record_restart_soak(SOAK_LOG.replace("140:5", "99:1").replace("conn 12", "conn 7"), WS, "x86_64", "glibc")
        (c,) = nr.record_restart_soak(SOAK_LOG, WS, "x86_64", "musl")
        self.assertEqual(nr.describe("restart-soak", a), nr.describe("restart-soak", b))
        self.assertNotEqual(nr.describe("restart-soak", a)[0], nr.describe("restart-soak", c)[0])
        self.assertIn("sudo", a["replay"])

    def test_the_fingerprint_ignores_the_run_counts_and_keys_on_arch(self):
        (a,) = nr.record_restart_soak(SOAK_LOG, WS, "x86_64", "glibc")
        # 20 runs instead of 3, and the test failing in one run, not two.
        fewer = SOAK_LOG.replace(" of 3", " of 20").split("== restart soak run 3")[0]
        (b,) = nr.record_restart_soak(fewer, WS, "x86_64", "glibc")
        self.assertIn("failed in 2 of 3 runs", a["panic_text"])
        self.assertIn("failed in 1 of 20 runs", b["panic_text"])
        self.assertEqual(nr.describe("restart-soak", a), nr.describe("restart-soak", b))
        (c,) = nr.record_restart_soak(SOAK_LOG, WS, "arm64", "glibc")
        self.assertNotEqual(nr.describe("restart-soak", a)[0], nr.describe("restart-soak", c)[0])

    def test_a_clean_or_missing_log_is_unrecognized(self):
        clean = SOAK_LOG.split("== restart soak run 2")[0]
        self.assertEqual(nr.record_restart_soak(clean, WS, "x86_64", "glibc")[0]["kind"], "unrecognized")
        self.assertEqual(nr.record_restart_soak(None, WS, "x86_64", "glibc")[0]["kind"], "unrecognized")

    def test_cli_record(self):
        with tempfile.TemporaryDirectory() as d:
            log = os.path.join(d, "soak.log")
            with open(log, "w") as fh:
                fh.write(SOAK_LOG)
            out = os.path.join(d, "failure.json")
            with contextlib.redirect_stdout(io.StringIO()):
                nr.main(["record", "--job", "restart-soak", "--arch", "x86_64", "--libc", "musl",
                         "--run-url", RUN, "--workspace", WS, "--out", out, log])
            (rec,) = nr.load_records(d)
            self.assertEqual(rec["job"], "restart-soak")
            self.assertEqual(rec["libc"], "musl")
            self.assertEqual(rec["failures"][0]["libc"], "musl")


# The run's jobs as the Actions API lists them (`jobs` of
# GET /repos/{repo}/actions/runs/{id}/jobs), trimmed to what is read.
def _legs(*pairs):
    return {"jobs": [{"name": n, "conclusion": c} for n, c in pairs]}


BENCH_LOG = """\
bench: run 1 of 3
bench: static-small requests=81234 errors=0 cpu_us_per_req=11.2 syscalls_per_req=4.1
bench failed: scenario=proxy-fresh reason=errors (17 errors in run 1)
bench: run 2 of 3
bench failed: scenario=proxy-fresh reason=errors (9 errors in run 2)
bench failed: scenario=static-large reason=strace (strace never attached)
"""


class Bench(unittest.TestCase):
    def test_one_failure_per_scenario_and_reason(self):
        fs = nr.record_bench(BENCH_LOG)
        self.assertEqual([(f["test"], f["detail"]) for f in fs],
                         [("proxy-fresh", "errors"), ("static-large", "strace")])
        self.assertIn("17 errors in run 1", fs[0]["panic_text"])
        self.assertIn("9 errors in run 2", fs[0]["panic_text"])
        self.assertIn("--scenarios proxy-fresh", fs[0]["replay"])

    def test_the_fingerprint_is_bench_scenario_reason(self):
        f = nr.record_bench(BENCH_LOG)[0]
        fp, title = nr.describe("bench", f)
        self.assertEqual(fp, nr.fingerprint("bench|proxy-fresh|errors"))
        self.assertEqual(title, f"nightly: bench proxy-fresh — errors [{fp}]")
        # The counts in the detail never reach the fingerprint.
        other = nr.record_bench(BENCH_LOG.replace("17 errors", "3 errors"))[0]
        self.assertEqual(nr.describe("bench", other)[0], fp)

    def test_untrusted_scenario_and_reason_stay_plain(self):
        log = "bench failed: scenario=x[y](https://evil) reason=@someone\n"
        (f,) = nr.record_bench(log)
        _, title = nr.describe("bench", f)
        self.assertNotIn("https://", title)
        self.assertNotIn("@", title)
        self.assertNotIn("[y]", title)

    def test_a_clean_or_missing_log_is_unrecognized(self):
        self.assertEqual(nr.record_bench("bench: run 1 of 1\n")[0]["kind"], "unrecognized")
        self.assertEqual(nr.record_bench(None)[0]["kind"], "unrecognized")

    def test_cli_record(self):
        with tempfile.TemporaryDirectory() as d:
            log = os.path.join(d, "bench.log")
            with open(log, "w") as fh:
                fh.write(BENCH_LOG)
            out = os.path.join(d, "failure.json")
            with contextlib.redirect_stdout(io.StringIO()):
                nr.main(["record", "--job", "bench", "--arch", "x86_64", "--run-url", RUN,
                         "--out", out, log])
            with open(out) as fh:
                rec = json.load(fh)
            self.assertEqual(rec["job"], "bench")
            self.assertEqual(len(rec["failures"]), 2)

    def test_legs(self):
        self.assertEqual(nr.parse_leg("bench"), ("bench", "x86_64", ""))
        self.assertEqual(nr.parse_leg("bench publish"), ("bench-publish", "x86_64", ""))
        (rec,) = nr.add_missing([], {}, RUN, _legs(("bench publish", "failure")))
        fp, _ = nr.describe("bench-publish", rec["failures"][0])
        self.assertEqual(fp, nr.fingerprint("bench-publish|unrecognized"))


class Legs(unittest.TestCase):
    """A matrix leg that timed out or was cancelled is reported even when
    a sibling leg of the same job wrote a record (`needs` gives one result
    per job, so the legs come from the run's jobs)."""

    def test_leg_names_map_to_job_arch_and_libc(self):
        self.assertEqual(nr.parse_leg("long runs (arm64)"), ("long-runs", "arm64", ""))
        self.assertEqual(nr.parse_leg("fuzz"), ("fuzz", "x86_64", ""))
        self.assertEqual(nr.parse_leg("older glibc (debian 12)"), ("older-glibc", "x86_64", ""))
        self.assertEqual(
            nr.parse_leg("restart soak (arm64, musl)"), ("restart-soak", "arm64", "musl")
        )
        self.assertIsNone(nr.parse_leg("report failures"))
        self.assertIsNone(nr.parse_leg("restart soak (arm64)"))

    def test_a_cancelled_leg_beside_a_failed_sibling_with_a_record(self):
        rec = nr.make_record("restart-soak", "x86_64", RUN,
                             nr.record_restart_soak(SOAK_LOG, WS, "x86_64", "glibc"), libc="glibc")
        needs = {"restart-soak": {"result": "failure"}, "fuzz": {"result": "success"}}
        legs = _legs(
            ("restart soak (x86_64, glibc)", "failure"),
            ("restart soak (arm64, musl)", "cancelled"),
            ("restart soak (x86_64, musl)", "success"),
            ("fuzz", "success"),
            ("report failures", None),
        )
        out = nr.add_missing([rec], needs, RUN, legs)
        self.assertEqual(len(out), 2)
        added = out[1]
        self.assertEqual((added["job"], added["arch"], added["libc"]), ("restart-soak", "arm64", "musl"))
        self.assertEqual(added["failures"][0]["kind"], "timeout-or-cancelled")
        fp, title = nr.describe("restart-soak", added["failures"][0])
        self.assertEqual(fp, nr.fingerprint("restart-soak|timeout-or-cancelled"))
        self.assertEqual(
            title,
            "nightly: restart soak (arm64, musl) — timed out or was cancelled, "
            f"no failure record [{fp}]",
        )
        occurrence = {"job": added["job"], "arch": added["arch"], "failure": added["failures"][0]}
        self.assertIn("restart soak musl (arm64)", nr._occurrence(occurrence, RUN))
        # Without the leg (from `needs` alone): the same fingerprint, no
        # parentheses in the title.
        (bare,) = nr.add_missing([], {"restart-soak": {"result": "cancelled"}}, RUN)
        fp_bare, title_bare = nr.describe("restart-soak", bare["failures"][0])
        self.assertEqual(fp_bare, fp)
        self.assertTrue(title_bare.startswith("nightly: restart soak — "), title_bare)

    def test_a_failed_leg_without_a_record_and_timed_out(self):
        rec = nr.make_record("long-runs", "x86_64", RUN, [nr._unrecognized("x")])
        legs = _legs(("long runs (x86_64)", "failure"), ("long runs (arm64)", "timed_out"),
                     ("older glibc (debian 12)", "failure"))
        out = nr.add_missing([rec], {}, RUN, legs)
        self.assertEqual([(r["job"], r["arch"]) for r in out],
                         [("long-runs", "x86_64"), ("long-runs", "arm64"), ("older-glibc", "x86_64")])
        self.assertEqual(out[1]["failures"][0]["kind"], "timeout-or-cancelled")
        self.assertEqual(out[2]["failures"][0]["kind"], "unrecognized")

    def test_a_job_the_legs_miss_falls_back_to_needs(self):
        # The jobs listing failed or lacks the job: `needs` still files it.
        out = nr.add_missing([], {"fuzz": {"result": "cancelled"}}, RUN, _legs())
        self.assertEqual([r["job"] for r in out], ["fuzz"])
        out = nr.add_missing([], {"fuzz": {"result": "cancelled"}}, RUN, None)
        self.assertEqual([r["job"] for r in out], ["fuzz"])

    def test_malformed_legs_are_ignored(self):
        for legs in ({"jobs": "x"}, [], {"jobs": [1, {"name": 3}, {"conclusion": "failure"}]}):
            self.assertEqual(nr.add_missing([], {}, RUN, legs), [])

    def test_cli_reads_the_jobs_file(self):
        with tempfile.TemporaryDirectory() as d:
            jobs = os.path.join(d, "jobs.json")
            with open(jobs, "w") as fh:
                json.dump(_legs(("restart soak (arm64, musl)", "cancelled")), fh)
            needs = os.path.join(d, "needs.json")
            with open(needs, "w") as fh:
                json.dump({"restart-soak": {"result": "failure"}}, fh)
            seen = []
            orig = nr.report
            nr.report = lambda records, gh, date, run_url: seen.append(records) or ""
            try:
                with contextlib.redirect_stdout(io.StringIO()):
                    nr.main(["report", "--records", os.path.join(d, "none"), "--needs", needs,
                             "--jobs", jobs, "--run-url", RUN, "--summary", ""])
            finally:
                nr.report = orig
            (records,) = seen
            self.assertEqual([(r["job"], r["arch"], r["libc"]) for r in records],
                             [("restart-soak", "arm64", "musl")])


def _record(job, arch, failures):
    return {"version": 1, "job": job, "arch": arch, "run_url": RUN, "failures": failures}


class Decision(unittest.TestCase):
    def groups(self, *logs):
        records = [_record("long-runs", "x86_64", nr.record_long_runs(log, WS)) for log in logs]
        return nr.group(records)

    def test_none_creates(self):
        (g,) = self.groups(SIM_LOG)
        (a,) = nr.decide([g], [])
        self.assertEqual((a["op"], a["number"]), ("create", None))

    def test_open_comments(self):
        (g,) = self.groups(SIM_LOG)
        issues = [{"number": 7, "state": "OPEN", "title": "nightly: x [%s]" % g["fp"]}]
        (a,) = nr.decide([g], issues)
        self.assertEqual((a["op"], a["number"]), ("comment", 7))

    def test_closed_reopens(self):
        (g,) = self.groups(SIM_LOG)
        issues = [{"number": 9, "state": "CLOSED", "title": "nightly: x [%s]" % g["fp"]}]
        (a,) = nr.decide([g], issues)
        self.assertEqual((a["op"], a["number"]), ("reopen", 9))

    def test_an_open_issue_wins_over_a_closed_one(self):
        (g,) = self.groups(SIM_LOG)
        issues = [
            {"number": 9, "state": "CLOSED", "title": "a [%s]" % g["fp"]},
            {"number": 12, "state": "OPEN", "title": "b [%s]" % g["fp"]},
        ]
        self.assertEqual(nr.decide([g], issues)[0]["number"], 12)

    def test_other_fingerprints_do_not_match(self):
        (g,) = self.groups(SIM_LOG)
        issues = [{"number": 7, "state": "OPEN", "title": "nightly: x [fp-00000000]"}]
        self.assertEqual(nr.decide([g], issues)[0]["op"], "create")

    def test_one_group_per_fingerprint(self):
        records = [
            _record("long-runs", "x86_64", nr.record_long_runs(SIM_LOG, WS)),
            _record("long-runs", "arm64", nr.record_long_runs(SIM_LOG_OTHER_SEED, WS)),
        ]
        (g,) = nr.group(records)
        self.assertEqual([o["arch"] for o in g["occurrences"]], ["x86_64", "arm64"])

    def test_at_most_three_new_issues(self):
        groups = [
            {"fp": "fp-%08d" % i, "title": "t%d" % i, "occurrences": []} for i in range(5)
        ]
        issues = [{"number": 1, "state": "OPEN", "title": "x [fp-00000004]"}]
        ops = [a["op"] for a in nr.decide(groups, issues)]
        self.assertEqual(ops, ["create", "create", "create", "skip", "comment"])


class Apply(unittest.TestCase):
    """The gh calls, against a fake gh that records argv lists."""

    def run_report(self, records, issues):
        calls = []

        def gh(args, body=None):
            calls.append((args, body))
            if args[:2] == ["issue", "list"]:
                return json.dumps(issues)
            return ""

        summary = nr.report(records, gh, "2026-11-02", RUN)
        return calls, summary

    def test_create_makes_the_label_and_the_issue(self):
        rec = _record("long-runs", "x86_64", nr.record_long_runs(SIM_LOG, WS))
        calls, summary = self.run_report([rec], [])
        argv = [c[0] for c in calls]
        self.assertEqual(argv[0][:2], ["issue", "list"])
        # Idempotent: `--force` creates the label or updates it; no list.
        self.assertIn(["label", "create", "nightly", "--color", "B60205",
                       "--description", "A failing nightly job (NFR-TEST-08)", "--force"], argv)
        self.assertFalse([a for a in argv if a[:2] == ["label", "list"]])
        create = [c for c in calls if c[0][:2] == ["issue", "create"]]
        self.assertEqual(len(create), 1)
        args, body = create[0]
        self.assertIn("--label", args)
        self.assertIn("nightly", args)
        self.assertIn("long runs (x86_64)", body)
        self.assertIn(RUN, body)
        self.assertIn("SIM_SEED=48213", body)
        self.assertIn("conn 17 got 503", body)
        self.assertIn("created", summary)

    def test_no_label_call_without_a_new_issue(self):
        rec = _record("long-runs", "x86_64", nr.record_long_runs(SIM_LOG, WS))
        fp = nr.describe("long-runs", rec["failures"][0])[0]
        calls, _ = self.run_report([rec], [{"number": 5, "state": "OPEN", "title": "t [%s]" % fp}])
        self.assertFalse([c for c in calls if c[0][0] == "label"])

    def test_comment_and_reopen(self):
        rec = _record("long-runs", "x86_64", nr.record_long_runs(SIM_LOG, WS))
        fp = nr.describe("long-runs", rec["failures"][0])[0]
        calls, _ = self.run_report([rec], [{"number": 5, "state": "OPEN", "title": "t [%s]" % fp}])
        (c,) = [c for c in calls if c[0][0] == "issue" and c[0][1] != "list"]
        self.assertEqual(c[0][:3], ["issue", "comment", "5"])
        self.assertIn("2026-11-02", c[1])
        self.assertIn(RUN, c[1])
        self.assertIn("SIM_SEED=48213", c[1])

        calls, _ = self.run_report([rec], [{"number": 5, "state": "CLOSED", "title": "t [%s]" % fp}])
        ops = [c for c in calls if c[0][0] == "issue" and c[0][1] != "list"]
        self.assertEqual([c[0][:3] for c in ops], [["issue", "reopen", "5"], ["issue", "comment", "5"]])
        self.assertIn("Regressed", ops[1][1])

    def test_skipped_ones_go_to_the_summary(self):
        recs = []
        for i in range(4):
            log = DEBIAN_LOG.replace("seccomp_kill_on_unlisted_call", "case_%s" % "abcd"[i])
            recs.append(_record("older-glibc", "x86_64", nr.record_driver_tests(log, WS)))
        calls, summary = self.run_report(recs, [])
        self.assertEqual(len([c for c in calls if c[0][:2] == ["issue", "create"]]), 3)
        self.assertIn("not filed", summary)
        self.assertIn("containment::case_d", summary)


EVIL = "@evil [x](https://evil.example/p) www.evil.example **b** __u__ _it_ <img src=x>"


def _outside_code(body):
    """`body` without its fenced blocks and inline code spans."""
    body = re.sub(r"(?ms)^(`{3,})\n.*?\n\1$", "", body)
    return re.sub(r"`[^`\n]*`", "", body)


def assert_inert(case, text):
    """No mention, link or markup from a record outside code: the only
    link is the reporting run's."""
    case.assertNotIn("evil.example/runs", text)
    outside = _outside_code(text).replace(RUN, "")
    for bad in ("@evil", "](", "<img", "http", "www.", "__", "**"):
        case.assertNotIn(bad, outside)
    case.assertNotRegex(outside, r"(?<!\w)_\w+_(?!\w)")


class Untrusted(unittest.TestCase):
    """Records come from jobs that run code under test: no field of one
    may put a mention, link or markup into an issue, and the run link is
    the reporting run's own."""

    run_report = Apply.run_report

    def records(self):
        failure = nr._failure(
            kind="crash",
            test="request_head" + EVIL,
            detail=EVIL,
            panic_text=EVIL,
            seed="7" + EVIL,
            input="crash-1" + EVIL,
            replay=EVIL,
        )
        rec = {"version": 1, "job": "fuzz", "arch": "x86_64" + EVIL,
               "run_url": "https://evil.example/runs/1", "failures": [failure]}
        return [rec]

    def bodies(self, calls):
        return [b for _, b in calls if b is not None]

    def test_create(self):
        calls, summary = self.run_report(self.records(), [])
        (create,) = [c for c in calls if c[0][:2] == ["issue", "create"]]
        title = create[0][create[0].index("--title") + 1]
        for text in [title, summary] + self.bodies(calls):
            assert_inert(self, text)
        self.assertIn(RUN, create[1])

    def test_comment_and_reopen(self):
        fp = nr.describe("fuzz", self.records()[0]["failures"][0])[0]
        for state in ("OPEN", "CLOSED"):
            calls, _ = self.run_report(self.records(), [{"number": 5, "state": state, "title": "t [%s]" % fp}])
            bodies = self.bodies(calls)
            self.assertTrue(bodies)
            for body in bodies:
                self.assertIn(RUN, body)
                assert_inert(self, body)

    def test_skipped_summary_links_the_reporting_run(self):
        (rec,) = self.records()
        recs = [dict(rec, failures=[dict(rec["failures"][0], detail=str(i))]) for i in range(4)]
        _, summary = self.run_report(recs, [])
        self.assertIn("not filed", summary)
        self.assertIn(RUN, summary)
        assert_inert(self, summary)

    def test_bodies_fit_github_and_later_actions_run(self):
        # GitHub refuses a body over 65536 characters; a failed `gh`
        # would end the run before the later actions.
        huge = "x" * 200_000
        big = nr._failure(kind="crash", test="request_head", detail="big",
                          panic_text=huge, replay=huge, seed="1" * 1000, input=huge)
        small = nr._failure(kind="crash", test="request_head", detail="small")
        rec = {"version": 1, "job": "fuzz", "arch": "x86_64", "run_url": RUN,
               "failures": [big] * 100 + [small]}
        for issues in ([], [{"number": 5, "state": "OPEN",
                             "title": "t [%s]" % nr.describe("fuzz", big)[0]}]):
            calls, _ = self.run_report([rec], issues)
            bodies = [b for _, b in calls if b is not None]
            self.assertEqual(len(bodies), 2)
            for body in bodies:
                self.assertLessEqual(len(body), 65536)
                self.assertIn(RUN, body)


class GhRunner(unittest.TestCase):
    """`_gh` runs `gh` with the body on stdin, or no stdin at all."""

    def run_gh(self, body):
        seen = {}

        def fake_run(argv, **kwargs):
            seen.update(kwargs, argv=argv)
            return nr.subprocess.CompletedProcess(argv, 0, stdout="out", stderr="")

        real = nr.subprocess.run
        nr.subprocess.run = fake_run
        try:
            out = nr._gh(["issue", "list"], body=body)
        finally:
            nr.subprocess.run = real
        self.assertEqual(out, "out")
        return seen

    def test_without_a_body_stdin_is_devnull(self):
        seen = self.run_gh(None)
        self.assertEqual(seen["argv"], ["gh", "issue", "list"])
        self.assertIs(seen.get("stdin"), nr.subprocess.DEVNULL)
        self.assertNotIn("input", seen)

    def test_a_body_goes_to_stdin(self):
        seen = self.run_gh("text")
        self.assertEqual(seen.get("input"), "text")
        self.assertNotIn("stdin", seen)


class Bodies(unittest.TestCase):
    def test_a_fence_longer_than_any_backtick_run(self):
        text = "a ```` b"
        body = nr.fenced(text)
        self.assertTrue(body.startswith("`````\n"))
        self.assertTrue(body.endswith("\n`````"))

    def test_panic_text_is_capped(self):
        log = SIM_COVERAGE_LOG.replace(
            "coverage: only 3 of 40 reset cases reached",
            "coverage\n" + "x" * 10000,
        )
        (f,) = nr.record_long_runs(log, WS)
        self.assertLessEqual(len(f["panic_text"]), nr.MAX_TEXT)


class Cli(unittest.TestCase):
    def test_record_writes_json_and_missing_jobs_are_synthesized(self):
        with tempfile.TemporaryDirectory() as d:
            log = os.path.join(d, "long-runs.log")
            with open(log, "w") as fh:
                fh.write(SIM_LOG)
            out = os.path.join(d, "rec", "failure.json")
            os.makedirs(os.path.dirname(out))
            with contextlib.redirect_stdout(io.StringIO()):
                nr.main(["record", "--job", "long-runs", "--arch", "arm64", "--run-url", RUN,
                         "--workspace", WS, "--out", out, log])
            with open(out) as fh:
                rec = json.load(fh)
            self.assertEqual((rec["job"], rec["arch"], rec["run_url"]), ("long-runs", "arm64", RUN))
            self.assertEqual(rec["failures"][0]["seed"], "48213")

            records = nr.load_records(d)
            needs = {
                "long-runs": {"result": "failure"},
                "fuzz": {"result": "failure"},
                "older-glibc": {"result": "success"},
            }
            records = nr.add_missing(records, needs, RUN)
            self.assertEqual([r["job"] for r in records], ["long-runs", "fuzz"])
            self.assertEqual(records[1]["failures"][0]["kind"], "unrecognized")

    def test_a_cancelled_job_without_a_record_is_timeout_or_cancelled(self):
        # A job ended by `timeout-minutes` (or cancelled by hand) concludes
        # `cancelled` and writes no record (controller ruling 2026-11-02).
        records = [nr.make_record("long-runs", "x86_64", RUN, [nr._unrecognized("x")])]
        needs = {
            "long-runs": {"result": "cancelled"},
            "fuzz": {"result": "cancelled"},
            "older-glibc": {"result": "skipped"},
        }
        records = nr.add_missing(records, needs, RUN)
        self.assertEqual([r["job"] for r in records], ["long-runs", "fuzz"])
        f = records[1]["failures"][0]
        self.assertEqual(f["kind"], "timeout-or-cancelled")
        fp, title = nr.describe("fuzz", f)
        self.assertEqual(fp, nr.fingerprint("fuzz|timeout-or-cancelled"))
        self.assertEqual(
            title, f"nightly: fuzz — timed out or was cancelled, no failure record [{fp}]"
        )
        groups = nr.group(records)
        self.assertEqual(len(groups), 2)

    def test_only_cancelled_jobs_and_no_records_still_file(self):
        # download-artifact found nothing: the records directory is
        # missing or empty; `needs` alone files the issues.
        needs = {
            "long-runs": {"result": "cancelled"},
            "fuzz": {"result": "cancelled"},
            "older-glibc": {"result": "success"},
        }
        with tempfile.TemporaryDirectory() as d:
            for directory in (os.path.join(d, "missing"), d):
                records = nr.add_missing(nr.load_records(directory), needs, RUN)
                calls, _ = Apply.run_report(self, records, [])
                creates = [c for c in calls if c[0][:2] == ["issue", "create"]]
                titles = [args[args.index("--title") + 1] for args, _ in creates]
                self.assertEqual(
                    [t.rsplit(" ", 1)[1] for t in titles],
                    ["[%s]" % nr.fingerprint("long-runs|timeout-or-cancelled"),
                     "[%s]" % nr.fingerprint("fuzz|timeout-or-cancelled")],
                )
                for _, body in creates:
                    self.assertIn(RUN, body)

    def test_a_missing_log_is_unrecognized(self):
        self.assertEqual(nr.record_driver_tests(None, WS)[0]["kind"], "unrecognized")


if __name__ == "__main__":
    unittest.main()
