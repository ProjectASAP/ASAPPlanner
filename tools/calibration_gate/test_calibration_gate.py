"""Tests for calibration_gate.py's resource rows (issue #1472): per view
bytes per window, writes, disk at the peak, cold reads and memory, and the
plan's totals, against a deployment's measured file.

Run from tools/calibration_gate: `python3 -m unittest -v test_calibration_gate`.
The cases are synthetic: a plan of one 1 s chain (node 3) read by a merge
(node 4), and a report with the planner's per-node storage figures.
"""

from __future__ import annotations

import contextlib
import copy
import io
import unittest

import calibration_gate

PLAN = {
    "dag": {
        "nodes": [
            {"id": 3, "output_state": {"timing": "ingestion_time"}, "retention_ms": 180_000,
             "payload": {"ASAP": {"SummaryTumblingWindowChain": {"tumbling_window_width_ms": 1000}}}},
            {"id": 4, "output_state": {"timing": "query_time"}, "retention_ms": None,
             "payload": {"ASAP": {"SummaryMerge": {}}}},
        ],
        "edges": [{"producer": 3, "consumer": 4}],
    }
}

COST_INPUTS = {"data_plane_execution": {"view_window_cache": {
    "idle_ms": 60_000, "max_bytes": 2_147_483_648,
    "file_span_ms": 900_000, "retention_margin_ms": 60_000}}}

# Chain: 440 KB per window, 1,140 windows at the peak (180 s + 60 s + 900 s).
STORAGE = {"average_memory_bytes": 31_000_000.0, "peak_memory_bytes": 31_000_000,
           "bytes_per_window": 440_000, "disk_bytes": 501_600_000,
           "disk_written_bytes_per_second": 440_000.0, "disk_read_bytes_per_second": 5_000_000.0}


def node(node_id, timing, storage):
    return {"node_id": node_id, "operator": "SummaryTumblingWindowChain", "timing": timing,
            "cpu_core_seconds_per_second": 0.1, "cost_per_second": None,
            "cost_source": "formula", "ingest_cpu_core_seconds_per_second": 0.1,
            "refresh_cpu_ms_per_window": 1.0, "refresh_wall_ms_per_window": 1.0,
            "query_ms_per_step": 0.0, **storage}


REPORT = {"predicted_usage": {
    "average_memory_bytes": 32_000_000.0, "peak_memory_bytes": 32_000_000,
    "disk_bytes": 501_600_000, "disk_written_bytes_per_second": 440_000.0,
    "disk_read_bytes_per_second": 5_000_000.0,
    "per_node": [node(3, "ingestion_time", STORAGE),
                 node(4, "query_time", {
                     "average_memory_bytes": 1_000_000.0, "peak_memory_bytes": 1_000_000,
                     "bytes_per_window": 0, "disk_bytes": 0,
                     "disk_written_bytes_per_second": 0.0,
                     "disk_read_bytes_per_second": 0.0})],
    "per_query": [],
    "recommended_configuration": {
        "ingest": {"predicted_busy_thread_seconds_per_second": 0.1, "recommended_threads": 1},
        "precompute": {"predicted_busy_thread_seconds_per_second": 0.0, "recommended_threads": 1},
        "query": {"predicted_busy_thread_seconds_per_second": 0.5, "recommended_threads": 1}},
}}

# Measured as predicted, after 20 min (past 180 s + 60 s + 900 s).
MEASURED = {
    "elapsed_ms": 1_200_000,
    "views": [{"node_id": 3, "bytes_per_window": 440_000, "disk_bytes": 501_600_000,
               "disk_written_bytes_per_second": 440_000.0,
               "disk_read_bytes_per_second": 5_000_000.0, "memory_bytes": 31_000_000.0}],
    "plan": {"memory_bytes": 32_000_000.0, "disk_bytes": 501_600_000,
             "disk_written_bytes_per_second": 440_000.0,
             "disk_read_bytes_per_second": 5_000_000.0,
             "ingest_cpu_core_seconds_per_second": 0.1,
             "query_cpu_core_seconds_per_second": 0.5},
}


def run(measured, report=REPORT):
    """`check` on the synthetic case: (passed, printed lines)."""
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        passed = calibration_gate.check(copy.deepcopy(report), PLAN, measured, 1.5, "case",
                                        cost_inputs=COST_INPUTS)
    return passed, out.getvalue().splitlines()


def line(lines, text):
    """The one printed line containing `text`."""
    found = [l for l in lines if text in l]
    assert len(found) == 1, f"{text!r} in {lines}"
    return found[0]


class ResourceRows(unittest.TestCase):

    def test_gate_passes_within_tolerance(self):
        """K3. Every view and plan row within its ratio: pass, one line per row."""
        passed, lines = run(MEASURED)
        self.assertTrue(passed, lines)
        for text in ["view 3 bytes/window", "view 3 disk written B/s", "view 3 disk peak B",
                     "view 3 disk read B/s", "view 3 memory B", "plan memory B",
                     "plan disk peak B", "plan disk written B/s", "plan disk read B/s",
                     "plan ingest cpu", "plan query cpu"]:
            self.assertIn(" ok", line(lines, text))

    def test_gate_fails_on_divergent_bytes_per_window(self):
        """K4. Bytes per window 3.3x over the measured (1.25x allowed): FAIL."""
        measured = copy.deepcopy(MEASURED)
        measured["views"][0]["bytes_per_window"] = 440_000 / 3.3
        passed, lines = run(measured)
        self.assertFalse(passed)
        self.assertIn("FAIL", line(lines, "view 3 bytes/window"))

    def test_writes_use_the_tighter_ratio(self):
        """K4b. Writes 1.4x off: within 1.5x but not 1.25x, so FAIL."""
        measured = copy.deepcopy(MEASURED)
        measured["views"][0]["disk_written_bytes_per_second"] = 440_000.0 * 1.4
        passed, lines = run(measured)
        self.assertFalse(passed)
        self.assertIn("FAIL", line(lines, "view 3 disk written B/s"))

    def test_gate_skips_disk_before_steady_state(self):
        """K5. Before retention + margin + file span (1,140 s) has elapsed, a
        view's disk is printed as not at steady state and not gated."""
        measured = copy.deepcopy(MEASURED)
        measured["elapsed_ms"] = 600_000
        measured["views"][0]["disk_bytes"] = 100_000_000
        measured["plan"]["disk_bytes"] = 100_000_000
        passed, lines = run(measured)
        self.assertTrue(passed, lines)
        self.assertIn("not at steady state", line(lines, "view 3 disk peak B"))

    def test_gate_noise_floor(self):
        """K6. A view row whose predicted and measured values are both below
        1% of the plan's totals is printed, not gated; otherwise it is
        gated. Here a second, small chain writes 3x off: predicted at 0.2%
        of the plan's writes (measured 0.6%) it passes, at 2% it fails."""
        for share, expect_pass in [(0.002, True), (0.02, False)]:
            small = share * 440_000.0 / (1 - share)
            report = copy.deepcopy(REPORT)
            report["predicted_usage"]["per_node"].append(node(5, "ingestion_time", {
                **STORAGE, "bytes_per_window": int(small), "disk_bytes": int(small * 1140),
                "disk_written_bytes_per_second": small, "disk_read_bytes_per_second": 0.0,
                "average_memory_bytes": 0.0, "peak_memory_bytes": 0}))
            report["predicted_usage"]["disk_written_bytes_per_second"] += small
            report["predicted_usage"]["disk_bytes"] += int(small * 1140)
            plan = copy.deepcopy(PLAN)
            plan["dag"]["nodes"].append({**PLAN["dag"]["nodes"][0], "id": 5})
            measured = copy.deepcopy(MEASURED)
            measured["views"].append({"node_id": 5, "bytes_per_window": small,
                                      "disk_bytes": small * 1140,
                                      "disk_written_bytes_per_second": 3 * small})
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                passed = calibration_gate.check(report, plan, measured, 1.5, "case",
                                                cost_inputs=COST_INPUTS)
            lines = out.getvalue().splitlines()
            self.assertEqual(passed, expect_pass, (share, lines))
            self.assertIn("view 5 disk written B/s", "\n".join(lines))

    def test_a_view_not_in_the_report_is_not_checked(self):
        """K8. A measured view that is no operator node of the report is
        printed as not checked, not an error."""
        measured = copy.deepcopy(MEASURED)
        measured["views"].append({"node_id": 99, "bytes_per_window": 1})
        passed, lines = run(measured)
        self.assertTrue(passed, lines)
        self.assertIn("not checked", line(lines, "view 99"))

    def test_disk_without_elapsed_time_is_not_checked(self):
        """K9. Without `elapsed_ms` the gate cannot tell steady state, so
        disk at the peak is printed, not gated."""
        measured = copy.deepcopy(MEASURED)
        del measured["elapsed_ms"]
        measured["views"][0]["disk_bytes"] = 1
        passed, lines = run(measured)
        self.assertTrue(passed, lines)
        self.assertIn("not checked", line(lines, "view 3 disk peak B"))

    def test_missing_sections_are_not_checked(self):
        """K7 (guard). A measured file without `views` and `plan` checks only
        the refresh groups and queries, as before."""
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            passed = calibration_gate.check(copy.deepcopy(REPORT), PLAN,
                                            {"refresh_groups": [], "queries": []}, 1.5, "case")
        lines = out.getvalue().splitlines()
        self.assertTrue(passed)
        self.assertEqual(lines, [])


if __name__ == "__main__":
    unittest.main()
