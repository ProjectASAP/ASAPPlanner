#!/usr/bin/env python3
"""Run or summarize frozen-binary pane-search benchmarks without rebuilding.

Linux /usr/bin/time measures whole-process peak RSS. Search counters describe
reported domains and must not be confused with a complete-catalog optimum.
"""
import argparse
import concurrent.futures
import hashlib
import json
import math
from pathlib import Path
import re
import subprocess
import time

REPO = Path(__file__).resolve().parents[1]
VIEWER = REPO / "tools/dag-viewer"
FOUR = {
    "memo-rangeheavy-t8-t5-1h", "memo-rangeheavy-t5-q09-15m-1h",
    "random-mixed-seed-862-workload-01", "random-mixed-seed-862-long-workload-01",
}


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def entries(suite):
    examples = [e for e in json.loads((VIEWER / "examples.json").read_text()) if "workload" in e]
    if suite == "four":
        return [e for e in examples if e["id"] in FOUR]
    if suite == "viewer":
        return examples
    result = []
    for entry in examples:
        if entry["workload"].startswith("workloads/random-mixed-seed-862"):
            folder = (VIEWER / entry["workload"]).parent
            for path in sorted(folder.glob("workload-*.json")):
                result.append(dict(entry, id=folder.name + "-" + path.stem,
                                   workload=str(path.relative_to(VIEWER))))
    assert len(result) == 30
    return result


def inputs(entry, budget):
    path = VIEWER / entry["workload"]
    tables = path.parent / "tables.json"
    # Fingerprint semantics rather than absolute checkout/output paths.
    return {
        "workload": json.loads(path.read_text()),
        "tables": json.loads(tables.read_text()) if tables.exists() else [],
        "deployment_capabilities": entry["deployment_capabilities"],
        "cost_model": entry["cost_model"], "enforce_accuracy": True,
        "budget_seconds": budget,
    }


def domain_certificate_valid(report):
    pending = report.get("pane_widths_pending") is True
    if pending and (report.get("lower_bound") is not None or report.get("relative_gap") is not None
                    or any(report.get(key) is True for key in
                           ("full_catalog_completed", "catalog_exhausted", "evaluation_complete"))):
        return False
    incomplete = pending or ("pane_widths_pending" in report and not report.get("full_catalog_completed"))
    return not (incomplete and report.get("status") == "optimal")


def inspect(entry, output, budget, wall=None, exit_code=None):
    name = entry["id"]
    specification = inputs(entry, budget)
    row = {
        "workload": name, "wall_seconds": wall, "exit_code": exit_code,
        "input_sha256": hashlib.sha256(json.dumps(specification, sort_keys=True).encode()).hexdigest(),
        "artifact": name + ".json", "search": {}, "passed": False,
    }
    timing = output / (name + ".time")
    if timing.exists():
        text = timing.read_text()
        rss = re.search(r"Maximum resident set size \(kbytes\): (\d+)", text)
        status = re.search(r"Exit status: (\d+)", text)
        row["peak_planner_rss_kib"] = int(rss[1]) if rss else None
        if exit_code is None and status:
            row["exit_code"] = int(status[1])
    workload = specification["workload"]["query_workload"]
    row["expected_queries"] = len(workload.get("query_batch") or []) + len(workload.get("repeating_queries") or [])
    artifact = output / row["artifact"]
    if not artifact.exists():
        row["error"] = "planner wrote no artifact"
        return row
    try:
        document = json.loads(artifact.read_text())
    except (OSError, json.JSONDecodeError) as error:
        row["error"] = f"unreadable planner artifact: {error}"
        return row
    selection = document["stage3_plan_selection"]
    selected = selection.get("selected")
    row.update(selected=selected, error=selection.get("error"),
               stored_outputs_error=selection.get("stored_outputs_error"),
               artifact_sha256=digest(artifact),
               search=selection.get("search", {}).get("candidate_connections", {}))
    accepted = selection.get("accepted", {}).get(selected, [])
    row["reported_catalog_metrics_available"] = row["search"].get("physical_configurations", 0) > 0
    row["accepted_queries"] = len(accepted)
    row["guarantees_enforced"] = bool(accepted) and all(
        not q.get("accepted_regardless_of_guarantee", True)
        and q.get("guarantee_check_rejection") is None for q in accepted)
    cost = selection.get("costs", {}).get(selected, {})
    value = cost.get("cost_per_second")
    row["modeled_cost_per_second"] = value
    row["modeled_usage"] = cost.get("usage")
    bound = row["search"].get("lower_bound")
    row["cost_certificate_valid"] = bool(
        isinstance(value, (int, float)) and math.isfinite(value) and value >= 0
        and row["search"].get("status") != "invalid_cost_certificate"
        and (bound is None or (math.isfinite(bound) and bound <= value + 1e-8 * max(abs(value), 1e-12))))
    row["domain_certificate_valid"] = domain_certificate_valid(row["search"])
    row["passed"] = bool(row["exit_code"] == 0 and selected and row["guarantees_enforced"]
                         and len(accepted) == row["expected_queries"] and row["cost_certificate_valid"]
                         and row["domain_certificate_valid"] and not row["error"]
                         and not row["stored_outputs_error"])
    return row


def run_one(entry, args):
    output, binary = args.output.resolve(), args.binary.resolve()
    name = entry["id"]
    artifact = output / (name + ".json")
    if artifact.exists():
        raise FileExistsError(f"Use a new output directory; refusing to overwrite {artifact}")
    command = ["/usr/bin/time", "-v", "-o", str(output / (name + ".time")),
               "timeout", f"{args.timeout}s", str(binary),
               "--workload", str(VIEWER / entry["workload"]),
               "--search", "candidate-connections", "--milp-time-limit-s", str(args.budget),
               "--cost-model", entry["cost_model"], "--enforce-accuracy-guarantees",
               "--deployment-capabilities", json.dumps(entry["deployment_capabilities"]),
               "--out", str(artifact)]
    for table in inputs(entry, args.budget)["tables"]:
        command += ["--table", json.dumps(table)]
    started = time.monotonic()
    with (output / (name + ".log")).open("w") as log:
        code = subprocess.run(command, cwd=REPO, stdout=log, stderr=subprocess.STDOUT).returncode
    row = inspect(entry, output, args.budget, time.monotonic() - started, code)
    row["command"] = command
    print(json.dumps({k: row.get(k) for k in ("workload", "passed", "wall_seconds", "modeled_cost_per_second", "error")}), flush=True)
    return row


def collect(args):
    args.output.mkdir(parents=True, exist_ok=True)
    metadata = {"binary_sha256": digest(args.binary), "suite": args.suite,
                "budget_seconds": args.budget, "parallel_workloads": args.jobs,
                "external_timeout_seconds": args.timeout, "results": []}
    cases = entries(args.suite)
    if args.command == "summarize":
        # Import the original baseline runner's wall times without rerunning it.
        old = args.output / "summary.json"
        historical = {r["workload"]: r for r in json.loads(old.read_text())} if old.exists() else {}
        rows = [inspect(e, args.output, args.budget,
                        historical.get(e["id"], {}).get("wall_seconds"),
                        historical.get(e["id"], {}).get("exit_code")) for e in cases]
        metadata["results"] = rows
    else:
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
            for row in pool.map(lambda e: run_one(e, args), cases):
                metadata["results"].append(row)
                (args.output / "results.json").write_text(json.dumps(metadata, indent=2) + "\n")
    (args.output / "results.json").write_text(json.dumps(metadata, indent=2) + "\n")
    return 0 if all(r["passed"] for r in metadata["results"]) else 1


def compare(args):
    before, after = (json.loads((p / "results.json").read_text()) for p in (args.before, args.after))
    if not args.quality_only and (before["budget_seconds"], before["parallel_workloads"]) != (after["budget_seconds"], after["parallel_workloads"]):
        raise ValueError("Comparison requires identical planning budget and concurrency")
    old = {r["workload"]: r for r in before["results"]}
    if set(old) != {r["workload"] for r in after["results"]}:
        raise ValueError("Comparison requires the same completed workload set")
    lines = ["# Pane-search benchmark comparison", "",
             f"Before binary: `{before['binary_sha256']}`. After binary: `{after['binary_sha256']}`.", "",
             "Single-run measurements; execution cost and accuracy are planner-model results, not measured stream execution.", "",
             "| Workload | Metric | Before | After | Change |", "|---|---|---:|---:|---:|"]
    metrics = ["wall_seconds", "peak_planner_rss_kib", "modeled_cost_per_second", "accepted_queries",
               "passed", "guarantees_enforced", "cost_certificate_valid", "domain_certificate_valid"]
    counters = ["logical_candidates", "physical_configurations", "compatible_connections", "supplier_sets", "stored_supplier_ids",
                "peak_milp_variables", "peak_milp_constraints", "local_candidate_generation_ms", "memo_expansion_ms",
                "inventory_construction_ms", "model_construction_ms", "solver_ms", "validation_ms", "solve_calls"]
    if args.quality_only:
        metrics = ["modeled_cost_per_second", "accepted_queries", "expected_queries", "guarantees_enforced", "passed"]
        counters = []
        lines.insert(4, "Quality-only comparison: planning times, RSS, and model sizes are deliberately omitted. Legacy inputs are checked against serialized workload, deployment, accuracy mode, and Stage 0 DAG; original pricing settings must additionally be confirmed from the baseline runner.")
    for row in after["results"]:
        previous = old[row["workload"]]
        if "input_sha256" in previous and "input_sha256" in row:
            identical_inputs = previous["input_sha256"] == row["input_sha256"]
        elif args.quality_only:
            left = json.loads((args.before / previous["artifact"]).read_text())
            right = json.loads((args.after / row["artifact"]).read_text())
            identical_inputs = all(left.get(key) == right.get(key) for key in
                                   ("workload", "deployment_capabilities", "accuracy_guarantees", "stage0_query_language_frontends"))
            missing_model_metadata = []
            for key in ("objective", "calibration_scope", "calibration_factor"):
                left_value, right_value = previous["search"].get(key), row["search"].get(key)
                # Default preparation-time reports can omit these labels.
                # Zero is not a valid calibration factor; treat it as missing,
                # not as evidence that the selected plan was priced at zero.
                if not left_value or not right_value:
                    missing_model_metadata.append(key)
                else:
                    identical_inputs &= left_value == right_value
            if missing_model_metadata:
                lines.append(f"| {row['workload']} | Unreported model metadata (confirm runner settings) | — | {', '.join(missing_model_metadata)} | — |")
        else:
            raise ValueError("Legacy results without input fingerprints require --quality-only")
        if not identical_inputs:
            raise ValueError("Inputs differ: " + row["workload"])
        for key in metrics + counters:
            a, b = (previous.get(key), row.get(key)) if key in metrics else (previous["search"].get(key), row["search"].get(key))
            # A final preparation timeout can carry default zero catalog
            # counters despite earlier solved rounds. Do not call that 100%
            # compression; preserve raw reports in results.json for diagnosis.
            catalog_counters = {"logical_candidates", "physical_configurations", "compatible_connections", "supplier_sets", "stored_supplier_ids"}
            if key in catalog_counters:
                if previous["search"].get("physical_configurations", 0) == 0:
                    a = None
                if row["search"].get("physical_configurations", 0) == 0:
                    b = None
            change = f"{(b / a - 1) * 100:+.2f}%" if type(a) in (int, float) and a != 0 and type(b) in (int, float) else "—"
            lines.append(f"| {row['workload']} | {key} | {a if a is not None else 'unreported'} | {b if b is not None else 'unreported'} | {change} |")
        domain_keys = ("status", "pane_width_rounds", "inventory_pane_width_domain", "pane_widths_pending", "full_catalog_completed", "catalog_exhausted", "evaluation_complete")
        for key in (() if args.quality_only else domain_keys):
            lines.append(f"| {row['workload']} | {key} | {previous['search'].get(key, 'unreported')} | {row['search'].get(key, 'unreported')} | — |")
    if not args.quality_only:
        lines += ["", "Supplier IDs measure storage sharing; compatible connections and MILP variables measure different things. Missing baseline counters are unreported, not zero.",
                  "Candidate/edge counters describe reported catalogs; peaks and times can span multiple rounds. A smaller restricted domain is not evidence of lossless full-domain compression."]
    args.output.write_text("\n".join(lines) + "\n")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    for command in ("run", "summarize"):
        p = sub.add_parser(command)
        p.add_argument("binary", type=Path)
        p.add_argument("output", type=Path)
        p.add_argument("--suite", choices=("four", "viewer", "asap"), default="four")
        p.add_argument("--budget", type=int, default=30)
        p.add_argument("--timeout", type=int, default=90)
        p.add_argument("--jobs", type=int, default=1)
    p = sub.add_parser("compare")
    p.add_argument("before", type=Path)
    p.add_argument("after", type=Path)
    p.add_argument("output", type=Path)
    p.add_argument("--quality-only", action="store_true", help="compare costs/acceptance only; permits legacy artifact input checks")
    args = parser.parse_args()
    if args.command != "compare" and (args.jobs < 1 or args.budget < 0 or args.timeout < 1):
        parser.error("jobs and timeout must be positive; budget must be nonnegative")
    return compare(args) if args.command == "compare" else collect(args)


if __name__ == "__main__":
    raise SystemExit(main())
