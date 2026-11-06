"""Tests of scripts/bench-chart.py (NFR-PERF-08): the benchmark rows
validated on their way into results.csv, medians per date and scenario,
and the Mermaid chart page.

Run with `python3 -m unittest discover -s scripts -p 'test_*.py'`.
"""

import contextlib
import importlib.util
import io
import os
import re
import tempfile
import unittest

_HERE = os.path.dirname(os.path.abspath(__file__))
_SPEC = importlib.util.spec_from_file_location("bench_chart", os.path.join(_HERE, "bench-chart.py"))
bc = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(bc)

HEADER = "date,commit,runner,scenario,requests,errors,seconds,cpu_us_per_req,syscalls_per_req,rss_kib"


def row(date, scenario, cpu, sys_, errors=0, commit="abc1234", runner="gha-ubuntu-24.04-x86_64"):
    return f"{date},{commit},{runner},{scenario},1000,{errors},10.0,{cpu},{sys_},2048"


# Two dates x two scenarios, three runs each; one run with errors whose
# numbers would move the median if it counted.
CSV = "\n".join(
    [
        HEADER,
        row("2026-11-06", "static-small", 10.0, 20.0),
        row("2026-11-06", "static-small", 12.0, 22.0),
        row("2026-11-06", "static-small", 11.0, 21.0),
        row("2026-11-06", "static-small", 0.37, 0.37, errors=3),
        row("2026-11-06", "proxy-small", 30.0, 40.0),
        row("2026-11-06", "proxy-small", 31.0, 41.0),
        row("2026-11-07", "static-small", 9.0, 19.0, commit="def5678"),
        row("2026-11-07", "static-small", 9.5, 19.5, commit="def5678"),
        row("2026-11-07", "proxy-small", 29.0, 39.0, commit="def5678"),
    ]
) + "\n"


def blocks(md):
    """The Mermaid blocks of a page, in order."""
    return re.findall(r"```mermaid\n(.*?)```", md, re.S)


class Validation(unittest.TestCase):
    def test_a_good_row_passes(self):
        rows, dropped = bc.parse(CSV)
        self.assertEqual(dropped, 0)
        self.assertEqual(len(rows), 9)
        self.assertEqual(rows[0]["scenario"], "static-small")
        self.assertEqual(rows[0]["cpu_us_per_req"], 10.0)
        self.assertEqual(rows[3]["errors"], 3)

    def test_untrusted_fields_drop_the_row(self):
        bad = [
            row("2026-11-06", '"static-small""]\nline [1"', 1, 1),
            row("2026-11-06", "static-huge", 1, 1),
            row("2026-11-06", "static-small", 1, 1, commit="zz12345"),
            row("2026-11-06", "static-small", 1, 1, commit="ABC1234"),
            row("2026-11-06", "static-small", 1, 1, runner="<img src=x>"),
            row("2026-11-06", "static-small", 1, 1, runner="[x](https://evil)"),
            row("2026-11-06", "static-small", "nan", 1),
            row("2026-11-06", "static-small", "inf", 1),
            row("2026-11-06", "static-small", "-1", 1),
            row("2026-11-06", "static-small", "1e400", 1),
            row("2026-13-06", "static-small", 1, 1),
            row("06.11.2026", "static-small", 1, 1),
            row("2026-11-06", "static-small", 1, 1) + ",extra",
            "2026-11-06,abc1234,local,static-small,x,0,10,1,1,1",
            "2026-11-06,abc1234,local,static-small,10,-2,10,1,1,1",
        ]
        rows, dropped = bc.parse("\n".join([HEADER] + bad) + "\n")
        self.assertEqual(rows, [])
        self.assertEqual(dropped, len(bad))

    def test_empty_per_request_values_are_allowed_but_never_charted(self):
        # A run with no complete request has no per-request numbers.
        text = HEADER + "\n2026-11-06,abc1234,local,static-small,0,5,10.0,,,\n"
        rows, dropped = bc.parse(text)
        self.assertEqual(dropped, 0)
        self.assertIsNone(rows[0]["cpu_us_per_req"])
        self.assertNotIn("static-small", bc.medians(rows, "cpu_us_per_req"))

    def test_a_wrong_header_is_refused(self):
        with self.assertRaises(ValueError):
            bc.parse("date,commit\n2026-11-06,abc1234\n")


class Medians(unittest.TestCase):
    def test_medians_per_date_and_scenario_without_error_runs(self):
        rows, _ = bc.parse(CSV)
        cpu = bc.medians(rows, "cpu_us_per_req")
        self.assertEqual(cpu["static-small"], [("2026-11-06", 11.0), ("2026-11-07", 9.25)])
        self.assertEqual(cpu["proxy-small"], [("2026-11-06", 30.5), ("2026-11-07", 29.0)])


class Page(unittest.TestCase):
    def setUp(self):
        rows, _ = bc.parse(CSV)
        self.md = bc.render(rows)

    def test_one_xychart_per_metric_and_scenario(self):
        charts = blocks(self.md)
        self.assertEqual(len(charts), 4)
        for chart in charts:
            self.assertTrue(chart.startswith("xychart-beta\n"), chart)
            self.assertIn('x-axis ["2026-11-06", "2026-11-07"]', chart)
        cpu_static = next(c for c in charts if "static-small" in c and "CPU" in c)
        self.assertIn("line [11.00, 9.25]", cpu_static)
        sys_proxy = next(c for c in charts if "proxy-small" in c and "system calls" in c)
        self.assertIn("line [40.50, 39.00]", sys_proxy)

    def test_the_error_run_never_reaches_the_page(self):
        self.assertNotIn("0.37", self.md)

    def test_latest_values_table(self):
        self.assertIn("| `static-small` | 2026-11-07 | `def5678` | `gha-ubuntu-24.04-x86_64` | 9.25 | 19.25 |", self.md)
        self.assertIn("| `proxy-small` | 2026-11-07 | `def5678` | `gha-ubuntu-24.04-x86_64` | 29.00 | 39.00 |", self.md)

    def test_the_method_is_stated(self):
        self.assertIn("utime+stime", self.md)
        self.assertIn("strace -c -f", self.md)
        self.assertIn("results.csv", self.md)

    def test_an_empty_csv_renders_a_page_without_charts(self):
        md = bc.render([])
        self.assertEqual(blocks(md), [])
        self.assertIn("No results yet", md)


class Append(unittest.TestCase):
    def test_valid_rows_are_appended_and_the_page_rendered(self):
        with tempfile.TemporaryDirectory() as d:
            new = os.path.join(d, "bench.csv")
            results = os.path.join(d, "results.csv")
            page = os.path.join(d, "BENCHMARKS.md")
            with open(new, "w") as fh:
                fh.write(CSV + row("2026-11-07", "static-huge", 1, 1) + "\n")
            with contextlib.redirect_stdout(io.StringIO()) as out:
                bc.main(["--append", new, results, page])
            self.assertIn("appended 9 rows, dropped 1", out.getvalue())
            with open(results) as fh:
                text = fh.read()
            self.assertTrue(text.startswith(HEADER + "\n"))
            self.assertEqual(len(text.splitlines()), 10)
            self.assertNotIn("static-huge", text)
            # A second artifact appends below; the header stays single.
            with contextlib.redirect_stdout(io.StringIO()):
                bc.main(["--append", new, results, page])
            with open(results) as fh:
                text = fh.read()
            self.assertEqual(text.count("date,commit"), 1)
            self.assertEqual(len(text.splitlines()), 19)
            with open(page) as fh:
                self.assertEqual(len(blocks(fh.read())), 4)

    def test_an_artifact_with_a_wrong_header_appends_nothing(self):
        with tempfile.TemporaryDirectory() as d:
            new = os.path.join(d, "bench.csv")
            results = os.path.join(d, "results.csv")
            with open(new, "w") as fh:
                fh.write("evil\n" + row("2026-11-06", "static-small", 1, 1) + "\n")
            with self.assertRaises(SystemExit):
                with contextlib.redirect_stderr(io.StringIO()):
                    bc.main(["--append", new, results, os.path.join(d, "B.md")])
            self.assertFalse(os.path.exists(results))

    def test_render_only(self):
        with tempfile.TemporaryDirectory() as d:
            results = os.path.join(d, "results.csv")
            page = os.path.join(d, "BENCHMARKS.md")
            with open(results, "w") as fh:
                fh.write(CSV)
            bc.main([results, page])
            with open(page) as fh:
                self.assertEqual(len(blocks(fh.read())), 4)


if __name__ == "__main__":
    unittest.main()
