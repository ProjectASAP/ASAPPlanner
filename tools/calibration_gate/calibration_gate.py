#!/usr/bin/env python3
"""Calibration gate (test C10 of
docs/design_docs/proposals/stage3-lowest-mean-latency.md, section 2.4):
check a deployment's calibration file against its own measurements.

For each calibration case (a workload, the plan that ran for it, and what
the deployment measured running it), the planner prices that plan with the
calibration file (`asap_planner --price-plan PLAN --report-file`). The gate
passes when every refresh group's CPU per window close and every query's
time per request is within the acceptance ratio (default 1.5x) of the
measured value.

A refresh group's prediction is the ingestion-time work of its views and
the ingestion-time nodes they read, per window close: each node's ingest
CPU over the window plus its refresh CPU (and wall: its refresh wall time).
A query's prediction is its query execution per request.

Measured file (one per case), JSON:

    {
      "refresh_groups": [
        {"views": [3, 4], "window_ms": 10000,
         "cpu_ms_per_window": 12.5, "wall_ms_per_window": 20.1}
      ],
      "queries": [
        {"query": 0, "cpu_ms_per_request": 31.0, "wall_ms_per_request": 33.5}
      ]
    }

`views` are node ids of the plan's views in the group; `query` is the
query's position in the workload. A missing `wall_ms_per_window` is not
checked.

Usage:

    calibration_gate.py --planner target/debug/asap_planner \\
        --cost-inputs calibration.json [--ratio 1.5] \\
        --case WORKLOAD.json PLAN.json MEASURED.json [--case ...]

Exit status 0 when every case passes, 1 otherwise; one line per row.
"""

import argparse
import json
import subprocess
import sys
import tempfile
from pathlib import Path


def priced(planner, workload, plan, cost_inputs):
    """The planner's report of `plan` for `workload` as JSON."""
    with tempfile.TemporaryDirectory() as tmp:
        report = Path(tmp) / "report.json"
        subprocess.run([planner, "--workload", workload, "--run-until", "stage3",
                        "--cost-inputs", cost_inputs, "--price-plan", plan,
                        "--report-file", str(report)], check=True)
        return json.loads(report.read_text())


def ingestion_nodes_read(plan, views):
    """`views` and the ingestion-time nodes they read, transitively."""
    dag = plan["dag"]
    timing = {node["id"]: node["output_state"]["timing"] for node in dag["nodes"]}
    producers = {}
    for edge in dag["edges"]:
        producers.setdefault(edge["consumer"], []).append(edge["producer"])
    seen, stack = set(), list(views)
    while stack:
        node = stack.pop()
        if node in seen or timing.get(node) != "ingestion_time":
            continue
        seen.add(node)
        stack.extend(producers.get(node, []))
    return seen


def within(measured, predicted_value, ratio):
    if predicted_value <= 0:
        return measured <= 0
    return 1 / ratio <= measured / predicted_value <= ratio


def check(report, plan, measured, ratio, label):
    """One line per row; True when every row is within `ratio`."""
    per_node = {node["node_id"]: node for node in report["predicted_usage"]["per_node"]}
    per_query = report["predicted_usage"]["per_query"]
    ok = True

    def row(name, measured_value, predicted_value):
        nonlocal ok
        passed = within(measured_value, predicted_value, ratio)
        ok &= passed
        ratio_text = f"{measured_value / predicted_value:.2f}" if predicted_value else "nan"
        print(f"{label} {name}: measured {measured_value:.3f} predicted {predicted_value:.3f} "
              f"ratio {ratio_text} {'ok' if passed else 'FAIL'}")

    for group in measured.get("refresh_groups", []):
        nodes = [per_node[n] for n in ingestion_nodes_read(plan, group["views"]) if n in per_node]
        window_s = group["window_ms"] / 1000.0
        ingest = sum(n["ingest_cpu_core_seconds_per_second"] for n in nodes) * window_s * 1000
        cpu = ingest + sum(n["refresh_cpu_ms_per_window"] for n in nodes)
        wall = ingest + sum(n["refresh_wall_ms_per_window"] for n in nodes)
        row(f"group {group['views']} cpu ms/window", group["cpu_ms_per_window"], cpu)
        if "wall_ms_per_window" in group:
            row(f"group {group['views']} wall ms/window", group["wall_ms_per_window"], wall)
    for query in measured.get("queries", []):
        # A query's execution is its wall time per request: a node it
        # shares with another query is computed once, by the first reader,
        # and waited for by the others, so its CPU is the first reader's
        # but its time is on every reader's path. CPU when no wall time is
        # given.
        measured_ms = query.get("wall_ms_per_request", query["cpu_ms_per_request"])
        row(f"query {query['query']} ms/request", measured_ms,
            per_query[query["query"]]["query_execution_ms"])
    return ok


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--planner", required=True)
    parser.add_argument("--cost-inputs", required=True)
    parser.add_argument("--ratio", type=float, default=1.5)
    parser.add_argument("--case", nargs=3, action="append", required=True,
                        metavar=("WORKLOAD", "PLAN", "MEASURED"))
    args = parser.parse_args()
    ok = True
    for workload, plan, measured in args.case:
        report = priced(args.planner, workload, plan, args.cost_inputs)
        ok &= check(report, json.loads(Path(plan).read_text()),
                    json.loads(Path(measured).read_text()), args.ratio, Path(measured).stem)
    print("PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
