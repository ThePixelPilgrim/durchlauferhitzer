"""Tests for bench/bisect.sh: the exit codes `git bisect run` relies on.

A stub `cargo` on PATH stands in for the build and for the bench tool: it
succeeds or fails per STUB_BUILD_RC / STUB_BENCH_RC and writes the CSV in
STUB_CSV to the bench's --out path.
"""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "bench" / "bisect.sh"

HEADER = (
    "date,commit,runner,scenario,requests,errors,seconds,"
    "cpu_us_per_req,syscalls_per_req,rss_kib\n"
)

STUB_CARGO = r"""#!/bin/sh
# Stub cargo: `build` (the product) and `run` (the bench tool).
echo "$*" >> "$STUB_LOG"
case "$1" in
build) exit "${STUB_BUILD_RC:-0}" ;;
run)
    while [ $# -gt 0 ]; do
        [ "$1" = --out ] && out=$2
        shift
    done
    [ -n "${STUB_CSV:-}" ] && printf '%s' "$STUB_CSV" > "$out"
    exit "${STUB_BENCH_RC:-0}"
    ;;
esac
exit 99
"""


def row(syscalls, errors=0, requests=1000, scenario="static-small"):
    return HEADER + (
        f"2026-11-06,abc1234,local,{scenario},{requests},{errors},2.000,"
        f"10.0,{syscalls},4000\n"
    )


class BisectScript(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        bindir = self.tmp / "bin"
        bindir.mkdir()
        cargo = bindir / "cargo"
        cargo.write_text(STUB_CARGO)
        cargo.chmod(0o755)
        self.bindir = bindir
        self.log = self.tmp / "log"
        self.tmpdir = self.tmp / "tmpdir"
        self.tmpdir.mkdir()

    def run_script(self, args=("static-small", "25"), shell="sh", **env):
        e = {
            "PATH": f"{self.bindir}:{os.environ['PATH']}",
            "STUB_LOG": str(self.log),
            "TMPDIR": str(self.tmpdir),
        }
        e.update(env)
        return subprocess.run(
            [shell, str(SCRIPT), *args],
            env=e,
            cwd=self.tmp,
            capture_output=True,
            text=True,
        )

    def test_at_or_below_threshold_is_good(self):
        self.assertEqual(self.run_script(STUB_CSV=row("12.500")).returncode, 0)
        self.assertEqual(
            self.run_script(("static-small", "12.5"), STUB_CSV=row("12.500")).returncode,
            0,
        )

    def test_above_threshold_is_bad(self):
        self.assertEqual(self.run_script(STUB_CSV=row("25.001")).returncode, 1)

    def test_build_failure_skips_the_commit(self):
        r = self.run_script(STUB_BUILD_RC="101", STUB_CSV=row("1"))
        self.assertEqual(r.returncode, 125)
        self.assertNotIn("run ", self.log.read_text())

    def test_bench_failure_cannot_judge(self):
        r = self.run_script(STUB_BENCH_RC="1", STUB_CSV=row("1"))
        self.assertEqual(r.returncode, 125)

    def test_errors_in_the_row_cannot_judge(self):
        self.assertEqual(self.run_script(STUB_CSV=row("1", errors=3)).returncode, 125)

    def test_missing_or_empty_value_cannot_judge(self):
        self.assertEqual(self.run_script().returncode, 125)
        self.assertEqual(self.run_script(STUB_CSV=row("")).returncode, 125)
        self.assertEqual(self.run_script(STUB_CSV=row("1", requests=0)).returncode, 125)

    def test_bad_usage_aborts_the_bisect(self):
        # Not 0, 1 or 125, and below 128: `git bisect run` stops.
        for args in ((), ("static-small",), ("static-small", "many")):
            self.assertEqual(self.run_script(args).returncode, 2, args)

    def test_bench_gets_one_syscalls_only_run_of_that_scenario(self):
        self.run_script(("proxy-fresh", "5"), STUB_CSV=row("1"))
        run = [l for l in self.log.read_text().splitlines() if l.startswith("run ")]
        self.assertEqual(len(run), 1)
        for flag in ("--runs 1", "--syscalls-only", "--scenarios proxy-fresh", "--locked"):
            self.assertIn(flag, run[0])

    def test_build_is_the_locked_release_of_the_product(self):
        self.run_script(STUB_CSV=row("1"))
        build = self.log.read_text().splitlines()[0]
        self.assertEqual(build, "build --release --locked -p durchlauferhitzer")

    def test_temp_files_are_removed(self):
        self.run_script(STUB_CSV=row("1"))
        self.run_script(STUB_CSV=row("99"))
        self.run_script(STUB_BUILD_RC="1")
        self.assertEqual(list(self.tmpdir.iterdir()), [])

    @unittest.skipUnless(shutil.which("dash"), "dash not installed")
    def test_works_under_dash(self):
        self.assertEqual(self.run_script(shell="dash", STUB_CSV=row("3")).returncode, 0)
        self.assertEqual(self.run_script(shell="dash", STUB_CSV=row("30")).returncode, 1)


if __name__ == "__main__":
    unittest.main()
