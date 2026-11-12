"""Tests for the ASAPPlanner Stage Viewer: the stage-document functions in
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
# A hand-written priced `memo` section for Example 1: four enumerated
# Pass 1 DAGs, E2 selected, E3 cheaper than it, E4 invalid.
MEMO_ENUMERATION = HERE / "test-fixtures" / "memo-enumeration-small.json"
# A hand-written `memo` section over example1's two queries (memo.js tests).
MEMO_SMALL = HERE / "test-fixtures" / "memo-small.json"
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


# Three exact PromQL queries every 10 s; planned without raw data, q1 and q2
# share one tumbling-window chain over m (6 tumbling windows × 10 s) and q3
# reads another over n.
TUMBLING_WINDOWS_WORKLOAD = json.loads((HERE / "test-fixtures" / "tumbling-windows-workload.json").read_text())


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


def tumbling_windows_doc() -> dict:
    """TUMBLING_WINDOWS_WORKLOAD planned without raw data: its plan keeps two
    tumbling-window chains and their materialized views."""
    return planned(TUMBLING_WINDOWS_WORKLOAD, NO_RAW_DATA)


def selected_id(doc: dict) -> str:
    """The selected physical candidate's id: `P1` in a document whose Stage 3
    section is the `Selection` (asap_planner's output), the section's
    `selected` in an archived one."""
    selection = doc["stage3_plan_selection"]
    return "P1" if "selected_dag" in selection else selection["selected"]


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

    def test_a_pass1_failure_is_shown_with_its_error(self):
        """When Pass 1 rejects a query, the document (Stage 2 and Stage 3
        null) passes the checks and keeps the Stage 1 error to show."""
        workload = promql_workload(["topk(5, rate(m[5m]))"], ["m"])
        workload["query_workload"]["repeating_queries"][0]["requirements"]["accuracy"] = {
            "max_error": 1.5, "failure_probability": 0.1}
        doc = planned(workload, ignore_accuracy_guarantees=True)
        self.assertIsNone(doc["stage2_physical_asap_optimization"])
        doc = self.js.call("fillStagesNotRun", doc)
        self.assertEqual(self.validate(doc), [])
        self.assertTrue(doc["stage1_logical_asap_optimization"]["error"].startswith("Pass 1: "))

    def test_a_workload_example_with_materialized_views_is_valid(self):
        """A plan with materialized views passes the checks."""
        doc = tumbling_windows_doc()
        self.assertTrue(doc["stage3_plan_selection"]["materialized_views"])
        self.assertEqual(self.validate(doc), [])

    def test_documents_without_the_new_sections_are_still_valid(self):
        """A document without data workload, views or accuracy mode is valid."""
        doc = tumbling_windows_doc()
        del doc["workload"]["data_workload"]
        del doc["stage3_plan_selection"]["materialized_views"]
        del doc["accuracy_guarantees"]
        self.assertEqual(self.validate(doc), [])

    def test_accuracy_guarantees_is_ignored_or_enforced(self):
        """accuracy_guarantees is "ignored" or "enforced"; another value is refused."""
        self.assertEqual(self.doc["accuracy_guarantees"], "enforced")
        for mode in ("ignored", "enforced"):
            self.assertEqual(self.validate(dict(self.doc, accuracy_guarantees=mode)), [])
        self.assertEqual(self.validate(dict(self.doc, accuracy_guarantees="off")),
                         ['accuracy_guarantees must be "ignored" or "enforced"'])

    def test_data_workload_and_materialized_view_violations_are_reported(self):
        """Each malformed data workload or materialized view is reported."""
        stored = lambda d: d["stage3_plan_selection"]["materialized_views"]
        cases = {
            "data workload not an object": lambda d: d["workload"].update(data_workload=[]),
            "sources not an object": lambda d: d["workload"]["data_workload"].update(sources=[]),
            "source facts not an object": lambda d: d["workload"]["data_workload"]["sources"].update(data=3),
            "materialized views not an array": lambda d: d["stage3_plan_selection"].update(materialized_views={}),
            "node id": lambda d: stored(d)[0].update(node_id="2"),
            "refresh": lambda d: stored(d)[0].update(refresh="later"),
            "definition id": lambda d: stored(d)[0]["coverage"].pop("definition_id"),
            "inputs": lambda d: stored(d)[0]["inputs"].pop("raw_data_windows"),
            "retention": lambda d: stored(d)[0].update(retention_ms=-1),
            "stage 2 node": lambda d: stored(d)[0].update(stage2_node_id=99999),
            "views without a selection": lambda d: d["stage3_plan_selection"].pop("cost"),
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                doc = tumbling_windows_doc()
                mutate(doc)
                self.assertTrue(self.validate(doc), name)

    def test_pass1_failure_is_valid(self):
        """A run whose Pass 1 failed has Stage 0, the error and no plan."""
        self.doc["stage1_logical_asap_optimization"] = {"combinations": 0, "capped": False, "candidates": [], "error": "Pass 1: bad"}
        self.doc["stage2_physical_asap_optimization"] = {"candidates": [], "pruned": []}
        self.doc["stage3_plan_selection"] = {"error": "Pass 1: bad"}
        self.assertEqual(self.validate(self.doc), [])

    def test_failed_selection_and_composition_errors_are_valid(self):
        """A failed selection and an uncomposed Stage 1 candidate are valid."""
        self.doc["stage1_logical_asap_optimization"]["candidates"].append({"id": "L99", "display_name": "x", "error": "cannot substitute"})
        self.doc["stage3_plan_selection"] = {"error": "no data workload"}
        self.assertEqual(self.validate(self.doc), [])

    def test_contract_violations_are_reported(self):
        """Each break of the document contract is reported."""
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
            "selected plan missing": lambda d: physical(d)[0].update(id="P9"),
            "unaccounted candidate": lambda d: physical(d).append(dict(physical(d)[0], id="P2")),
            "per_node": lambda d: selection(d)["cost"]["per_node"].update({"99": {"cost_per_second": 1, "multiplier": 1}}),
            "cost per second": lambda d: selection(d)["cost"].pop("cost_per_second"),
            "missing stage": lambda d: d.pop("stage2_physical_asap_optimization"),
            "binary operand names no node": lambda d: stage0(d)["nodes"].append(
                {"operator": {"NonASAP": {"BinaryOp": {"lhs": 0, "rhs": 42, "operator": {}, "return_bool": False}}}}),
            "scalar root past the queries": lambda d: physical(d)[0]["dag"].update(
                roots=physical(d)[0]["dag"]["roots"][:1], scalar_roots=[[2, "EvalTimestamp"]]),
            "scalar roots out of order": lambda d: physical(d)[0]["dag"].update(
                roots=[], scalar_roots=[[1, "EvalTimestamp"], [0, "EvalTimestamp"]]),
            "scalar roots not an array": lambda d: physical(d)[0]["dag"].update(scalar_roots={"0": "EvalTimestamp"}),
            "scalar root reads no node": lambda d: physical(d)[0]["dag"].update(
                roots=physical(d)[0]["dag"]["roots"][:1], scalar_roots=[[1, {"PromqlScalarFromVector": 99}]]),
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
class CandidateRowTests(unittest.TestCase):
    """The Stage 3 list: every candidate the document shows, with its
    outcome."""

    def setUp(self):
        self.js = stages_js()
        self.doc = example1()

    def test_pruned_and_composition_errors_come_last_with_their_reasons(self):
        """A Stage 2 pruned candidate and a Stage 1 candidate that did not
        compose are rows too, after the Stage 3 ones."""
        self.doc["stage2_physical_asap_optimization"]["pruned"].append({"from_logical": "L2", "display_name": "Q1 exact", "reason": "no implementation"})
        self.doc["stage1_logical_asap_optimization"]["candidates"].append({"id": "L99", "display_name": "Q1 exact", "error": "cannot substitute"})
        rows = self.js.call("stageCandidateRows", self.doc)
        self.assertEqual(
            rows[-2:],
            [
                {"id": "L2", "logical": "L2", "display_name": "Q1 exact", "total": None, "rank": None, "status": "pruned", "reason": "no implementation"},
                {"id": "L99", "logical": "L99", "display_name": "Q1 exact", "total": None, "rank": None, "status": "composition error", "reason": "cannot substitute"},
            ],
        )

    def test_failed_selection_leaves_candidates_not_judged(self):
        """When selection fails with `error`, Stage 3 judged no candidate:
        the rows are "not judged", not invalid."""
        self.doc["stage3_plan_selection"] = {"error": "no data workload"}
        rows = self.js.call("stageCandidateRows", self.doc)
        self.assertEqual({row["status"] for row in rows}, {"not judged"})
        self.assertTrue(all(row["reason"] is None for row in rows))

    def test_valid_rows_show_the_accepted_metric(self):
        """Every valid candidate's row says, per query, which metric Stage 3
        accepted and whether the target set one; invalid rows say nothing."""
        rows = self.js.call("stageCandidateRows", self.doc)
        for row in rows:
            if row["status"] in ("selected", "costlier"):
                self.assertEqual(row["accepted"], "q1: metric not set; accepted exact; q2: metric not set; accepted exact")
            else:
                self.assertIsNone(row["accepted"])

    def test_accuracy_text_reads_the_target_json(self):
        """An accuracy target reads as its ε, δ, metric and scope."""
        text = lambda target: self.js.call("accuracyText", target)
        self.assertEqual(text({"max_error": 0.0, "failure_probability": 0.0, "accuracy_scope": "whole_result"}), "exact")
        self.assertEqual(text({"max_error": 0.01, "accuracy_scope": "whole_result"}), "ε=0.01 (metric not set)")
        self.assertEqual(text({"accuracy_metric": "rank", "max_error": 0.01, "failure_probability": 0.001, "accuracy_scope": "whole_result"}), "ε=0.01, δ=0.001 (rank)")
        self.assertEqual(self.js.call("accuracyText", {"max_error": 0.01}, "relative"), "ε=0.01 (metric not set; default relative)")
        self.assertEqual(text(None), "")
        self.assertEqual(text({"accuracy_metric": "relative", "max_error": 0.05, "failure_probability": 0.05, "accuracy_scope": "per_value"}), "ε=0.05, δ=0.05 (relative; per value)")
        self.assertEqual(text({"accuracy_metric": "relative", "max_error": 0.05, "failure_probability": 0.05, "accuracy_scope": {"groups_with_record_share_at_least": 0.05}}), "ε=0.05, δ=0.05 (relative; groups ≥ 5% of records)")
        self.assertEqual(text({"max_error": 0.05, "accuracy_scope": {"groups_with_record_share_at_least": 0.01}}), "ε=0.05 (metric not set; groups ≥ 1% of records)")

    def test_header_counts_candidates_and_shows_a_selection_error(self):
        """The header counts the candidates and shows a selection error."""
        combinations = self.doc["stage1_logical_asap_optimization"]["combinations"]
        self.assertEqual(self.js.call("stageCandidateHeader", self.doc), f"{combinations} candidates")
        self.doc["stage1_logical_asap_optimization"]["capped"] = True
        self.doc["stage3_plan_selection"] = {"error": "no data workload"}
        shown = len(self.doc["stage1_logical_asap_optimization"]["candidates"])
        self.assertEqual(
            self.js.call("stageCandidateHeader", self.doc),
            f"{combinations} candidates (document shows {shown}) · selection failed: no data workload",
        )

    def test_header_shows_the_stage1_error(self):
        """The header shows a Stage 1 failure."""
        self.doc["stage1_logical_asap_optimization"] = {"combinations": 0, "capped": False, "candidates": [], "error": "Pass 1: bad accuracy"}
        self.assertEqual(self.js.call("stageCandidateHeader", self.doc), "0 candidates · Stage 1 failed: Pass 1: bad accuracy")


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
@needs_planner
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

    def test_physical_lane_shows_timing_and_stage3_costs(self):
        """The selected plan's lane shows each node's timing and Stage 3 cost."""
        candidate = next(c for c in self.doc["stage2_physical_asap_optimization"]["candidates"] if c["id"] == selected_id(self.doc))
        per_node = self.doc["stage3_plan_selection"]["cost"]["per_node"]
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
        """A lane drawn without costs shows none."""
        candidate = self.doc["stage2_physical_asap_optimization"]["candidates"][0]
        for node in lane_nodes(self.lane(candidate["dag"], True)):
            self.assertNotIn("cost", node["data"]["label"])

    def test_summary_nodes_print_their_configuration(self):
        """Summary nodes print their sketch, grouping and instances."""
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
        """Node kinds map to their colour groups."""
        self.assertEqual([self.js.call("nodeGroup", k) for k in ("Scan", "SummaryAgg", "Filter", "Scalar")], ["source", "summary", "rel", "scalar"])


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
@needs_planner
class ScalarQueryTests(unittest.TestCase):
    """A standalone scalar query is no operator root: Stage 0 and Stage 1
    keep it as a `{"Scalar": expr}` root, and a physical DAG in
    `scalar_roots`, so query i is not `roots[i]`. Each lane shows the
    scalar expression as a node of its own, over the nodes it reads."""

    @classmethod
    def setUpClass(cls):
        cls.doc = planned(promql_workload(SCALAR_QUERIES, ["m", "n", "k"]))

    def setUp(self):
        self.js = stages_js()
        self.selected = next(c for c in self.doc["stage2_physical_asap_optimization"]["candidates"] if c["id"] == selected_id(self.doc))
        self.query_ids = [q["id"] for q in self.doc["workload"]["queries"]]

    def lanes(self):
        options = {"physical": False, "queryIds": self.query_ids}
        yield self.js.call("stageLaneElements", self.doc["stage0_query_language_frontends"]["dag"], options)
        yield self.js.call("stageLaneElements", self.selected["dag"], dict(options, physical=True))

    def roots(self, elements):
        return {n["data"]["rootFor"][0]: n for n in lane_nodes(elements) if n["data"]["rootFor"]}

    def test_document_is_valid(self):
        """The scalar-query document is valid, with scalar roots at 0, 2 and 4."""
        self.assertEqual(self.js.call("validateStagePipeline", self.doc), [])
        self.assertEqual([entry[0] for entry in self.selected["dag"]["scalar_roots"]], [0, 2, 4])

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
        Aggregate (Stage 0) or the sum's physical node."""
        for elements in self.lanes():
            root = self.roots(elements)[self.query_ids[0]]
            reads = [e["data"]["source"] for e in lane_edges(elements) if e["data"]["target"] == root["data"]["id"]]
            self.assertEqual(len(reads), 1)
            self.assertIn(reads[0], {n["data"]["id"] for n in lane_nodes(elements)})


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
@needs_planner
class SearchTests(unittest.TestCase):
    """asap_planner's search keeps only the selected candidate: the document
    is valid and the header names the search and its report."""

    def setUp(self):
        self.js = stages_js()
        # Two queries over two metrics, jointly planned.
        self.doc = planned(promql_workload(["sum(rate(a[1m]))", "topk(5, rate(b[1m]))"], ["a", "b"]))

    def test_document_is_valid_and_holds_the_selected_candidate(self):
        """The document is valid and holds only the selected plan, P1."""
        self.assertEqual(self.doc["search"], "candidate-connections")
        self.assertEqual(self.js.call("validateStagePipeline", self.doc), [])
        physical = self.doc["stage2_physical_asap_optimization"]["candidates"]
        self.assertEqual([p["id"] for p in physical], [selected_id(self.doc)])

    def test_guarantees_are_enforced_by_default_and_the_header_says_which(self):
        """The header says whether the run enforced accuracy guarantees
        (the default) or ignored them."""
        self.assertEqual(self.doc["accuracy_guarantees"], "enforced")
        self.assertEqual(self.js.call("accuracyGuaranteesText", self.doc), "accuracy guarantees enforced")
        ignored = planned(promql_workload(["sum(rate(a[1m]))", "topk(5, rate(b[1m]))"], ["a", "b"]), ignore_accuracy_guarantees=True)
        self.assertEqual(ignored["accuracy_guarantees"], "ignored")
        self.assertEqual(self.js.call("validateStagePipeline", ignored), [])
        self.assertTrue(self.js.call("accuracyGuaranteesText", ignored).startswith("accuracy guarantees ignored"))
        self.assertEqual(self.js.call("accuracyGuaranteesText", {}), "")

    def test_header_names_the_search_and_its_report(self):
        """The header names the candidate-connection search and its MILP
        report; an archived document's search still renders."""
        text = self.js.call("searchText", self.doc)
        self.assertTrue(text.startswith("candidate-connection search · "), text)
        report = self.doc["stage3_plan_selection"]["search_report"]
        self.assertIn(f"search {report['status'].replace('_', ' ')}", text)
        self.assertIn(f"MILP {report['milp_variables']} variables", text)
        self.assertNotIn("round", text)
        outputs = self.doc["stage3_plan_selection"]["selected_dag"]["node_of_output_reference"]
        self.assertEqual(len(outputs), 2)
        # Archived documents (before Stage 3's `Selection`) still render
        # their search's checked candidates and component list.
        archived = json.loads(json.dumps(self.doc))
        search = {"candidates_checked": 3, "candidate_connections": {"independent_components": 2}}
        archived["stage3_plan_selection"]["search"] = search
        self.assertIn("3 physical candidates checked", self.js.call("searchText", archived))
        self.assertIn("2 independent components searched separately",
                      self.js.call("searchText", archived))
        search["candidate_connections"]["independent_components"] = 1
        search["components"] = [{}, {}]
        self.assertNotIn("components searched separately",
                         self.js.call("searchText", archived))
        del search["candidate_connections"]
        self.assertIn("2 independent components searched separately",
                      self.js.call("searchText", archived))

@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
@needs_planner
class BinaryOperatorTests(unittest.TestCase):
    """A binary operator reads both operands (`lhs`, `rhs`): its node has an
    input edge from each in Stage 0, Stage 1 and Stage 2."""

    def test_binary_operator_reads_both_operands_in_every_lane(self):
        """A binary operator has an input edge from each operand in every lane."""
        doc = planned(promql_workload(["sum(rate(a[1m])) / sum(rate(b[1m]))"], ["a", "b"]))
        js = stages_js()
        self.assertEqual(js.call("validateStagePipeline", doc), [])
        selected = selected_id(doc)
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


@unittest.skipIf(py_mini_racer is None, "viewer tests require py_mini_racer")
@needs_planner
class WorkloadViewTests(unittest.TestCase):
    """The data workload panel, the materialized views table and the
    workload-set rows."""

    def setUp(self):
        self.js = stages_js()
        self.doc = tumbling_windows_doc()

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

    def test_materialized_view_rows_map_to_stage2_nodes_and_their_readers(self):
        """Each materialized view names its Stage 2 node and the queries
        reading it."""
        rows = self.js.call("materializedViewRows", self.doc)
        stored = self.doc["stage3_plan_selection"]["materialized_views"]
        self.assertEqual(len(rows), len(stored))
        first = rows[0]
        self.assertEqual(first["definition"], stored[0]["coverage"]["definition_id"])
        self.assertTrue(first["kind"].startswith("SummaryTumblingWindowChain · "), first["kind"])
        self.assertRegex(first["tumblingWindow"], r"^tumbling window 1 of \d+ \(newest first\), ")
        self.assertEqual(first["refresh"], "at ingestion")
        self.assertTrue(all(r["stage2NodeId"] is not None and r["readBy"] for r in rows))
        # q1 and q2 share one tumbling-window chain of exact sums.
        self.assertEqual(first["readBy"], ["q1", "q2"])

    def test_queries_show_recurrence_and_predictability(self):
        """A query shows its schedule, predictability and range."""
        self.assertEqual(self.js.call("queryScheduleText", self.doc["workload"]["queries"][0]), "every 10 s · predictable")
        query = {"recurrence": {"one_time": {"invocations": 1, "execute_at": None}}, "predictability": "ad_hoc",
                 "requirements": {"range_evaluation": {"step_ms": 60000, "span_ms": 3600000}}}
        self.assertEqual(self.js.call("queryScheduleText", query), "once · ad hoc · range of 1 h in 1 min steps")

    def test_tumbling_window_chain_box_shows_its_tumbling_windows(self):
        """A tumbling-window chain's box shows its tumbling-window count and width."""
        selected = self.doc["stage2_physical_asap_optimization"]["candidates"][0]
        chain = next(n for n in selected["dag"]["nodes"] if "SummaryTumblingWindowChain" in n["payload"].get("ASAP", {}))
        lines = self.js.call("stageNodeLines", chain["payload"], None)
        body = chain["payload"]["ASAP"]["SummaryTumblingWindowChain"]
        self.assertEqual(lines[0], "SummaryTumblingWindowChain")
        self.assertIn(f"{body['tumbling_window_count']} tumbling windows × ", lines[2])

    def test_usage_and_deployment_lines(self):
        """The usage and deployment lines read the plan's cost and capabilities."""
        cost = self.doc["stage3_plan_selection"]["cost"]
        self.assertIn("peak cores", self.js.call("usageText", cost["usage"]))
        self.assertTrue(self.js.call("deploymentText", self.doc["deployment_capabilities"]).startswith("raw data not retained · "))

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
class TumblingWindowsAndPathsTests(unittest.TestCase):
    """Display only: a tumbling-window chain expands into its tumbling windows on request, each
    node lists the queries whose root reaches it, and the query and node
    neighborhoods the page highlights. TUMBLING_WINDOWS_WORKLOAD's plan keeps two tumbling-window
    chains; q1 and q2 read metric m, q3 reads n."""

    def setUp(self):
        self.js = stages_js()
        self.doc = tumbling_windows_doc()
        self.selected = self.doc["stage2_physical_asap_optimization"]["candidates"][0]
        self.dag = self.selected["dag"]
        self.query_ids = [q["id"] for q in self.doc["workload"]["queries"]]
        self.chain = next(n for n in self.dag["nodes"] if "SummaryTumblingWindowChain" in n["payload"].get("ASAP", {}))
        self.body = self.chain["payload"]["ASAP"]["SummaryTumblingWindowChain"]

    def elements(self, **options):
        return self.js.call("stageLaneElements", self.dag, dict({"physical": True, "queryIds": self.query_ids}, **options))

    def node(self, elements, element_id):
        return next(e for e in lane_nodes(elements) if e["data"]["id"] == element_id)

    def test_chains_stay_collapsed_by_default_and_print_their_tumbling_windows(self):
        """A tumbling-window chain is drawn collapsed, with its tumbling windows in its label."""
        chain = self.node(self.elements(), f"n{self.chain['id']}")
        self.assertEqual(chain["data"]["kind"], "SummaryTumblingWindowChain")
        self.assertIn(f"{self.body['tumbling_window_count']} tumbling windows × 10 s", chain["data"]["label"])
        self.assertTrue(chain["data"]["expandable"])

    def test_an_expanded_chain_shows_each_tumbling_window_with_its_time_range_shift_and_window(self):
        """An expanded chain draws each tumbling window with its time range, shift and window."""
        count, chain_id = self.body["tumbling_window_count"], self.chain["id"]
        elements = self.elements(expandedChains=[chain_id])
        ids = {e["data"]["id"] for e in lane_nodes(elements)}
        self.assertNotIn(f"n{chain_id}", ids)
        tumbling_windows = [e for e in lane_nodes(elements) if e["data"].get("tumblingWindowOf") == chain_id]
        kinds = sorted(e["data"]["kind"] for e in tumbling_windows)
        # One build, one TimeRange per tumbling window; a TimeShift for every tumbling window but the newest.
        self.assertEqual(kinds.count("tumblingWindow"), count)
        self.assertEqual(kinds.count("TimeRange"), count)
        self.assertEqual(kinds.count("TimeShift"), count - 1)
        newest = self.node(elements, f"n{chain_id}p0")
        self.assertIn("tumbling window 1 of 6", newest["data"]["label"])
        self.assertIn("window t−10 s … t", newest["data"]["label"])
        oldest = self.node(elements, f"n{chain_id}p{count - 1}")
        self.assertIn("window t−1 min … t−50 s", oldest["data"]["label"])
        shift = self.node(elements, f"n{chain_id}p1s")
        self.assertIn("shift: 10 s", shift["data"]["label"])
        edges = [(e["data"]["source"], e["data"]["target"]) for e in lane_edges(elements)]
        child = f"n{self.body['child']}"
        # The chain's input feeds every tumbling window's TimeRange; every tumbling window feeds the chain's readers.
        self.assertIn((child, f"n{chain_id}p0r"), edges)
        self.assertIn((child, f"n{chain_id}p1s"), edges)
        self.assertIn((f"n{chain_id}p1s", f"n{chain_id}p1r"), edges)
        self.assertIn((f"n{chain_id}p1r", f"n{chain_id}p1"), edges)
        readers = {c for (p, c) in [(e["producer"], e["consumer"]) for e in self.dag["edges"]] if p == chain_id}
        for reader in readers:
            self.assertEqual(sum(1 for edge in edges if edge == (f"n{chain_id}p0", f"n{reader}")), 1)
            self.assertEqual(sum(1 for (p, c) in edges if c == f"n{reader}" and p.startswith(f"n{chain_id}p") and not p.endswith(("r", "s"))), count)

    def test_each_node_lists_the_queries_reaching_it_and_shared_nodes_say_so(self):
        """Each node lists the queries reaching it; shared nodes say so."""
        elements = self.elements()
        # q1 and q2 read the same 1 min window of one tumbling-window chain over m, so
        # they share its scan and merge.
        scans = [e for e in lane_nodes(elements) if e["data"]["kind"] == "Scan"]
        shared = next(e for e in scans if e["data"]["reachedBy"] == ["q1", "q2"])
        self.assertIn("shared ×2", shared["data"]["label"])
        merge = next(e for e in lane_nodes(elements) if e["data"]["kind"] == "SummaryMerge" and e["data"]["reachedBy"] == ["q1", "q2"])
        self.assertIn("shared ×2", merge["data"]["label"])
        root = self.node(elements, f"n{self.dag['roots'][2]}")
        self.assertEqual(root["data"]["reachedBy"], ["q3"])
        self.assertNotIn("shared", root["data"]["label"])

    def test_logical_lanes_list_the_queries_reaching_each_node_too(self):
        """Stage 0 nodes list the queries reaching them too."""
        stage0 = self.doc["stage0_query_language_frontends"]["dag"]
        elements = self.js.call("stageLaneElements", stage0, {"physical": False, "queryIds": self.query_ids})
        self.assertTrue(all(e["data"]["reachedBy"] for e in lane_nodes(elements)))

    def test_a_query_reaches_its_nodes_and_edges_only(self):
        """Highlighting a query keeps exactly its nodes and edges."""
        elements = self.elements()
        kept = set(self.js.call("stageQueryElementIds", elements, "q3"))
        nodes = {e["data"]["id"] for e in lane_nodes(elements) if e["data"]["id"] in kept}
        self.assertEqual(nodes, {e["data"]["id"] for e in lane_nodes(elements) if "q3" in e["data"]["reachedBy"]})
        for edge in lane_edges(elements):
            self.assertEqual(edge["data"]["id"] in kept, edge["data"]["source"] in nodes and edge["data"]["target"] in nodes)
        self.assertNotIn(f"n{self.chain['id']}", kept)

    def test_a_node_neighborhood_is_everything_upstream_and_downstream(self):
        """A node's neighborhood is everything upstream and downstream of it."""
        elements = self.elements()
        chain = f"n{self.chain['id']}"
        kept = set(self.js.call("stageNeighborhoodIds", elements, chain))
        self.assertIn(chain, kept)
        self.assertIn(f"n{self.body['child']}", kept)  # upstream: the scan
        for root in (self.dag["roots"][0], self.dag["roots"][1]):  # downstream: q1's and q2's roots
            self.assertIn(f"n{root}", kept)
        self.assertNotIn(f"n{self.dag['roots'][2]}", kept)  # q3 reads another metric

    def test_materialized_views_group_by_tumbling_window_chain(self):
        """Materialized views are grouped by their tumbling-window chain."""
        groups = self.js.call("materializedViewGroups", self.doc)
        self.assertEqual(len(groups), 2)
        self.assertEqual(sum(len(g["tumblingWindows"]) for g in groups), len(self.doc["stage3_plan_selection"]["materialized_views"]))
        first = groups[0]
        self.assertEqual(first["stage2NodeId"], self.chain["id"])
        self.assertEqual(first["tumblingWindowsText"], "6 × 10 s")
        self.assertEqual((first["refresh"], first["retention"]), ("at ingestion", "1 min"))
        self.assertEqual(first["readBy"], ["q1", "q2"])
        self.assertEqual(first["tumblingWindows"][0]["window"], "t−10 s … t")
        self.assertEqual(first["tumblingWindows"][1]["window"], "t−20 s … t−10 s")


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
        for script in ("app.js", "workload-sets.js", "enumeration.js", "memo.js"):
            used = set(re.findall(r"\$\('([^']+)'\)", (HERE / script).read_text()))
            with self.subTest(script):
                self.assertLessEqual(used, ids)

    def test_candidate_ids_do_not_overlap_their_descriptions(self):
        """The candidate list's id column fits its id: a fixed width let
        ids such as P18031-m15 run over the description beside them."""
        page = (HERE / "index.html").read_text()
        rule = re.search(r"\.rank button \{[^}]*grid-template-columns:\s*([^;]+);", page)
        self.assertIsNotNone(rule)
        self.assertRegex(rule.group(1), r"^(max-content|minmax\(\d+px,\s*max-content\))\s")
        self.assertIn("white-space: nowrap", re.search(r"\.pid \{([^}]*)\}", page).group(1))

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
var document = { getElementById: __el, documentElement: {}, body: { classList: { add() {}, remove() {} } }, addEventListener() {} };
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
        for name in ("node-style.js", "stages.js", "enumeration.js", "memo.js"):
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

    def test_doc_parameter_opens_one_document_with_the_selected_plan(self):
        """?doc= opens one document with its selected plan in both lanes."""
        js = self.page("?doc=examples/e1.json", {"examples/e1.json": json.dumps(example1())})
        selected = selected_id(example1())
        self.assertIn("Candidates per stage", self.text(js, "counts"))
        self.assertIn("✓ selected", self.text(js, "lane2-meta"))
        self.assertEqual(js.eval("__els.pick2.value"), selected)
        # Lane 1 shows the Stage 1 candidate the selected plan came from.
        from_logical = next(c["from_logical"] for c in example1()["stage2_physical_asap_optimization"]["candidates"] if c["id"] == selected)
        self.assertEqual(from_logical, "L1")
        self.assertEqual(js.eval("__els.pick1.value"), from_logical)
        combinations = example1()["stage1_logical_asap_optimization"]["combinations"]
        rank_meta = js.eval("__els['rank-meta'].textContent")
        self.assertIn(f"{combinations} candidates", rank_meta)
        self.assertIn("accuracy guarantees enforced", rank_meta)
        self.assertIn("metric not set; accepted exact", self.text(js, "lane2-meta"))
        self.assertGreater(js.eval("__nodes"), 0)

    def test_rank_list_has_composition_error_rows(self):
        """The candidate list shows composition errors."""
        doc = example1()
        doc["stage1_logical_asap_optimization"]["candidates"].append({"id": "L99", "display_name": "Q1 exact", "error": "cannot substitute"})
        js = self.page("?doc=d.json", {"d.json": json.dumps(doc)})
        rank = self.text(js, "rank")
        self.assertIn("composition error", rank)
        self.assertIn("cannot substitute", rank)

    def test_failed_selection_shows_stage0_and_the_error(self):
        """A Stage 1 failure still draws Stage 0 and shows the error."""
        doc = example1()
        doc["stage1_logical_asap_optimization"] = {"combinations": 0, "capped": False, "candidates": [], "error": "Pass 1: bad accuracy"}
        doc["stage2_physical_asap_optimization"] = {"candidates": [], "pruned": []}
        doc["stage3_plan_selection"] = {"error": "Pass 1: bad"}
        js = self.page("?doc=d.json", {"d.json": json.dumps(doc)})
        self.assertIn("Stage 1 failed: Pass 1: bad accuracy", js.eval("__els['rank-meta'].textContent"))
        self.assertIn("no candidates", js.eval("__els['lane1-meta'].textContent"))
        self.assertGreater(js.eval("__nodes"), 0)

    def test_examples_become_tabs_and_the_hash_picks_one(self):
        """examples.json entries become tabs; the hash picks one."""
        sample = json.dumps(example1())
        manifest = json.dumps([{"id": "a", "name": "A", "file": "out/a.json", "story": "story A"},
                               {"id": "b", "name": "B", "file": "out/b.json", "story": "story B"}])
        js = self.page("", {"examples.json": manifest, "out/a.json": sample, "out/b.json": sample}, "#b")
        self.assertIn('data-id="b"', js.eval("__els.tabs.innerHTML"))
        self.assertEqual(js.eval("__els.story.textContent"), "story B")

    def test_choosing_an_uncomposed_stage1_candidate_clears_stage2(self):
        """Choosing an uncomposed Stage 1 candidate clears the Stage 2 lane."""
        doc = example1()
        doc["stage1_logical_asap_optimization"]["candidates"].append({"id": "L99", "display_name": "Q1 exact", "error": "cannot substitute"})
        js = self.page("?doc=d.json", {"d.json": json.dumps(doc)})
        js.eval("__els.pick1.__on.change({ target: { value: 'L99' } })")
        self.assertEqual(js.eval("__els.pick2.value"), "")
        self.assertIn("composition error: cannot substitute", self.text(js, "lane1-meta"))

    def test_node_details_without_a_numeric_cost(self):
        """A per_node entry without a numeric `cost` does not break the details panel."""
        js = self.page("?doc=e1.json", {"e1.json": json.dumps(example1())})
        js.eval("""__taps['cy2 node']({ target: { data() { return {
            kind: 'Scan', nodeId: 0, rootFor: [], timing: 'query_time', cost: { detail: 'not priced' },
            node: { payload: { NonASAP: { Scan: {} } }, guarantee: null } }; } } })""")
        self.assertIn("not priced", self.text(js, "details"))

    def test_a_workload_document_fills_the_data_workload_and_materialized_views(self):
        """A planned workload fills the data workload, deployment and views panels."""
        js = self.page("?doc=s.json", {"s.json": json.dumps(tumbling_windows_doc())})
        self.assertIn("q1, q2", self.text(js, "dataSources"))
        self.assertIn("ingesting", js.eval("__els.dataArrival.textContent"))
        self.assertIn("raw data not retained", js.eval("__els.deployment.textContent"))
        stored = len(tumbling_windows_doc()["stage3_plan_selection"]["materialized_views"])
        self.assertIn(f"{stored} materialized views", self.text(js, "views-meta"))
        self.assertIn("every 10 s · predictable", self.text(js, "queries"))
        self.assertIn("Stage 3 usage: CPU", self.text(js, "lane2-meta"))

    def test_workload_sets_without_a_server_mark_what_needs_it(self):
        """Without the server every workload-set row says it needs the server."""
        index = json.loads((WORKLOADS / "index.json").read_text())
        files = {"workloads/index.json": json.dumps(index)}
        for workload_set in index["sets"]:
            for entry in workload_set["workloads"]:
                files[f"workloads/{entry['file']}"] = (WORKLOADS / entry["file"]).read_text()
        js = self.page("", files)
        js.eval((HERE / "workload-sets.js").read_text())
        time.sleep(0.1)
        table = js.eval("__els.setTable.innerHTML")
        self.assertEqual(table.count("<tr class=\"clickable\""), 10)
        self.assertEqual(table.count("needs the server"), 10)
        self.assertIn("q01-sum-fine", table)
        self.assertIn("no server", js.eval("__els.setsServer.textContent"))
        self.assertIn("Random mixed · seed-862 (10)", js.eval("__els.setPick.innerHTML"))

    def test_materialized_views_table_has_one_row_per_tumbling_window_chain(self):
        """The materialized views table has one row per tumbling-window chain."""
        js = self.page("?doc=s.json", {"s.json": json.dumps(tumbling_windows_doc())})
        self.assertEqual(js.eval("(__els.materializedViews.innerHTML.match(/data-node=/g) || []).length"), 2)
        self.assertIn("12 materialized views in 2 groups", self.text(js, "views-meta"))
        self.assertIn("6 × 10 s", self.text(js, "materializedViews"))

    def test_picking_a_query_dims_what_it_does_not_reach(self):
        """Picking a query dims what it does not reach; clearing it undims."""
        js = self.page("?doc=s.json", {"s.json": json.dumps(tumbling_windows_doc())})
        self.assertIn('value="q3"', js.eval("__els.queryPick.innerHTML"))
        js.eval("__els.queryPick.__on.change({ target: { value: 'q3' } })")
        dimmed = set(self.ev(js, "__dimmed.cy2"))
        rendered = self.ev(js, "__rendered.cy2")
        reached = {e["data"]["id"] for e in rendered if "source" not in e["data"] and "q3" in e["data"]["reachedBy"]}
        self.assertTrue(dimmed)
        self.assertFalse(dimmed & reached)
        self.assertTrue(js.eval("__dimmed.cy0.length") > 0)
        js.eval("__els.queryPick.__on.change({ target: { value: '' } })")
        self.assertEqual(js.eval("__dimmed.cy2.length"), 0)

    def test_tapping_a_tumbling_window_chain_expands_it_and_a_tumbling_window_collapses_it(self):
        """Tapping a chain expands it; tapping a tumbling window collapses it."""
        js = self.page("?doc=s.json", {"s.json": json.dumps(tumbling_windows_doc())})
        chain = next(e for e in self.ev(js, "__rendered.cy2") if e["data"].get("kind") == "SummaryTumblingWindowChain")
        js.eval(f"__taps['cy2 node']({{ target: __rendered.cy2.find((e) => e.data.id === {json.dumps(chain['data']['id'])}) && {{ data() {{ return __rendered.cy2.find((e) => e.data.id === {json.dumps(chain['data']['id'])}).data; }}, id() {{ return {json.dumps(chain['data']['id'])}; }} }} }})")
        ids = {e["data"]["id"] for e in self.ev(js, "__rendered.cy2")}
        self.assertNotIn(chain["data"]["id"], ids)
        tumbling_window = chain["data"]["id"] + "p0"
        self.assertIn(tumbling_window, ids)
        js.eval(f"__taps['cy2 node']({{ target: {{ data() {{ return __rendered.cy2.find((e) => e.data.id === {json.dumps(tumbling_window)}).data; }}, id() {{ return {json.dumps(tumbling_window)}; }} }} }})")
        self.assertIn(chain["data"]["id"], {e["data"]["id"] for e in self.ev(js, "__rendered.cy2")})

    def test_an_expanded_chain_stays_expanded_after_switching_plans_and_back(self):
        """A chain expanded in one plan is still expanded after showing another plan and coming back."""
        doc = tumbling_windows_doc()
        physical = doc["stage2_physical_asap_optimization"]
        copy = json.loads(json.dumps(physical["candidates"][0]))
        selected, copy["id"] = copy["id"], "P-copy"
        physical["candidates"].append(copy)
        # Only the reference pricer's table (`--exhaustive`) lists pruned Stage 2 candidates.
        doc["reference_pricing"] = {"pruned_candidates": [{"candidate": "P-copy", "cause": "costlier", "reason": "a copy of the selected plan"}]}
        js = self.page("?doc=s.json", {"s.json": json.dumps(doc)})
        chain = next(e for e in self.ev(js, "__rendered.cy2") if e["data"].get("kind") == "SummaryTumblingWindowChain")["data"]["id"]
        tap = lambda id_: js.eval(f"__taps['cy2 node']({{ target: {{ data() {{ return __rendered.cy2.find((e) => e.data.id === {json.dumps(id_)}).data; }}, id() {{ return {json.dumps(id_)}; }} }} }})")
        ids = lambda: {e["data"]["id"] for e in self.ev(js, "__rendered.cy2")}
        tap(chain)
        self.assertIn(chain + "p0", ids())
        js.eval("__els.pick2.__on.change({ target: { value: 'P-copy' } })")
        self.assertIn(chain, ids())
        js.eval(f"__els.pick2.__on.change({{ target: {{ value: {json.dumps(selected)} }} }})")
        self.assertIn(chain + "p0", ids())
        self.assertNotIn(chain, ids())

    def test_tapping_a_node_highlights_its_neighborhood_and_lists_its_queries(self):
        """Tapping a node highlights its neighborhood and lists its queries."""
        js = self.page("?doc=s.json", {"s.json": json.dumps(tumbling_windows_doc())})
        scan = next(e for e in self.ev(js, "__rendered.cy2") if e["data"].get("kind") == "Scan")
        js.eval(f"__taps['cy2 node']({{ target: {{ data() {{ return __rendered.cy2.find((e) => e.data.id === {json.dumps(scan['data']['id'])}).data; }}, id() {{ return {json.dumps(scan['data']['id'])}; }} }} }})")
        self.assertIn("queries reaching it", self.text(js, "details"))
        self.assertIn(", ".join(scan["data"]["reachedBy"]), self.text(js, "details"))
        self.assertTrue(js.eval("__dimmed.cy2.length") > 0)
        self.assertNotIn(scan["data"]["id"], self.ev(js, "__dimmed.cy2"))
        js.eval("__taps['cy2 background']({ target: { isNode: false } })")
        self.assertEqual(js.eval("__dimmed.cy2.length"), 0)

    def test_a_document_without_memo_hides_the_enumerated_dags(self):
        """A memo without an enumeration hides the Enumerated DAGs panel."""
        js = self.page("?doc=e1.json", {"e1.json": json.dumps(example1())})
        self.assertTrue(js.eval("__els.enumPanel.hidden"))
        self.assertEqual(js.eval("__els.enumRows.innerHTML"), "")

    def test_clicking_an_enumerated_dag_draws_it_in_the_stage1_lane(self):
        """The panel lists the enumerated choices; a click draws the
        choice's composed DAG in lane 1 and clears lane 2."""
        doc = {**example1(), **json.loads(MEMO_ENUMERATION.read_text())}
        js = self.page("?doc=d.json", {"d.json": json.dumps(doc)})
        self.assertFalse(js.eval("__els.enumPanel.hidden"))
        meta = js.eval("__els['enum-meta'].textContent")
        self.assertIn("Pass 1: 4", meta)
        self.assertIn("1 cheaper than the selected plan", meta)
        rows = js.eval("__els.enumRows.innerHTML")
        self.assertEqual(rows.count("<li>"), 4)
        self.assertIn('data-id="E3" aria-pressed="false" class="cheaper"', rows)
        e3 = doc["memo"]["enumeration"]["choices"][2]
        js.eval("__els.enumRows.__on.click({ target: { closest() { return { dataset: { id: 'E3' } }; } } })")
        drawn = [e for e in self.ev(js, "__rendered.cy1") if "source" not in e["data"]]
        self.assertEqual(len(drawn), len(self.memo_node_dag(js, doc, e3["roots"])["nodes"]))
        self.assertIn("enumerated DAG E3", self.text(js, "lane1-meta"))
        self.assertIn("no physical plan for an enumerated DAG", js.eval("__els['lane2-meta'].textContent"))
        self.assertIn('data-id="E3" aria-pressed="true"', js.eval("__els.enumRows.innerHTML"))
        # Choosing a Stage 1 candidate again releases the row.
        js.eval("__els.pick1.__on.change({ target: { value: 'L1' } })")
        self.assertNotIn('aria-pressed="true"', js.eval("__els.enumRows.innerHTML"))

    def memo_node_dag(self, js, doc, roots):
        """The page's `memoNodeDag` over the document's node table."""
        return json.loads(js.eval(f"JSON.stringify(memoNodeDag({json.dumps(doc['memo']['nodes'])}, {json.dumps(roots)}))"))

    def enumerated_roots(self, doc, choice_id):
        js = self.page("?doc=d.json", {"d.json": json.dumps(doc)})
        js.eval(f"__els.enumRows.__on.click({{ target: {{ closest() {{ return {{ dataset: {{ id: {json.dumps(choice_id)} }} }}; }} }} }})")
        roots = {e["data"]["nodeId"]: e["data"]["rootFor"] for e in self.ev(js, "__rendered.cy1") if e["data"].get("rootFor")}
        return roots, self.text(js, "lane1-meta")

    def test_enumerated_roots_are_named_by_their_root_groups_queries(self):
        """Roots come in increasing root-group id, not query order: with G3
        the root of q2 and G7 the root of q1, the first root is q2's."""
        doc = {**example1(), **json.loads(MEMO_ENUMERATION.read_text())}
        groups = {g["id"]: g for g in doc["memo"]["steps"][1]["groups"]}
        groups[3]["roots_of"], groups[7]["roots_of"] = [1], [0]
        roots, _ = self.enumerated_roots(doc, "E1")
        js = self.page("?doc=d.json", {"d.json": json.dumps(doc)})
        dag = self.memo_node_dag(js, doc, doc["memo"]["enumeration"]["choices"][0]["roots"])
        first, second = (r["Operator"] for r in dag["roots"])
        self.assertEqual(roots, {first: ["q2"], second: ["q1"]})

    def test_a_root_group_shared_by_two_queries_is_both_queries_root(self):
        """One root group answering q1 and q2: its root is the root of both,
        and highlighting q2 keeps it bright."""
        doc = {**example1(), **json.loads(MEMO_ENUMERATION.read_text())}
        groups = {g["id"]: g for g in doc["memo"]["steps"][1]["groups"]}
        groups[3]["roots_of"], groups[7]["roots_of"] = [0, 1], []
        choice = doc["memo"]["enumeration"]["choices"][0]
        choice["roots"] = choice["roots"][:1]
        js = self.page("?doc=d.json", {"d.json": json.dumps(doc)})
        root = self.memo_node_dag(js, doc, choice["roots"])["roots"][0]["Operator"]
        js.eval("__els.enumRows.__on.click({ target: { closest() { return { dataset: { id: 'E1' } }; } } })")
        roots = {e["data"]["nodeId"]: e["data"]["rootFor"] for e in self.ev(js, "__rendered.cy1") if e["data"].get("rootFor")}
        self.assertEqual(roots, {root: ["q1", "q2"]})
        js.eval("__els.queryPick.__on.change({ target: { value: 'q2' } })")
        self.assertNotIn(f"n{root}", self.ev(js, "__dimmed.cy1"))
        self.assertNotIn("not matched", self.text(js, "lane1-meta"))

    def test_enumerated_roots_without_the_steps_memo_name_no_query(self):
        """Without the steps' memo, enumerated roots name no query."""
        doc = {**example1(), **json.loads(MEMO_ENUMERATION.read_text())}
        doc["memo"]["steps"] = doc["memo"]["steps"][:1]
        # The selected choice names groups of the dropped steps, which the
        # Memo view's checks reject.
        doc["memo"]["selected_choice"] = None
        doc["memo"]["selected_choice_unavailable"] = "steps dropped by the test"
        roots, meta = self.enumerated_roots(doc, "E1")
        self.assertEqual(sorted(roots.values()), [["root group #1"], ["root group #2"]])
        self.assertIn("roots not matched to queries", meta)

    def test_a_broken_enumeration_refuses_the_document(self):
        """A malformed enumeration refuses the document."""
        doc = {**example1(), **json.loads(MEMO_ENUMERATION.read_text())}
        doc["memo"]["enumeration"]["limit"] = 0
        js = self.page("?doc=d.json", {"d.json": json.dumps(doc)})
        self.assertIn("limit must be a positive integer", js.eval("__els.queries.innerHTML"))

    def test_an_invalid_document_shows_an_error(self):
        """An invalid document shows an error."""
        js = self.page("?doc=bad.json", {"bad.json": json.dumps({"format": "nope"})})
        self.assertIn("Could not load", js.eval("__els.queries.innerHTML"))

    def memo_page(self):
        doc = example1()
        doc.update(json.loads(MEMO_SMALL.read_text()))
        return self.page("?doc=m.json", {"m.json": json.dumps(doc)})

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
        self.assertIn("✓ selected", self.text(js, "lane2-meta"))

    def test_the_memo_view_has_a_tab_per_step_and_opens_on_the_last(self):
        """The memo view has a tab per step and opens on the last."""
        js = self.memo_page()
        self.assertFalse(js.eval("__els.memoView.hidden"))
        tabs = js.eval("__els.memoTabs.innerHTML")
        self.assertEqual(re.findall(r">([^<]+)</button>", tabs), ["Stage 0", "Stage 1, Pass 1", "Stage 1, Pass 2"])
        self.assertIn('data-step="2" aria-pressed="true"', tabs)
        meta = self.text(js, "memoView-meta")
        self.assertIn("7 groups (+1) · 30 candidates (+20)", meta)
        self.assertIn("selected plan's candidates highlighted (7 groups)", meta)
        # G2 is collapsed to its origin counts; the rest list their candidates.
        self.assertEqual(self.memo_ids(js, "g2"), ["g2opass1", "g2otumbling_windows", "g2oshared_build"])
        js.eval("window.MemoView.selectStep(0)")
        self.assertEqual(len(self.memo_ids(js)), 6)
        self.assertIn("6 groups · 0 candidates", self.text(js, "memoView-meta"))
        self.assertIn("shown on the last tab", self.text(js, "memoView-meta"))

    def test_tapping_a_count_expands_its_group_and_the_group_collapses_it(self):
        """Tapping an origin count expands its group; tapping the group collapses it."""
        js = self.memo_page()
        self.tap_memo(js, "g2otumbling_windows")
        self.assertEqual(len(self.memo_ids(js, "g2")), 13)
        self.assertIn("Memo group G2", self.text(js, "details"))
        self.assertIn("20 (13 listed): 2 Pass1, 9 TumblingWindows, 9 SharedBuild", self.text(js, "details"))
        self.assertIn("selected plan#2 rate accumulator over G6's panes, variant 1 (TumblingWindows)", self.text(js, "details"))
        self.tap_memo(js, "g2")
        self.assertEqual(len(self.memo_ids(js, "g2")), 3)

    def test_tapping_a_candidate_lists_its_own_nodes(self):
        """Tapping a memo candidate lists its own nodes and the groups it reads."""
        js = self.memo_page()
        self.tap_memo(js, "g5c1")
        details = self.text(js, "details")
        self.assertIn("Candidate #1 of G5", details)
        self.assertIn("chosen for this group", details)
        self.assertIn("Project", details)
        self.assertIn("SummaryEstimate", details)
        memo = json.loads(MEMO_SMALL.read_text())["memo"]
        g5 = next(g for g in memo["steps"][-1]["groups"] if g["id"] == 5)
        boundary = next(c for c in g5["candidates"] if c["index"] == 1)["group_of_node"]
        self.assertEqual(list(boundary.values()), [1])
        self.assertIn(f"Reads G1 (node {next(iter(boundary))}).", details)
        self.assertNotIn("node 0</code>", js.eval("__els.details.innerHTML"))

    def test_highlighting_a_query_dims_the_memo_groups_it_does_not_reach(self):
        """Highlighting a query dims the memo groups it does not reach."""
        js = self.memo_page()
        js.eval("__els.queryPick.__on.change({ target: { value: 'q2' } })")
        dimmed = set(self.ev(js, "__dimmed.cyMemo"))
        self.assertIn("g2", dimmed)
        self.assertIn("g3c0", dimmed)
        self.assertNotIn("g5c1", dimmed)
        js.eval("__els.queryPick.__on.change({ target: { value: '' } })")
        self.assertEqual(js.eval("__dimmed.cyMemo.length"), 0)

    def test_a_broken_memo_is_refused(self):
        """A malformed memo refuses the document."""
        doc = example1()
        memo = json.loads(MEMO_SMALL.read_text())["memo"]
        memo["steps"][1]["groups"][0]["children"] = [42]
        doc["memo"] = memo
        js = self.page("?doc=m.json", {"m.json": json.dumps(doc)})
        self.assertIn("G0: children and parents must name groups", js.eval("__els.queries.innerHTML"))

    def test_the_real_memo_example(self):
        """On the memo asap_planner writes for #1005's ratio-of-quantiles
        query, each tab counts the groups and candidates of its step."""
        doc = planned(json.loads(RATIO_OF_QUANTILES.read_text()))
        js = self.page("?doc=r.json", {"r.json": json.dumps(doc)})
        steps = doc["memo"]["steps"]
        names = [step["step"] for step in steps]
        js.eval(f"window.MemoView.selectStep({names.index('stage0')})")
        groups = [e for e in self.ev(js, "__rendered.cyMemo") if e["data"].get("kind") == "group"]
        self.assertEqual(len(groups), len(steps[names.index("stage0")]["groups"]))
        pass1 = steps[names.index("pass1")]
        added = len(pass1["groups"]) - len(steps[names.index("stage0")]["groups"])
        candidates = sum(len(g["candidates"]) for g in pass1["groups"])
        js.eval(f"window.MemoView.selectStep({names.index('pass1')})")
        self.assertIn(f"{len(pass1['groups'])} groups (+{added}) · {candidates} candidates (+{candidates})",
                      self.text(js, "memoView-meta"))
        js.eval(f"window.MemoView.selectStep({len(steps) - 1})")
        self.assertIn(f"{len(steps[-1]['groups'])} groups", self.text(js, "memoView-meta"))


if __name__ == "__main__":
    unittest.main()
