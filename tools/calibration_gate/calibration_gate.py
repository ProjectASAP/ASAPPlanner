#!/usr/bin/env python3
"""Calibration gate (test C10 of
docs/design_docs/proposals/stage3-lowest-mean-latency.md, section 2.4):
check a deployment's calibration file against its own measurements.

For each calibration workload, the planner plans it with the calibration
file and writes its report (`asap_planner --report-file`); the deployment
measured the same plan running. The gate passes when every refresh group's
CPU and wall time per window, and every query's time per request, is within
the acceptance ratio (default 1.5x) of the measured value.

Measured file (one per workload), JSON:

    {
      "refresh_groups": [
        {"views": [3, 4], "cpu_ms_per_window": 12.5, "wall_ms_per_window": 20.1}
      ],
      "queries": [
        {"query": 0, "cpu_ms_per_request": 31.0}
      ]
    }

`views` are the plan's node ids of the group's views (the installed plan's
`ingestion_refresh_groups`); `query` is the query's position in the
workload. A missing `wall_ms_per_window` is not checked.

Usage:

    calibration_gate.py --planner target/debug/asap_planner \\
        --cost-inputs calibration.json [--deployment-capabilities caps.json] \\
        [--ratio 1.5] --case WORKLOAD.json MEASURED.json [--case ...]

Exit status 0 when every case passes, 1 otherwise; one line per checked
row.
"""

import argparse
import json
import subprocess
import sys
import tempfile
from pathlib import Path


def predicted(planner, workload, cost_inputs, capabilities):
    """The planner's report for `workload` as JSON."""
    with tempfile.TemporaryDirectory() as tmp:
        report = Path(tmp) / "report.json"
        command = [
            planner,
            "--workload", workload,
            "--run-until", "stage3",
            "--cost-inputs", cost_inputs,
            "--out", str(Path(tmp) / "stages.txt"),
            "--report-file", str(report),
        ]
        if capabilities:
            command += ["--deployment-capabilities", capabilities]
        subprocess.run(command, check=True)
        return json.loads(report.read_text())


def within(measured, predicted_value, ratio):
    if predicted_value <= 0:
        return measured <= 0
    return 1 / ratio <= measured / predicted_value <= ratio


def check(report, measured, ratio, label):
    """One line per row; True when every row is within `ratio`."""
    per_node = {node["node_id"]: node for node in report["predicted_usage"]["per_node"]}
    per_query = report["predicted_usage"]["per_query"]
    ok = True
    for group in measured.get("refresh_groups", []):
        nodes = [per_node[view] for view in group["views"]]
        for field, key in [("refresh_cpu_ms_per_window", "cpu_ms_per_window"),
                           ("refresh_wall_ms_per_window", "wall_ms_per_window")]:
            if key not in group:
                continue
            value = sum(node[field] for node in nodes)
            passed = within(group[key], value, ratio)
            ok &= passed
            print(f"{label} group {group['views']} {key}: measured {group[key]:.3f} "
                  f"predicted {value:.3f} ratio {group[key] / value if value else float('nan'):.2f} "
                  f"{'ok' if passed else 'FAIL'}")
    for query in measured.get("queries", []):
        value = per_query[query["query"]]["query_execution_ms"]
        passed = within(query["cpu_ms_per_request"], value, ratio)
        ok &= passed
        print(f"{label} query {query['query']} ms per request: measured "
              f"{query['cpu_ms_per_request']:.3f} predicted {value:.3f} "
              f"{'ok' if passed else 'FAIL'}")
    return ok


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--planner", required=True)
    parser.add_argument("--cost-inputs", required=True)
    parser.add_argument("--deployment-capabilities")
    parser.add_argument("--ratio", type=float, default=1.5)
    parser.add_argument("--case", nargs=2, action="append", required=True,
                        metavar=("WORKLOAD", "MEASURED"))
    args = parser.parse_args()
    ok = True
    for workload, measured in args.case:
        report = predicted(args.planner, workload, args.cost_inputs,
                           args.deployment_capabilities)
        ok &= check(report, json.loads(Path(measured).read_text()), args.ratio,
                    Path(workload).stem)
    print("PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
