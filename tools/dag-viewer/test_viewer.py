"""Tests for the ASAPPlanner Plan Viewer: the stage-document functions in
stages.js and the page itself (app.js) run in V8 through py_mini_racer with
a stub DOM, plus the page's files and the example list.

Run from tools/dag-viewer: `python3 -m unittest test_viewer`. The JS tests
are skipped without py_mini_racer (`pip install py-mini-racer==0.6.0`); the
ones over a planned document need a built asap_planner (`cargo build -p
asap-devtools --bin asap_planner`; CARGO_TARGET_DIR is honored).
"""

from __future__ import annotations

import functools
import json
import re
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

import server

HERE = Path(__file__).resolve().parent
WORKLOADS = HERE / "workloads"
# #509 Example 1: two PromQL panels over one metric, accuracy guarantees enforced.
EXAMPLE1 = WORKLOADS / "planner-layering" / "example1.json"
# #509 Example 3a: five p99 reports over historical intervals; the plan
# shares one tumbling-window chain among the five queries.
EXAMPLE3A = WORKLOADS / "planner-layering" / "example3a.json"
# The document asap_planner writes for test-fixtures/p90-workload.json under
# test-fixtures/kll-only-capabilities.json: one p90 query with accuracy
# enforced, whose Pass 2 prunes G3's four smallest KLL sketches (memo.js
# tests).
P90_KLL = HERE / "test-fixtures" / "p90-kll.json"
# #1005's ratio-of-quantiles query.
RATIO_OF_QUANTILES = HERE / "memo-examples" / "promql" / "ratio-of-quantiles-5m.json"
# Standalone scalar queries at positions 0, 2 and 4 (first, middle, last):
# scalar(sum(m)), sum(n), time(), sum by (job) (k), 1 + 2.
SCALAR_QUERIES = ["scalar(sum(m))", "sum(n)", "time()", "sum by (job) (k)", "1 + 2"]
NO_RAW_DATA = {"raw_data_retained": False, "query_time_results_retained": False}

try:
    from py_mini_racer import py_mini_racer
except ImportError:
    py_mini_racer = None

needs_planner = unittest.skipUnless(server.BINARY.exists(), "needs a built asap_planner")


def promql_workload(queries: list[str], sources: list[str]) -> dict:
    """A PlanningWorkload of exact PromQL `queries`, each every 10 s, over
    `sources` of 100 series sampled every second."""
    repeating = [{"query": query, "demand": {"fixed_interval_at": {"interval": 10000, "evaluation_phase": 0}},
                  "requirements": {"accuracy": {"max_error": 0.0, "failure_probability": 0.0, "accuracy_scope": "whole_result"},
                                   "max_latency_ms": None},
                  "predictability": {"predictable": {"known_at": None}}} for query in queries]
    return {"query_workload": {"language": "promql", "query_batch": None, "repeating_queries": repeating},
            "data_workload": {"arrival": {"ingesting": {"rows_per_second": 100.0 * len(sources)}},
                              "sources": {name: {"series_count": 100, "sample_interval": 1000} for name in sources}}}


@functools.lru_cache(maxsize=None)
def _planned(workload: str, capabilities: str | None, ignore_accuracy_guarantees: bool, run_until: str = "stage3") -> str:
    with tempfile.TemporaryDirectory() as tmp:
        workload_path, out = Path(tmp) / "workload.json", Path(tmp) / "doc.json"
        workload_path.write_text(workload)
        args = [str(server.BINARY), "--workload", str(workload_path), "--run-until", run_until, "--format", "json", "--out", str(out)]
        if capabilities is not None:
            (Path(tmp) / "capabilities.json").write_text(capabilities)
            args += ["--deployment-capabilities", str(Path(tmp) / "capabilities.json")]
        if ignore_accuracy_guarantees:
            args.append("--ignore-accuracy-guarantees")
        # A failed stage exits 1 and still writes the document.
        result = subprocess.run(args, capture_output=True)
        if result.returncode not in (0, 1) or not out.exists():
            raise RuntimeError(result.stderr.decode())
        return out.read_text()


def planned(workload: dict, capabilities: dict | None = None, ignore_accuracy_guarantees: bool = False) -> dict:
    """A fresh copy of the document asap_planner writes for `workload`
    through Stage 3; each distinct run is planned once."""
    capabilities_json = None if capabilities is None else json.dumps(capabilities)
    return json.loads(_planned(json.dumps(workload), capabilities_json, ignore_accuracy_guarantees))


def example1() -> dict:
    return planned(json.loads(EXAMPLE1.read_text()))


def example3a() -> dict:
    return planned(json.loads(EXAMPLE3A.read_text()))


def p90_kll() -> dict:
    return json.loads(P90_KLL.read_text())


def materialized_view(node_id: int) -> dict:
    """A materialized view of plan node `node_id`, refreshed at ingestion
    over the last 5 min of raw data. No workload the tests plan today
    yields an installable plan with views, so the view tests add this one."""
    return {"node_id": node_id, "refresh": "at_ingestion", "coverage": {"definition_id": "d1", "selection": []},
            "inputs": {"raw_data_windows": [{"end_offset_ms": 0, "width_ms": 300000}], "materialized_views": []},
            "retention_ms": 300000}


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
@needs_planner
class ValidationTests(unittest.TestCase):
    """The viewer refuses a document that breaks the format rather than
    guessing what it means."""

    def setUp(self):
        self.js = stages_js()
        self.doc = example1()

    def validate(self, doc):
        return self.js.call("validateStagePipeline", doc)

    def test_example1_is_valid(self):
        """Example 1 as asap_planner plans it passes the checks."""
        self.assertEqual(self.validate(self.doc), [])

    def test_a_run_stopped_before_stage3_is_shown_with_empty_later_stages(self):
        """A document from each earlier `--run-until`, whose later stages
        are null, passes the checks once those stages are shown empty."""
        workload = EXAMPLE1.read_text()
        for run_until in ["stage0", "stage1_pass1", "stage1_pass2", "stage2"]:
            with self.subTest(run_until=run_until):
                doc = json.loads(_planned(workload, None, False, run_until))
                self.assertIsNone(doc["stage3_plan_selection"])
                self.assertEqual(self.validate(self.js.call("fillStagesNotRun", doc)), [])

    def test_a_stage1_failure_is_shown_with_its_error(self):
        """When Stage 1 fails (here: Example 3a's stored data without raw
        data retained), the document passes the checks and keeps the Stage 1
        error to show, with no plan."""
        doc = planned(json.loads(EXAMPLE3A.read_text()), NO_RAW_DATA)
        self.assertEqual(self.validate(doc), [])
        self.assertTrue(doc["stage1_logical_asap_optimization"]["error"].startswith("deployment capabilities: "))
        self.assertIsNone(doc["stage3_plan_selection"]["plan"])

    def test_documents_without_the_optional_sections_are_valid(self):
        """A document without data workload, views or accuracy mode is valid."""
        del self.doc["workload"]["data_workload"]
        del self.doc["stage3_plan_selection"]["materialized_views"]
        del self.doc["accuracy_guarantees"]
        self.assertEqual(self.validate(self.doc), [])

    def test_accuracy_guarantees_is_ignored_or_enforced(self):
        """accuracy_guarantees is "ignored" or "enforced"; another value is refused."""
        self.assertEqual(self.doc["accuracy_guarantees"], "enforced")
        for mode in ("ignored", "enforced"):
            self.assertEqual(self.validate(dict(self.doc, accuracy_guarantees=mode)), [])
        self.assertEqual(self.validate(dict(self.doc, accuracy_guarantees="off")),
                         ['accuracy_guarantees must be "ignored" or "enforced"'])

    def test_data_workload_and_materialized_view_violations_are_reported(self):
        """Each malformed data workload or materialized view is reported."""
        views = lambda d: d["stage3_plan_selection"]["materialized_views"]
        cases = {
            "data workload not an object": lambda d: d["workload"].update(data_workload=[]),
            "sources not an object": lambda d: d["workload"]["data_workload"].update(sources=[]),
            "source facts not an object": lambda d: d["workload"]["data_workload"]["sources"].update(data=3),
            "materialized views not an array": lambda d: d["stage3_plan_selection"].update(materialized_views={}),
            "node id": lambda d: views(d)[0].update(node_id="2"),
            "refresh": lambda d: views(d)[0].update(refresh="later"),
            "definition id": lambda d: views(d)[0]["coverage"].pop("definition_id"),
            "inputs": lambda d: views(d)[0]["inputs"].pop("raw_data_windows"),
            "retention": lambda d: views(d)[0].update(retention_ms=-1),
            "views without a plan": lambda d: d["stage3_plan_selection"].update(plan=None),
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                doc = example1()
                doc["stage3_plan_selection"]["materialized_views"] = [materialized_view(0)]
                self.assertEqual(self.validate(doc), [])
                mutate(doc)
                self.assertTrue(self.validate(doc), name)

    def test_failed_selection_is_valid(self):
        """A failed selection has no plan, only its error."""
        self.doc["stage3_plan_selection"] = {"plan": None, "error": "no data workload", "materialized_views": []}
        self.assertEqual(self.validate(self.doc), [])

    def test_contract_violations_are_reported(self):
        """Each break of the document contract is reported."""
        stage0 = lambda d: d["stage0_query_language_frontends"]["dag"]
        plan = lambda d: d["stage3_plan_selection"]["plan"]
        selection = lambda d: d["stage3_plan_selection"]
        physical = lambda d: d["stage2_physical_asap_optimization"]
        cases = {
            "format": lambda d: d.update(format="asap-stage-pipeline/v0"),
            "no queries": lambda d: d["workload"].update(queries=[]),
            "roots per query": lambda d: stage0(d)["roots"].pop(),
            "root names no node": lambda d: stage0(d)["roots"].__setitem__(0, {"Operator": 99}),
            "logical root shape": lambda d: stage0(d)["roots"].__setitem__(0, 3),
            "child names no node": lambda d: stage0(d)["nodes"][1]["operator"]["NonASAP"]["TimeRange"].update(child=42),
            "plan root shape": lambda d: plan(d)["roots"].__setitem__(0, {"Operator": 3}),
            "plan roots per query": lambda d: plan(d)["roots"].pop(),
            "edge endpoint": lambda d: plan(d)["edges"][0].update(producer=42),
            "edge producer names no physical candidate": lambda d: next(
                c for c in physical(d)["physical_candidates"] if c["inputs"])["inputs"][0]["edges"][0].update(producer=99999),
            "stage 2 pruned without reason": lambda d: physical(d)["pruned_candidates"][0].pop("reason"),
            "stage 3 pruned dominated by no physical candidate": lambda d: selection(d)["pruned_candidates"].append(
                dict(physical(d)["pruned_candidates"][0], reason={"dominated_by": 99999})),
            "pass 2 pruned without causes": lambda d: d["stage1_logical_asap_optimization"]["pruned_candidates"][0].pop("causes"),
            "per_node": lambda d: selection(d)["cost"]["per_node"].update({"99": {"cost_per_second": 1, "multiplier": 1}}),
            "cost per second": lambda d: selection(d)["cost"].pop("cost_per_second"),
            "missing stage": lambda d: d.pop("stage2_physical_asap_optimization"),
            "binary operand names no node": lambda d: stage0(d)["nodes"].append(
                {"operator": {"NonASAP": {"BinaryOp": {"lhs": 0, "rhs": 42, "operator": {}, "return_bool": False}}}}),
            "scalar root past the queries": lambda d: plan(d).update(
                roots=plan(d)["roots"][:1], scalar_roots=[[2, "EvalTimestamp"]]),
            "scalar roots out of order": lambda d: plan(d).update(
                roots=[], scalar_roots=[[1, "EvalTimestamp"], [0, "EvalTimestamp"]]),
            "scalar roots not an array": lambda d: plan(d).update(scalar_roots={"0": "EvalTimestamp"}),
            "scalar root reads no node": lambda d: plan(d).update(
                roots=plan(d)["roots"][:1], scalar_roots=[[1, {"PromqlScalarFromVector": 99}]]),
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                doc = example1()
                mutate(doc)
                self.assertNotEqual(self.validate(doc), [])


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
class ScalarTextTests(unittest.TestCase):
    """A scalar expression's short text."""

    def test_arithmetic_names_its_operator(self):
        """`ArithmeticOpKind` serializes as a string, e.g. `"Add"`."""
        expr = {"Arithmetic": {"op": "Add", "left": "EvalTimestamp", "right": {"Literal": {"Float64": 2}}, "semantics": "Sql"}}
        self.assertEqual(stages_js().call("stageScalarText", expr), "(time() Add 2)")


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
@needs_planner
class PlanRowTests(unittest.TestCase):
    """Stage 3's outcome: one row for the selected plan or selection's error,
    under a header with each stage's failure and the selection's warnings."""

    def setUp(self):
        self.js = stages_js()
        self.doc = example1()

    def test_a_failed_selection_shows_the_error(self):
        """Without a plan the outcome line shows selection's error."""
        self.doc["stage3_plan_selection"] = {"plan": None, "error": "no data workload", "materialized_views": []}
        self.assertEqual(self.js.call("planHeader", self.doc), "selection failed: no data workload")

    def test_accuracy_text_reads_the_target_json(self):
        """An accuracy target reads as its ε, δ, metric and scope."""
        text = lambda target: self.js.call("accuracyText", target)
        self.assertEqual(text({"max_error": 0.0, "failure_probability": 0.0, "accuracy_scope": "whole_result"}), "exact")
        self.assertEqual(text({"max_error": 0.01, "accuracy_scope": "whole_result"}), "ε=0.01 (error metric not set)")
        self.assertEqual(text({"accuracy_metric": "rank", "max_error": 0.01, "failure_probability": 0.001, "accuracy_scope": "whole_result"}), "ε=0.01, δ=0.001 (rank)")
        self.assertEqual(self.js.call("accuracyText", {"max_error": 0.01}, "relative"), "ε=0.01 (error metric not set; default relative)")
        self.assertEqual(text(None), "")
        self.assertEqual(text({"accuracy_metric": "relative", "max_error": 0.05, "failure_probability": 0.05, "accuracy_scope": "per_value"}), "ε=0.05, δ=0.05 (relative; per value)")
        self.assertEqual(text({"accuracy_metric": "relative", "max_error": 0.05, "failure_probability": 0.05, "accuracy_scope": {"groups_with_record_share_at_least": 0.05}}), "ε=0.05, δ=0.05 (relative; groups ≥ 5% of records)")
        self.assertEqual(text({"max_error": 0.05, "accuracy_scope": {"groups_with_record_share_at_least": 0.01}}), "ε=0.05 (error metric not set; groups ≥ 1% of records)")

    def test_counts_line_counts_each_stages_candidates(self):
        """The counts line counts the Pass 2 memo's candidates, the physical
        candidates and what each stage pruned."""
        pass2 = next(s for s in self.doc["memo"]["steps"] if s["step"] == "pass2")
        logical = sum(g["candidate_count"] for g in pass2["groups"])
        physical = self.doc["stage2_physical_asap_optimization"]
        self.assertEqual(self.js.call("candidateCountsText", self.doc),
                         f"Candidates per stage: {logical} logical ({len(self.doc['stage1_logical_asap_optimization']['pruned_candidates'])} pruned by Pass 2)"
                         f" → {len(physical['physical_candidates'])} physical ({len(physical['pruned_candidates'])} pruned by Stage 2, 0 by Stage 3)"
                         " → plan selected")


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
@needs_planner
class LaneTests(unittest.TestCase):
    """Each lane is the whole workload's DAG at one stage, as cytoscape
    elements: Stage 0's DAG and Stage 3's plan."""

    def setUp(self):
        self.js = stages_js()
        self.doc = example1()
        self.query_ids = [q["id"] for q in self.doc["workload"]["queries"]]

    def lane(self, dag, physical=False, per_node=None):
        return self.js.call("stageLaneElements", dag, {"physical": physical, "costPerNode": per_node, "queryIds": self.query_ids})

    def test_logical_lane_has_an_edge_per_operator_input_and_marks_roots(self):
        """The Stage 0 lane has an edge per operator input and marks each query's root."""
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

    def test_plan_lane_shows_timing_and_stage3_costs(self):
        """The plan's lane shows each node's timing and Stage 3 cost."""
        plan = self.doc["stage3_plan_selection"]["plan"]
        per_node = self.doc["stage3_plan_selection"]["cost"]["per_node"]
        elements = self.lane(plan, True, per_node)
        nodes = lane_nodes(elements)
        self.assertEqual(len(nodes), len(plan["nodes"]))
        self.assertEqual(len(lane_edges(elements)), len(plan["edges"]))
        for node in nodes:
            self.assertIn("⏱ query time", node["data"]["label"])
            self.assertEqual(node["data"]["cost"], per_node[str(node["data"]["nodeId"])])
            self.assertIn("\ncost ", node["data"]["label"])
        self.assertEqual([n["data"]["nodeId"] for n in nodes if n["data"]["rootFor"]], sorted(plan["roots"]))

    def test_plan_lane_without_stage3_cost_has_none(self):
        """A lane drawn without costs shows none."""
        for node in lane_nodes(self.lane(self.doc["stage3_plan_selection"]["plan"], True)):
            self.assertNotIn("cost", node["data"]["label"])

    def test_summary_nodes_print_their_summary_type_and_grouping(self):
        """Summary nodes print their summary type and grouping."""
        kll = {"ASAP": {"SummaryAgg": {"child": 0, "filter": None, "reduction": {"Reduce": [0]}, "summary_type": {"Sketch": "Kll"}}}}
        schema = {"fields": [{"name": "job"}]}
        self.assertEqual(self.js.call("stageNodeLines", kll, schema), ["SummaryAgg", "summary: Kll", "group by job"])
        exact = {"ASAP": {"SummaryAgg": {"child": 0, "summary_type": {"ExactAggregate": ["Sum", "Sum"]}, "reduction": "PerEntity"}}}
        self.assertEqual(self.js.call("stageNodeLines", exact, None)[1:], ["summary: exact Sum", "per series"])
        self.assertEqual(self.js.call("stageNodeLines", {"ASAP": {"FinalizeExactAccumulator": {"child": 0}}}, None), ["FinalizeExactAccumulator"])

    def test_node_groups(self):
        """Node kinds map to their colour groups."""
        self.assertEqual([self.js.call("nodeGroup", k) for k in ("Scan", "SummaryAgg", "Filter", "Scalar")], ["source", "summary", "rel", "scalar"])


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
@needs_planner
class ScalarQueryTests(unittest.TestCase):
    """A standalone scalar query is no operator root: Stage 0 keeps it as a
    `{"Scalar": expr}` root, and the plan in `scalar_roots`, so query i is
    not `roots[i]`. Each lane shows the scalar expression as a node of its
    own, over the nodes it reads."""

    @classmethod
    def setUpClass(cls):
        cls.doc = planned(promql_workload(SCALAR_QUERIES, ["m", "n", "k"]))

    def setUp(self):
        self.js = stages_js()
        self.plan = self.doc["stage3_plan_selection"]["plan"]
        self.query_ids = [q["id"] for q in self.doc["workload"]["queries"]]

    def lanes(self):
        options = {"physical": False, "queryIds": self.query_ids}
        yield self.js.call("stageLaneElements", self.doc["stage0_query_language_frontends"]["dag"], options)
        yield self.js.call("stageLaneElements", self.plan, dict(options, physical=True))

    def roots(self, elements):
        return {n["data"]["rootFor"][0]: n for n in lane_nodes(elements) if n["data"]["rootFor"]}

    def test_document_is_valid(self):
        """The scalar-query document is valid, with scalar roots at 0, 2 and 4."""
        self.assertEqual(self.js.call("validateStagePipeline", self.doc), [])
        self.assertEqual([entry[0] for entry in self.plan["scalar_roots"]], [0, 2, 4])

    def test_scalar_queries_first_middle_and_last_show_their_expression(self):
        """Each scalar query's root is a Scalar node showing its expression."""
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
        Aggregate (Stage 0) or the sum's plan node."""
        for elements in self.lanes():
            root = self.roots(elements)[self.query_ids[0]]
            reads = [e["data"]["source"] for e in lane_edges(elements) if e["data"]["target"] == root["data"]["id"]]
            self.assertEqual(len(reads), 1)
            self.assertIn(reads[0], {n["data"]["id"] for n in lane_nodes(elements)})


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
@needs_planner
class SearchTests(unittest.TestCase):
    """asap_planner's search keeps only the selected plan: the document is
    valid and the header names the search and its MILP."""

    def setUp(self):
        self.js = stages_js()
        # Two queries over two metrics, jointly planned.
        self.doc = planned(promql_workload(["sum(rate(a[1m]))", "topk(5, rate(b[1m]))"], ["a", "b"]))

    def test_document_is_valid_and_holds_one_plan_for_both_queries(self):
        """The document is valid and its plan has a root per query."""
        self.assertEqual(self.doc["search"], "candidate-connections")
        self.assertEqual(self.js.call("validateStagePipeline", self.doc), [])
        self.assertEqual(len(self.doc["stage3_plan_selection"]["plan"]["roots"]), 2)

    def test_guarantees_are_enforced_by_default_and_the_header_says_which(self):
        """The settings say whether the run enforced accuracy guarantees
        (the default) or ignored them."""
        guarantees = lambda doc: dict(self.js.call("settingsRows", doc))["accuracy guarantees"]
        self.assertEqual(self.doc["accuracy_guarantees"], "enforced")
        self.assertEqual(guarantees(self.doc), "enforced")
        ignored = planned(promql_workload(["sum(rate(a[1m]))", "topk(5, rate(b[1m]))"], ["a", "b"]), ignore_accuracy_guarantees=True)
        self.assertEqual(ignored["accuracy_guarantees"], "ignored")
        self.assertEqual(self.js.call("validateStagePipeline", ignored), [])
        self.assertTrue(guarantees(ignored).startswith("ignored"))
        self.assertEqual(guarantees({}), "not recorded")

    def test_header_names_the_search_and_its_milp(self):
        """The header names the candidate-connection search and its MILP's
        size and times."""
        text = self.js.call("searchText", self.doc)
        stage3 = self.doc["stage3_plan_selection"]
        self.assertEqual(text, f"candidate-connection search · MILP {stage3['milp_variables']} variables, {stage3['milp_constraints']} constraints"
                               f" · construction {stage3['milp_construction_ms']} ms, solve {stage3['milp_solve_ms']} ms")


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
@needs_planner
class BinaryOperatorTests(unittest.TestCase):
    """A binary operator reads both operands (`lhs`, `rhs`): its node has an
    input edge from each in Stage 0 and in the plan."""

    def test_binary_operator_reads_both_operands_in_every_lane(self):
        """A binary operator has an input edge from each operand in every lane."""
        doc = planned(promql_workload(["sum(rate(a[1m])) / sum(rate(b[1m]))"], ["a", "b"]))
        js = stages_js()
        self.assertEqual(js.call("validateStagePipeline", doc), [])
        lanes = [(doc["stage0_query_language_frontends"]["dag"], False), (doc["stage3_plan_selection"]["plan"], True)]
        for dag, physical in lanes:
            with self.subTest(physical=physical):
                elements = js.call("stageLaneElements", dag, {"physical": physical, "queryIds": ["q1"]})
                binary = [n for n in lane_nodes(elements) if n["data"]["kind"] == "BinaryOp"]
                self.assertEqual(len(binary), 1)
                inputs = [e for e in lane_edges(elements) if e["data"]["target"] == binary[0]["data"]["id"]]
                self.assertEqual(len(inputs), 2)


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
@needs_planner
class WorkloadViewTests(unittest.TestCase):
    """The data workload panel, the materialized views table and the
    workload-set rows."""

    def setUp(self):
        self.js = stages_js()
        self.doc = example1()

    def test_data_workload_rows_show_exactly_the_declared_facts(self):
        """Each source row shows the facts seed-862's workload-01 declares
        and the queries reading it."""
        workload = json.loads((WORKLOADS / "random-mixed-seed-862" / "workload-01.json").read_text())
        queries = [{"id": f"q{k + 1}", "text": q["query"]} for k, q in enumerate(workload["query_workload"]["repeating_queries"])]
        view = self.js.call("dataWorkloadView", workload["data_workload"], queries)
        self.assertEqual(view["arrival"], "ingesting · 200000 rows/s")
        rows = {r["name"]: r for r in view["rows"]}
        self.assertEqual(sorted(rows), ["data", "data_total", "events"])
        self.assertEqual((rows["data"]["series"], rows["data"]["type"], rows["data"]["nonNegative"], rows["data"]["sampleInterval"]),
                         (10000, "gauge", "yes", "100 ms"))
        self.assertEqual(rows["data_total"]["nonNegative"], "yes (counter)")
        self.assertEqual((rows["events"]["maxDistinct"], rows["events"]["sampleInterval"]), (10000, "—"))
        self.assertEqual(rows["data"]["readBy"], ["q1", "q2", "q3", "q4", "q5", "q6", "q7"])
        self.assertEqual(rows["data_total"]["readBy"], ["q8", "q9"])
        self.assertEqual(rows["events"]["readBy"], [])

    def test_data_workload_rows_show_each_labels_distinct_values(self):
        """A source row lists each label's distinct values."""
        data_workload = {"arrival": "at_rest", "sources": {
            "data": {"series_count": 10000, "distinct_values_per_label": {"host": 1000, "job": 10}},
            "other": {"series_count": 5}}}
        rows = {r["name"]: r for r in self.js.call("dataWorkloadView", data_workload, [])["rows"]}
        self.assertEqual(rows["data"]["distinctValuesPerLabel"], "host 1000, job 10")
        self.assertEqual(rows["other"]["distinctValuesPerLabel"], "—")

    def test_data_workload_rows_show_each_label_sets_distinct_values(self):
        """A source row lists each label set's distinct values."""
        data_workload = {"arrival": "at_rest", "sources": {
            "http": {"series_count": 10000, "distinct_values_per_label": {"service": 25, "endpoint": 625},
                     "distinct_values_per_label_set": [{"labels": ["service", "endpoint"], "distinct_values": 625}]},
            "other": {"series_count": 5}}}
        rows = {r["name"]: r for r in self.js.call("dataWorkloadView", data_workload, [])["rows"]}
        self.assertEqual(rows["http"]["distinctValuesPerLabelSet"], "(service, endpoint) 625")
        self.assertEqual(rows["other"]["distinctValuesPerLabelSet"], "—")

    def test_materialized_view_rows_name_their_plan_node_and_the_queries_reading_it(self):
        """Each materialized view names its plan node, its kind and the
        queries reading it."""
        plan = self.doc["stage3_plan_selection"]["plan"]
        shared = next(n["id"] for n in plan["nodes"] if "SummaryAgg" in n["payload"].get("ASAP", {}))
        self.doc["stage3_plan_selection"]["materialized_views"] = [materialized_view(shared)]
        [row] = self.js.call("materializedViewRows", self.doc)
        self.assertEqual((row["nodeId"], row["definition"], row["refresh"], row["window"]), (shared, "d1", "at ingestion", "t−5 min … t"))
        self.assertTrue(row["kind"].startswith("SummaryAgg · exact "), row["kind"])
        self.assertTrue(row["readBy"])

    def test_queries_show_recurrence_and_predictability(self):
        """A query shows its schedule, predictability and range."""
        self.assertEqual(self.js.call("queryScheduleText", self.doc["workload"]["queries"][0]), "every 10 s · predictable")
        query = {"recurrence": {"one_time": {"invocations": 1, "execute_at": None}}, "predictability": "ad_hoc",
                 "requirements": {"range_evaluation": {"step_ms": 60000, "span_ms": 3600000}}}
        self.assertEqual(self.js.call("queryScheduleText", query), "once · ad hoc · range of 1 h in 1 min steps")

    def test_tumbling_window_chain_box_shows_its_summary_type_and_width(self):
        """A tumbling-window chain's box shows its summary type and tumbling-window width."""
        chain = next(n for n in example3a()["stage3_plan_selection"]["plan"]["nodes"] if "SummaryTumblingWindowChain" in n["payload"].get("ASAP", {}))
        lines = self.js.call("stageNodeLines", chain["payload"], None)
        self.assertEqual(lines[:3], ["SummaryTumblingWindowChain", "summary: Kll", "tumbling windows of 24 h"])

    def test_usage_and_deployment_lines(self):
        """The usage and deployment lines read the plan's cost and capabilities."""
        cost = self.doc["stage3_plan_selection"]["cost"]
        self.assertIn("peak cores", self.js.call("usageText", cost["usage"]))
        no_raw = dict(self.js.call("settingsRows", {"deployment_capabilities": NO_RAW_DATA}))
        self.assertEqual([no_raw["raw data"], no_raw["query-time results"], no_raw["summaries"]], ["not retained", "not retained", "every summary the planner builds"])
        rows = dict(self.js.call("settingsRows", self.doc))
        self.assertEqual(rows["raw data"], "retained")
        self.assertTrue(rows["summaries"].startswith("Cms (Total), "))
        self.assertEqual(rows["cost model"], cost["source"])

    def test_planning_workload_queries_read_the_workload_json(self):
        """A PlanningWorkload is recognised and its queries listed."""
        workload = json.loads((WORKLOADS / "random-mixed-seed-862" / "workload-01.json").read_text())
        self.assertTrue(self.js.call("isPlanningWorkload", workload))
        self.assertFalse(self.js.call("isPlanningWorkload", self.doc))
        queries = self.js.call("planningWorkloadQueries", workload)
        self.assertEqual(len(queries), 9)
        self.assertEqual(queries[0]["runs"], "every 20 s")
        self.assertIn("δ=0.3", queries[0]["accuracy"])


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
@needs_planner
class PathsTests(unittest.TestCase):
    """Each node lists the queries whose root reaches it, and the query and
    node neighborhoods the page highlights. Example 3a's five queries share
    one tumbling-window chain over latency_ms."""

    def setUp(self):
        self.js = stages_js()
        self.doc = example3a()
        self.plan = self.doc["stage3_plan_selection"]["plan"]
        self.query_ids = [q["id"] for q in self.doc["workload"]["queries"]]
        self.chain = next(n for n in self.plan["nodes"] if "SummaryTumblingWindowChain" in n["payload"].get("ASAP", {}))

    def elements(self):
        return self.js.call("stageLaneElements", self.plan, {"physical": True, "queryIds": self.query_ids})

    def node(self, elements, element_id):
        return next(e for e in lane_nodes(elements) if e["data"]["id"] == element_id)

    def test_each_node_lists_the_queries_reaching_it_and_shared_nodes_say_so(self):
        """Each node lists the queries reaching it; shared nodes say so."""
        elements = self.elements()
        chain = self.node(elements, f"n{self.chain['id']}")
        self.assertEqual(chain["data"]["reachedBy"], self.query_ids)
        self.assertIn(f"shared ×{len(self.query_ids)}", chain["data"]["label"])
        root = self.node(elements, f"n{self.plan['roots'][0]}")
        self.assertEqual(root["data"]["reachedBy"], [self.query_ids[0]])
        self.assertNotIn("shared", root["data"]["label"])

    def test_logical_lanes_list_the_queries_reaching_each_node_too(self):
        """Stage 0 nodes list the queries reaching them too."""
        stage0 = self.doc["stage0_query_language_frontends"]["dag"]
        elements = self.js.call("stageLaneElements", stage0, {"physical": False, "queryIds": self.query_ids})
        self.assertTrue(all(e["data"]["reachedBy"] for e in lane_nodes(elements)))

    def test_a_query_reaches_its_nodes_and_edges_only(self):
        """Highlighting a query keeps exactly its nodes and edges."""
        elements = self.elements()
        shown = set(self.js.call("stageQueryElementIds", elements, "q3"))
        nodes = {e["data"]["id"] for e in lane_nodes(elements) if e["data"]["id"] in shown}
        self.assertEqual(nodes, {e["data"]["id"] for e in lane_nodes(elements) if "q3" in e["data"]["reachedBy"]})
        for edge in lane_edges(elements):
            self.assertEqual(edge["data"]["id"] in shown, edge["data"]["source"] in nodes and edge["data"]["target"] in nodes)
        self.assertIn(f"n{self.chain['id']}", shown)
        self.assertNotIn(f"n{self.plan['roots'][0]}", shown)

    def test_a_node_neighborhood_is_everything_upstream_and_downstream(self):
        """A node's neighborhood is everything upstream and downstream of it."""
        elements = self.elements()
        root = f"n{self.plan['roots'][0]}"
        shown = set(self.js.call("stageNeighborhoodIds", elements, root))
        self.assertIn(f"n{self.chain['id']}", shown)  # upstream: the shared chain
        self.assertIn(f"n{self.chain['payload']['ASAP']['SummaryTumblingWindowChain']['child']}", shown)  # and its scan
        self.assertNotIn(f"n{self.plan['roots'][1]}", shown)  # another query's root
        around = set(self.js.call("stageNeighborhoodIds", elements, f"n{self.chain['id']}"))
        self.assertTrue(all(f"n{r}" in around for r in self.plan["roots"]))  # downstream: every root


class WorkloadSetTests(unittest.TestCase):
    """workloads/index.json lists sets of PlanningWorkload files; the Rust
    test validates each file with `PlanningWorkload::validate`."""

    def test_index_names_existing_files(self):
        """Every set names existing PlanningWorkload files, and every
        workload file outside planner-layering/ (examples.json's) is in a set."""
        index = json.loads((WORKLOADS / "index.json").read_text())
        self.assertEqual(index["format"], "asap-workload-sets/v1")
        files = set()
        for workload_set in index["sets"]:
            for field in ("id", "name", "description", "source"):
                self.assertTrue(workload_set[field], field)
            for entry in workload_set["workloads"]:
                path = WORKLOADS / entry["file"]
                self.assertTrue(path.exists(), path)
                self.assertIn("query_workload", json.loads(path.read_text()))
                files.add(path)
                self.assertNotIn("document", entry)
        self.assertEqual(files, {path for path in WORKLOADS.glob("*/*.json")
                                 if path.name != server.TABLES_FILE and path.parent.name != "planner-layering"})
        self.assertEqual(len(files), 30)


class PageTests(unittest.TestCase):
    """The page's scripts, element ids and examples line up."""

    def test_scripts_exist(self):
        """Every script the page loads exists."""
        page = (HERE / "index.html").read_text()
        for src in re.findall(r'<script src="([^"]+)"', page):
            with self.subTest(src):
                self.assertTrue((HERE / src).exists())

    def test_every_element_the_scripts_use_is_on_the_page(self):
        """Every element id the scripts look up is on the page."""
        page = (HERE / "index.html").read_text()
        ids = set(re.findall(r'id="([^"]+)"', page))
        for script in ("app.js", "workload-sets.js", "memo.js"):
            used = set(re.findall(r"\$\('([^']+)'\)", (HERE / script).read_text()))
            with self.subTest(script):
                self.assertLessEqual(used, ids)

    def test_workload_sets_send_the_tables_next_to_a_sql_workload(self):
        """A SQL workload is planned with the tables.json next to its file,
        sent as the request's tables (server.py passes one
        --sql-catalog-table each)."""
        self.assertIn("tables.json", (HERE / "workload-sets.js").read_text())
        self.assertRegex((HERE / "app.js").read_text(), r"JSON\.stringify\(\{ workload, tables")

    def test_examples_are_workloads_written_into_out(self):
        """Each example is a planner-layering workload file whose document
        server.py writes into the git-ignored out/."""
        examples = json.loads((HERE / "examples.json").read_text())
        self.assertEqual([e["id"] for e in examples], ["example1", "example3a", "example3b"])
        for example in examples:
            with self.subTest(example["id"]):
                self.assertTrue(example["story"])
                self.assertTrue(example["workload"].startswith("workloads/planner-layering/"))
                self.assertTrue((HERE / example["workload"]).exists())
                self.assertTrue(example["file"].startswith("out/"))
        self.assertEqual((HERE / ".gitignore").read_text().split(), ["out/"])

    def test_no_stage_documents_are_committed(self):
        """Stage documents are planned on demand: none is committed."""
        self.assertFalse((HERE / "examples").exists())


STUB_DOM = r"""
var __els = {}; var __files = {};
function __el(id) {
  if (!__els[id]) __els[id] = { id, innerHTML: '', textContent: '', hidden: false, value: '', dataset: {},
    __on: {}, addEventListener(type, fn) { this.__on[type] = fn; }, setAttribute() {}, click() {}, querySelectorAll() { return []; } };
  return __els[id];
}
var document = { getElementById: __el, querySelectorAll() { return []; }, documentElement: {}, body: { classList: { add() {}, remove() {} } }, addEventListener() {} };
var window = { matchMedia() { return { addEventListener() {} }; } };
var location = { hash: '', search: '', pathname: '/' };
var history = { replaceState(a, b, h) { location.hash = h; } };
function getComputedStyle() { return { getPropertyValue() { return ''; } }; }
function MutationObserver() { this.observe = function () {}; }
var URLSearchParams = function (s) { this.get = (k) => { const m = s.match(new RegExp('[?&]' + k + '=([^&]*)')); return m ? decodeURIComponent(m[1]) : null; }; };
var __nodes = 0;
var __taps = {};
var __dimmed = {};
var __rendered = {};
function cytoscape(o) {
  __nodes += o.elements.length;
  const lane = o.container.id;
  __rendered[lane] = o.elements;
  __dimmed[lane] = [];
  const element = (e) => ({ id() { return e.data.id; }, data() { return e.data; }, length: 1,
    toggleClass(name, on) { if (name === 'dimmed' && on) __dimmed[lane].push(e.data.id); }, select() {}, unselect() {} });
  const all = () => { const list = o.elements.map(element); list.unselect = () => {}; return list; };
  return { on(event, what, fn) { __taps[lane + ' ' + (typeof what === 'function' ? 'background' : what)] = typeof what === 'function' ? what : fn; },
    destroy() {}, style() {}, elements: all, batch(fn) { __dimmed[lane] = []; fn(); },
    getElementById(id) { const e = o.elements.find((x) => x.data.id === id); return e ? element(e) : { length: 0 }; },
    center() {} };
}
function fetch(u) { return Promise.resolve({ ok: u in __files, status: 404, json() { return Promise.resolve(JSON.parse(__files[u])); } }); }
"""


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
@needs_planner
class AppTests(unittest.TestCase):
    """app.js loads documents and fills the page (stub DOM, no browser)."""

    def page(self, search="", files=None, hash_=""):
        js = py_mini_racer.MiniRacer()
        js.eval(STUB_DOM)
        for name in ("node-style.js", "stages.js", "memo.js"):
            js.eval((HERE / name).read_text())
        for url, text in (files or {}).items():
            js.eval(f"__files[{json.dumps(url)}] = {json.dumps(text)};")
        js.eval(f"location.search = {json.dumps(search)}; location.hash = {json.dumps(hash_)};")
        js.eval((HERE / "app.js").read_text())
        time.sleep(0.05)
        return js

    def ev(self, js, expression):
        return json.loads(js.eval(f"JSON.stringify({expression})"))

    def text(self, js, element):
        return js.eval(f"__els[{json.dumps(element)}].innerHTML.replace(/<[^>]+>/g, '')")

    def tap(self, js, lane, element_id):
        js.eval(f"__taps['{lane} node']({{ target: {{ data() {{ return __rendered.{lane}.find((e) => e.data.id === {json.dumps(element_id)}).data; }}, id() {{ return {json.dumps(element_id)}; }} }} }})")

    def test_doc_parameter_opens_one_document_with_stage0_and_the_plan(self):
        """?doc= opens one document: Stage 0 and the plan drawn, the plan's
        row with its cost and accepted accuracy, the counts and the header."""
        js = self.page("?doc=examples/e1.json", {"examples/e1.json": json.dumps(example1())})
        self.assertIn("Candidates per stage", js.eval("__els.counts.textContent"))
        lane = self.text(js, "lane1-meta")
        self.assertRegex(lane, r"^\d+ nodes · cost [\d.]+ per second · q1: accepted exact")
        self.assertIn("Stage 3 usage: CPU", lane)
        outcome = js.eval("__els.outcome.textContent")
        self.assertTrue(outcome.startswith("plan selected · "))
        self.assertIn("candidate-connection search · MILP", outcome)
        self.assertIn("accuracy guaranteesenforced", self.text(js, "deployment"))
        self.assertEqual(len(lane_nodes(self.ev(js, "__rendered.cy1"))), len(example1()["stage3_plan_selection"]["plan"]["nodes"]))
        self.assertGreater(len(self.ev(js, "__rendered.cy0")), 0)

    def test_a_failed_selection_shows_stage0_and_the_error(self):
        """Without a plan, Stage 0 is still drawn and the row, the plan lane
        and the header show selection's error."""
        doc = example1()
        doc["stage3_plan_selection"] = {"plan": None, "error": "no valid candidate", "materialized_views": []}
        js = self.page("?doc=d.json", {"d.json": json.dumps(doc)})
        self.assertEqual(js.eval("__els['lane1-meta'].textContent"), "no plan: no valid candidate")
        self.assertTrue(js.eval("__els.outcome.textContent").startswith("no plan selected: no valid candidate"))
        self.assertFalse(js.eval("'cy1' in __rendered"))
        self.assertGreater(len(self.ev(js, "__rendered.cy0")), 0)
        self.assertEqual(js.eval("__els['views-meta'].textContent"), "no plan selected")

    def test_a_stage1_failure_shows_stage0_and_the_error(self):
        """A Stage 1 failure still draws Stage 0 and shows the error."""
        doc = planned(json.loads(EXAMPLE3A.read_text()), NO_RAW_DATA)
        js = self.page("?doc=d.json", {"d.json": json.dumps(doc)})
        self.assertIn("Stage 1 failed: deployment capabilities", js.eval("__els.outcome.textContent"))
        self.assertGreater(len(self.ev(js, "__rendered.cy0")), 0)

    def test_examples_become_tabs_and_the_hash_picks_one(self):
        """examples.json entries become tabs; the hash picks one."""
        sample = json.dumps(example1())
        manifest = json.dumps([{"id": "a", "name": "A", "file": "out/a.json", "story": "story A"},
                               {"id": "b", "name": "B", "file": "out/b.json", "story": "story B"}])
        js = self.page("", {"examples.json": manifest, "out/a.json": sample, "out/b.json": sample}, "#b")
        self.assertIn('data-id="b"', js.eval("__els.tabs.innerHTML"))
        self.assertEqual(js.eval("__els.story.textContent"), "story B")

    def test_node_details_without_a_numeric_cost(self):
        """A per_node entry without a numeric `cost` does not break the details panel."""
        js = self.page("?doc=e1.json", {"e1.json": json.dumps(example1())})
        js.eval("""__taps['cy1 node']({ target: { data() { return {
            kind: 'Scan', nodeId: 0, rootFor: [], timing: 'query_time', cost: { detail: 'not costed' },
            node: { payload: { NonASAP: { Scan: {} } }, guarantee: null } }; } } })""")
        self.assertIn("not costed", self.text(js, "details"))

    def test_a_document_fills_the_data_workload_and_says_why_the_plan_has_no_views(self):
        """A planned workload fills the data workload and deployment panels;
        a plan that is not installable says why under materialized views."""
        doc = example1()
        js = self.page("?doc=s.json", {"s.json": json.dumps(doc)})
        self.assertIn("q1, q2", self.text(js, "dataSources"))
        self.assertIn("ingesting", js.eval("__els.dataArrival.textContent"))
        self.assertIn("raw dataretained", self.text(js, "deployment"))
        self.assertIn("every 10 s · predictable", self.text(js, "queries"))
        self.assertIn(f"no installable plan: {doc['stage3_plan_selection']['materialized_views_error']}", self.text(js, "views-meta"))

    def test_materialized_views_table_has_one_row_per_view_and_a_row_selects_its_node(self):
        """The materialized views table has one row per view; clicking it
        shows its plan node."""
        doc = example1()
        doc["stage3_plan_selection"].pop("materialized_views_error")
        doc["stage3_plan_selection"]["materialized_views"] = [materialized_view(2)]
        js = self.page("?doc=s.json", {"s.json": json.dumps(doc)})
        self.assertEqual(js.eval("(__els.materializedViews.innerHTML.match(/data-node=/g) || []).length"), 1)
        self.assertIn("1 materialized view", self.text(js, "views-meta"))
        self.assertIn("t−5 min … t", self.text(js, "materializedViews"))

    def test_picking_a_query_dims_what_it_does_not_reach(self):
        """Picking a query dims what it does not reach; clearing it undims."""
        js = self.page("?doc=s.json", {"s.json": json.dumps(example3a())})
        self.assertIn('value="q3"', js.eval("__els.queryPick.innerHTML"))
        js.eval("__els.queryPick.__on.change({ target: { value: 'q3' } })")
        dimmed = set(self.ev(js, "__dimmed.cy1"))
        rendered = self.ev(js, "__rendered.cy1")
        reached = {e["data"]["id"] for e in rendered if "source" not in e["data"] and "q3" in e["data"]["reachedBy"]}
        self.assertTrue(dimmed)
        self.assertFalse(dimmed & reached)
        self.assertTrue(js.eval("__dimmed.cy0.length") > 0)
        js.eval("__els.queryPick.__on.change({ target: { value: '' } })")
        self.assertEqual(js.eval("__dimmed.cy1.length"), 0)

    def test_tapping_a_node_highlights_its_neighborhood_and_lists_its_queries(self):
        """Tapping a node highlights its neighborhood and lists its queries."""
        js = self.page("?doc=s.json", {"s.json": json.dumps(example3a())})
        root = next(e for e in self.ev(js, "__rendered.cy1") if e["data"].get("rootFor"))
        self.tap(js, "cy1", root["data"]["id"])
        self.assertIn("queries reaching it", self.text(js, "details"))
        self.assertIn(", ".join(root["data"]["reachedBy"]), self.text(js, "details"))
        self.assertTrue(js.eval("__dimmed.cy1.length") > 0)
        self.assertNotIn(root["data"]["id"], self.ev(js, "__dimmed.cy1"))
        js.eval("__taps['cy1 background']({ target: { isNode: false } })")
        self.assertEqual(js.eval("__dimmed.cy1.length"), 0)

    def test_an_invalid_document_shows_an_error(self):
        """An invalid document shows an error."""
        js = self.page("?doc=bad.json", {"bad.json": json.dumps({"format": "nope"})})
        self.assertIn("Could not load", js.eval("__els.queries.innerHTML"))

    def memo_page(self, doc=None, show_pruned=True):
        """The p90 fixture document (or `doc`) on the page, with Pass 2's
        pruned candidates shown unless `show_pruned` is false."""
        js = self.page("?doc=m.json", {"m.json": json.dumps(doc or p90_kll())})
        if show_pruned:
            js.eval("__els.memoShowPruned.checked = true; __els.memoShowPruned.__on.change()")
        return js

    def test_the_memo_view_hides_pruned_candidates_until_asked(self):
        """Pass 2's pruned candidates are hidden until the box is ticked."""
        js = self.memo_page(show_pruned=False)
        self.assertEqual(self.memo_ids(js, "g3"), ["g3c4", "g3c5", "g3c6", "g3c7"])
        js.eval("__els.memoShowPruned.checked = true; __els.memoShowPruned.__on.change()")
        self.assertEqual(len(self.memo_ids(js, "g3")), 8)

    def tap_memo(self, js, element_id):
        js.eval(f"__taps['cyMemo node']({{ target: {{ data() {{ return __rendered.cyMemo.find((e) => e.data.id === {json.dumps(element_id)}).data; }} }} }})")

    def memo_ids(self, js, parent=None):
        return [e["data"]["id"] for e in self.ev(js, "__rendered.cyMemo") if "source" not in e["data"] and (parent is None or e["data"].get("parent") == parent)]

    def test_a_document_without_a_memo_hides_the_memo_view(self):
        """Without a memo section the memo view stays hidden."""
        doc = example1()
        del doc["memo"]
        js = self.page("?doc=e1.json", {"e1.json": json.dumps(doc)})
        self.assertTrue(js.eval("__els.memoView.hidden"))
        self.assertFalse(js.eval("'cyMemo' in __rendered"))
        self.assertTrue(js.eval("__els.outcome.textContent").startswith("plan selected"))

    def test_the_memo_view_has_a_tab_per_step_and_opens_on_the_last(self):
        """The memo view has a tab per step and opens on the last."""
        js = self.memo_page()
        self.assertFalse(js.eval("__els.memoView.hidden"))
        tabs = js.eval("__els.memoTabs.innerHTML")
        self.assertEqual(re.findall(r">([^<]+)</button>", tabs), ["Stage 0", "Stage 1 · Pass 1", "Stage 1 · Pass 2"])
        self.assertIn('data-step="2" aria-pressed="true"', tabs)
        meta = self.text(js, "memoView-meta")
        self.assertIn("7 groups (+2) · 17 candidates (+5)", meta)
        self.assertIn("selected plan's candidates highlighted (5 groups)", meta)
        self.assertEqual(len(self.memo_ids(js, "g3")), 8)
        js.eval("window.MemoView.selectStep(0)")
        self.assertEqual(len(self.memo_ids(js)), 3)
        self.assertIn("3 groups · 0 candidates", self.text(js, "memoView-meta"))
        self.assertIn("shown on the last tab", self.text(js, "memoView-meta"))

    def test_tapping_counts_drills_down_to_candidates_and_the_group_goes_back(self):
        """Tapping a count drills into it until the group shows candidates
        (at most 12); the details offer the dimensions to split by; tapping
        the group goes back one level at a time."""
        doc = example1()
        js = self.memo_page(doc)
        pass2 = doc["memo"]["steps"][-1]
        big = next(g for g in pass2["groups"] if g["candidate_count"] > 12)
        group = f"g{big['id']}"
        levels = [self.memo_ids(js, group)]
        self.assertTrue(all("b" in i for i in levels[0]))
        self.tap_memo(js, group)
        self.assertIn('data-dim="', js.eval("__els.details.innerHTML"))
        while "b" in levels[-1][0]:
            self.tap_memo(js, levels[-1][0])
            levels.append(self.memo_ids(js, group))
        self.assertTrue(all("c" in i for i in levels[-1]))
        self.assertLessEqual(len(levels[-1]), 12)
        self.assertIn(f"Memo group G{big['id']}", self.text(js, "details"))
        for level in reversed(levels[:-1]):
            self.tap_memo(js, group)
            self.assertEqual(self.memo_ids(js, group), level)

    def test_tapping_a_candidate_lists_its_logical_edges_and_physical_candidates(self):
        """Tapping a memo candidate shows its θ and required accuracy, its
        logical edges (α^log, with why it is 0), and its physical
        candidates with their physical edges (α^phys), the selected edge marked."""
        js = self.memo_page()
        self.tap_memo(js, "g4c0")
        details = self.text(js, "details")
        self.assertIn("Candidate #0 of G4", details)
        self.assertIn("used by the selected plan", details)
        self.assertIn("required accuracyG3: rank ≤ 0.01, δ 0.00001", details)
        self.assertIn("pruned by Pass 2no", details)
        self.assertIn("logical edges (α^log)input G3 · required rank ≤ 0.01, δ 0.00001", details)
        self.assertIn("✗ #0: α^log = 0: the producer's guarantee exceeds", details)
        self.assertIn("✓ #5", details)
        self.assertIn("C19 (query_time, OneEvaluation(Memory))", details)
        self.assertIn("✓ C12 · selected", details)
        self.tap_memo(js, "g3c5")
        self.assertIn("parameters θθ Kll(k=1404)", self.text(js, "details"))

    def test_tapping_a_pruned_candidate_shows_why_pass2_pruned_it(self):
        """A candidate Pass 2 pruned shows its root cause and has no
        physical candidate."""
        js = self.memo_page()
        self.tap_memo(js, "g3c0")
        details = self.text(js, "details")
        self.assertIn("pruned by Pass 2yes", details)
        self.assertIn("why Pass 2 pruned it", details)
        self.assertIn("guarantee exceeds Rank ≤ 0.01 (δ 0.00001)", details)
        self.assertIn("physical candidates (Stage 2) and their edges (α^phys)none", details)

    def test_tapping_a_group_lists_its_candidates_and_pruned_candidates(self):
        """A group's panel lists each candidate with θ, and the candidates
        Pass 2 (with their cause), Stage 2 and Stage 3 pruned there."""
        doc = p90_kll()
        doc["stage3_plan_selection"]["pruned_candidates"] = [
            {"group": 3, "index_in_group": 6, "properties": {"timing": "query_time", "lifetime": "None"}, "reason": {"dominated_by": 12}}]
        js = self.memo_page(doc)
        self.tap_memo(js, "g3")
        details = self.text(js, "details")
        self.assertIn("#0 SummaryAgg Kll · θ Kll(k=26) · pruned by Pass 2", details)
        self.assertIn("pruned by Pass 24", details)
        self.assertIn("#0: the producer", details)
        self.assertIn("#4 (query_time, AcrossEvaluations(Memory)): query time results not retained", details)
        self.assertIn("#6 (query_time, None): dominated by C12", details)

    def test_highlighting_a_query_dims_the_memo_groups_it_does_not_reach(self):
        """Highlighting a query dims the memo groups it does not reach."""
        doc = p90_kll()
        doc["memo"]["steps"][-1]["groups"][6]["queries"] = []
        js = self.memo_page(doc)
        js.eval("__els.queryPick.__on.change({ target: { value: 'q1' } })")
        dimmed = set(self.ev(js, "__dimmed.cyMemo"))
        self.assertIn("g6", dimmed)
        self.assertIn("g6c0", dimmed)
        self.assertNotIn("g3c5", dimmed)
        js.eval("__els.queryPick.__on.change({ target: { value: '' } })")
        self.assertEqual(js.eval("__dimmed.cyMemo.length"), 0)

    def test_a_broken_memo_is_refused(self):
        """A malformed memo refuses the document."""
        doc = p90_kll()
        doc["memo"]["steps"][1]["groups"][0]["children"] = [42]
        js = self.memo_page(doc)
        self.assertIn("G0: children and parents must name groups", js.eval("__els.queries.innerHTML"))

    def test_the_real_memo_example(self):
        """On the memo asap_planner writes for #1005's ratio-of-quantiles
        query, each tab counts the groups and candidates of its step."""
        doc = planned(json.loads(RATIO_OF_QUANTILES.read_text()))
        js = self.page("?doc=r.json", {"r.json": json.dumps(doc)})
        steps = doc["memo"]["steps"]
        names = [step["step"] for step in steps]
        self.assertEqual(names, ["stage0", "pass1", "pass2"])
        js.eval(f"window.MemoView.selectStep({names.index('stage0')})")
        groups = [e for e in self.ev(js, "__rendered.cyMemo") if e["data"].get("kind") == "group"]
        self.assertEqual(len(groups), len(steps[0]["groups"]))
        pass1 = steps[1]
        added = len(pass1["groups"]) - len(steps[0]["groups"])
        candidates = sum(len(g["candidates"]) for g in pass1["groups"])
        js.eval("window.MemoView.selectStep(1)")
        self.assertIn(f"{len(pass1['groups'])} groups (+{added}) · {candidates} candidates (+{candidates})",
                      self.text(js, "memoView-meta"))
        js.eval("window.MemoView.selectStep(2)")
        self.assertIn(f"{len(steps[-1]['groups'])} groups", self.text(js, "memoView-meta"))


if __name__ == "__main__":
    unittest.main()
