"""Tests for server.py: the editor's request becomes `stage_pipeline`
arguments, /api/plan answers with the stage_pipeline document, and the
example documents are written on demand.

Run from tools/dag-viewer: `python3 -m unittest test_server`. The endpoint
test needs a built stage_pipeline (`cargo build -p asap-devtools --bin
stage_pipeline`; CARGO_TARGET_DIR is honored).
"""

from __future__ import annotations

import json
import subprocess
import tempfile
import threading
import time
import unittest
import urllib.request
from http.server import ThreadingHTTPServer
from pathlib import Path
from unittest import mock

import server


class PlanArgsTests(unittest.TestCase):
    def test_promql_queries_with_max_error(self):
        args = server.plan_args(
            {"queries": [{"language": "promql", "text": "sum(rate(m[1m]))"}], "max_error": 0.01},
            "out.json",
        )
        self.assertEqual(
            args[1:],
            ["--promql", "sum(rate(m[1m]))", "--name", "q1", "--max-error", "0.01", "--sample-interval-ms", "15000", "--out", "out.json"],
        )

    def test_declared_source_facts_become_flags(self):
        query = {"language": "promql", "text": "sum(rate(m[1m]))"}
        args = server.plan_args({"queries": [query], "series_count": 1000, "rows_per_second": 66.5, "rows_held": 5000}, "out.json")
        self.assertEqual(args[5:11], ["--series-count", "1000", "--rows-per-second", "66.5", "--rows-held", "5000"])
        for bad in ({"series_count": 0}, {"rows_held": 1.5}, {"rows_per_second": -1}, {"series_count": True}):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                server.plan_args({"queries": [query], **bad}, "out.json")

    def test_metric_names_the_targets_error_metric(self):
        query = {"language": "promql", "text": "quantile_over_time(0.99, m[1m])"}
        for metric in ("absolute", "relative", "rank"):
            with self.subTest(metric=metric):
                args = server.plan_args({"queries": [query], "max_error": 0.01, "accuracy_metric": metric}, "out.json")
                self.assertEqual(args[5:9], ["--max-error", "0.01", "--accuracy-metric", metric])
        for empty in ({}, {"accuracy_metric": ""}, {"accuracy_metric": None}):
            with self.subTest(empty=empty):
                args = server.plan_args({"queries": [query], "max_error": 0.01, **empty}, "out.json")
                self.assertNotIn("--accuracy-metric", args)

    def test_sql_queries_declare_their_tables(self):
        table = {"name": "metrics", "columns": [{"name": "ts", "type": "timestamp"}], "time_index": 0}
        args = server.plan_args(
            {"schemas": [table], "queries": [{"language": "sql", "text": "SELECT 1 FROM metrics", "schemas": ["metrics"]}]},
            "out.json",
        )
        self.assertEqual(args[1:3], ["--table", json.dumps(table, separators=(",", ":"))])
        self.assertEqual(args[3:7], ["--sql", "SELECT 1 FROM metrics", "--name", "q1"])

    def test_mixed_languages_keep_order_and_names(self):
        table = {"name": "metrics", "columns": [{"name": "ts", "type": "timestamp"}], "time_index": 0}
        args = server.plan_args(
            {
                "schemas": [table],
                "queries": [
                    {"name": "rate", "language": "promql", "text": "m"},
                    {"name": "count", "language": "sql", "text": "SELECT 1 FROM metrics", "schemas": ["metrics"]},
                ],
            },
            "out.json",
        )
        self.assertEqual(
            args[3:11],
            ["--promql", "m", "--name", "rate", "--sql", "SELECT 1 FROM metrics", "--name", "count"],
        )

    def test_request_must_be_an_object(self):
        for payload in ([], "q", None):
            with self.assertRaisesRegex(ValueError, "request must be a JSON object"):
                server.plan_args(payload, "out.json")

    def test_field_types_are_checked(self):
        table = {"name": "metrics", "columns": [{"name": "ts", "type": "timestamp"}]}
        cases = [
            ({"queries": [{"language": ["sql"], "text": "m"}]}, "query #1: language must be sql or promql"),
            ({"queries": [{"language": "promql", "name": 7, "text": "m"}]}, "query #1: name must be a string"),
            ({"queries": [{"language": "promql", "text": 7}]}, "query #1: text must contain"),
            (
                {"schemas": [table], "queries": [{"language": "sql", "text": "SELECT 1", "schemas": [["metrics"]]}]},
                "query #1: selected schemas must be names of declared tables",
            ),
            ({"queries": [{"language": "promql", "text": "m"}], "max_error": True}, "max_error must be a number"),
            ({"queries": [{"language": "promql", "text": "m"}], "max_error": "0.1"}, "max_error must be a number"),
            ({"queries": [{"language": "promql", "text": "m"}], "interval_ms": True}, "interval_ms must be a positive integer"),
            ({"queries": [{"language": "promql", "text": "m"}], "max_error": 0.01, "accuracy_metric": "rnak"}, "accuracy_metric must be absolute, relative or rank"),
            ({"queries": [{"language": "promql", "text": "m"}], "max_error": 0.01, "accuracy_metric": 3}, "accuracy_metric must be absolute, relative or rank"),
            ({"queries": [{"language": "promql", "text": "m"}], "accuracy_metric": "rank"}, "accuracy_metric needs max_error"),
            ({"schemas": [{"name": 3, "columns": [{}]}], "queries": [{"language": "sql", "text": "SELECT 1"}]}, "schema #1: name must be a non-empty string"),
        ]
        for payload, message in cases:
            with self.subTest(message=message):
                with self.assertRaisesRegex(ValueError, message):
                    server.plan_args(payload, "out.json")

    def test_columns_and_time_index_are_type_checked(self):
        """Malformed tables are a 400, not a stage_pipeline failure."""
        sql = [{"language": "sql", "text": "SELECT 1 FROM metrics"}]
        ts = {"name": "ts", "type": "timestamp"}
        cases = [
            ({"name": "metrics", "columns": ["ts"]}, "schema #1: column #1 must be an object"),
            ({"name": "metrics", "columns": [{"type": "timestamp"}]}, "schema #1: column #1: name must be a non-empty string"),
            ({"name": "metrics", "columns": [{"name": "ts", "type": 3}]}, "schema #1: column #1: type must be a string"),
            ({"name": "metrics", "columns": [{"name": "ts", "type": "timestamp", "nullable": "no"}]}, "schema #1: column #1: nullable must be a boolean"),
            ({"name": "metrics", "columns": [ts], "time_index": "0"}, "schema #1: time_index must be the index of one of its 1 columns"),
            ({"name": "metrics", "columns": [ts], "time_index": -1}, "schema #1: time_index must be the index of one of its 1 columns"),
            ({"name": "metrics", "columns": [ts], "time_index": 1}, "schema #1: time_index must be the index of one of its 1 columns"),
            ({"name": "metrics", "columns": [ts], "time_index": True}, "schema #1: time_index must be the index of one of its 1 columns"),
        ]
        for table, message in cases:
            with self.subTest(message=message, table=table):
                with self.assertRaisesRegex(ValueError, message):
                    server.plan_args({"schemas": [table], "queries": sql}, "out.json")
        server.plan_args({"schemas": [{"name": "metrics", "columns": [ts], "time_index": 0}], "queries": sql}, "out.json")

    def test_duplicate_table_names_are_rejected(self):
        table = {"name": "metrics", "columns": [{"name": "ts", "type": "timestamp"}]}
        with self.assertRaisesRegex(ValueError, "schema #2: table metrics is declared twice"):
            server.plan_args(
                {"schemas": [table, table], "queries": [{"language": "sql", "text": "SELECT 1 FROM metrics"}]},
                "out.json",
            )

    def test_duplicate_query_names_say_which_query_has_the_name(self):
        with self.assertRaisesRegex(ValueError, r"^query #2: name a is already used by query #1$"):
            server.plan_args(
                {"queries": [{"name": "a", "language": "promql", "text": "m"}, {"name": "a", "language": "promql", "text": "n"}]},
                "out.json",
            )
        with self.assertRaisesRegex(ValueError, r"^query #2: name q2 \(its default name\) is already used by query #1$"):
            server.plan_args(
                {"queries": [{"name": "q2", "language": "promql", "text": "m"}, {"language": "promql", "text": "n"}]},
                "out.json",
            )


# A SQL workload (one query of the 2026-10-22 evaluation's W-RollupHeavy)
# and the table it reads, which only a --table declares.
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


class WorkloadArgsTests(unittest.TestCase):
    """A PlanningWorkload request becomes `stage_pipeline --workload` with the
    set's deployment capabilities, cost model and accuracy guarantee mode."""

    def test_workload_with_capabilities_and_cost_model(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = str(Path(tmp) / "w.json")
            workload = {"query_workload": {"language": "promql", "query_batch": None, "repeating_queries": []}}
            args = server.workload_args(
                {"workload": workload, "deployment_capabilities": {"raw_data_retained": False}, "cost_model": "aws-fargate-us-east-1"},
                path, "out.json")
            self.assertEqual(args[1:], ["--workload", path, "--deployment-capabilities", '{"raw_data_retained":false}',
                                        "--cost-model", "aws-fargate-us-east-1", "--out", "out.json"])
            self.assertEqual(json.loads(Path(path).read_text()), workload)

    def test_workload_enforces_accuracy_guarantees_only_when_asked(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = str(Path(tmp) / "w.json")
            workload = {"query_workload": {"language": "promql", "query_batch": None, "repeating_queries": []}}
            enforced = server.workload_args({"workload": workload, "accuracy_guarantees": "enforced"}, path, "out.json")
            self.assertEqual(enforced[1:], ["--workload", path, "--enforce-accuracy-guarantees", "--out", "out.json"])
            for mode in (None, "ignored"):
                args = server.workload_args({"workload": workload, "accuracy_guarantees": mode}, path, "out.json")
                self.assertNotIn("--enforce-accuracy-guarantees", args)

    def test_sql_tables_become_table_flags(self):
        """A SQL workload's tables, which a PlanningWorkload does not
        declare, are passed to stage_pipeline as one --table each."""
        with tempfile.TemporaryDirectory() as tmp:
            path = str(Path(tmp) / "w.json")
            args = server.workload_args({"workload": SQL_WORKLOAD, "tables": [FLOWS_TABLE]}, path, "out.json")
            self.assertEqual(args[1:], ["--workload", path, "--table", json.dumps(FLOWS_TABLE, separators=(",", ":")),
                                        "--out", "out.json"])
            for absent in ({"workload": SQL_WORKLOAD}, {"workload": SQL_WORKLOAD, "tables": None}):
                self.assertNotIn("--table", server.workload_args(absent, path, "out.json"))

    def test_malformed_tables_are_rejected(self):
        for tables, message in [
            ({"name": "flows"}, "tables must be an array"),
            ([{"name": "flows"}], "table #1: columns"),
            ([FLOWS_TABLE, FLOWS_TABLE], "table #2: table flows is declared twice"),
        ]:
            with self.subTest(message), self.assertRaisesRegex(ValueError, message):
                server.workload_args({"workload": SQL_WORKLOAD, "tables": tables}, "w.json", "out.json")

    def test_example_args_read_the_tables_next_to_the_workload(self):
        """An examples.json entry planned from a workload file passes the
        tables in the tables.json next to it."""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "workloads" / "s").mkdir(parents=True)
            (root / "workloads" / "s" / "tables.json").write_text(json.dumps([FLOWS_TABLE]))
            with mock.patch.object(server, "HERE", root):
                args = server.example_args({"workload": "workloads/s/w.json"}, "o.json")
                self.assertEqual(args[3:], ["--table", json.dumps(FLOWS_TABLE, separators=(",", ":")), "--out", "o.json"])
                (root / "workloads" / "s" / "tables.json").unlink()
                self.assertEqual(server.example_args({"workload": "workloads/s/w.json"}, "o.json")[3:], ["--out", "o.json"])

    def test_malformed_requests_are_rejected(self):
        for payload, message in [
            ([], "request must be a JSON object"),
            ({"workload": {}}, "workload must be a PlanningWorkload"),
            ({"workload": {"query_workload": {}}, "deployment_capabilities": 3}, "deployment_capabilities"),
            ({"workload": {"query_workload": {}}, "cost_model": "free"}, "cost_model"),
            ({"workload": {"query_workload": {}}, "accuracy_guarantees": "off"}, "accuracy_guarantees"),
        ]:
            with self.subTest(message), self.assertRaisesRegex(ValueError, message):
                server.workload_args(payload, "w.json", "out.json")

    def test_example_args(self):
        self.assertEqual(server.example_args({"example": "planner-layering-1"}, "o.json")[1:],
                         ["--example", "planner-layering-1", "--exhaustive", "--out", "o.json"])
        args = server.example_args({"workload": "workloads/s/w.json", "deployment_capabilities": {"raw_data_retained": False},
                                    "cost_model": "aws-fargate-us-east-1"}, "o.json")
        self.assertEqual(args[1:3], ["--workload", str(server.HERE / "workloads/s/w.json")])
        self.assertEqual(args[3:], ["--deployment-capabilities", '{"raw_data_retained":false}', "--cost-model", "aws-fargate-us-east-1", "--out", "o.json"])

    def test_example_args_enforce_accuracy_guarantees_only_when_asked(self):
        """An entry ignores accuracy guarantees, stage_pipeline's default,
        unless its accuracy_guarantees is "enforced"."""
        self.assertEqual(server.example_args({"example": "planner-layering-1", "accuracy_guarantees": "enforced"}, "o.json")[1:],
                         ["--example", "planner-layering-1", "--exhaustive", "--enforce-accuracy-guarantees", "--out", "o.json"])
        args = server.example_args({"workload": "w.json", "accuracy_guarantees": "enforced"}, "o.json")
        self.assertEqual(args[-3:], ["--enforce-accuracy-guarantees", "--out", "o.json"])
        self.assertNotIn("--enforce-accuracy-guarantees",
                         server.example_args({"example": "planner-layering-1", "accuracy_guarantees": "ignored"}, "o.json"))
        with self.assertRaisesRegex(ValueError, "accuracy_guarantees"):
            server.example_args({"id": "x", "example": "planner-layering-1", "accuracy_guarantees": "off"}, "o.json")


class PlanTests(unittest.TestCase):
    def test_planning_requests_run_one_at_a_time(self):
        """Concurrent /api/plan requests never run stage_pipeline at once."""
        running, peak, guard = 0, 0, threading.Lock()

        def fake_run(args, **kwargs):
            nonlocal running, peak
            with guard:
                running += 1
                peak = max(peak, running)
            time.sleep(0.05)
            out = args[args.index("--out") + 1]
            Path(out).write_text(json.dumps({"stage3_plan_selection": {"selected": "P1"}}))
            with guard:
                running -= 1
            return subprocess.CompletedProcess(args, 0, "", "")

        payload = {"queries": [{"language": "promql", "text": "m"}]}
        with mock.patch.object(server.subprocess, "run", fake_run):
            threads = [threading.Thread(target=server.plan, args=(payload,)) for _ in range(4)]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()
        self.assertEqual(peak, 1)

    def test_too_many_combinations_is_a_rejected_request(self):
        """stage_pipeline's exit status 3 (too many candidate combinations)
        reaches the editor as a rejected request with its message."""
        message = "stage_pipeline: workload has 19683 candidate combinations (limit 10000)"

        def fake_run(args, **kwargs):
            return subprocess.CompletedProcess(args, 3, "", message + "\n")

        with mock.patch.object(server.subprocess, "run", fake_run):
            with self.assertRaises(server.PlanRejected) as raised:
                server.plan({"queries": [{"language": "promql", "text": "m"}]})
        self.assertEqual(str(raised.exception), message)


class WriteExamplesTests(unittest.TestCase):
    """The examples.json documents are written into out/ when missing, and
    all of them again with --regenerate."""

    def run_write(self, root: Path, force: bool) -> list[list[str]]:
        manifest = root / "examples.json"
        manifest.write_text(json.dumps([
            {"id": "a", "example": "planner-layering-1", "file": "out/a.json"},
            {"id": "b", "example": "planner-layering-3b", "file": "out/b.json"},
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
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "out").mkdir()
            (root / "out" / "a.json").write_text("kept")
            calls = self.run_write(root, force=False)
            self.assertEqual(calls, [[str(server.BINARY), "--example", "planner-layering-3b", "--exhaustive", "--out", str(root / "out" / "b.json")]])
            self.assertEqual((root / "out" / "a.json").read_text(), "kept")

    def test_regenerate_rewrites_every_document(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            calls = self.run_write(root, force=True)
            self.assertEqual([call[2] for call in calls], ["planner-layering-1", "planner-layering-3b"])
            self.assertTrue((root / "out" / "a.json").exists())


def post_plan(payload: dict, path: str = "/api/plan") -> dict:
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
        self.assertEqual(get_status(), {"planner": True})


@unittest.skipUnless(server.BINARY.exists(), "needs a built stage_pipeline")
class PlanEndpointTests(unittest.TestCase):
    def test_plan_workload_endpoint_plans_a_planning_workload(self):
        workload = {
            "query_workload": {"language": "promql", "query_batch": None, "repeating_queries": [{
                "query": "sum by (job) (sum_over_time(m[1m]))", "demand": {"fixed_interval": 10000},
                "requirements": {"accuracy": {"max_error": 0.0, "failure_probability": 0.0, "accuracy_scope": "whole_result"}, "max_latency_ms": None},
                "predictability": {"predictable": {"known_at": None}}}]},
            "data_workload": {"arrival": {"ingesting": {"rows_per_second": 1000.0}},
                              "sources": {"m": {"series_count": 100, "sample_interval": 1000}}},
        }
        document = post_plan({"workload": workload, "deployment_capabilities": {"raw_data_retained": False}}, "/api/plan-workload")
        self.assertEqual(document["search"], "candidate-connections")
        self.assertEqual(document["workload"]["data_workload"]["sources"]["m"]["series_count"], 100)
        self.assertFalse(document["deployment_capabilities"]["raw_data_retained"])
        self.assertIsNotNone(document["stage3_plan_selection"]["selected"])
        self.assertTrue(document["stage3_plan_selection"]["stored_outputs"])

    def test_plan_workload_endpoint_plans_a_sql_workload_with_its_tables(self):
        document = post_plan({"workload": SQL_WORKLOAD, "tables": [FLOWS_TABLE]}, "/api/plan-workload")
        self.assertEqual(document["workload"]["queries"][0]["language"], "sql")
        self.assertIsNotNone(document["stage3_plan_selection"]["selected"])

    def test_plan_endpoint_returns_the_stage_pipeline_document(self):
        document = post_plan({"queries": [{"name": "job_rate", "language": "promql", "text": "sum by (job) (rate(m[1m]))"}],
                              "series_count": 1000})
        self.assertEqual(document["format"], "asap-stage-pipeline/v1")
        self.assertEqual(document["workload"]["queries"][0]["text"], "sum by (job) (rate(m[1m]))")
        self.assertEqual(document["workload"]["queries"][0]["id"], "job_rate")
        self.assertIsNotNone(document["stage3_plan_selection"]["selected"])

    def test_a_plan_without_a_rate_fact_names_the_missing_fact(self):
        """Pricing uses only declared facts: with no series count (and no
        arrival rate or rows held), no plan is selected, and the document
        says which source lacks which fact."""
        document = post_plan({"queries": [{"language": "promql", "text": "sum(rate(m[1m]))"}]})
        selection = document["stage3_plan_selection"]
        self.assertIsNone(selection["selected"])
        self.assertIn("pricing source m needs an arrival rate", json.dumps(selection))

    def test_a_quantile_query_plans_with_or_without_a_metric(self):
        """A quantile target may set its metric or leave it unset: either
        way a plan is selected, and Stage 3 reports the metric it accepted."""
        query = {"name": "p99", "language": "promql", "text": "quantile_over_time(0.99, m[1m])"}
        named = post_plan({"queries": [query], "max_error": 0.01, "accuracy_metric": "rank", "series_count": 1000})
        self.assertEqual(named["workload"]["queries"][0]["requirements"]["accuracy"]["accuracy_metric"], "rank")
        selected = named["stage3_plan_selection"]["selected"]
        self.assertIsNotNone(selected)
        self.assertTrue(named["stage3_plan_selection"]["accepted"][selected][0]["summary"].startswith("metric rank; accepted "))
        unnamed = post_plan({"queries": [query], "max_error": 0.01, "series_count": 1000})
        self.assertNotIn("accuracy_metric", unnamed["workload"]["queries"][0]["requirements"]["accuracy"])
        selected = unnamed["stage3_plan_selection"]["selected"]
        self.assertIsNotNone(selected)
        self.assertTrue(unnamed["stage3_plan_selection"]["accepted"][selected][0]["summary"].startswith("metric not set; accepted "))

if __name__ == "__main__":
    unittest.main()
