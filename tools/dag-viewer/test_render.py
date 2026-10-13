"""Tests for the DAG viewer: render.py's standalone page, stage-workload.js
(which reads the asap-stage-pipeline/v1 document the viewer shows) and the
viewer's selection logic, the JS run headless with optional py_mini_racer.

Run from tools/dag-viewer: `python3 test_render.py` (or
`python3 -m unittest test_render`). Not wired into CI.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from render import _json_script, load_document, render

HERE = Path(__file__).resolve().parent
STAGE_DOCUMENT = HERE / "examples" / "planner-layering-example1.json"

try:
    from py_mini_racer import py_mini_racer
except ImportError:
    py_mini_racer = None


def document() -> dict:
    return json.loads(STAGE_DOCUMENT.read_text())


def embedded(html: str) -> dict:
    match = re.search(r'<script type="application/json" id="embedded-workload">(.*?)</script>', html, re.S)
    return json.loads(match.group(1))


class LoadDocumentTests(unittest.TestCase):
    def test_reads_a_stage_document(self):
        self.assertEqual(load_document(STAGE_DOCUMENT), document())

    def test_rejects_any_other_json(self):
        with tempfile.TemporaryDirectory() as d:
            old = Path(d) / "old.json"
            old.write_text(json.dumps({"queries": []}))
            with self.assertRaisesRegex(ValueError, "asap-stage-pipeline/v1"):
                load_document(old)


class RenderTests(unittest.TestCase):
    def test_no_script_src_tags_remain(self):
        self.assertNotRegex(render(document()), r'<script src="[^"]+"></script>')

    def test_embedded_document_round_trips_with_its_reader(self):
        html = render(document())
        self.assertEqual(embedded(html), document())
        self.assertIn("function stageWorkload", html)

    def test_standalone_export_carries_the_bulk_selection_control(self):
        html = render(document())
        self.assertIn('id="selectAllToggle"', html)
        self.assertIn("function bulkSelectionState(", html)
        self.assertNotRegex(html, r"selectAllToggle\.addEventListener[^}]*queries = \[\]")

    def test_standalone_export_lanes_are_pannable(self):
        lanes = re.findall(r"classes: 'laneParent'[^}]*}", render(document()))
        self.assertEqual(len(lanes), 2, "expected the union and single-query lanes")
        for lane in lanes:
            self.assertIn("pannable: true", lane)

    def test_render_does_not_mutate_callers_document(self):
        doc = document()
        render(doc)
        self.assertEqual(doc, document())

    def test_embedded_data_placed_before_viewer_js_body(self):
        html = render(document())
        self.assertLess(html.index('id="embedded-workload"'), html.index("cytoscape.use(window.cytoscapeDagre)"))

    def test_angle_bracket_in_query_text_does_not_break_out_of_script_tag(self):
        doc = document()
        doc["workload"]["queries"][0]["text"] = "m{a='</script><script>evil()</script>'}"
        self.assertEqual(embedded(render(doc)), doc)


class JsonScriptTests(unittest.TestCase):
    def test_escapes_angle_bracket_but_stays_valid_json(self):
        encoded = _json_script({"x": "</script>"})
        self.assertNotIn("</script>", encoded)
        self.assertEqual(json.loads(encoded), {"x": "</script>"})


class MainCliTests(unittest.TestCase):
    def run_render_py(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run([sys.executable, str(HERE / "render.py"), *args], capture_output=True, text=True)

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

    def test_other_json_gives_a_clean_error(self):
        with tempfile.TemporaryDirectory() as d:
            old = Path(d) / "old.json"
            old.write_text(json.dumps({"queries": []}))
            result = self.run_render_py(str(old), "-o", "/dev/null")
        self.assertEqual(result.returncode, 1)
        self.assertIn("asap-stage-pipeline/v1", result.stderr)
        self.assertNotIn("Traceback", result.stderr)


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
class StageWorkloadTests(unittest.TestCase):
    """stage-workload.js turns a stage_pipeline document into the viewer's
    per-query pre/post DAGs: Stage 0 before, the Stage 3 selected physical
    plan after, with each node's Stage 3 cost."""

    def setUp(self):
        self.js = py_mini_racer.MiniRacer()
        self.js.eval((HERE / "stage-workload.js").read_text())
        self.document = document()

    def test_one_query_per_workload_query_with_reachable_dags(self):
        workload = self.js.call("stageWorkload", self.document)
        queries = workload["queries"]
        self.assertEqual([q["name"] for q in queries], ["q1", "q2"])
        self.assertEqual(queries[0]["source"], self.document["workload"]["queries"][0]["text"])
        for query in queries:
            for key in ("dag", "post_dag"):
                dag = query[key]
                ids = {node["id"] for node in dag["nodes"]}
                self.assertIn(dag["root"], ids)
                for node in dag["nodes"]:
                    self.assertTrue(set(node["children"]) <= ids)
                    self.assertEqual(node["workload_node_id"], node["id"])
                    self.assertTrue(node["kind"] and node["label"])

    def test_post_nodes_carry_the_selected_plans_costs(self):
        selection = self.document["stage3_plan_selection"]
        per_node = selection["costs"][selection["selected"]]["per_node"]
        post = self.js.call("stageWorkload", self.document)["queries"][0]["post_dag"]
        node = post["nodes"][0]
        cost = per_node[str(node["id"])]
        self.assertIn("\ncost ", node["label"])
        self.assertEqual(node["detail"]["cost"], cost)

    def test_candidates_rank_the_selected_plan_first_then_by_cost(self):
        """The Stage 3 table: the selected candidate, the other costed ones
        cheapest first, then the rejected ones with their reasons."""
        selection = self.document["stage3_plan_selection"]
        rows = self.js.call("stageCandidateRows", self.document)
        self.assertEqual(rows[0]["id"], selection["selected"])
        self.assertEqual(rows[0]["status"], "selected")
        self.assertEqual(rows[0]["total"], selection["costs"][selection["selected"]]["total"])
        totals = [row["total"] for row in rows[1:] if row["status"] == "costlier"]
        self.assertEqual(totals, sorted(totals))
        self.assertTrue(all(row["total"] >= rows[0]["total"] for row in rows[1:] if row["status"] == "costlier"))
        rejected = {r["id"]: r["reason"] for r in selection["rejected"] if not r["valid"]}
        for row in rows:
            if row["status"] == "rejected":
                self.assertEqual(row["reason"], rejected[row["id"]])
        physical = self.document["stage2_physical_asap_optimization"]
        self.assertEqual(len(rows), len(physical["candidates"]) + len(physical["pruned"]))
        self.assertTrue(all(row["display_name"] for row in rows))

    def test_failed_selection_has_no_post_dag(self):
        self.document["stage3_plan_selection"] = {"costs": {}, "selected": None, "rejected": []}
        queries = self.js.call("stageWorkload", self.document)["queries"]
        self.assertTrue(all("post_dag" not in q for q in queries))


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
class ViewerSelectionTests(unittest.TestCase):
    def setUp(self):
        self.js = py_mini_racer.MiniRacer()
        source = (HERE / "viewer.js").read_text()
        for name in ["escapeHtml", "bulkSelectionState"]:
            function = re.search(r"^function " + name + r"\(.*?^}", source, re.M | re.S)
            self.assertIsNotNone(function, name)
            self.js.eval(function.group(0))

    def test_bulk_selection_distinguishes_none_some_and_all(self):
        """The checkbox has three states to show, and `indeterminate` is the
        one a plain checkbox cannot: with fifty queries, "some" is normal."""
        cases = {
            # (queries, selected): (hidden, checked, indeterminate)
            (0, 0): (True, False, False),      # nothing loaded
            (50, 0): (False, False, False),    # after Deselect all
            (50, 1): (False, False, True),     # the default single selection
            (50, 49): (False, False, True),
            (50, 50): (False, True, False),    # after Select all
            (1, 1): (False, True, False),      # a one-query workload is "all"
        }
        for (count, selected), expected in cases.items():
            with self.subTest(queries=count, selected=selected):
                state = self.js.call("bulkSelectionState", count, selected)
                self.assertEqual(
                    (state["hidden"], state["checked"], state["indeterminate"]),
                    expected,
                )

    def test_empty_render_resets_bulk_selection(self):
        """Initial load and Clear all hide and reset the bulk checkbox."""
        source = (HERE / "viewer.js").read_text()
        self.js.eval("""
            function element() {
                return {style: {}, classList: {remove() {}, toggle() {}}};
            }
            let queries = [], participants = new Set(), cy = null;
            const tabsEl = element(), scopePickerEl = element(),
                emptyEl = element(), cyOuterEl = element(),
                sidepanel = element(), sideResizeHandle = element(),
                selectAllTab = element(), selectAllToggle = element();
        """)
        for name in ["render", "renderTabs"]:
            function = re.search(r"^function " + name + r"\(.*?^}", source, re.M | re.S)
            self.assertIsNotNone(function, name)
            self.js.eval(function.group(0))
        for checked, indeterminate in [(False, False), (True, False), (False, True)]:
            with self.subTest(checked=checked, indeterminate=indeterminate):
                self.js.eval(f"""
                    selectAllTab.hidden = false;
                    selectAllToggle.checked = {json.dumps(checked)};
                    selectAllToggle.indeterminate = {json.dumps(indeterminate)};
                    render();
                """)
                self.assertTrue(self.js.eval("selectAllTab.hidden"))
                self.assertFalse(self.js.eval("selectAllToggle.checked"))
                self.assertFalse(self.js.eval("selectAllToggle.indeterminate"))


if __name__ == "__main__":
    unittest.main()
