"""Tests for the ASAPPlanner Stage Viewer: the stage-document functions in
stages.js and the page itself (app.js) run in V8 through py_mini_racer with
a stub DOM, plus the page's files and the example list.

Run from tools/dag-viewer: `python3 -m unittest test_viewer`. The JS tests
are skipped without py_mini_racer (`pip install py-mini-racer==0.6.0`); the
scalar-query tests need a built stage_pipeline (`cargo build -p
asap-devtools --bin stage_pipeline`; CARGO_TARGET_DIR is honored).
"""

from __future__ import annotations

import json
import re
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

import server

HERE = Path(__file__).resolve().parent
# The one committed example document, which crates/devtools/tests/stage_pipeline.rs
# keeps current: #509 Example 1, with its Pass 2 identical-expression candidates.
EXAMPLE1 = HERE / "examples" / "planner-layering-example1.json"
# Standalone scalar queries at positions 0, 2 and 4 (first, middle, last):
# scalar(sum(m)), sum(n), time(), sum by (job) (k), 1 + 2.
SCALAR_QUERIES = ["scalar(sum(m))", "sum(n)", "time()", "sum by (job) (k)", "1 + 2"]

try:
    from py_mini_racer import py_mini_racer
except ImportError:
    py_mini_racer = None


def example1() -> dict:
    return json.loads(EXAMPLE1.read_text())


def stage_pipeline(*args: str) -> dict:
    """A document written on demand by the built stage_pipeline."""
    with tempfile.TemporaryDirectory() as tmp:
        out = Path(tmp) / "doc.json"
        subprocess.run([str(server.BINARY), *args, "--out", str(out)], check=True, capture_output=True)
        return json.loads(out.read_text())


def stages_js():
    js = py_mini_racer.MiniRacer()
    js.eval((HERE / "node-style.js").read_text().replace("console.warn", "(() => {})"))
    js.eval((HERE / "stages.js").read_text())
    return js


def lane_nodes(elements):
    return [e for e in elements if "source" not in e["data"]]


def lane_edges(elements):
    return [e for e in elements if "source" in e["data"]]


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
class ValidationTests(unittest.TestCase):
    """The viewer refuses a document that breaks the format rather than
    guessing what it means."""

    def setUp(self):
        self.js = stages_js()
        self.doc = example1()

    def validate(self, doc):
        return self.js.call("validateStagePipeline", doc)

    def test_example1_is_valid(self):
        self.assertEqual(self.validate(self.doc), [])

    def test_pass1_failure_is_valid(self):
        """A run whose Pass 1 failed has Stage 0, the error and no plan."""
        self.doc["stage1_logical_asap_optimization"] = {"combinations": 0, "capped": False, "candidates": [], "error": "Pass 1: bad"}
        self.doc["stage2_physical_asap_optimization"] = {"candidates": [], "pruned": []}
        self.doc["stage3_plan_selection"] = {"costs": {}, "selected": None, "rejected": []}
        self.assertEqual(self.validate(self.doc), [])

    def test_failed_selection_and_composition_errors_are_valid(self):
        self.doc["stage1_logical_asap_optimization"]["candidates"].append({"id": "L99", "display_name": "x", "error": "cannot substitute"})
        self.doc["stage3_plan_selection"] = {"costs": {}, "selected": None, "rejected": [], "error": "no data workload"}
        self.assertEqual(self.validate(self.doc), [])

    def test_contract_violations_are_reported(self):
        stage0 = lambda d: d["stage0_query_language_frontends"]["dag"]
        logical = lambda d: d["stage1_logical_asap_optimization"]["candidates"]
        physical = lambda d: d["stage2_physical_asap_optimization"]["candidates"]
        selection = lambda d: d["stage3_plan_selection"]
        cases = {
            "format": lambda d: d.update(format="asap-stage-pipeline/v0"),
            "no queries": lambda d: d["workload"].update(queries=[]),
            "roots per query": lambda d: stage0(d)["roots"].pop(),
            "root names no node": lambda d: stage0(d)["roots"].__setitem__(0, {"Operator": 99}),
            "logical root shape": lambda d: stage0(d)["roots"].__setitem__(0, 3),
            "child names no node": lambda d: stage0(d)["nodes"][1]["operator"]["NonASAP"]["TimeRange"].update(child=42),
            "duplicate logical id": lambda d: logical(d).append(dict(logical(d)[0])),
            "logical neither dag nor error": lambda d: logical(d)[0].pop("dag"),
            "from_logical": lambda d: physical(d)[0].update(from_logical="L99"),
            "physical root shape": lambda d: physical(d)[0]["dag"]["roots"].__setitem__(0, {"Operator": 3}),
            "physical roots per query": lambda d: physical(d)[0]["dag"]["roots"].pop(),
            "edge endpoint": lambda d: physical(d)[0]["dag"]["edges"][0].update(producer=42),
            "pruned from_logical": lambda d: d["stage2_physical_asap_optimization"]["pruned"].append({"from_logical": "L99", "display_name": "x", "reason": "r"}),
            "selected": lambda d: selection(d).update(selected="P99"),
            "rejected also selected": lambda d: selection(d)["rejected"].append({"id": selection(d)["selected"], "valid": True, "reason": "x"}),
            "unaccounted candidate": lambda d: selection(d)["rejected"].pop(),
            "valid flag": lambda d: selection(d)["rejected"][0].pop("valid"),
            "per_node": lambda d: selection(d)["costs"]["P1"]["per_node"].update({"99": {"cost": 1, "multiplier": 1}}),
            "cost total": lambda d: selection(d)["costs"]["P1"].pop("total"),
            "cost for unknown candidate": lambda d: selection(d)["costs"].update({"P99": {"total": 1, "unit": "u", "per_node": {}}}),
            "missing stage": lambda d: d.pop("stage2_physical_asap_optimization"),
            "binary operand names no node": lambda d: stage0(d)["nodes"].append(
                {"operator": {"NonASAP": {"BinaryOp": {"lhs": 0, "rhs": 42, "operator": {}, "return_bool": False}}}}),
            "scalar root past the queries": lambda d: physical(d)[0]["dag"].update(
                roots=physical(d)[0]["dag"]["roots"][:1], scalar_roots=[[2, "EvalTimestamp"]]),
            "scalar roots out of order": lambda d: physical(d)[0]["dag"].update(
                roots=[], scalar_roots=[[1, "EvalTimestamp"], [0, "EvalTimestamp"]]),
            "scalar root reads no node": lambda d: physical(d)[0]["dag"].update(
                roots=physical(d)[0]["dag"]["roots"][:1], scalar_roots=[[1, {"PromqlScalarFromVector": 99}]]),
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                doc = example1()
                mutate(doc)
                self.assertNotEqual(self.validate(doc), [])


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
class CandidateRowTests(unittest.TestCase):
    """The Stage 3 list: every candidate the document shows, with its
    outcome."""

    def setUp(self):
        self.js = stages_js()
        self.doc = example1()

    def test_selected_first_then_costlier_by_cost_then_rejected(self):
        selection = self.doc["stage3_plan_selection"]
        rows = self.js.call("stageCandidateRows", self.doc)
        self.assertEqual((rows[0]["id"], rows[0]["status"]), (selection["selected"], "selected"))
        self.assertEqual(rows[0]["total"], selection["costs"][selection["selected"]]["total"])
        self.assertEqual(rows[0]["rank"], 1)
        costlier = [row for row in rows if row["status"] == "costlier"]
        self.assertEqual([r["total"] for r in costlier], sorted(r["total"] for r in costlier))
        self.assertEqual([r["rank"] for r in costlier], list(range(2, len(costlier) + 2)))
        rejected = {r["id"]: r["reason"] for r in selection["rejected"] if not r["valid"]}
        self.assertTrue(rejected)
        for row in rows:
            if row["status"] == "rejected":
                self.assertEqual((row["reason"], row["rank"], row["total"]), (rejected[row["id"]], None, None))
        physical = self.doc["stage2_physical_asap_optimization"]["candidates"]
        self.assertEqual(len(rows), len(physical))
        self.assertTrue(all(row["display_name"] and row["logical"] for row in rows))

    def test_pruned_and_composition_errors_come_last_with_their_reasons(self):
        """A Stage 2 pruned candidate and a Stage 1 candidate that did not
        compose are rows too, after the Stage 3 ones."""
        self.doc["stage2_physical_asap_optimization"]["pruned"].append({"from_logical": "L2", "display_name": "Q1 exact", "reason": "no implementation"})
        self.doc["stage1_logical_asap_optimization"]["candidates"].append({"id": "L99", "display_name": "Q1 exact", "error": "cannot substitute"})
        rows = self.js.call("stageCandidateRows", self.doc)
        self.assertEqual(
            rows[-2:],
            [
                {"id": "L2", "logical": "L2", "display_name": "Q1 exact", "total": None, "unit": None, "rank": None, "status": "pruned", "reason": "no implementation"},
                {"id": "L99", "logical": "L99", "display_name": "Q1 exact", "total": None, "unit": None, "rank": None, "status": "composition error", "reason": "cannot substitute"},
            ],
        )

    def test_failed_selection_leaves_candidates_not_judged(self):
        """When selection fails with `error`, Stage 3 judged no candidate:
        the rows are "not judged", not invalid."""
        self.doc["stage3_plan_selection"] = {"costs": {}, "selected": None, "rejected": [], "error": "no data workload"}
        rows = self.js.call("stageCandidateRows", self.doc)
        self.assertEqual({row["status"] for row in rows}, {"not judged"})
        self.assertTrue(all(row["reason"] is None for row in rows))

    def test_header_counts_candidates_and_shows_a_selection_error(self):
        combinations = self.doc["stage1_logical_asap_optimization"]["combinations"]
        self.assertEqual(self.js.call("stageCandidateHeader", self.doc), f"{combinations} candidates")
        self.doc["stage1_logical_asap_optimization"]["capped"] = True
        self.doc["stage3_plan_selection"] = {"costs": {}, "selected": None, "rejected": [], "error": "no data workload"}
        shown = len(self.doc["stage1_logical_asap_optimization"]["candidates"])
        self.assertEqual(
            self.js.call("stageCandidateHeader", self.doc),
            f"{combinations} candidates (document shows {shown}) · selection failed: no data workload",
        )

    def test_header_shows_the_stage1_error(self):
        self.doc["stage1_logical_asap_optimization"] = {"combinations": 0, "capped": False, "candidates": [], "error": "Pass 1: bad accuracy"}
        self.assertEqual(self.js.call("stageCandidateHeader", self.doc), "0 candidates · Stage 1 failed: Pass 1: bad accuracy")


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
class LaneTests(unittest.TestCase):
    """Each lane is the whole workload's DAG at one stage, as cytoscape
    elements."""

    def setUp(self):
        self.js = stages_js()
        self.doc = example1()
        self.query_ids = [q["id"] for q in self.doc["workload"]["queries"]]

    def lane(self, dag, physical=False, per_node=None):
        return self.js.call("stageLaneElements", dag, {"physical": physical, "costPerNode": per_node, "queryIds": self.query_ids})

    def test_logical_lane_has_an_edge_per_operator_input_and_marks_roots(self):
        dag = self.doc["stage0_query_language_frontends"]["dag"]
        elements = self.lane(dag)
        nodes, edges = lane_nodes(elements), lane_edges(elements)
        self.assertEqual(len(nodes), len(dag["nodes"]))
        self.assertTrue(edges)
        self.assertTrue(all(int(e["data"]["source"][1:]) < int(e["data"]["target"][1:]) for e in edges))
        roots = {n["data"]["nodeId"]: n["data"]["rootFor"] for n in nodes if n["data"]["rootFor"]}
        self.assertEqual(roots, {r["Operator"]: [q] for r, q in zip(dag["roots"], self.query_ids)})
        self.assertEqual(nodes[0]["data"]["kind"], "Scan")
        self.assertEqual(nodes[0]["data"]["label"], "Scan\nsource: http_requests_total")
        self.assertEqual(nodes[0]["data"]["group"], "source")

    def test_physical_lane_shows_timing_and_stage3_costs(self):
        selection = self.doc["stage3_plan_selection"]
        candidate = next(c for c in self.doc["stage2_physical_asap_optimization"]["candidates"] if c["id"] == selection["selected"])
        per_node = selection["costs"][selection["selected"]]["per_node"]
        elements = self.lane(candidate["dag"], True, per_node)
        nodes = lane_nodes(elements)
        self.assertEqual(len(nodes), len(candidate["dag"]["nodes"]))
        self.assertEqual(len(lane_edges(elements)), len(candidate["dag"]["edges"]))
        for node in nodes:
            self.assertIn("⏱ query time", node["data"]["label"])
            self.assertEqual(node["data"]["cost"], per_node[str(node["data"]["nodeId"])])
            self.assertIn("\ncost ", node["data"]["label"])
        self.assertEqual([n["data"]["nodeId"] for n in nodes if n["data"]["rootFor"]], candidate["dag"]["roots"])

    def test_physical_lane_without_stage3_cost_has_none(self):
        candidate = self.doc["stage2_physical_asap_optimization"]["candidates"][0]
        for node in lane_nodes(self.lane(candidate["dag"], True)):
            self.assertNotIn("cost", node["data"]["label"])

    def test_pass2_shared_scan_is_one_node_read_by_both_queries(self):
        """L2 shares Scan #1 between Q1 and Q2 (the Pass 2 identical-expression
        rule): its lane has one Scan fewer than L1's."""
        logical = {c["id"]: c for c in self.doc["stage1_logical_asap_optimization"]["candidates"]}
        self.assertIn("shared Scan", logical["L2"]["display_name"])
        scans = lambda c: [n for n in lane_nodes(self.lane(logical[c]["dag"])) if n["data"]["kind"] == "Scan"]
        self.assertEqual((len(scans("L1")), len(scans("L2"))), (2, 1))

    def test_summary_nodes_print_their_configuration(self):
        cms = {"ASAP": {"SummaryAgg": {
            "child": 0, "filter": None, "reduction": {"Reduce": [0]}, "grouping": "PerSubpopulationInstance",
            "family": {"Sketch": [{"algorithm": "CmsWithHeap", "category": "TopK",
                                   "params": {"CmsWithHeap": {"depth": 7, "heap_size": 100, "width": 272}}},
                                  "PerSubpopulationInstance"]},
        }}}
        schema = {"fields": [{"name": "job"}]}
        lines = self.js.call("stageNodeLines", cms, schema)
        self.assertEqual(lines, ["SummaryAgg", "summary: CmsWithHeap · depth 7 · heap 100 · width 272", "group by job", "instances: one per group"])
        shared = json.loads(json.dumps(cms))
        shared["ASAP"]["SummaryAgg"]["grouping"] = {"SharedMultiSubpopulation": {"kind": "HydraCms", "params": {"HydraCms": {"width": 272, "depth": 7}}}}
        self.assertIn("instances: one shared HydraCms · width 272 · depth 7", self.js.call("stageNodeLines", shared, schema))
        exact = {"ASAP": {"SummaryAgg": {"child": 0, "family": {"ExactAggregate": ["Sum", "Sum"]}, "grouping": "PerSubpopulationInstance", "reduction": "PerEntity"}}}
        self.assertEqual(self.js.call("stageNodeLines", exact, None)[1:], ["summary: exact Sum", "per series", "instances: one per series"])
        self.assertEqual(self.js.call("stageNodeLines", {"ASAP": {"FinalizeExactAccumulator": {"child": 0}}}, None), ["FinalizeExactAccumulator"])

    def test_node_groups(self):
        self.assertEqual([self.js.call("nodeGroup", k) for k in ("Scan", "SummaryAgg", "Filter", "Scalar")], ["source", "summary", "rel", "scalar"])


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
@unittest.skipUnless(server.BINARY.exists(), "needs a built stage_pipeline")
class ScalarQueryTests(unittest.TestCase):
    """A standalone scalar query is no operator root: Stage 0 and Stage 1
    keep it as a `{"Scalar": expr}` root, and a physical DAG in
    `scalar_roots`, so query i is not `roots[i]`. Each lane shows the
    scalar expression as a node of its own, over the nodes it reads."""

    @classmethod
    def setUpClass(cls):
        args = []
        for query in SCALAR_QUERIES:
            args += ["--promql", query]
        cls.doc = stage_pipeline(*args)

    def setUp(self):
        self.js = stages_js()
        selection = self.doc["stage3_plan_selection"]
        self.selected = next(c for c in self.doc["stage2_physical_asap_optimization"]["candidates"] if c["id"] == selection["selected"])
        self.query_ids = [q["id"] for q in self.doc["workload"]["queries"]]

    def lanes(self):
        options = {"physical": False, "queryIds": self.query_ids}
        yield self.js.call("stageLaneElements", self.doc["stage0_query_language_frontends"]["dag"], options)
        yield self.js.call("stageLaneElements", self.selected["dag"], dict(options, physical=True))

    def roots(self, elements):
        return {n["data"]["rootFor"][0]: n for n in lane_nodes(elements) if n["data"]["rootFor"]}

    def test_document_is_valid(self):
        self.assertEqual(self.js.call("validateStagePipeline", self.doc), [])
        self.assertEqual([entry[0] for entry in self.selected["dag"]["scalar_roots"]], [0, 2, 4])

    def test_scalar_queries_first_middle_and_last_show_their_expression(self):
        for elements in self.lanes():
            roots = self.roots(elements)
            self.assertEqual(sorted(roots), self.query_ids)
            for index, text in {0: "scalar(#{})", 2: "time()", 4: "3"}.items():
                root = roots[self.query_ids[index]]
                self.assertEqual(root["data"]["kind"], "Scalar")
                self.assertEqual(root["data"]["group"], "scalar")
                reads = [e["data"]["source"][1:] for e in lane_edges(elements) if e["data"]["target"] == root["data"]["id"]]
                self.assertEqual(root["data"]["label"].split("\n")[:2], ["Scalar", text.format(*reads)])
            for index in (1, 3):
                self.assertNotEqual(roots[self.query_ids[index]]["data"]["kind"], "Scalar")

    def test_scalar_of_a_vector_reads_its_operator(self):
        """scalar(sum(m)) reads the sum: an edge into the Scalar node from an
        Aggregate (Stage 0) or the sum's physical node."""
        for elements in self.lanes():
            root = self.roots(elements)[self.query_ids[0]]
            reads = [e["data"]["source"] for e in lane_edges(elements) if e["data"]["target"] == root["data"]["id"]]
            self.assertEqual(len(reads), 1)
            self.assertIn(reads[0], {n["data"]["id"] for n in lane_nodes(elements)})


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
@unittest.skipUnless(server.BINARY.exists(), "needs a built stage_pipeline")
class BinaryOperatorTests(unittest.TestCase):
    """A binary operator reads both operands (`lhs`, `rhs`): its node has an
    input edge from each in Stage 0, Stage 1 and Stage 2."""

    def test_binary_operator_reads_both_operands_in_every_lane(self):
        doc = stage_pipeline("--promql", "sum(rate(a[1m])) / sum(rate(b[1m]))")
        js = stages_js()
        self.assertEqual(js.call("validateStagePipeline", doc), [])
        selected = doc["stage3_plan_selection"]["selected"]
        lanes = [
            (doc["stage0_query_language_frontends"]["dag"], False),
            (doc["stage1_logical_asap_optimization"]["candidates"][0]["dag"], False),
            (next(c for c in doc["stage2_physical_asap_optimization"]["candidates"] if c["id"] == selected)["dag"], True),
        ]
        for dag, physical in lanes:
            with self.subTest(physical=physical):
                elements = js.call("stageLaneElements", dag, {"physical": physical, "queryIds": ["q1"]})
                binary = [n for n in lane_nodes(elements) if n["data"]["kind"] == "BinaryOp"]
                self.assertEqual(len(binary), 1)
                inputs = [e for e in lane_edges(elements) if e["data"]["target"] == binary[0]["data"]["id"]]
                self.assertEqual(len(inputs), 2)


class PageTests(unittest.TestCase):
    """The page's scripts, element ids and examples line up."""

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
            with self.subTest(script):
                self.assertLessEqual(used, ids)

    def test_examples_are_stage_pipeline_examples_written_into_out(self):
        examples = json.loads((HERE / "examples.json").read_text())
        tool = (HERE.parents[1] / "crates/devtools/src/bin/stage_pipeline.rs").read_text()
        self.assertEqual(len({e["id"] for e in examples}), len(examples))
        for example in examples:
            with self.subTest(example["id"]):
                self.assertIn(f'"{example["example"]}" =>', tool)
                self.assertTrue(example["file"].startswith("out/"))
                self.assertTrue(example["story"])

    def test_only_example1_is_committed(self):
        self.assertEqual(sorted(p.name for p in (HERE / "examples").iterdir()), ["planner-layering-example1.json"])
        self.assertEqual((HERE / ".gitignore").read_text().split(), ["out/"])


STUB_DOM = r"""
var __els = {}; var __files = {};
function __el(id) {
  if (!__els[id]) __els[id] = { id, innerHTML: '', textContent: '', hidden: false, value: '', dataset: {},
    __on: {}, addEventListener(type, fn) { this.__on[type] = fn; }, setAttribute() {}, click() {}, querySelectorAll() { return []; } };
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
var __taps = {};
function cytoscape(o) {
  __nodes += o.elements.length;
  const lane = o.container.id;
  return { on(event, what, fn) { __taps[lane + ' ' + what] = fn; }, destroy() {}, style() {} };
}
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

    def test_doc_parameter_opens_one_document_with_the_selected_plan(self):
        js = self.page("?doc=examples/e1.json", {"examples/e1.json": EXAMPLE1.read_text()})
        selected = example1()["stage3_plan_selection"]["selected"]
        self.assertIn("Candidates per stage", self.text(js, "counts"))
        self.assertIn("✓ selected", self.text(js, "lane2-meta"))
        self.assertEqual(js.eval("__els.pick2.value"), selected)
        self.assertEqual(js.eval("__els.pick1.value"), "L1")
        self.assertIn("9 candidates", js.eval("__els['rank-meta'].textContent"))
        self.assertGreater(js.eval("__nodes"), 0)

    def test_rank_list_has_composition_error_rows(self):
        doc = example1()
        doc["stage1_logical_asap_optimization"]["candidates"].append({"id": "L99", "display_name": "Q1 exact", "error": "cannot substitute"})
        js = self.page("?doc=d.json", {"d.json": json.dumps(doc)})
        rank = self.text(js, "rank")
        self.assertIn("composition error", rank)
        self.assertIn("cannot substitute", rank)

    def test_failed_selection_shows_stage0_and_the_error(self):
        doc = example1()
        doc["stage1_logical_asap_optimization"] = {"combinations": 0, "capped": False, "candidates": [], "error": "Pass 1: bad accuracy"}
        doc["stage2_physical_asap_optimization"] = {"candidates": [], "pruned": []}
        doc["stage3_plan_selection"] = {"costs": {}, "selected": None, "rejected": []}
        js = self.page("?doc=d.json", {"d.json": json.dumps(doc)})
        self.assertIn("Stage 1 failed: Pass 1: bad accuracy", js.eval("__els['rank-meta'].textContent"))
        self.assertIn("no candidates", js.eval("__els['lane1-meta'].textContent"))
        self.assertGreater(js.eval("__nodes"), 0)

    def test_examples_become_tabs_and_the_hash_picks_one(self):
        sample = EXAMPLE1.read_text()
        manifest = json.dumps([{"id": "a", "name": "A", "file": "out/a.json", "story": "story A"},
                               {"id": "b", "name": "B", "file": "out/b.json", "story": "story B"}])
        js = self.page("", {"examples.json": manifest, "out/a.json": sample, "out/b.json": sample}, "#b")
        self.assertIn('data-id="b"', js.eval("__els.tabs.innerHTML"))
        self.assertEqual(js.eval("__els.story.textContent"), "story B")

    def test_choosing_a_stage1_candidate_shows_its_physical_plan(self):
        js = self.page("?doc=e1.json", {"e1.json": EXAMPLE1.read_text()})
        js.eval("__els.pick1.__on.change({ target: { value: 'L2' } })")
        self.assertEqual(js.eval("__els.pick2.value"), "P2")
        self.assertIn("from L2", self.text(js, "lane2-meta"))

    def test_choosing_an_uncomposed_stage1_candidate_clears_stage2(self):
        doc = example1()
        doc["stage1_logical_asap_optimization"]["candidates"].append({"id": "L99", "display_name": "Q1 exact", "error": "cannot substitute"})
        js = self.page("?doc=d.json", {"d.json": json.dumps(doc)})
        js.eval("__els.pick1.__on.change({ target: { value: 'L99' } })")
        self.assertEqual(js.eval("__els.pick2.value"), "")
        self.assertIn("composition error: cannot substitute", self.text(js, "lane1-meta"))

    def test_node_details_without_a_numeric_cost(self):
        """A per_node entry without a numeric `cost` does not break the details panel."""
        js = self.page("?doc=e1.json", {"e1.json": EXAMPLE1.read_text()})
        js.eval("""__taps['cy2 node']({ target: { data() { return {
            kind: 'Scan', nodeId: 0, rootFor: [], timing: 'query_time', cost: { detail: 'not priced' },
            node: { payload: { NonASAP: { Scan: {} } }, guarantee: null } }; } } })""")
        self.assertIn("not priced", self.text(js, "details"))

    def test_show_document_reports_whether_it_was_shown(self):
        """The editor reports success only when the document was shown."""
        js = self.page()
        self.assertTrue(js.eval(f"window.StageViewer.showDocument({EXAMPLE1.read_text()}, 'e1')"))
        self.assertFalse(js.eval("window.StageViewer.showDocument({ format: 'nope' }, 'bad')"))
        self.assertIn("Could not load", js.eval("__els.queries.innerHTML"))

    def test_an_invalid_document_shows_an_error(self):
        js = self.page("?doc=bad.json", {"bad.json": json.dumps({"format": "nope"})})
        self.assertIn("Could not load", js.eval("__els.queries.innerHTML"))


if __name__ == "__main__":
    unittest.main()
