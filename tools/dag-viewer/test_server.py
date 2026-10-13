"""Tests for server.py: the editor's request becomes `stage_pipeline`
arguments, and /api/plan answers with the stage_pipeline document.

Run from tools/dag-viewer: `python3 -m unittest test_server`. The endpoint
test needs a built stage_pipeline (`cargo build -p asap-devtools --bin
stage_pipeline`; CARGO_TARGET_DIR is honored).
"""

from __future__ import annotations

import json
import threading
import unittest
import urllib.request
from http.server import ThreadingHTTPServer

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
