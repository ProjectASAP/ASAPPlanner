"""Tests for render.py's data merging and HTML assembly, plus viewer cache
provenance checks executed with optional py_mini_racer (no browser required).

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

try:
    from py_mini_racer import py_mini_racer
except ImportError:
    py_mini_racer = None


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
    def test_loads_summary_maintenance_export_as_a_lifecycle_plan(self):
        graph = named_graph("unused")["graph"]
        summary = {
            "selected_raw_recompute": True,
            "summary_total_cost": None,
            "raw_recompute_total_cost": 7.5,
            "horizon_seconds": 60.0,
            "evaluation_rate_per_second": 2.0,
            "update_rate_per_second": 3.0,
            "expected_reads": 120.0,
        }
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "lifecycle.json"
            path.write_text(json.dumps({"graph": graph, "deployments": [], **summary}))
            workload = load_workload([path])

        query = workload["queries"][0]
        self.assertEqual(query["name"], "lifecycle")
        self.assertTrue(query["lifecycle_plan"])
        self.assertEqual(query["post_graph"], graph)
        self.assertEqual(
            query["lifecycle_summary"],
            {**summary, "deployment_count": 0},
        )

    def test_preserves_summary_plan_deployment_count(self):
        graph = named_graph("unused")["graph"]
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "lifecycle.json"
            path.write_text(json.dumps({"graph": graph, "deployments": [{}, {}]}))
            workload = load_workload([path])

        self.assertEqual(
            workload["queries"][0]["lifecycle_summary"]["deployment_count"],
            2,
        )

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

    def test_inlined_edge_cost_key_contains_no_literal_nul(self):
        html = render({"queries": [named_graph("q1")]})
        self.assertNotIn("\x00", html)
        self.assertIn(r"\u0000", html)

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
            "Aggregate\nmeasure: quantile(col[6], q=0.95)\ngroup by col[1]",
        )

    def test_summary_aggregate_names_reduction_as_group_by(self):
        node = {
            "kind": "SummaryAgg",
            "detail": {
                "family": "Sketch(Cms)",
                "col": "SampleValue",
                "reduction": {"Reduce": [1]},
                "grouping": "PerSubpopulationInstance",
            },
        }
        self.assertEqual(
            _semantic_label(node),
            "SummaryAgg\nfamily: Sketch(Cms)\ninput: SampleValue\n… +2 more",
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
        def graph():
            return {"nodes": [dict(node)], "root": 0}
        workload = {
            "queries": [
                {
                    "graph": graph(),
                    "post_graph": graph(),
                    "replacements": [{"before": graph(), "after": {"graph": graph()}}],
                }
            ]
        }
        prepared = prepare_workload(workload)
        query = prepared["queries"][0]
        labels = [
            query["graph"]["nodes"][0]["label"],
            query["post_graph"]["nodes"][0]["label"],
        ]
        self.assertEqual(labels, ["Aggregate\nmeasure: avg(col[3])"] * 2)
        self.assertEqual(
            query["replacements"][0]["before"]["nodes"][0]["label"],
            "Aggregate(1 measures)",
        )

    def test_replacement_subgraphs_are_not_prepared_for_the_current_viewer(self):
        def graph():
            return {"nodes": [{"id": 0, "kind": "Scan", "label": "legacy", "detail": {}, "children": []}], "root": 0}
        workload = {"queries": [{"graph": graph(), "replacements": [{"before": graph(), "after": {"graph": graph()}}]}]}
        replacement = prepare_workload(workload)["queries"][0]["replacements"][0]
        self.assertEqual(replacement["before"]["nodes"][0]["label"], "legacy")
        self.assertEqual(replacement["after"]["graph"]["nodes"][0]["label"], "legacy")


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


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
class ViewerCacheTests(unittest.TestCase):
    def setUp(self):
        self.js = py_mini_racer.MiniRacer()
        source = (HERE / "viewer.js").read_text()
        for name in ["escapeHtml", "formatCostUnit", "formatBaselineRef",
                     "formatCostNumber", "renderCostAnnotation",
                     "computeSelectionWorkloadCost"]:
            function = re.search(r"^function " + name + r"\(.*?^}", source, re.M | re.S)
            self.assertIsNotNone(function, name)
            self.js.eval(function.group(0))

    @staticmethod
    def query(profile, selected_profile=None, batch=0):
        def annotation(value, cache):
            result = {"value": value, "unit": "CostUnits", "source": "Modeled"}
            if cache is not None:
                result["cache_profile"] = cache
            return result

        return {"sourceBatch": batch, "post_graph": {"nodes": [{"decision": {
            "id": 1,
            "baseline_cost": annotation(10, profile),
            "selected_cost": annotation(4, selected_profile or profile),
        }}]}}

    def test_cache_profile_and_inputs_are_rendered(self):
        """The sidebar exposes the assumptions behind a cache-adjusted cost."""
        annotation = self.query("warm-cache-v1")["post_graph"]["nodes"][0]["decision"]["baseline_cost"]
        annotation["inputs"] = [{"name": "result_cache_hit_ratio", "value": 0.5, "unit": "ratio"}]
        html = self.js.call("renderCostAnnotation", "Baseline", annotation)
        self.assertIn("cache warm-cache-v1", html)
        self.assertIn("result_cache_hit_ratio", html)
        self.assertIn("0.5 ratio", html)

    def test_same_cache_profile_aggregates_and_preserves_provenance(self):
        """Comparable decisions total normally and retain their common profile."""
        result = self.js.call("computeSelectionWorkloadCost", [self.query("warm-v1"), self.query("warm-v1", batch=1)])
        self.assertEqual(result["baseline_cost"]["value"], 20)
        self.assertEqual(result["benefit"]["value"], 12)
        self.assertEqual(result["benefit"]["cache_profile"], "warm-v1")

    def test_legacy_profiles_preserve_existing_totals(self):
        """Models without cache provenance retain their existing aggregation."""
        result = self.js.call("computeSelectionWorkloadCost", [self.query(None), self.query(None, batch=1)])
        self.assertEqual(result["benefit"]["value"], 12)
        self.assertIsNone(result["benefit"]["cache_profile"])

    def test_different_or_mixed_cache_profiles_make_totals_unavailable(self):
        """Selection totals cannot claim benefits across incompatible assumptions."""
        cases = [
            [self.query("cold-v1"), self.query("warm-v1", batch=1)],
            [self.query("cold-v1", selected_profile="warm-v1")],
            [self.query("warm-v1"), self.query(None, batch=1)],
            [self.query(None), self.query("warm-v1", batch=1)],
        ]
        for queries in cases:
            with self.subTest(queries=queries):
                result = self.js.call("computeSelectionWorkloadCost", queries)
                for annotation in result.values():
                    self.assertIsNone(annotation["value"])
                    self.assertEqual(annotation["source"], "Unavailable")


if __name__ == "__main__":
    unittest.main()
