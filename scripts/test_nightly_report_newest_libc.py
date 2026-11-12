"""Tests of scripts/nightly-report.py for the `newest libc` job
(NFR-TEST-08): its two legs, glibc and musl, file apart from each other
and from the older glibc job, and each replay line names its image.

Run with `python3 -m unittest discover -s scripts -p 'test_*.py'`.
"""

import contextlib
import importlib.util
import io
import json
import os
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

# A driver test killed by the seccomp filter, as `cargo test --nocapture`
# prints it.
KILL_LOG = """\
     Running tests/containment.rs (/tmp/target/debug/deps/containment-0f3a9c2b1d4e5f60)

running 2 tests
test landlock_denies_outside_root ... ok

thread 'serves_under_kill' (4242) panicked at driver/tests/containment.rs:211:5:
server exited with signal 31 (SIGSYS) after 37 ms
test serves_under_kill ... FAILED

failures:
    serves_under_kill

test result: FAILED. 1 passed; 1 failed
error: test failed, to rerun pass `-p durchlauferhitzer --test containment`
"""


def record(libc):
    """The failure record the `newest libc (<libc>)` leg writes."""
    with tempfile.TemporaryDirectory() as d:
        log = os.path.join(d, "driver-tests.log")
        with open(log, "w") as fh:
            fh.write(KILL_LOG)
        out = os.path.join(d, "failure.json")
        with contextlib.redirect_stdout(io.StringIO()):
            nr.main(["record", "--job", "newest-libc", "--arch", "x86_64", "--libc", libc,
                     "--run-url", RUN, "--workspace", WS, "--out", out, log])
        with open(out) as fh:
            return json.load(fh)


class NewestLibc(unittest.TestCase):
    def test_each_leg_records_its_libc_and_replays_in_its_image(self):
        for libc, image in (("glibc", "fedora:latest"), ("musl", "alpine:edge")):
            rec = record(libc)
            self.assertEqual((rec["job"], rec["arch"], rec["libc"]), ("newest-libc", "x86_64", libc))
            (f,) = rec["failures"]
            self.assertEqual((f["kind"], f["test"], f["libc"]),
                             ("test", "containment::serves_under_kill", libc))
            self.assertIn("SIGSYS", f["panic_text"])
            self.assertIn(
                "DURCHLAUFERHITZER_REQUIRE_ROOT=1 DURCHLAUFERHITZER_REQUIRE_MIGRATE=1 "
                "DURCHLAUFERHITZER_REQUIRE_LANDLOCK=1 DURCHLAUFERHITZER_TEST_SECCOMP_KILL=1 "
                "cargo test -p durchlauferhitzer --locked --test containment -- "
                "serves_under_kill --exact",
                f["replay"],
            )
            self.assertIn(image, f["replay"])
            fp, title = nr.describe("newest-libc", f)
            self.assertEqual(
                title, f"nightly: newest libc ({libc}) — containment::serves_under_kill failed [{fp}]"
            )

    def test_the_legs_file_apart_and_apart_from_older_glibc(self):
        (g,) = record("glibc")["failures"]
        (m,) = record("musl")["failures"]
        (old,) = nr.record_driver_tests(KILL_LOG, WS)
        fps = {nr.describe("newest-libc", g)[0], nr.describe("newest-libc", m)[0],
               nr.describe("older-glibc", old)[0]}
        self.assertEqual(len(fps), 3)

    def test_leg_names_and_occurrence_heading(self):
        self.assertEqual(nr.parse_leg("newest libc (glibc)"), ("newest-libc", "x86_64", "glibc"))
        self.assertEqual(nr.parse_leg("newest libc (musl)"), ("newest-libc", "x86_64", "musl"))
        self.assertIsNone(nr.parse_leg("newest libc"))
        rec = record("musl")
        occurrence = {"job": rec["job"], "arch": rec["arch"], "failure": rec["failures"][0]}
        self.assertIn("newest libc musl (x86_64)", nr._occurrence(occurrence, RUN))

    def test_a_cancelled_leg_beside_a_failed_sibling(self):
        rec = record("glibc")
        legs = {"jobs": [{"name": "newest libc (glibc)", "conclusion": "failure"},
                         {"name": "newest libc (musl)", "conclusion": "cancelled"}]}
        out = nr.add_missing([rec], {"newest-libc": {"result": "failure"}}, RUN, legs)
        self.assertEqual([(r["job"], r["libc"]) for r in out],
                         [("newest-libc", "glibc"), ("newest-libc", "musl")])
        f = out[1]["failures"][0]
        self.assertEqual(f["kind"], "timeout-or-cancelled")
        self.assertTrue(nr.describe("newest-libc", f)[1].startswith("nightly: newest libc (musl) — "))

    def test_every_failing_binary_is_recorded(self):
        # `--no-fail-fast`: cargo runs every test binary, so two can fail.
        log = KILL_LOG + KILL_LOG.replace("tests/containment.rs", "tests/restart/main.rs").replace(
            "serves_under_kill", "reload_loses_nothing"
        )
        tests = [f["test"] for f in nr.record_driver_tests(log, WS)]
        self.assertEqual(tests, ["containment::serves_under_kill", "restart::reload_loses_nothing"])


if __name__ == "__main__":
    unittest.main()
