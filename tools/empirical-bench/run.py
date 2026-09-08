#!/usr/bin/env python3
"""Run the real sketch-bench CLI and export offline planner evidence (stdlib only)."""
import argparse
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shlex
import statistics
import subprocess
import tempfile


BUILD_PROVENANCE = {
    "runtime_field_status": "declared build preconditions, not introspected from the executable",
    "compiler_status": "rustc --version observed on driver host; executable compiler not independently verified",
    "required_build": "run cargo build --release --locked -p aqpbm-cli from the pinned sketch-bench directory with default features",
    "declared_settings": "release; repository target-cpu=native; default jemalloc",
    "verified_execution_setting": "driver sets POLARS_MAX_THREADS=1",
    "binary_identity": "SHA-256 recorded; digest identifies executable but does not verify build settings",
}


def command(argv, **kwargs):
    return subprocess.check_output(argv, text=True, **kwargs).strip()


def measurement(values):
    if not values or any(not math.isfinite(x) or x < 0 for x in values):
        raise ValueError("invalid or missing measurement")
    return {"value": statistics.mean(values),
            "stddev": statistics.stdev(values) if len(values) > 1 else None,
            "samples": len(values)}


def cpu_per_op(row, op):
    """Recover each run's work from paired rate and elapsed samples, as upstream does."""
    rate = row.get(op + ("_folds_per_sec" if op == "merge" else "_throughput_items_per_sec"))
    cpu = row.get(op + "_cpu_time_ms")
    wall = row.get(op + "_wall_time_ms")
    if not rate or not cpu or not wall:
        return None
    samples = [rate["samples"], wall["samples"], cpu["user_ms"]["samples"], cpu["sys_ms"]["samples"]]
    if len({len(x) for x in samples}) != 1:
        raise ValueError("unaligned CPU/rate/time samples")
    values = []
    for r, elapsed, user, system in zip(*samples):
        work = r * elapsed / 1000
        if work <= 0:
            raise ValueError("zero benchmark work")
        values.append((user + system) * 1_000_000 / work)
    result = measurement(values)
    result["method"] = "mean of paired run (process user+system CPU ns)/(throughput*wall seconds); per " + ("binary merge" if op == "merge" else "input item" if op == "insert" else "point-frequency key lookup")
    return result


def cpu_batch(row, op):
    cpu = row.get(op + "_cpu_time_ms")
    if not cpu:
        return None
    return measurement([(u + s) * 1_000_000 for u, s in
                        zip(cpu["user_ms"]["samples"], cpu["sys_ms"]["samples"])])


def export(raw, manifest, operation_reports, memory_rows, resource_rows=None):
    records, baselines = [], []
    if not (len(raw) == len(manifest["invocations"]) == len(operation_reports)):
        raise ValueError("misaligned invocation reports")
    for row, invocation, operations in zip(raw, manifest["invocations"], operation_reports):
        if row["schema_version"] != 5:
            raise ValueError("adapter requires sketch-bench schema 5")
        accuracy = row["query_accuracy"]
        resource = next((r for r in (resource_rows or []) if r["sketch"] == row["sketch"]
                        and r["impl"] == row["impl"] and r["sketch_config"] == row["sketch_config"]
                        and r["workload"] == row["workload"]), None)
        if resource_rows is not None and (resource is None or "phases" not in resource):
            raise ValueError("complete comparison requires matched disjoint live-state phase measurements for every record")
        def phase_metric(phase):
            return (dict(measurement(resource["phases"][phase + "_cpu_ns_samples"]), method=resource["phases"]["method"])
                    if resource and resource.get("phases") else None)
        shape = row["workload"]["synthetic"]["description"]
        distribution = {"id": invocation["distribution_id"],
                        "family": invocation["distribution"],
                        "sample_count": shape["row_num"],
                        "distinct_count": int(accuracy["probes_all"]),
                        "parameters": shape["column_spec"][0]["distribution"]}
        if invocation["algorithm"] == "Exact":
            exact_memory = next((m for m in memory_rows if m.get("impl") == "polars" and m["workload"] == row["workload"]), None)
            baselines.append({"distribution": distribution, "implementation": "polars exact group_by + HashMap",
                              "insert_cpu_ns_per_item": phase_metric("update") or cpu_per_op(row, "insert"),
                              "prepare_cpu_ns_per_dataset": phase_metric("prepare") or cpu_batch(row, "prepare"),
                              "read_cpu_ns_per_key": phase_metric("read") or cpu_per_op(row, "query"),
                              "retained_bytes": max(r["bench"]["memory_bytes"] for r in operations
                                                    if r["bench"].get("operation") == "query"),
                              "retained_bytes_method": "upstream prepared-query memory_bytes formula: buffer capacity plus HashMap capacity footprint; not measured allocated heap or peak",
                              "accuracy": accuracy})
            baselines[-1]["empty_build_cpu_ns"] = (dict(measurement(resource["build_cpu_ns_samples"]), method=resource["build_method"])
                                                       if resource else None)
            if exact_memory and all(delta == 0 for delta in exact_memory["cleanup_deltas"]):
                baselines[-1]["retained_heap_bytes"] = dict(measurement([s[0] for s in exact_memory["samples"]]), method=exact_memory["method"])
                baselines[-1]["peak_heap_bytes"] = dict(measurement([s[1] for s in exact_memory["samples"]]), method=exact_memory["method"])
            continue
        algorithm = invocation["algorithm"]
        memory = next(m for m in memory_rows if m["sketch"] == row["sketch"] and m["workload"] == row["workload"]
                      and (m.get("sketch_config", row["sketch_config"]) == row["sketch_config"]))
        params = row["sketch_config"]["params"]
        timestamp = row["insert_timestamp"]
        if not timestamp.endswith("Z"):
            raise ValueError("expected upstream UTC timestamp")
        measured = int(datetime.datetime.fromisoformat(timestamp[:19] + "+00:00").timestamp())
        environment = dict(manifest["environment"])
        environment["implementation"] = "asap_sketchlib RegularPath Vector2D"
        # The algorithm distinguishes families; storage/hash path must also match.
        environment["id"] = hashlib.sha256(json.dumps(environment, sort_keys=True).encode()).hexdigest()[:16]
        records.append({"id": invocation["id"], "algorithm": algorithm,
                        "params": {algorithm: {"width": params["cols"], "depth": params["rows"]}},
                        "distribution": distribution, "environment": environment,
                        "measured_at_unix_seconds": measured,
                        "valid_until_unix_seconds": measured + 30 * 86400,
                        "provenance": {"command": shlex.join(invocation["argv"]),
                                       "dataset": "deterministic synthetic i64 frequency stream, seed 42",
                                       "source_revision": manifest["source_revision"],
                                       "repetitions": row["runs"]},
                        "metrics": {"build_cpu_ns": (dict(measurement(resource["build_cpu_ns_samples"]), method=resource["build_method"])
                                                      if resource else None),
                                    "update_cpu_ns": phase_metric("update") or cpu_per_op(row, "insert"),
                                    "merge_cpu_ns": phase_metric("merge") or cpu_per_op(row, "merge"),
                                    "read_cpu_ns": phase_metric("read") or cpu_per_op(row, "query"),
                                    "retained_bytes": dict(measurement([float(s[0]) for s in memory["samples"]]), method=memory["method"]),
                                    "peak_bytes": dict(measurement([float(s[1]) for s in memory["samples"]]), method=memory["method"]),
                                    "serialized_bytes": (dict(measurement([resource["serialized_bytes"]]), method="actual serialize_to_bytes MsgPack length; estimate-equivalent roundtrip verified for every input key") if resource else None),
                                    "disk_bytes": (dict(measurement([resource["disk_bytes"]]), method=resource["disk_method"]) if resource else None)},
                        "error": {"metric": "mean_absolute_relative_frequency_error_all_distinct_keys",
                                  "mean": accuracy["are_all"], "max": None,
                                  "trials": int(accuracy.get("accuracy_runs", 1)),
                                  "ground_truth_method": "exact offline HashMap counts; every distinct key probed",
                                  "query": {"kind": "point_frequency", "value_type": "i64",
                                            "accuracy_stddev": accuracy.get("are_all_stddev"),
                                            "full_accuracy": accuracy}}})
    return {"schema_version": 1,
            "benchmark_version": "sketch-bench@" + manifest["source_revision"] + ("; adapter-v2-disjoint" if resource_rows else "; adapter-v1"),
            "model_version": "empirical-update-cpu-v1", "records": records}, baselines


def comparison_artifact(artifact, baselines, manifest):
    query = {"kind": "point_frequency", "value_type": "i64", "probe_set": "all_distinct_keys"}
    exact_records = []
    for baseline in baselines:
        if baseline["empty_build_cpu_ns"] is None:
            continue
        reference = next(r for r in artifact["records"] if r["distribution"] == baseline["distribution"])
        environment = dict(manifest["environment"])
        environment["implementation"] = "polars group_by + std HashMap"
        environment["implementation_version"] = "polars 0.46.0"
        environment["id"] = hashlib.sha256(json.dumps(environment, sort_keys=True).encode()).hexdigest()[:16]
        invocation = next(i for i in manifest["invocations"] if i["algorithm"] == "Exact"
                          and i["distribution_id"] == baseline["distribution"]["id"])
        exact_records.append({"id": invocation["id"], "distribution": baseline["distribution"],
                              "environment": environment, "query": query,
                              "measured_at_unix_seconds": reference["measured_at_unix_seconds"],
                              "valid_until_unix_seconds": reference["valid_until_unix_seconds"],
                              "provenance": {"command": shlex.join(invocation["argv"]),
                                             "dataset": "deterministic synthetic i64 frequency stream, seed 42; exact zero-error comparison verified offline",
                                             "source_revision": manifest["source_revision"], "repetitions": 5},
                              "metrics": {"empty_build_cpu_ns": baseline["empty_build_cpu_ns"],
                                          "update_cpu_ns": baseline["insert_cpu_ns_per_item"],
                                          "prepare_cpu_ns": baseline["prepare_cpu_ns_per_dataset"],
                                          "read_cpu_ns": baseline["read_cpu_ns_per_key"],
                                          "retained_bytes": baseline.get("retained_heap_bytes") or dict(measurement([baseline["retained_bytes"]]), method=baseline["retained_bytes_method"]),
                                          "peak_bytes": baseline.get("peak_heap_bytes")}})
    return {"schema_version": 1, "timing_contract": "disjoint_live_state_v1", "sketch_evidence": artifact,
            "query_bindings": [{"record_id": r["id"], "query": query} for r in artifact["records"]],
            "exact_records": exact_records}


def refresh_memory(bench_repo, raw_path, memory_path):
    deps = bench_repo.resolve() / "target/release/deps"
    with tempfile.TemporaryDirectory(prefix="asap-memory-probe-") as temporary:
        executable = str(Path(temporary) / "memory-probe")
        argv = ["rustc", "--edition=2021", "-C", "opt-level=3", "-C", "panic=abort", "-C", "target-cpu=native",
                "-L", f"dependency={deps}", str(Path(__file__).with_name("memory_probe.rs")), "-o", executable]
        for name in ["asap_sketchlib", "aqpbm_datagen", "serde_json", "sketch_bench"]:
            candidates = list(deps.glob(f"lib{name}-*.rlib"))
            if len(candidates) != 1:
                raise ValueError(f"expected one pinned release library for {name}, got {len(candidates)}")
            argv += ["--extern", f"{name}={candidates[0]}"]
        for pattern in ["*/out", "*/out/lib"]:
            for native in (bench_repo.resolve() / "target/release/build").glob(pattern):
                argv += ["-L", f"native={native}"]
        subprocess.check_call(argv)
        memory_path.write_text(command([executable, str(raw_path)], env=dict(os.environ, POLARS_MAX_THREADS="1")) + "\n")
    return {"source_sha256": hashlib.sha256(Path(__file__).with_name("memory_probe.rs").read_bytes()).hexdigest(),
            "compile_argv": argv,
            "allocator": "System + requested-byte tracking; separate from uninstrumented jemalloc CPU measurements"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bench-repo", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--size", type=int, default=20000)
    parser.add_argument("--cardinality", type=int, default=1000)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--sweep", action="store_true", help="measure three widths per frequency family")
    parser.add_argument("--export-only", action="store_true")
    parser.add_argument("--refresh-memory", action="store_true", help="with --export-only, remeasure heap without rerunning CPU timings")
    parser.add_argument("--resume", action="store_true", help="continue a partially completed run with identical parameters")
    args = parser.parse_args()
    if args.refresh_memory and not args.export_only:
        parser.error("--refresh-memory requires --export-only")
    args.output.mkdir(parents=True, exist_ok=True)
    manifest_path = args.output / "manifest.json"
    raw_path = args.output / "raw.json"
    operations_path = args.output / "operation-reports.json"
    if args.export_only:
        manifest, raw = json.loads(manifest_path.read_text()), json.loads(raw_path.read_text())
        operation_reports = json.loads(operations_path.read_text())
    else:
        if (raw_path.exists() or manifest_path.exists()) and not args.resume:
            parser.error("output already contains a run; use a new directory or --export-only")
        repo = args.bench_repo.resolve()
        if 'name = "asap_sketchlib"\nversion = "0.2.2"' not in (repo / "Cargo.lock").read_text():
            parser.error("adapter is validated with asap_sketchlib 0.2.2; do not relabel another version")
        binary = repo / "target/release/approxbench"
        cpu = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
                    if line.startswith("model name")), platform.processor())
        manifest = {"source_revision": command(["git", "rev-parse", "HEAD"], cwd=repo),
                    "source_dirty": bool(command(["git", "status", "--porcelain"], cwd=repo)),
                    "environment": {"cpu": cpu, "os": platform.platform(),
                                    "runtime": command(["rustc", "--version"]) + "; release; target-cpu=native; jemalloc; POLARS_MAX_THREADS=1",
                                    "implementation_version": "asap_sketchlib 0.2.2"},
                    "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                    "runtime_provenance": BUILD_PROVENANCE,
                    "invocations": []}
        raw, operation_reports = [], []
        if args.resume:
            previous = json.loads(manifest_path.read_text())
            if previous["binary_sha256"] != manifest["binary_sha256"] or previous["environment"] != manifest["environment"]:
                parser.error("resume requires the same binary and environment")
            manifest, raw = previous, json.loads(raw_path.read_text())
            operation_reports = json.loads(operations_path.read_text())
            for invocation in manifest["invocations"]:
                argv = invocation["argv"]
                if any(argv[argv.index(flag) + 1] != str(value) for flag, value in
                       [("--size", args.size), ("--cardinality", args.cardinality), ("--runs", args.runs)]):
                    parser.error("resume requires the same size, cardinality, and runs")
        env = dict(os.environ, POLARS_MAX_THREADS="1")
        for distribution in ["uniform", "zipf"]:
            targets = [
                    ("Cms", "cms-regularpath-vector2d", "lib", 5, 272),
                    ("CountSketch", "countsketch-regularpath-vector2d", "lib", 83, 30000),
                    ("Exact", "cms", "polars", 5, 272)]
            if args.sweep:
                targets += [("Cms", "cms-regularpath-vector2d", "lib", 5, width) for width in [2720, 27200, 512, 4096, 32768]]
                targets += [("CountSketch", "countsketch-regularpath-vector2d", "lib", 83, width) for width in [300, 3000]]
            for algorithm, variant, library, rows, cols in targets:
                record_id = f"{algorithm.lower()}-{distribution}-seed42" + (f"-w{cols}-d{rows}" if args.sweep else "")
                if any(i["id"] == record_id for i in manifest["invocations"]):
                    continue
                argv = [str(binary), "sketchbench", "--variant", variant, "--library", library,
                        "--config", f"rows={rows} cols={cols}", "--dataset", distribution,
                        "--size", str(args.size), "--cardinality", str(args.cardinality),
                        "--dtype", "i64", "--seed", "42", "--runs", str(args.runs),
                        "--warmup-runs", "2", "--operations",
                        "insert,query" if algorithm == "Exact" else "insert,query,merge",
                        "--metrics", "throughput,cpu,memory"]
                if distribution == "zipf":
                    argv += ["--zipf-s", "1.1"]
                print(shlex.join(argv), flush=True)
                output = command(argv, env=env)
                accuracy_argv = list(argv)
                accuracy_argv[accuracy_argv.index("--operations") + 1] = "query"
                accuracy_argv[accuracy_argv.index("--metrics") + 1] = "accuracy"
                output += "\n" + command(accuracy_argv, env=env)
                prepare_argv = None
                if algorithm == "Exact":
                    prepare_argv = list(argv)
                    prepare_argv[prepare_argv.index("--operations") + 1] = "prepare"
                    prepare_argv[prepare_argv.index("--metrics") + 1] = "latency,cpu,memory"
                    output += "\n" + command(prepare_argv, env=env)
                operations = [json.loads(line) for line in output.splitlines() if line.strip()]
                row = json.loads(command([str(binary), "flatten"], input=output, env=env))
                raw.append(row)
                operation_reports.append(operations)
                manifest["invocations"].append({"id": record_id,
                    "distribution_id": f"{distribution}-n{args.size}-c{args.cardinality}-seed42",
                    "distribution": distribution, "algorithm": algorithm, "argv": argv,
                    "accuracy_argv": accuracy_argv, "prepare_argv": prepare_argv})
                raw_path.write_text(json.dumps(raw, indent=2) + "\n")
                manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
                operations_path.write_text(json.dumps(operation_reports, indent=2) + "\n")
    memory_path = args.output / "memory-probe.json"
    resources_path = args.output / "resource-probe.json"
    if not args.export_only:
        # Older partial runs may lack prepare: CPU/memory alone select no upstream timing pass.
        for index, (row, invocation) in enumerate(zip(raw, manifest["invocations"])):
            if invocation["algorithm"] == "Exact" and not row.get("prepare_cpu_time_ms"):
                prepare_argv = list(invocation["argv"])
                prepare_argv[prepare_argv.index("--operations") + 1] = "prepare"
                prepare_argv[prepare_argv.index("--metrics") + 1] = "latency,cpu,memory"
                output = command(prepare_argv, env=env)
                operation_reports[index].extend(json.loads(line) for line in output.splitlines() if line.strip())
                raw[index] = json.loads(command([str(binary), "flatten"],
                    input="\n".join(json.dumps(r) for r in operation_reports[index]), env=env))
                invocation["prepare_argv"] = prepare_argv
        raw_path.write_text(json.dumps(raw, indent=2) + "\n")
        operations_path.write_text(json.dumps(operation_reports, indent=2) + "\n")
        deps = args.bench_repo.resolve() / "target/release/deps"
        manifest["memory_probe"] = refresh_memory(args.bench_repo, raw_path, memory_path)
        with tempfile.TemporaryDirectory(prefix="asap-resource-probe-") as temporary:
            executable = str(Path(temporary) / "resource-probe")
            argv = ["rustc", "--edition=2021", "-C", "opt-level=3", "-C", "panic=abort", "-C", "target-cpu=native",
                    "-L", f"dependency={deps}", str(Path(__file__).with_name("resource_probe.rs")), "-o", executable]
            for name in ["asap_sketchlib", "aqpbm_datagen", "serde_json", "tikv_jemallocator", "sketch_bench"]:
                candidates = list(deps.glob(f"lib{name}-*.rlib"))
                if len(candidates) != 1:
                    raise ValueError(f"expected one pinned release library for {name}, got {len(candidates)}")
                argv += ["--extern", f"{name}={candidates[0]}"]
            for native in (args.bench_repo.resolve() / "target/release/build").glob("tikv-jemalloc-sys-*/out/lib"):
                argv += ["-L", f"native={native}"]
            for native in (args.bench_repo.resolve() / "target/release/build").glob("*/out"):
                argv += ["-L", f"native={native}"]
            subprocess.check_call(argv)
            resources_path.write_text(command([executable, str(raw_path), temporary], env=env) + "\n")
        manifest["resource_probe"] = {"source_sha256": hashlib.sha256(Path(__file__).with_name("resource_probe.rs").read_bytes()).hexdigest(),
                                     "compile_argv": argv, "allocator": "jemalloc", "disk_filesystem_path": tempfile.gettempdir()}
        manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    if args.refresh_memory:
        if hashlib.sha256((args.bench_repo.resolve() / "target/release/approxbench").read_bytes()).hexdigest() != manifest["binary_sha256"]:
            parser.error("memory refresh requires the original benchmark binary")
        manifest["memory_probe"] = refresh_memory(args.bench_repo, raw_path, memory_path)
    manifest["runtime_provenance"] = BUILD_PROVENANCE
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    artifact, baselines = export(raw, manifest, operation_reports, json.loads(memory_path.read_text()),
                                json.loads(resources_path.read_text()) if resources_path.exists() else None)
    (args.output / "planner-evidence.json").write_text(json.dumps(artifact, indent=2) + "\n")
    (args.output / "exact-baselines.json").write_text(json.dumps(baselines, indent=2) + "\n")
    if resources_path.exists():
        comparison = comparison_artifact(artifact, baselines, manifest)
        (args.output / "comparison-evidence.json").write_text(json.dumps(comparison, indent=2) + "\n")
        for exact in comparison["exact_records"]:
            row = next(r for r in artifact["records"] if r["distribution"] == exact["distribution"])
            for budget, suffix in [(0.01, "001"), (0.05, "005")]:
                request = {"context": {"distribution": row["distribution"], "environment": row["environment"],
                                        "now_unix_seconds": max(r["measured_at_unix_seconds"] for r in artifact["records"])},
                           "exact_environment": exact["environment"], "query": exact["query"],
                           "accuracy": {"metric": row["error"]["metric"], "max_observed_mean": budget, "minimum_trials": 1},
                           "workload": {"input_items_per_state": row["distribution"]["sample_count"],
                                        "reads_per_state": 1000, "merges_per_state": 0, "state_instances": 1, "horizon_seconds": 300.0},
                           "weights": {"cpu_ns_weight": 1.0, "retained_byte_seconds_weight": 0.0}, "formal_minimums": None}
                (args.output / ("request-" + row["distribution"]["family"] + "-are" + suffix + ".json")).write_text(json.dumps(request, indent=2) + "\n")
            request["accuracy"]["max_observed_mean"] = 0.01
            request["weights"]["retained_byte_seconds_weight"] = 0.01
            request["formal_minimums"] = [{"algorithm": "Cms", "params": {"Cms": {"width": 512, "depth": 5}}}]
            (args.output / ("request-" + row["distribution"]["family"] + "-memory-weighted.json")).write_text(json.dumps(request, indent=2) + "\n")
    for row in artifact["records"]:
        context = {"distribution": row["distribution"], "environment": row["environment"],
                   "now_unix_seconds": max(r["measured_at_unix_seconds"] for r in artifact["records"])}
        (args.output / ("context-" + row["distribution"]["family"] + ".json")).write_text(json.dumps(context, indent=2) + "\n")
    print(f"Exported {len(artifact['records'])} sketch records and {len(baselines)} exact baselines.")


if __name__ == "__main__":
    main()
