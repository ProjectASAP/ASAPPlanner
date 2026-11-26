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

Resource rows (issue #1472), from optional sections of the measured file:

    {
      "elapsed_ms": 1200000,
      "views": [
        {"node_id": 3, "bytes_per_window": 440000, "disk_bytes": 501600000,
         "disk_written_bytes_per_second": 440000.0,
         "disk_read_bytes_per_second": 5000000.0, "memory_bytes": 31000000.0}
      ],
      "plan": {"memory_bytes": 32000000.0, "peak_memory_bytes": 40000000,
               "disk_bytes": 501600000, "disk_written_bytes_per_second": 440000.0,
               "disk_read_bytes_per_second": 5000000.0,
               "ingest_cpu_core_seconds_per_second": 0.1,
               "query_cpu_core_seconds_per_second": 0.5}
    }

`elapsed_ms` is how long the data plane ran before measuring. A missing
field or section is not checked. Bytes per window, disk writes and disk at
the peak must be within the bytes ratio (default 1.25x); reads, memory and
CPU within the acceptance ratio. Disk at the peak is checked only once the
run is past each view's retention plus the window cache's retention margin
and file span (from the cost inputs); before, it is printed as not at
steady state (without `elapsed_ms`, it is not checked). A view row whose
predicted and measured values are both below 1% of the plan's totals for
that resource is printed but not gated; bytes per window and the plan rows
are always gated. The measured `memory_bytes` is compared with the
predicted average memory. Bytes per window are compared with the
report's `stored_bytes_per_window` (a window's bytes in its files) when it
has one, else with `bytes_per_window`.

Usage:

    calibration_gate.py --planner target/debug/asap_planner \\
        --cost-inputs calibration.json [--ratio 1.5] [--bytes-ratio 1.25] \\
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


# A view row whose predicted and measured values are both below this share
# of the plan's totals is printed, not gated.
MIN_GATED_SHARE = 0.01

# The acceptance ratio of bytes per window, disk writes and disk at the
# peak, which follow from the plan's rows and bytes alone.
BYTES_RATIO = 1.25


def check(report, plan, measured, ratio, label, cost_inputs=None, bytes_ratio=BYTES_RATIO):
    """One line per row; True when every gated row is within its ratio:
    `bytes_ratio` for bytes per window, disk writes and disk at the peak,
    `ratio` for the others."""
    usage = report["predicted_usage"]
    per_node = {node["node_id"]: node for node in usage["per_node"]}
    per_query = usage["per_query"]
    ok = True

    def row(name, measured_value, predicted_value, row_ratio=ratio, note=None):
        """Print one row; gate it unless `note` says why not."""
        nonlocal ok
        passed = within(measured_value, predicted_value, row_ratio)
        if note is None:
            ok &= passed
        ratio_text = f"{measured_value / predicted_value:.2f}" if predicted_value else "nan"
        verdict = note or ("ok" if passed else "FAIL")
        print(f"{label} {name}: measured {measured_value:.3f} predicted {predicted_value:.3f} "
              f"ratio {ratio_text} {verdict}")

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
    resource_rows(row, usage, per_node, plan, measured, cost_inputs or {},
                  {"bytes": bytes_ratio, "acceptance": ratio})
    return ok


# (row name, measured field, predicted field, which ratio: "bytes" or
# "acceptance"); a view row of bytes per window is a size, gated whatever its
# share of the plan.
VIEW_ROWS = [
    ("bytes/window", "bytes_per_window", "bytes_per_window", "bytes"),
    ("disk written B/s", "disk_written_bytes_per_second", "disk_written_bytes_per_second", "bytes"),
    ("disk peak B", "disk_bytes", "disk_bytes", "bytes"),
    ("disk read B/s", "disk_read_bytes_per_second", "disk_read_bytes_per_second", "acceptance"),
    ("memory B", "memory_bytes", "average_memory_bytes", "acceptance"),
]


def resource_rows(row, usage, per_node, plan, measured, cost_inputs, ratios):
    """The `views` and `plan` rows of the measured file (module doc);
    `ratios` maps "bytes" and "acceptance" to their acceptance ratios."""
    cache = cost_inputs.get("data_plane_execution", {}).get("view_window_cache") or {}
    disk_slack_ms = cache.get("retention_margin_ms", 0) + cache.get("file_span_ms", 0)
    retention_ms_by_node = {node["id"]: node.get("retention_ms") or 0
                            for node in plan["dag"]["nodes"]}
    elapsed_ms = measured.get("elapsed_ms")

    def disk_not_checked(node_ids):
        """Why disk is not checked yet for views `node_ids`, or None."""
        if elapsed_ms is None:
            return "no elapsed_ms, not checked"
        if any(elapsed_ms < retention_ms_by_node.get(node_id, 0) + disk_slack_ms
               for node_id in node_ids):
            return "not at steady state"
        return None

    predicted_totals = {field: sum(node.get(field, 0) for node in per_node.values())
                        for _, _, field, _ in VIEW_ROWS}
    measured_totals = {field: sum(view.get(field, 0) for view in measured.get("views", []))
                       for _, field, _, _ in VIEW_ROWS}
    measured_plan = measured.get("plan", {})
    for _, field, _, _ in VIEW_ROWS:
        measured_totals[field] = max(measured_totals[field], measured_plan.get(field, 0))
    for view in measured.get("views", []):
        node_id = view["node_id"]
        node = per_node.get(node_id)
        if node is None:
            print(f"view {node_id}: not an operator node of the report, not checked")
            continue
        for name, measured_field, predicted_field, which in VIEW_ROWS:
            if measured_field not in view:
                continue
            predicted_value = node.get(predicted_field, 0)
            if predicted_field == "bytes_per_window":
                # A view's measured bytes per window are what its files
                # hold: the stored size, when the report gives one.
                predicted_value = node.get("stored_bytes_per_window", predicted_value)
            note = None
            if predicted_field == "disk_bytes":
                note = disk_not_checked([node_id])
            if note is None and predicted_field != "bytes_per_window" and (
                    predicted_value < MIN_GATED_SHARE * predicted_totals[predicted_field]
                    and view[measured_field] < MIN_GATED_SHARE * measured_totals[measured_field]):
                note = "below 1% of the plan, not gated"
            row(f"view {node_id} {name}", view[measured_field], predicted_value,
                ratios[which], note)
    recommended_configuration = usage.get("recommended_configuration", {})

    def role_cpu(role):
        # Busy thread-seconds per second of a role is its CPU core-seconds
        # per second: one busy thread occupies one core.
        return recommended_configuration.get(role, {}).get(
            "predicted_busy_thread_seconds_per_second", 0)

    plan_rows = [
        ("memory B", "memory_bytes", usage.get("average_memory_bytes", 0), "acceptance"),
        ("peak memory B", "peak_memory_bytes", usage.get("peak_memory_bytes", 0), "acceptance"),
        ("disk peak B", "disk_bytes", usage.get("disk_bytes", 0), "bytes"),
        ("disk written B/s", "disk_written_bytes_per_second",
         usage.get("disk_written_bytes_per_second", 0), "bytes"),
        ("disk read B/s", "disk_read_bytes_per_second",
         usage.get("disk_read_bytes_per_second", 0), "acceptance"),
        ("ingest cpu", "ingest_cpu_core_seconds_per_second", role_cpu("ingest"), "acceptance"),
        ("query cpu", "query_cpu_core_seconds_per_second", role_cpu("query"), "acceptance"),
    ]
    views_with_disk = [node_id for node_id, node in per_node.items()
                       if node.get("disk_bytes", 0) > 0]
    for name, field, predicted_value, which in plan_rows:
        if field not in measured_plan:
            continue
        note = disk_not_checked(views_with_disk) if field == "disk_bytes" else None
        row(f"plan {name}", measured_plan[field], predicted_value, ratios[which], note)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--planner", required=True)
    parser.add_argument("--cost-inputs", required=True)
    parser.add_argument("--ratio", type=float, default=1.5)
    parser.add_argument("--bytes-ratio", type=float, default=BYTES_RATIO)
    parser.add_argument("--case", nargs=3, action="append", required=True,
                        metavar=("WORKLOAD", "PLAN", "MEASURED"))
    args = parser.parse_args()
    cost_inputs = json.loads(Path(args.cost_inputs).read_text())
    ok = True
    for workload, plan, measured in args.case:
        report = priced(args.planner, workload, plan, args.cost_inputs)
        ok &= check(report, json.loads(Path(plan).read_text()),
                    json.loads(Path(measured).read_text()), args.ratio, Path(measured).stem,
                    cost_inputs=cost_inputs, bytes_ratio=args.bytes_ratio)
    print("PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
