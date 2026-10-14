"""Tests for server.py: the editor's request becomes `stage_pipeline`
arguments, and /api/plan answers with the stage_pipeline document.

Run from tools/dag-viewer: `python3 -m unittest test_server`. The endpoint
test needs a built stage_pipeline (`cargo build -p asap-devtools --bin
stage_pipeline`; CARGO_TARGET_DIR is honored).
"""

from __future__ import annotations

import json
import subprocess
import threading
import time
import unittest
import urllib.request
from http.server import ThreadingHTTPServer
from pathlib import Path
from unittest import mock

import server


class PlanArgsTests(unittest.TestCase):
    def test_promql_queries_with_epsilon(self):
        args = server.plan_args(
            {"queries": [{"language": "promql", "text": "sum(rate(m[1m]))"}], "epsilon": 0.01},
            "out.json",
        )
        self.assertEqual(
            args[1:],
            ["--promql", "sum(rate(m[1m]))", "--name", "q1", "--epsilon", "0.01", "--interval-ms", "15000", "--out", "out.json"],
        )

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
            ({"queries": [{"language": "promql", "text": "m"}], "epsilon": True}, "epsilon must be a number"),
            ({"queries": [{"language": "promql", "text": "m"}], "epsilon": "0.1"}, "epsilon must be a number"),
            ({"queries": [{"language": "promql", "text": "m"}], "interval_ms": True}, "interval_ms must be a positive integer"),
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


@unittest.skipUnless(server.BINARY.exists(), "needs a built stage_pipeline")
class PlanEndpointTests(unittest.TestCase):
    def test_plan_endpoint_returns_the_stage_pipeline_document(self):
        httpd = ThreadingHTTPServer(("127.0.0.1", 0), server.Handler)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        try:
            body = json.dumps({"queries": [{"name": "job_rate", "language": "promql", "text": "sum by (job) (rate(m[1m]))"}]}).encode()
            request = urllib.request.Request(
                f"http://127.0.0.1:{httpd.server_address[1]}/api/plan",
                data=body,
                headers={"Content-Type": "application/json"},
            )
            with urllib.request.urlopen(request, timeout=120) as response:
                document = json.loads(response.read())
        finally:
            httpd.shutdown()
            httpd.server_close()
        self.assertEqual(document["format"], "asap-stage-pipeline/v1")
        self.assertEqual(document["workload"]["queries"][0]["text"], "sum by (job) (rate(m[1m]))")
        self.assertEqual(document["workload"]["queries"][0]["id"], "job_rate")
        self.assertIsNotNone(document["stage3_plan_selection"]["selected"])


if __name__ == "__main__":
    unittest.main()
