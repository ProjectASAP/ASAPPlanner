"""Tests for print_stages.py on a document stage_pipeline writes now.

Run from tools/dag-viewer: `python3 -m unittest test_print_stages`. Needs a
built stage_pipeline (`cargo build -p asap-devtools --bin stage_pipeline`).
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import server

HERE = Path(__file__).resolve().parent


@unittest.skipUnless(server.BINARY.exists(), "needs a built stage_pipeline")
class PrintStagesTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.tmp = tempfile.TemporaryDirectory()
        cls.doc = Path(cls.tmp.name) / "run.json"
        subprocess.run(
            [
                str(server.BINARY),
                "--promql", "quantile_over_time(0.5, data[5m])",
                "--promql", "quantile_over_time(0.9, data[5m])",
                "--series-count", "100", "--sample-interval-ms", "15000",
                "--max-error", "0.01", "--failure-probability", "0.001",
                "--milp-time-limit-s", "5",
                "--out", str(cls.doc),
            ],
            check=True,
            capture_output=True,
        )

    @classmethod
    def tearDownClass(cls) -> None:
        cls.tmp.cleanup()

    def run_printer(self, *flags: str) -> str:
        return subprocess.run(
            [sys.executable, str(HERE / "print_stages.py"), str(self.doc), *flags],
            check=True,
            capture_output=True,
            text=True,
        ).stdout

    def test_prints_every_stage_and_memo_step(self) -> None:
        """Each stage and each memo step gets a section; the two queries share one group."""
        out = self.run_printer()
        for heading in [
            "STAGE 0: frontends' DAG",
            "MEMO after stage0",
            "MEMO after pass1",
            "MEMO after pass2",
            "MEMO after concrete_states",
            "STAGE 1: logical candidates",
            "STAGE 2: physical candidates",
            "STAGE 3: plan selection",
        ]:
            self.assertIn(heading, out)
        self.assertIn("queries=[0, 1]", out)
        self.assertIn("cost/s", out)

    def test_enumerated_lists_memo_choices(self) -> None:
        """--enumerated prints one DAG per memo choice, each with its query roots."""
        out = self.run_printer("--enumerated")
        self.assertIn("E1  G0:c0", out)
        self.assertIn("<- ROOT q1", out)


if __name__ == "__main__":
    unittest.main()
