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


def named_dag(name: str, source: str = "SELECT 1") -> dict:
    """A minimal well-formed NamedDAG: one leaf Scan node, its own root."""
    return {
        "name": name,
        "source": source,
        "dag": {
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
    def test_boundary_terms_and_provenance_survive_standalone_export(self):
        """The standalone viewer retains byte totals, physical terms, and provenance."""
        dag = named_dag("boundary-example")
        annotation = {
            "value": 1080.0,
            "unit": "CostUnits",
            "source": "Modeled",
            "model_version": "physical-boundary-bytes-v1+bytes-v1",
            "evidence_version": "evidence-v1",
            "inputs": [
                {"name": "network_bytes", "value": 480, "unit": "bytes"},
                {"name": "materialization_bytes", "value": 40, "unit": "bytes"},
                {"name": "physical_node:scan:network_bytes", "value": 480, "unit": "bytes"},
                {"name": "boundary:persist:materialization_bytes", "value": 40, "unit": "bytes"},
            ],
        }
        dag["dag"]["nodes"][0]["selected_cost"] = annotation
        html = render({"queries": [dag]})
        for term in annotation["inputs"]:
            self.assertIn(term["name"], html)
        self.assertIn(annotation["model_version"], html)
        self.assertIn(annotation["evidence_version"], html)

    def test_merges_queries_across_files_in_order(self):
        with tempfile.TemporaryDirectory() as d:
            f1 = Path(d) / "a.json"
            f2 = Path(d) / "b.json"
            f1.write_text(json.dumps({"queries": [named_dag("q1")]}))
            f2.write_text(json.dumps({"queries": [named_dag("q2")]}))

            workload = load_workload([f1, f2])

        self.assertEqual([q["name"] for q in workload["queries"]], ["q1", "q2"])

    def test_colliding_name_is_disambiguated_by_source_filename(self):
        # Matches viewer.js's loadFiles(): a later file's query named the
        # same as an earlier one gets " (filename)" appended instead of
        # silently overwriting or erroring.
        with tempfile.TemporaryDirectory() as d:
            f1 = Path(d) / "a.json"
            f2 = Path(d) / "b.json"
            f1.write_text(json.dumps({"queries": [named_dag("q1")]}))
            f2.write_text(json.dumps({"queries": [named_dag("q1")]}))

            workload = load_workload([f1, f2])

        self.assertEqual(workload["queries"][0]["name"], "q1")
        self.assertEqual(workload["queries"][1]["name"], "q1 (b.json)")

    def test_non_colliding_names_pass_through_unchanged(self):
        with tempfile.TemporaryDirectory() as d:
            f1 = Path(d) / "a.json"
            f1.write_text(json.dumps({"queries": [named_dag("q1"), named_dag("q2")]}))

            workload = load_workload([f1])

        self.assertEqual([q["name"] for q in workload["queries"]], ["q1", "q2"])


class RenderTests(unittest.TestCase):
    def test_no_script_src_tags_remain(self):
        # Substring-only "<script src=" would also match viewer.js's own
        # header comment (which documents index.html's <script src=...>
        # usage in prose) once that file is inlined verbatim -- so match the
        # real tag shape instead of a bare substring.
        workload = {"queries": [named_dag("q1"), named_dag("q2")]}
        html = render(workload)
        self.assertNotRegex(html, r'<script src="[^"]+"></script>')

    def test_embedded_workload_round_trips(self):
        workload = {"queries": [named_dag("q1"), named_dag("q2")]}
        html = render(workload)

        m = re.search(
            r'<script type="application/json" id="embedded-workload">(.*?)</script>',
            html,
            re.S,
        )
        self.assertIsNotNone(m, "embedded-workload <script> tag not found")
        self.assertEqual(json.loads(m.group(1)), prepare_workload(workload))

    def test_standalone_export_carries_the_bulk_selection_control(self):
        """A generated page gets Select-all for free: render.py inlines the
        markup and viewer.js verbatim, so neither fix needs its own step."""
        html = render({"queries": [named_dag("q1"), named_dag("q2")]})
        self.assertIn('id="selectAllToggle"', html)
        self.assertIn("function bulkSelectionState(", html)
        # And it must stay a selection control, not a second Clear all: the
        # embedded workload is parsed once at load, so dropping `queries`
        # from a generated page is unrecoverable without a reload.
        self.assertNotRegex(
            html, r"selectAllToggle\.addEventListener[^}]*queries = \[\]"
        )

    def test_standalone_export_lanes_are_pannable(self):
        """Dragging the lane background pans the viewport in the generated
        page too -- `grabbable: false` alone made it a dead zone."""
        html = render({"queries": [named_dag("q1")]})
        lanes = re.findall(r"classes: 'laneParent'[^}]*}", html)
        self.assertEqual(len(lanes), 4, "expected the union, single-query, stage, and not-produced lanes")
        for lane in lanes:
            self.assertIn("pannable: true", lane)

    def test_render_does_not_mutate_callers_workload(self):
        workload = {"queries": [named_dag("q1")]}
        original = json.loads(json.dumps(workload))
        render(workload)
        self.assertEqual(workload, original)

    def test_render_adds_no_legacy_mode_config(self):
        # Bare "__DAG_RENDER__" also matches viewer.js's own header comment
        # (which documents the window.__DAG_RENDER__ config object in prose)
        # once that file is inlined verbatim -- match the actual assignment
        # statement instead.
        workload = {"queries": [named_dag("q1"), named_dag("q2")]}
        html = render(workload)
        self.assertNotRegex(html, r"window\.__DAG_RENDER__ =")

    def test_embedded_data_placed_before_viewer_js_body(self):
        # viewer.js's top-level startup code reads #embedded-workload and
        # synchronously as soon as it runs, so it must appear earlier in the
        # document than viewer.js's own inlined
        # <script> block.
        workload = {"queries": [named_dag("q1"), named_dag("q2")]}
        html = render(workload)
        embedded_pos = html.index('id="embedded-workload"')
        viewer_pos = html.index("cytoscape.use(window.cytoscapeDagre)")  # viewer.js's first line
        self.assertLess(embedded_pos, viewer_pos)

    def test_inlined_edge_cost_key_contains_no_literal_nul(self):
        html = render({"queries": [named_dag("q1")]})
        self.assertNotIn("\x00", html)
        self.assertIn(r"\u0000", html)

    def test_angle_bracket_in_query_source_does_not_break_out_of_script_tag(self):
        # A pathological (but legal JSON) query source containing a literal
        # "</script>" substring must not prematurely close the embedded
        # <script type="application/json"> tag when the browser parses it.
        workload = {"queries": [named_dag("q1", source="SELECT '</script><script>evil()</script>'")]}
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

    def test_project_names_columns_from_fields_and_legacy_columns(self):
        """Exports write `fields`; older ones wrote `columns`. Both name columns."""
        for key, dtype in (("fields", {"Plain": "utf8"}), ("columns", "utf8")):
            node = {
                "kind": "Project",
                "detail": {"cols": [0]},
                "schema": {key: [{"name": "service", "dtype": dtype, "nullable": False}]},
            }
            self.assertEqual(_semantic_label(node), "Project\ncolumns: service", key)

    def test_prepares_before_after_and_whole_post_asap_dags(self):
        node = {
            "id": 0,
            "kind": "Aggregate",
            "label": "Aggregate(1 measures)",
            "detail": {"measures": [{"kind": "avg", "col": 3}]},
            "children": [],
        }
        def dag():
            return {"nodes": [dict(node)], "root": 0}
        workload = {
            "queries": [
                {
                    "dag": dag(),
                    "post_dag": dag(),
                    "replacements": [{"before": dag(), "after": {"dag": dag()}}],
                }
            ]
        }
        prepared = prepare_workload(workload)
        query = prepared["queries"][0]
        labels = [
            query["dag"]["nodes"][0]["label"],
            query["post_dag"]["nodes"][0]["label"],
        ]
        self.assertEqual(labels, ["Aggregate\nmeasure: avg(col[3])"] * 2)
        self.assertEqual(
            query["replacements"][0]["before"]["nodes"][0]["label"],
            "Aggregate(1 measures)",
        )

    def test_replacement_sub_dags_are_not_prepared_for_the_current_viewer(self):
        def dag():
            return {"nodes": [{"id": 0, "kind": "Scan", "label": "legacy", "detail": {}, "children": []}], "root": 0}
        workload = {"queries": [{"dag": dag(), "replacements": [{"before": dag(), "after": {"dag": dag()}}]}]}
        replacement = prepare_workload(workload)["queries"][0]["replacements"][0]
        self.assertEqual(replacement["before"]["nodes"][0]["label"], "legacy")
        self.assertEqual(replacement["after"]["dag"]["nodes"][0]["label"], "legacy")


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
                     "computeSelectionWorkloadCost", "bulkSelectionState"]:
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

        return {"sourceBatch": batch, "post_dag": {"nodes": [{"decision": {
            "id": 1,
            "baseline_cost": annotation(10, profile),
            "selected_cost": annotation(4, selected_profile or profile),
        }}]}}

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
            let viewMode = 'prepost', stageDoc = null;
            function renderModeToggle() {}
            const tabsRowEl = element(), tabsEl = element(), scopePickerEl = element(),
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

    def test_cache_profile_and_inputs_are_rendered(self):
        """The sidebar exposes the assumptions behind a cache-adjusted cost."""
        annotation = self.query("warm-cache-v1")["post_dag"]["nodes"][0]["decision"]["baseline_cost"]
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


STAGE_FIXTURE = HERE / "examples" / "stage-pipeline.sample.json"


class StageFixtureTests(unittest.TestCase):
    """Pure-Python checks of the committed asap-stage-pipeline/v1 fixture,
    so its cross-references hold even where no JS engine is installed."""

    def test_fixture_cross_references_resolve(self):
        """Roots match the workload, costs live only in Stage 3, and every
        physical candidate is selected or rejected exactly once."""
        doc = json.loads(STAGE_FIXTURE.read_text())
        self.assertEqual(doc["format"], "asap-stage-pipeline/v1")
        query_count = len(doc["workload"]["queries"])
        self.assertEqual(query_count, 2)
        dags = [doc["stage0_logical"]["dag"]]
        dags += [c["dag"] for c in doc["stage1_logical_asap"]["candidates"]]
        dags += [c["dag"] for c in doc["stage2_physical_asap"]["candidates"]]
        for dag in dags:
            self.assertEqual(len(dag["roots"]), query_count)
        logical = {c["id"] for c in doc["stage1_logical_asap"]["candidates"]}
        physical = {c["id"]: c for c in doc["stage2_physical_asap"]["candidates"]}
        for candidate in physical.values():
            self.assertIn(candidate["from_logical"], logical)
            self.assertNotIn("cost", candidate)
        selection = doc["stage3_selection"]
        for pid, cost in selection["costs"].items():
            node_ids = {str(node["id"]) for node in physical[pid]["dag"]["nodes"]}
            self.assertLessEqual(set(cost["per_node"]), node_ids)
            self.assertAlmostEqual(sum(e["cost"] for e in cost["per_node"].values()), cost["total"])
        accounted = [selection["selected"]] + [e["id"] for e in selection["rejected"]]
        self.assertCountEqual(accounted, physical)
        self.assertEqual({e["valid"] for e in selection["rejected"]}, {True, False})


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
class StagePipelineTests(unittest.TestCase):
    def setUp(self):
        self.js = py_mini_racer.MiniRacer()
        self.js.eval((HERE / "stages.js").read_text())
        self.doc = json.loads(STAGE_FIXTURE.read_text())

    def copy(self):
        return json.loads(json.dumps(self.doc))

    def validate(self, doc):
        return self.js.call("validateStagePipeline", doc)

    def test_fixture_is_a_valid_document(self):
        """The committed sample passes the viewer's own shape validation."""
        self.assertEqual(self.validate(self.doc), [])

    def test_partial_documents_are_valid(self):
        """A run that stopped after Stage 1 or Stage 2 still loads."""
        through_stage1 = self.copy()
        del through_stage1["stage2_physical_asap"], through_stage1["stage3_selection"]
        self.assertEqual(self.validate(through_stage1), [])
        through_stage2 = self.copy()
        del through_stage2["stage3_selection"]
        self.assertEqual(self.validate(through_stage2), [])

    def test_legacy_single_root_is_accepted(self):
        """A one-query document written with `root` instead of `roots` loads."""
        doc = self.copy()
        doc["workload"]["queries"] = doc["workload"]["queries"][:1]
        dag = doc["stage0_logical"]["dag"]
        dag["root"] = dag.pop("roots")[0]
        del doc["stage1_logical_asap"], doc["stage2_physical_asap"], doc["stage3_selection"]
        self.assertEqual(self.validate(doc), [])

    def test_validation_rejects_contract_violations(self):
        """Broken references, missing physical fields, root/query mismatches,
        and unaccounted candidates are reported, not rendered."""
        physical = lambda d: d["stage2_physical_asap"]["candidates"][0]
        selection = lambda d: d["stage3_selection"]
        cases = {
            "format": lambda d: d.update(format="asap-stage-pipeline/v0"),
            "from_logical": lambda d: physical(d).update(from_logical="L9"),
            "selected": lambda d: selection(d).update(selected="P9"),
            "rejected also selected": lambda d: selection(d)["rejected"].append({"id": selection(d)["selected"], "valid": True, "reason": "x"}),
            "unaccounted candidate": lambda d: selection(d)["rejected"].pop(),
            "valid flag": lambda d: selection(d)["rejected"][0].pop("valid"),
            "per_node": lambda d: selection(d)["costs"]["P1"]["per_node"].update({"99": {"cost": 1}}),
            "cost total": lambda d: selection(d)["costs"]["P1"].pop("total"),
            "cost for unknown candidate": lambda d: selection(d)["costs"].update({"P9": {"total": 1, "unit": "u"}}),
            "timing": lambda d: physical(d)["dag"]["nodes"][0].pop("output_state"),
            "data_state": lambda d: physical(d)["dag"]["edges"][0].pop("data_state"),
            "edge endpoint": lambda d: d["stage0_logical"]["dag"]["edges"][0].update(producer=42),
            "logical root shape": lambda d: d["stage0_logical"]["dag"]["roots"].__setitem__(0, 3),
            "physical root shape": lambda d: physical(d)["dag"]["roots"].__setitem__(0, {"Operator": 3}),
            "roots per query": lambda d: d["stage0_logical"]["dag"]["roots"].pop(),
            "stage 2 without stage 1": lambda d: d.pop("stage1_logical_asap"),
            "stage 3 without stage 2": lambda d: d.pop("stage2_physical_asap"),
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                doc = self.copy()
                mutate(doc)
                self.assertNotEqual(self.validate(doc), [])

    def test_ranking_uses_stage3_costs_and_outcomes(self):
        """Candidates rank cheapest first from stage3_selection.costs, with
        selected, valid-but-costlier, and invalid kept apart."""
        ranked = self.js.call("rankPhysicalCandidates", self.doc)
        totals = [row["total"] for row in ranked]
        self.assertEqual(totals, sorted(totals))
        self.assertEqual([row["rank"] for row in ranked], list(range(1, len(ranked) + 1)))
        by_id = {row["id"]: row for row in ranked}
        selection = self.doc["stage3_selection"]
        self.assertEqual(by_id[selection["selected"]]["status"], "selected")
        for entry in selection["rejected"]:
            row = by_id[entry["id"]]
            self.assertEqual(row["status"], "rejected_valid" if entry["valid"] else "rejected_invalid")
            self.assertEqual(row["reason"], entry["reason"])
            self.assertEqual(row["source"], selection["costs"][entry["id"]]["source"])

    def test_ranking_without_stage3_has_no_costs(self):
        """Stage 2 alone carries no cost, so nothing is ranked."""
        doc = self.copy()
        del doc["stage3_selection"]
        ranked = self.js.call("rankPhysicalCandidates", doc)
        self.assertEqual([row["id"] for row in ranked], [c["id"] for c in doc["stage2_physical_asap"]["candidates"]])
        for row in ranked:
            self.assertEqual((row["total"], row["rank"], row["status"]), (None, None, "no_selection"))

    def test_ranking_puts_uncosted_candidates_last_and_keeps_ties_in_order(self):
        """Ties keep document order; a candidate Stage 3 did not cost is unranked."""
        doc = self.copy()
        costs = doc["stage3_selection"]["costs"]
        for cost in costs.values():
            cost["total"] = 5
        del costs["P1"]
        ranked = self.js.call("rankPhysicalCandidates", doc)
        self.assertEqual([row["id"] for row in ranked], ["P2", "P3", "P4", "P1"])
        self.assertEqual([row["rank"] for row in ranked], [1, 2, 3, None])

    def test_lane_construction_for_each_stage(self):
        """Each lane has one parent, one element per node and edge, edges run
        producer -> consumer, and every root is labelled with its query."""
        query_ids = [q["id"] for q in self.doc["workload"]["queries"]]
        logical_dag = self.doc["stage0_logical"]["dag"]
        lane = self.js.call("stageLaneElements", "stage0", "Logical", logical_dag, {"queryIds": query_ids})
        parent, *rest = lane
        self.assertTrue(parent["data"]["isLane"])
        nodes = [e for e in rest if "source" not in e["data"]]
        edges = [e for e in rest if "source" in e["data"]]
        self.assertEqual(len(nodes), len(logical_dag["nodes"]))
        self.assertEqual(len(edges), len(logical_dag["edges"]))
        first = logical_dag["edges"][0]
        self.assertEqual((edges[0]["data"]["source"], edges[0]["data"]["target"]),
                         (f"stage0-n{first['producer']}", f"stage0-n{first['consumer']}"))
        roots = {n["data"]["stageNode"]["id"]: n["data"]["rootFor"] for n in nodes if n["data"]["root"]}
        self.assertEqual(roots, {r["Operator"]: [qid] for r, qid in zip(logical_dag["roots"], query_ids)})
        for node in nodes:
            if node["data"]["root"]:
                self.assertIn(f"root of {node['data']['rootFor'][0]}", node["data"]["label"])
        self.assertEqual(nodes[0]["data"]["kind"], "Scan")
        self.assertEqual(nodes[0]["data"]["label"], "Scan\nsource: http_requests_total")

        selected = self.doc["stage3_selection"]["selected"]
        candidate = next(c for c in self.doc["stage2_physical_asap"]["candidates"] if c["id"] == selected)
        cost = self.doc["stage3_selection"]["costs"][selected]
        lane = self.js.call("stageLaneElements", "stage2", "Physical", candidate["dag"],
                            {"physical": True, "costPerNode": cost["per_node"], "queryIds": query_ids})
        nodes = [e for e in lane[1:] if "source" not in e["data"]]
        for node in nodes:
            label = node["data"]["label"]
            self.assertIn("⏱ query time", label)
            self.assertIn(f"cost {cost['per_node'][str(node['data']['stageNode']['id'])]['cost']:g}", label)
        self.assertEqual([n["data"]["stageNode"]["id"] for n in nodes if n["data"]["root"]],
                         candidate["dag"]["roots"])

    def test_physical_lane_without_stage3_has_no_cost(self):
        """Without Stage 3 costs, physical nodes show timing but no cost."""
        candidate = self.doc["stage2_physical_asap"]["candidates"][0]
        lane = self.js.call("stageLaneElements", "stage2", "Physical", candidate["dag"], {"physical": True})
        for node in lane[1:]:
            if "source" not in node["data"]:
                self.assertNotIn("cost", node["data"]["label"])

    def test_summary_nodes_print_their_configuration(self):
        """A summary node names its family's parameters and whether it keeps one state per group or one shared state."""
        per_group = {
            "kind": "summary_agg",
            "family": {"Sketch": [{"algorithm": "CmsWithHeap", "category": "TopK",
                                   "params": {"CmsWithHeap": {"depth": 7, "heap_size": 100, "width": 272}}},
                                  "PerSubpopulationInstance"]},
            "grouping": "PerSubpopulationInstance",
        }
        lines = self.js.call("stageNodeLines", {"id": 0, "payload": per_group}, None)
        self.assertIn("summary: CmsWithHeap · depth 7 · heap 100 · width 272", lines)
        self.assertIn("instances: one per group", lines)
        shared = dict(per_group, grouping={"SharedMultiSubpopulation": {
            "kind": "HydraCms", "params": {"HydraCms": {"width": 272, "depth": 7}}}})
        lines = self.js.call("stageNodeLines", {"id": 0, "payload": shared}, None)
        self.assertIn("instances: one shared HydraCms · width 272 · depth 7", lines)
        exact = {"kind": "summary_agg", "family": {"ExactAggregate": ["Sum", "Sum"]},
                 "grouping": "PerSubpopulationInstance"}
        self.assertIn("summary: exact Sum", self.js.call("stageNodeLines", {"id": 0, "payload": exact}, None))

    def test_deployment_inputs_are_listed(self):
        """The document's deployment section reads as capability, cost-model and accuracy-model rows."""
        deployment = {
            "capabilities": {
                "source": "asap_executor::capabilities",
                "summaries": [{"summary": "exact Sum", "readouts": []}, {"summary": "exact Count", "readouts": []},
                              {"summary": "Kll", "readouts": ["Quantile"]},
                              {"summary": "CmsWithHeap", "readouts": ["TopK"]}],
                "ingestion_time": True, "query_time_retention": False, "memory_budget_bytes": None,
                "raw_data_retained": False, "raw_bytes_per_sample": 16,
            },
            "cost_model": {"name": "analytical-cost-v2", "unit": "cost_per_second",
                           "calibration": {"version": "illustrative-v2", "cost_per_retained_byte_second": 1.25e-7}},
            "accuracy_model": {"name": "DefaultAccuracyModel", "evidence": "none"},
        }
        rows = dict(self.js.call("deploymentRows", deployment))
        self.assertEqual(rows["Exact aggregates"], "Sum, Count")
        self.assertEqual(rows["Sketches → estimates"], "Kll → Quantile · CmsWithHeap → TopK")
        self.assertEqual(rows["Ingestion-time maintenance"], "supported")
        self.assertEqual(rows["Memory budget"], "none")
        self.assertEqual(rows["Raw data"], "not kept by the deployment: query-time plans pay for 16 bytes per sample")
        self.assertIn("cost_per_retained_byte_second 1.25e-7", rows["Cost model"])
        self.assertEqual(rows["Accuracy model"], "DefaultAccuracyModel · evidence: none")
        unrestricted = dict(deployment, capabilities=dict(deployment["capabilities"], summaries=None, raw_data_retained=True))
        rows = dict(self.js.call("deploymentRows", unrestricted))
        self.assertEqual(rows["Summaries"], "unrestricted")
        self.assertEqual(rows["Raw data"], "kept by the deployment (not charged)")
        self.assertEqual(self.js.call("deploymentRows", None), [])

    def test_payload_kinds_map_onto_node_style_names(self):
        """Wire payload kinds reuse node-style.js categories."""
        cases = {
            "Scan": {"kind": "relational", "operator": {"kind": "scan"}},
            "TimeRange": {"kind": "relational", "operator": {"kind": "time_range"}},
            "SQLWindowFunc": {"kind": "relational", "operator": {"kind": "sql_window_func"}},
            "SummaryAgg": {"kind": "summary_agg"},
            "FinalizeExactAccumulator": {"kind": "finalize_exact_accumulator"},
        }
        style = (HERE / "node-style.js").read_text()
        for expected, payload in cases.items():
            with self.subTest(expected):
                self.assertEqual(self.js.call("stagePayloadKind", payload), expected)
                self.assertIn(f'"{expected}":', style)

if __name__ == "__main__":
    unittest.main()
