"""Tests for the ASAPPlanner Stage Viewer: the stage-document functions in
stages.js and the page itself (app.js) run in V8 through py_mini_racer with
a stub DOM, plus the server's argument checks and the example manifest.

Run from tools/dag-viewer: `python3 -m unittest test_viewer`. The JS tests
are skipped without py_mini_racer (`pip install py-mini-racer==0.6.0`).
"""

from __future__ import annotations

import json
import re
import time
import unittest
from pathlib import Path

import server

HERE = Path(__file__).resolve().parent

try:
    from py_mini_racer import py_mini_racer
except ImportError:
    py_mini_racer = None


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

    def test_planner_document_is_valid_after_normalizing(self):
        """`stage_pipeline` writes operators as `{"NonASAP": {"Scan": ...}}`
        and logical stages as flat DAGs; normalized, they validate, and a
        flat DAG gets one edge per operator input."""
        doc = json.loads((HERE / "examples" / "planner-layering-example1.json").read_text())
        normalized = self.js.call("normalizeStagePipeline", doc)
        self.assertEqual(self.validate(normalized), [])
        stage0 = normalized["stage0_logical"]["dag"]
        self.assertEqual(stage0["nodes"][0]["payload"]["kind"], "relational")
        self.assertEqual(stage0["nodes"][0]["payload"]["operator"]["kind"], "scan")
        self.assertTrue(stage0["edges"])
        self.assertTrue(all(e["producer"] < e["consumer"] for e in stage0["edges"]))
        self.assertEqual(self.js.call("normalizeStagePipeline", self.doc), self.doc)

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

    def test_partial_documents_say_how_many_plans_they_carry(self):
        """`shown_of` from `stage_pipeline --max-candidates` reads as N of M plans."""
        doc = {"shown_of": {"logical": 486, "physical": 486, "priced": 486},
               "stage2_physical_asap": {"candidates": [{"id": "P1"}, {"id": "P2"}]}}
        self.assertEqual(self.js.call("shownOfText", doc),
                         "showing 2 of 486 plans, cheapest first; Stage 3 priced 486 and selected among all of them")
        self.assertEqual(self.js.call("shownOfText", {"stage2_physical_asap": {"candidates": []}}), "")

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


class PageTests(unittest.TestCase):
    """The page's scripts and element ids line up."""

    def test_scripts_exist(self):
        page = (HERE / "index.html").read_text()
        for src in re.findall(r'<script src="([^"]+)"', page):
            with self.subTest(src):
                self.assertTrue((HERE / src).exists())

    def test_every_element_the_scripts_use_is_on_the_page(self):
        page = (HERE / "index.html").read_text()
        ids = set(re.findall(r'id="([^"]+)"', page))
        for script in ("app.js", "editor.js"):
            used = set(re.findall(r"\$\('([^']+)'\)", (HERE / script).read_text()))
            used = {u for u in used if not u.startswith("cy") or u in ids}
            with self.subTest(script):
                self.assertLessEqual(used - {"cy"}, ids)

    def test_examples_are_stage_pipeline_examples(self):
        examples = json.loads((HERE / "examples.json").read_text())
        tool = (HERE.parents[1] / "crates/devtools/src/bin/stage_pipeline.rs").read_text()
        self.assertEqual(len({e["id"] for e in examples}), len(examples))
        for example in examples:
            with self.subTest(example["id"]):
                self.assertIn(f'Some("{example["example"]}")', tool)
                self.assertTrue(example["file"].startswith("out/"))
                self.assertTrue(example["story"])


class ServerTests(unittest.TestCase):
    """The editor's request becomes `stage_pipeline` arguments."""

    def test_queries_and_accuracy_become_arguments(self):
        args = server.plan_args({"queries": ["up", "rate(x[1m])"], "epsilon": 0.01, "delta": 0.001, "interval_ms": 1000}, "o.json")
        self.assertEqual(args[1:], ["--promql", "up", "--promql", "rate(x[1m])", "--epsilon", "0.01",
                                    "--delta", "0.001", "--interval-ms", "1000", "--out", "o.json"])

    def test_without_epsilon_the_queries_are_exact(self):
        args = server.plan_args({"queries": ["up"]}, "o.json")
        self.assertNotIn("--epsilon", args)
        self.assertIn("15000", args)

    def test_bad_requests_are_rejected(self):
        for payload in ({}, {"queries": []}, {"queries": [""]}, {"queries": ["up"], "delta": 0.1},
                        {"queries": ["up"], "epsilon": 1.5}, {"queries": ["up"], "interval_ms": 0}):
            with self.subTest(payload):
                with self.assertRaises(ValueError):
                    server.plan_args(payload, "o.json")

    FLOWS = {"name": "flows", "columns": [{"name": "ts", "type": "timestamp", "nullable": False},
                                          {"name": "src_ip", "type": "utf8"}], "time_index": 0}

    def test_sql_queries_and_tables_become_arguments(self):
        args = server.plan_args({"language": "sql", "queries": ["SELECT COUNT(*) FROM flows"],
                                 "tables": [self.FLOWS], "epsilon": 0.02}, "o.json")
        self.assertEqual(args[1:5], ["--table", json.dumps(self.FLOWS), "--sql", "SELECT COUNT(*) FROM flows"])
        self.assertEqual(args[5:7], ["--epsilon", "0.02"])
        self.assertNotIn("--promql", args)

    def test_bad_sql_requests_are_rejected(self):
        column = {"name": "x", "type": "int64"}
        sql = lambda tables: {"language": "sql", "queries": ["SELECT 1"], "tables": tables}
        for payload in ({"language": "cypher", "queries": ["x"]},
                        {"queries": ["up"], "tables": [self.FLOWS]},
                        sql({"name": "t"}), sql([{"name": "", "columns": [column]}]),
                        sql([{"name": "t", "columns": []}]), sql([{"name": "t", "columns": [column] * 501}]),
                        sql([{"name": "t", "columns": [{"name": "x", "type": "decimal"}]}]),
                        sql([{"name": "t", "columns": [{"name": "", "type": "utf8"}]}]),
                        sql([{"name": "t", "columns": [column], "time_index": 1}]),
                        sql([self.FLOWS, self.FLOWS]),
                        sql([{"name": f"t{i}", "columns": [column]} for i in range(51)])):
            with self.subTest(payload):
                with self.assertRaises(ValueError):
                    server.plan_args(payload, "o.json")


STUB_DOM = r"""
var __els = {}; var __files = {};
function __el(id) {
  if (!__els[id]) __els[id] = { id, innerHTML: '', textContent: '', hidden: false, value: '', dataset: {},
    addEventListener() {}, setAttribute() {}, click() {}, querySelectorAll() { return []; } };
  return __els[id];
}
var document = { getElementById: __el, documentElement: {}, body: { classList: { add() {}, remove() {} } }, addEventListener() {} };
var window = { matchMedia() { return { addEventListener() {} }; } };
var location = { hash: '', search: '', pathname: '/' };
var history = { replaceState(a, b, h) { location.hash = h; } };
function getComputedStyle() { return { getPropertyValue() { return ''; } }; }
function MutationObserver() { this.observe = function () {}; }
var URLSearchParams = function (s) { this.get = (k) => { const m = s.match(new RegExp('[?&]' + k + '=([^&]*)')); return m ? decodeURIComponent(m[1]) : null; }; };
var __nodes = 0;
function cytoscape(o) { __nodes += o.elements.length; return { on() {}, destroy() {}, style() {} }; }
function fetch(u) { return Promise.resolve({ ok: u in __files, status: 404, json() { return Promise.resolve(JSON.parse(__files[u])); } }); }
"""


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
class AppTests(unittest.TestCase):
    """app.js loads documents and fills the page (stub DOM, no browser)."""

    def page(self, search="", files=None, hash_=""):
        js = py_mini_racer.MiniRacer()
        js.eval(STUB_DOM)
        for name in ("node-style.js", "stages.js"):
            js.eval((HERE / name).read_text())
        for url, text in (files or {}).items():
            js.eval(f"__files[{json.dumps(url)}] = {json.dumps(text)};")
        js.eval(f"location.search = {json.dumps(search)}; location.hash = {json.dumps(hash_)};")
        js.eval((HERE / "app.js").read_text())
        time.sleep(0.05)
        return js

    def text(self, js, element):
        return js.eval(f"__els[{json.dumps(element)}].innerHTML.replace(/<[^>]+>/g, '')")

    def test_doc_parameter_opens_one_document(self):
        sample = STAGE_FIXTURE.read_text()
        js = self.page("?doc=examples/s.json", {"examples/s.json": sample})
        doc = json.loads(sample)
        self.assertIn("Candidates per stage", self.text(js, "counts"))
        self.assertIn("✓ selected", self.text(js, "lane2-meta"))
        self.assertEqual(js.eval("__els.pick2.value"), doc["stage3_selection"]["selected"])
        self.assertGreater(js.eval("__nodes"), 0)

    def test_examples_become_tabs_and_the_hash_picks_one(self):
        sample = STAGE_FIXTURE.read_text()
        manifest = json.dumps([{"id": "a", "name": "A", "file": "out/a.json", "story": "story A"},
                               {"id": "b", "name": "B", "file": "out/b.json", "story": "story B"}])
        js = self.page("", {"examples.json": manifest, "out/a.json": sample, "out/b.json": sample}, "#b")
        self.assertIn('data-id="b"', js.eval("__els.tabs.innerHTML"))
        self.assertEqual(js.eval("__els.story.textContent"), "story B")

    def test_an_invalid_document_shows_an_error(self):
        js = self.page("?doc=bad.json", {"bad.json": json.dumps({"format": "nope"})})
        self.assertIn("Could not load", js.eval("__els.queries.innerHTML"))

    def test_node_groups(self):
        js = self.page()
        self.assertEqual([js.call("nodeGroup", k) for k in ("Scan", "SummaryAgg", "Filter")], ["source", "summary", "rel"])


if __name__ == "__main__":
    unittest.main()
