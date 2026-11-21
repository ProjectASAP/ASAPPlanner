#!/usr/bin/env python3
"""Build the job list and per-node shards.

usage: manifest.py <out_dir> [--nodes 4] [--only probe|calib|full]

Writes <out_dir>/workloads/<job>.json, <out_dir>/catalogs/<job>.json and
<out_dir>/shard<i>.jsonl (one job per line). A job is one (config, seed).
`series` + `rank` order a ladder: the runner skips a job when a smaller-rank
job of the same series already failed at the largest tier.
"""
import argparse
import itertools
import json
import os
import random

from gen import AGG_FAMILIES, BASELINE, workload

SEEDS = [1, 2, 3]
SCALE_N = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024]

PROFILES = {
    "baseline": {},
    # Most sharing: one interval, nested windows and prefix GROUP BYs.
    "high_sharing": {"gb_structure": "nested", "gb_width": 3, "window_pool": "nested",
                     "n_windows": 3, "n_intervals": 1, "accuracy": "uniform"},
    # Least sharing: wide random GROUP BYs, coprime windows/intervals, random phases.
    "low_sharing": {"gb_structure": "random", "gb_width": 4, "window_pool": "coprime",
                    "n_windows": 7, "interval_pool": "coprime", "n_intervals": 4,
                    "phases": "random", "accuracy": "mixed"},
    "only_sum": {"agg_mix": ["sum"]},
    "only_quantile": {"agg_mix": ["quantile"]},
    "only_count_distinct": {"agg_mix": ["count_distinct"]},
    "only_topk": {"agg_mix": ["topk"]},
}
# Scaling ladders need more distinct queries than the axes alone give (e.g.
# only_sum saturates at 63): half the queries filter one non-grouped column
# on one of 64 values, like one dashboard panel repeated per customer.
SCALE_DIVERSITY = {"filter_frac": 0.5, "filter_kind": "eq", "filter_values": 64}

# One-factor-at-a-time: axis -> values, each applied to the baseline.
OFAT = {
    "gb": [{"gb_width": w, "gb_structure": s} for w in range(0, 7)
           for s in ["same", "nested", "random"] if not (w == 0 and s != "same")],
    "agg_mix": [{"agg_mix": [f]} for f in AGG_FAMILIES] + [{"agg_mix": AGG_FAMILIES}],
    "aggs_per_query": [{"aggs_per_query": k} for k in [1, 2, 3, 4]],
    "windows": [{"n_windows": n, "window_pool": p} for n in [1, 2, 4, 7]
                for p in ["nested", "coprime"]],
    "intervals": [{"n_intervals": n, "interval_pool": p, "phases": ph}
                  for n in [1, 2, 4] for p in ["aligned", "coprime"] for ph in ["zero", "random"]],
    "filters": [{"filter_frac": f, "filter_kind": k, "filter_values": v}
                for f in [0.0, 0.5, 1.0] for k in ["eq", "in"] for v in [1, 4, 16]
                if not (f == 0.0 and (k, v) != ("eq", 1)) and not (k == "in" and v == 1)],
    "accuracy": [{"accuracy": a} for a in ["uniform", "mixed", "exact", "ignore"]],
    "capabilities": [{"capabilities": c} for c in ["every", "default"]],
    "tables": [{"n_tables": t} for t in [1, 2, 4]],
}
OFAT_N = [16, 64]

RANDOM_CONFIGS = 300


def random_config(rng):
    c = dict(BASELINE)
    c["n_queries"] = int(round(2 ** rng.uniform(2, 8)))  # 4..256, log-uniform
    k = rng.randint(1, len(AGG_FAMILIES))
    c["agg_mix"] = rng.sample(AGG_FAMILIES, k)
    c["aggs_per_query"] = rng.choice([1, 1, 2, 3])
    c["gb_width"] = rng.randint(0, 6)
    c["gb_structure"] = rng.choice(["same", "nested", "random"])
    c["window_pool"] = rng.choice(["nested", "coprime"])
    c["n_windows"] = rng.randint(1, 7)
    c["interval_pool"] = rng.choice(["aligned", "coprime"])
    c["n_intervals"] = rng.randint(1, 4)
    c["phases"] = rng.choice(["zero", "random"])
    c["filter_frac"] = rng.choice([0.0, 0.25, 0.5, 1.0])
    c["filter_kind"] = rng.choice(["eq", "in"])
    c["filter_values"] = rng.choice([2, 4, 16])
    c["accuracy"] = rng.choice(["uniform", "mixed", "exact", "ignore"])
    c["capabilities"] = rng.choice(["every", "every", "default"])
    c["n_tables"] = rng.choice([1, 1, 1, 2, 4])
    return c


def configs(only):
    """(series, rank, config) triples."""
    out = []
    if only == "probe":
        # Each aggregate family and filter kind alone, tiny: does it lower and plan?
        for f in AGG_FAMILIES:
            out.append((f"probe_{f}", 0, dict(BASELINE, n_queries=2, agg_mix=[f])))
        out.append(("probe_in", 0, dict(BASELINE, n_queries=2, filter_frac=1.0, filter_kind="in", filter_values=4)))
        out.append(("probe_multi", 0, dict(BASELINE, n_queries=2, aggs_per_query=4)))
        out.append(("probe_tables", 0, dict(BASELINE, n_queries=4, n_tables=2)))
        out.append(("probe_default_caps", 0, dict(BASELINE, n_queries=2, capabilities="default")))
        return out
    if only == "calib":
        for n in SCALE_N[:9]:
            for p in ["baseline", "low_sharing", "high_sharing"]:
                out.append((f"scale_{p}", n, dict(BASELINE, **SCALE_DIVERSITY, **PROFILES[p], n_queries=n)))
        return out
    for p, over in PROFILES.items():
        for n in SCALE_N:
            out.append((f"scale_{p}", n, dict(BASELINE, **SCALE_DIVERSITY, **over, n_queries=n)))
    for axis, values in OFAT.items():
        for n in OFAT_N:
            for i, over in enumerate(values):
                out.append((f"ofat_{axis}_n{n}_{i}", n, dict(BASELINE, **over, n_queries=n)))
    rng = random.Random(12345)
    for i in range(RANDOM_CONFIGS):
        c = random_config(rng)
        out.append((f"random_{i}", c["n_queries"], c))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--nodes", type=int, default=4)
    ap.add_argument("--only", default="full", choices=["probe", "calib", "full"])
    args = ap.parse_args()
    os.makedirs(f"{args.out}/workloads", exist_ok=True)
    os.makedirs(f"{args.out}/catalogs", exist_ok=True)
    seeds = SEEDS[:1] if args.only == "probe" else SEEDS
    jobs = []
    for (series, rank, config), seed in itertools.product(configs(args.only), seeds):
        job_id = f"{series}_r{rank}_s{seed}"
        wl, catalog = workload(config, seed)
        json.dump(wl, open(f"{args.out}/workloads/{job_id}.json", "w"))
        json.dump(catalog, open(f"{args.out}/catalogs/{job_id}.json", "w"))
        jobs.append({"id": job_id, "series": series, "rank": rank, "seed": seed,
                     "config": config,
                     "generated_queries": len(wl["query_workload"]["repeating_queries"])})
    # Shard by series so a ladder's skip rule sees all of it; spread the
    # expensive scale series first, round-robin.
    by_series = {}
    for j in jobs:
        by_series.setdefault(j["series"], []).append(j)
    order = sorted(by_series, key=lambda s: (not s.startswith("scale_"), s))
    shards = [[] for _ in range(args.nodes)]
    for i, s in enumerate(order):
        shards[i % args.nodes].extend(by_series[s])
    for i, shard in enumerate(shards):
        with open(f"{args.out}/shard{i}.jsonl", "w") as f:
            for j in shard:
                f.write(json.dumps(j) + "\n")
    print(f"{len(jobs)} jobs in {len(by_series)} series -> {args.nodes} shards "
          f"({', '.join(str(len(s)) for s in shards)})")


if __name__ == "__main__":
    main()
