"""Tests for server.py: a PlanningWorkload request becomes `asap_planner`
arguments, /api/plan-workload answers with the planner's document, and the
example documents are written on demand.

Run from tools/dag-viewer: `python3 -m unittest test_server`. The endpoint
tests need a built asap_planner (`cargo build -p asap-devtools --bin
asap_planner`; CARGO_TARGET_DIR is honored).
"""

from __future__ import annotations

import json
import subprocess
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
from http.server import ThreadingHTTPServer
from pathlib import Path
from unittest import mock

import server

# The arguments every planning run ends with, before `--out`.
STAGE3_JSON = ["--run-until", "stage3", "--format", "json"]
TS = {"name": "ts", "type": "timestamp"}


class TableArgsTests(unittest.TestCase):
    """SQL table declarations are checked and passed one
    `--sql-catalog-table` each."""

    def test_a_table_becomes_one_flag(self):
        """A well-formed table is passed as its compact JSON."""
        table = {"name": "metrics", "columns": [TS], "time_index": 0}
        self.assertEqual(server.table_args([table], "table"),
                         ["--sql-catalog-table", json.dumps(table, separators=(",", ":"))])

    def test_columns_and_time_index_are_type_checked(self):
        """Malformed tables are a 400, not a planner failure."""
        cases = [
            (3, "table #1 must be an object"),
            ({"name": 3, "columns": [TS]}, "table #1: name must be a non-empty string"),
            ({"name": "metrics", "columns": []}, "table #1: columns must contain"),
            ({"name": "metrics", "columns": ["ts"]}, "table #1: column #1 must be an object"),
            ({"name": "metrics", "columns": [{"type": "timestamp"}]}, "table #1: column #1: name must be a non-empty string"),
            ({"name": "metrics", "columns": [{"name": "ts", "type": 3}]}, "table #1: column #1: type must be a string"),
            ({"name": "metrics", "columns": [{"name": "ts", "type": "timestamp", "nullable": "no"}]}, "table #1: column #1: nullable must be a boolean"),
            ({"name": "metrics", "columns": [TS], "time_index": "0"}, "table #1: time_index must be the index of one of its 1 columns"),
            ({"name": "metrics", "columns": [TS], "time_index": -1}, "table #1: time_index must be the index of one of its 1 columns"),
            ({"name": "metrics", "columns": [TS], "time_index": 1}, "table #1: time_index must be the index of one of its 1 columns"),
            ({"name": "metrics", "columns": [TS], "time_index": True}, "table #1: time_index must be the index of one of its 1 columns"),
        ]
        for table, message in cases:
            with self.subTest(message=message, table=table):
                with self.assertRaisesRegex(ValueError, message):
                    server.table_args([table], "table")

    def test_duplicate_table_names_are_rejected(self):
        """Two tables of one name are refused, naming the second."""
        table = {"name": "metrics", "columns": [TS]}
        with self.assertRaisesRegex(ValueError, "table #2: table metrics is declared twice"):
            server.table_args([table, table], "table")


# A SQL workload (one query of the 2026-10-22 evaluation's W-RollupHeavy)
# and the table it reads, which only a --sql-catalog-table declares.
FLOWS_TABLE = {"name": "flows", "time_index": 0, "columns": [
    {"name": "ts", "type": "timestamp", "nullable": False},
    {"name": "dst_port", "type": "utf8", "nullable": False},
    {"name": "src_addr", "type": "float64", "nullable": False}]}
SQL_WORKLOAD = {
    "query_workload": {"language": {"sql": "datafusion_sql"}, "query_batch": None, "repeating_queries": [{
        "query": "SELECT dst_port, COUNT(*) AS value FROM flows WHERE ts >= now() - INTERVAL '5 minutes' GROUP BY dst_port",
        "demand": {"fixed_interval": 60000},
        "requirements": {"accuracy": {"max_error": 0.0, "failure_probability": 0.0, "accuracy_scope": "whole_result"}, "max_latency_ms": None},
        "predictability": {"predictable": {"known_at": None}}}]},
    "data_workload": {"arrival": {"ingesting": {"rows_per_second": 1000.0}},
                      "sources": {"flows": {"series_count": 100, "sample_interval": 1000, "distinct_values_per_label": {"dst_port": 100}}}},
}


PROMQL_WORKLOAD = {"query_workload": {"language": "promql", "query_batch": None, "repeating_queries": []}}


class WorkloadArgsTests(unittest.TestCase):
    """A PlanningWorkload request becomes `asap_planner --workload ...
    --run-until stage3 --format json` with the set's deployment
    capabilities, cost model and accuracy guarantee mode."""

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.tmp = Path(directory.name)

    def args(self, payload: object) -> tuple[list[str], Path]:
        return server.workload_args(payload, str(self.tmp), "out.json"), self.tmp

    def test_capabilities_go_to_a_file_and_the_cost_model_to_its_cost_inputs(self):
        """Deployment capabilities are written to a file passed by path; a
        cost model name selects cost-inputs/<name>.json."""
        args, tmp = self.args({"workload": PROMQL_WORKLOAD, "deployment_capabilities": {"raw_data_retained": False},
                               "cost_model": "aws-fargate-us-east-1", "accuracy_guarantees": "enforced"})
        capabilities = str(tmp / "deployment-capabilities.json")
        self.assertEqual(args[1:], ["--workload", str(tmp / "workload.json"), *STAGE3_JSON,
                                    "--deployment-capabilities", capabilities,
                                    "--cost-inputs", str(server.COST_INPUTS / "aws-fargate-us-east-1.json"),
                                    "--out", "out.json"])
        self.assertEqual(json.loads((tmp / "workload.json").read_text()), PROMQL_WORKLOAD)
        self.assertEqual(json.loads(Path(capabilities).read_text()), {"raw_data_retained": False})

    def test_accuracy_guarantees_are_ignored_unless_enforced(self):
        """Null and "ignored" pass --ignore-accuracy-guarantees; "enforced"
        passes nothing (the planner enforces them by default)."""
        enforced, _ = self.args({"workload": PROMQL_WORKLOAD, "accuracy_guarantees": "enforced"})
        self.assertNotIn("--ignore-accuracy-guarantees", enforced)
        for mode in (None, "ignored"):
            with self.subTest(mode=mode):
                args, _ = self.args({"workload": PROMQL_WORKLOAD, "accuracy_guarantees": mode})
                self.assertEqual(args[-3:], ["--ignore-accuracy-guarantees", "--out", "out.json"])

    def test_every_cost_model_has_its_cost_inputs(self):
        """The cost models a request may name are the three files in
        cost-inputs/."""
        self.assertEqual(server.COST_MODELS, ("aws-fargate-us-east-1", "usage-over-time"))

    def test_sql_tables_become_catalog_table_flags(self):
        """A SQL workload's tables, which a PlanningWorkload does not
        declare, are passed as one --sql-catalog-table each."""
        args, tmp = self.args({"workload": SQL_WORKLOAD, "tables": [FLOWS_TABLE]})
        self.assertEqual(args[1:5], ["--workload", str(tmp / "workload.json"),
                                     "--sql-catalog-table", json.dumps(FLOWS_TABLE, separators=(",", ":"))])
        for absent in ({"workload": SQL_WORKLOAD}, {"workload": SQL_WORKLOAD, "tables": None}):
            self.assertNotIn("--sql-catalog-table", self.args(absent)[0])

    def test_malformed_tables_are_rejected(self):
        """A malformed tables field is refused with the table it names."""
        for tables, message in [
            ({"name": "flows"}, "tables must be an array"),
            ([{"name": "flows"}], "table #1: columns"),
            ([FLOWS_TABLE, FLOWS_TABLE], "table #2: table flows is declared twice"),
        ]:
            with self.subTest(message), self.assertRaisesRegex(ValueError, message):
                self.args({"workload": SQL_WORKLOAD, "tables": tables})

    def test_malformed_requests_are_rejected(self):
        """Each malformed field is refused with its name."""
        for payload, message in [
            ([], "request must be a JSON object"),
            ({"workload": {}}, "workload must be a PlanningWorkload"),
            ({"workload": {"query_workload": {}}, "deployment_capabilities": 3}, "deployment_capabilities"),
            ({"workload": {"query_workload": {}}, "cost_model": "free"}, "cost_model"),
            ({"workload": {"query_workload": {}}, "accuracy_guarantees": "off"}, "accuracy_guarantees"),
        ]:
            with self.subTest(message), self.assertRaisesRegex(ValueError, message):
                self.args(payload)


class ExampleArgsTests(unittest.TestCase):
    """An examples.json entry is its workload file planned through Stage 3."""

    def test_example_args_plan_the_workload_with_the_tables_next_to_it(self):
        """The entry's workload is planned with the tables.json next to it,
        when there is one."""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "workloads" / "s").mkdir(parents=True)
            (root / "workloads" / "s" / "tables.json").write_text(json.dumps([FLOWS_TABLE]))
            with mock.patch.object(server, "HERE", root):
                args = server.example_args({"workload": "workloads/s/w.json", "accuracy_guarantees": "enforced"}, "o.json")
                self.assertEqual(args[1:], ["--workload", str(root / "workloads/s/w.json"),
                                            "--sql-catalog-table", json.dumps(FLOWS_TABLE, separators=(",", ":")),
                                            *STAGE3_JSON, "--out", "o.json"])
                (root / "workloads" / "s" / "tables.json").unlink()
                self.assertEqual(server.example_args({"workload": "workloads/s/w.json", "accuracy_guarantees": "enforced"}, "o.json")[3:],
                                 [*STAGE3_JSON, "--out", "o.json"])

    def test_example_args_ignore_accuracy_guarantees_unless_enforced(self):
        """An entry ignores accuracy guarantees unless its
        accuracy_guarantees is "enforced"; another value is refused."""
        self.assertNotIn("--ignore-accuracy-guarantees",
                         server.example_args({"workload": "w.json", "accuracy_guarantees": "enforced"}, "o.json"))
        for entry in ({"workload": "w.json"}, {"workload": "w.json", "accuracy_guarantees": "ignored"}):
            self.assertIn("--ignore-accuracy-guarantees", server.example_args(entry, "o.json"))
        with self.assertRaisesRegex(ValueError, "accuracy_guarantees"):
            server.example_args({"id": "x", "workload": "w.json", "accuracy_guarantees": "off"}, "o.json")


class PlanTests(unittest.TestCase):
    def test_planning_requests_run_one_at_a_time(self):
        """Concurrent /api/plan-workload requests never run asap_planner at once."""
        running, peak, guard = 0, 0, threading.Lock()

        def fake_run(args, **kwargs):
            nonlocal running, peak
            with guard:
                running += 1
                peak = max(peak, running)
            time.sleep(0.05)
            out = args[args.index("--out") + 1]
            Path(out).write_text(json.dumps({"stage3_plan_selection": {"cost": {}}}))
            with guard:
                running -= 1
            return subprocess.CompletedProcess(args, 0, "", "")

        with mock.patch.object(server.subprocess, "run", fake_run):
            threads = [threading.Thread(target=server.plan_workload, args=({"workload": PROMQL_WORKLOAD},)) for _ in range(4)]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()
        self.assertEqual(peak, 1)

    def test_a_planner_failure_carries_its_message(self):
        """A non-zero exit raises the planner's standard error."""
        def fake_run(args, **kwargs):
            return subprocess.CompletedProcess(args, 1, "", "asap_planner: bad workload\n")

        with mock.patch.object(server.subprocess, "run", fake_run):
            with self.assertRaisesRegex(RuntimeError, "^asap_planner: bad workload$"):
                server.plan_workload({"workload": PROMQL_WORKLOAD})


class WriteExamplesTests(unittest.TestCase):
    """The examples.json documents are written into out/ when missing, and
    all of them again with --regenerate."""

    def run_write(self, root: Path, force: bool) -> list[list[str]]:
        manifest = root / "examples.json"
        manifest.write_text(json.dumps([
            {"id": "a", "workload": "workloads/a.json", "file": "out/a.json"},
            {"id": "b", "workload": "workloads/b.json", "file": "out/b.json"},
        ]))
        calls = []

        def fake_run(args, **kwargs):
            calls.append(args)
            Path(args[args.index("--out") + 1]).write_text("{}")
            return subprocess.CompletedProcess(args, 0, "", "")

        with mock.patch.object(server, "HERE", root), mock.patch.object(server, "EXAMPLES", manifest), \
                mock.patch.object(server.subprocess, "run", fake_run):
            server.write_examples(force)
        return calls

    def test_missing_documents_are_written_and_existing_ones_kept(self):
        """Only the missing document is planned; an existing one is left as it is."""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "out").mkdir()
            (root / "out" / "a.json").write_text("existing")
            calls = self.run_write(root, force=False)
            self.assertEqual(calls, [[str(server.BINARY), "--workload", str(root / "workloads" / "b.json"), *STAGE3_JSON,
                                      "--ignore-accuracy-guarantees", "--out", str(root / "out" / "b.json")]])
            self.assertEqual((root / "out" / "a.json").read_text(), "existing")

    def test_regenerate_rewrites_every_document(self):
        """--regenerate plans every entry again."""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            calls = self.run_write(root, force=True)
            self.assertEqual([Path(call[2]).name for call in calls], ["a.json", "b.json"])
            self.assertTrue((root / "out" / "a.json").exists())


def post_plan(payload: dict, path: str = "/api/plan-workload") -> dict:
    """POST `payload` to `path` on a fresh server and return the document."""
    httpd = ThreadingHTTPServer(("127.0.0.1", 0), server.Handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    try:
        request = urllib.request.Request(
            f"http://127.0.0.1:{httpd.server_address[1]}{path}",
            data=json.dumps(payload).encode(),
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(request, timeout=120) as response:
            return json.loads(response.read())
    finally:
        httpd.shutdown()
        httpd.server_close()


def get_status() -> dict:
    httpd = ThreadingHTTPServer(("127.0.0.1", 0), server.Handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{httpd.server_address[1]}/api/status", timeout=10) as response:
            return json.loads(response.read())
    finally:
        httpd.shutdown()
        httpd.server_close()


class StatusEndpointTests(unittest.TestCase):
    def test_status_says_the_page_can_plan(self):
        """/api/status tells the page it is served by the planning server."""
        self.assertEqual(get_status(), {"planner": True})

    def test_the_query_editor_endpoint_is_gone(self):
        """/api/plan, the removed query editor's endpoint, is not found."""
        with self.assertRaises(urllib.error.HTTPError) as raised:
            post_plan({"queries": []}, "/api/plan")
        self.assertEqual(raised.exception.code, 404)


@unittest.skipUnless(server.BINARY.exists(), "needs a built asap_planner")
class PlanEndpointTests(unittest.TestCase):
    def test_plan_workload_endpoint_plans_a_planning_workload(self):
        """A PromQL workload without raw data is planned through Stage 3,
        with its deployment capabilities, keeping materialized views."""
        workload = {
            "query_workload": {"language": "promql", "query_batch": None, "repeating_queries": [{
                "query": "sum by (job) (sum_over_time(m[1m]))", "demand": {"fixed_interval": 10000},
                "requirements": {"accuracy": {"max_error": 0.0, "failure_probability": 0.0, "accuracy_scope": "whole_result"}, "max_latency_ms": None},
                "predictability": {"predictable": {"known_at": None}}}]},
            "data_workload": {"arrival": {"ingesting": {"rows_per_second": 1000.0}},
                              "sources": {"m": {"series_count": 100, "sample_interval": 1000}}},
        }
        document = post_plan({"workload": workload, "deployment_capabilities": {"raw_data_retained": False},
                              "cost_model": "usage-over-time"}, "/api/plan-workload")
        self.assertEqual(document["format"], "asap-stage-pipeline/v2")
        self.assertEqual(document["run_until"], "stage3")
        self.assertEqual(document["accuracy_guarantees"], "ignored")
        self.assertEqual(document["workload"]["data_workload"]["sources"]["m"]["series_count"], 100)
        self.assertFalse(document["deployment_capabilities"]["raw_data_retained"])
        self.assertIsNotNone(document["stage3_plan_selection"]["selected_dag"])
        self.assertTrue(document["stage3_plan_selection"]["materialized_views"])

    def test_plan_workload_endpoint_plans_a_sql_workload_with_its_tables(self):
        """A SQL workload plans with the tables the request declares, and
        enforces accuracy guarantees when asked."""
        document = post_plan({"workload": SQL_WORKLOAD, "tables": [FLOWS_TABLE], "accuracy_guarantees": "enforced"},
                             "/api/plan-workload")
        self.assertEqual(document["workload"]["queries"][0]["language"], "sql")
        self.assertEqual(document["accuracy_guarantees"], "enforced")
        self.assertIsNotNone(document["stage3_plan_selection"]["selected_dag"])


if __name__ == "__main__":
    unittest.main()
