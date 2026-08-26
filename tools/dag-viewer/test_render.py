"""Unit tests for render.py's data-merging and HTML-assembly logic --
deliberately exercised without py_mini_racer or a browser (see render.py's
own ad hoc py_mini_racer validation, not committed here, and PR #249's
description for how the shared viewer.js/node-style.js logic itself was
checked). These tests only cover what render.py adds on top of index.html:
merging input files and correctly inlining/embedding into one HTML page.

Run with (from the repo root): python3 -m unittest discover -s tools/dag-viewer -p 'test_render.py'
(or `cd tools/dag-viewer && python3 -m unittest test_render`, or
`python3 -m pytest tools/dag-viewer/` if pytest is available -- these are
plain `unittest.TestCase`s, any of those runners work). Plain
`python3 -m unittest tools/dag-viewer/test_render.py` does NOT work run
from the repo root: unittest resolves that path to a bare `test_render`
module without adding tools/dag-viewer/ to sys.path, so this file's own
`from render import ...` below fails with `ModuleNotFoundError: No module
named 'render'` -- discover's `-s` (or a plain module name run from inside
the directory) puts the right directory on sys.path instead. Not wired
into CI -- this repo doesn't run Python in CI at all today.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from render import _json_script, _semantic_label, load_workload, prepare_workload, render

HERE = Path(__file__).resolve().parent


def named_graph(name: str, source: str = "SELECT 1") -> dict:
    """A minimal well-formed NamedGraph: one leaf Scan node, its own root."""
    return {
        "name": name,
        "source": source,
        "graph": {
            "nodes": [
                {
                    "id": 0,
                    "kind": "Scan",
                    "label": "Scan(t)",
                    "detail": {},
                    "children": [],
                    "hash": 1,
                }
            ],
            "root": 0,
        },
    }


class LoadWorkloadTests(unittest.TestCase):
    def test_merges_queries_across_files_in_order(self):
        with tempfile.TemporaryDirectory() as d:
            f1 = Path(d) / "a.json"
            f2 = Path(d) / "b.json"
            f1.write_text(json.dumps({"queries": [named_graph("q1")]}))
            f2.write_text(json.dumps({"queries": [named_graph("q2")]}))

            workload = load_workload([f1, f2])

        self.assertEqual([q["name"] for q in workload["queries"]], ["q1", "q2"])

    def test_colliding_name_is_disambiguated_by_source_filename(self):
        # Matches viewer.js's loadFiles(): a later file's query named the
        # same as an earlier one gets " (filename)" appended instead of
        # silently overwriting or erroring.
        with tempfile.TemporaryDirectory() as d:
            f1 = Path(d) / "a.json"
            f2 = Path(d) / "b.json"
            f1.write_text(json.dumps({"queries": [named_graph("q1")]}))
            f2.write_text(json.dumps({"queries": [named_graph("q1")]}))

            workload = load_workload([f1, f2])

        self.assertEqual(workload["queries"][0]["name"], "q1")
        self.assertEqual(workload["queries"][1]["name"], "q1 (b.json)")

    def test_non_colliding_names_pass_through_unchanged(self):
        with tempfile.TemporaryDirectory() as d:
            f1 = Path(d) / "a.json"
            f1.write_text(json.dumps({"queries": [named_graph("q1"), named_graph("q2")]}))

            workload = load_workload([f1])

        self.assertEqual([q["name"] for q in workload["queries"]], ["q1", "q2"])


class RenderTests(unittest.TestCase):
    def test_no_script_src_tags_remain(self):
        # Substring-only "<script src=" would also match viewer.js's own
        # header comment (which documents index.html's <script src=...>
        # usage in prose) once that file is inlined verbatim -- so match the
        # real tag shape instead of a bare substring.
        workload = {"queries": [named_graph("q1"), named_graph("q2")]}
        html = render(workload)
        self.assertNotRegex(html, r'<script src="[^"]+"></script>')

    def test_embedded_workload_round_trips(self):
        workload = {"queries": [named_graph("q1"), named_graph("q2")]}
        html = render(workload)

        m = re.search(
            r'<script type="application/json" id="embedded-workload">(.*?)</script>',
            html,
            re.S,
        )
        self.assertIsNotNone(m, "embedded-workload <script> tag not found")
        self.assertEqual(json.loads(m.group(1)), prepare_workload(workload))

    def test_render_does_not_mutate_callers_workload(self):
        workload = {"queries": [named_graph("q1")]}
        original = json.loads(json.dumps(workload))
        render(workload)
        self.assertEqual(workload, original)

    def test_render_adds_no_legacy_mode_config(self):
        # Bare "__DAG_RENDER__" also matches viewer.js's own header comment
        # (which documents the window.__DAG_RENDER__ config object in prose)
        # once that file is inlined verbatim -- match the actual assignment
        # statement instead.
        workload = {"queries": [named_graph("q1"), named_graph("q2")]}
        html = render(workload)
        self.assertNotRegex(html, r"window\.__DAG_RENDER__ =")

    def test_embedded_data_placed_before_viewer_js_body(self):
        # viewer.js's top-level startup code reads #embedded-workload and
        # synchronously as soon as it runs, so it must appear earlier in the
        # document than viewer.js's own inlined
        # <script> block.
        workload = {"queries": [named_graph("q1"), named_graph("q2")]}
        html = render(workload)
        embedded_pos = html.index('id="embedded-workload"')
        viewer_pos = html.index("cytoscape.use(window.cytoscapeDagre)")  # viewer.js's first line
        self.assertLess(embedded_pos, viewer_pos)

    def test_angle_bracket_in_query_source_does_not_break_out_of_script_tag(self):
        # A pathological (but legal JSON) query source containing a literal
        # "</script>" substring must not prematurely close the embedded
        # <script type="application/json"> tag when the browser parses it.
        workload = {"queries": [named_graph("q1", source="SELECT '</script><script>evil()</script>'")]}
        html = render(workload)

        m = re.search(
            r'<script type="application/json" id="embedded-workload">(.*?)</script>',
            html,
            re.S,
        )
        self.assertIsNotNone(m)
        parsed = json.loads(m.group(1))
        self.assertEqual(parsed["queries"][0]["source"], "SELECT '</script><script>evil()</script>'")


class JsonScriptTests(unittest.TestCase):
    def test_escapes_angle_bracket_but_stays_valid_json(self):
        encoded = _json_script({"x": "</script>"})
        self.assertNotIn("</script>", encoded)
        self.assertEqual(json.loads(encoded), {"x": "</script>"})


class SemanticLabelTests(unittest.TestCase):
    def test_scan_recovers_source_from_legacy_label(self):
        node = {"kind": "Scan", "label": "Scan(metrics)", "detail": {}}
        self.assertEqual(_semantic_label(node), "Scan\nsource: metrics")

    def test_aggregate_names_measure_input_and_grouping(self):
        node = {
            "kind": "Aggregate",
            "detail": {
                "measures": [{"kind": "quantile", "col": 6, "q": 0.95}],
                "reduction": {"Reduce": [1]},
            },
        }
        self.assertEqual(
            _semantic_label(node),
            "Aggregate\ncompute: quantile(col[6], q=0.95)\ngroup by col[1]",
        )

    def test_sort_names_expression_direction_and_null_order(self):
        node = {
            "kind": "Sort",
            "detail": {
                "keys": [{"expr": {"Column": 2}, "ascending": False, "nulls_first": True}]
            },
        }
        self.assertEqual(_semantic_label(node), "Sort\nsort: col[2] descending, nulls first")

    def test_prepares_before_after_and_whole_post_asap_graphs(self):
        node = {
            "id": 0,
            "kind": "Aggregate",
            "label": "Aggregate(1 measures)",
            "detail": {"measures": [{"kind": "avg", "col": 3}]},
            "children": [],
        }
        graph = {"nodes": [node], "root": 0}
        workload = {
            "queries": [
                {
                    "graph": graph,
                    "post_graph": graph,
                    "replacements": [{"before": graph, "after": {"graph": graph}}],
                }
            ]
        }
        prepared = prepare_workload(workload)
        query = prepared["queries"][0]
        labels = [
            query["graph"]["nodes"][0]["label"],
            query["post_graph"]["nodes"][0]["label"],
            query["replacements"][0]["before"]["nodes"][0]["label"],
            query["replacements"][0]["after"]["graph"]["nodes"][0]["label"],
        ]
        self.assertEqual(labels, ["Aggregate\ncompute: avg(col[3])"] * 4)

    def test_cse_before_after_makes_reuse_decision_visible(self):
        node = {
            "id": 0,
            "kind": "Scan",
            "label": "Scan(metrics)",
            "detail": {},
            "children": [],
        }
        def graph():
            return {"nodes": [dict(node)], "root": 0}

        workload = {
            "queries": [
                {
                    "graph": graph(),
                    "replacements": [
                        {
                            "strategy": "SharedSubtree",
                            "provenance": "CseShare",
                            "rationale": "Scan(metrics) has 2 consumers across this workload",
                            "before": graph(),
                            "after": {"graph": graph()},
                        }
                    ],
                }
            ]
        }
        replacement = prepare_workload(workload)["queries"][0]["replacements"][0]
        self.assertEqual(
            replacement["before"]["nodes"][0]["label"],
            "Scan\nsource: metrics\nreuse: recomputed per consumer",
        )
        self.assertEqual(
            replacement["after"]["graph"]["nodes"][0]["label"],
            "Scan\nsource: metrics\nreuse: shared across workload (2 consumers)",
        )


class MainCliTests(unittest.TestCase):
    """Exercises main()'s own error handling via subprocess, rather than
    calling main() in-process, since what's under test here is exactly the
    CLI's user-facing stderr message + exit code, not internal state."""

    def run_render_py(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(HERE / "render.py"), *args],
            capture_output=True,
            text=True,
        )

    def test_missing_input_file_gives_a_clean_error_not_a_traceback(self):
        result = self.run_render_py("/no/such/path/dag.json", "-o", "/dev/null")
        self.assertEqual(result.returncode, 1)
        self.assertIn("no such file", result.stderr)
        self.assertNotIn("Traceback", result.stderr)

    def test_malformed_json_gives_a_clean_error_not_a_traceback(self):
        with tempfile.TemporaryDirectory() as d:
            bad = Path(d) / "bad.json"
            bad.write_text("{not json")
            result = self.run_render_py(str(bad), "-o", "/dev/null")
        self.assertEqual(result.returncode, 1)
        self.assertIn("isn't valid JSON", result.stderr)
        self.assertNotIn("Traceback", result.stderr)


if __name__ == "__main__":
    unittest.main()
