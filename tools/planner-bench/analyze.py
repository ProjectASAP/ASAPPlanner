#!/usr/bin/env python3
"""Analyze collected planner_bench results.

usage: analyze.py <bench_dir>   (reads <bench_dir>/results/node*_results.jsonl)

Writes <bench_dir>/analysis/: results.csv (one row per job), plots, and
summary.md with growth exponents per scaling series, one-factor effects and
a log-log regression of stage time on workload and intermediate sizes.
"""
import glob
import json
import os
import re
import sys

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np
import pandas as pd

STAGES_MS = ["lowering_ms", "stage0_pass1_ms", "pass2_ms", "stage2_ms", "stage3_ms"]
MB = 2**20


def query_features(bench_dir, job_id):
    """Workload features read back from the generated workload."""
    path = f"{bench_dir}/workloads/{job_id}.json"
    if not os.path.exists(path):
        return {}
    qs = json.load(open(path))["query_workload"]["repeating_queries"]
    gbs, windows, intervals, phases, filtered = set(), set(), set(), set(), 0
    for e in qs:
        q = e["query"]
        m = re.search(r"GROUP BY ([^O]+?)(?: ORDER|$)", q)
        gbs.add(m.group(1).strip() if m else "")
        windows.add(re.search(r"INTERVAL '(\d+)' (\w+)", q).groups())
        d = e["demand"]["fixed_interval_at"]
        intervals.add(d["interval"])
        phases.add((d["interval"], d["evaluation_phase"]))
        filtered += " AND " in q
    return {"distinct_group_bys": len(gbs), "distinct_windows": len(windows),
            "distinct_intervals": len(intervals), "distinct_schedules": len(phases),
            "filtered_queries": filtered}


def load(bench_dir):
    rows = []
    for f in sorted(glob.glob(f"{bench_dir}/results/node*_results.jsonl")):
        latest = {}
        for line in open(f):
            r = json.loads(line)
            latest[r["id"]] = r  # a tier-2 line supersedes tier 1
        for r in latest.values():
            row = {k: r.get(k) for k in ["id", "series", "rank", "seed", "outcome", "tier",
                                         "wall_s", "watchdog_peak_rss_bytes", "generated_queries"]}
            row.update({f"cfg_{k}": (",".join(v) if isinstance(v, list) else v)
                        for k, v in r.get("config", {}).items()})
            b = r.get("bench") or {}
            for k, v in b.items():
                if k == "milp" and isinstance(v, dict):
                    row.update({f"milp_{mk}": mv for mk, mv in v.items()})
                elif not isinstance(v, (dict, list)):
                    row[k] = v
            row.update(query_features(bench_dir, r["id"]))
            rows.append(row)
    df = pd.DataFrame(rows)
    if "stage2_ms" in df:
        df["stage0_2_ms"] = df[["stage0_pass1_ms", "pass2_ms", "stage2_ms"]].sum(axis=1, min_count=1)
    return df


def fit_exponent(x, y):
    m = (x > 0) & (y > 0)
    if m.sum() < 3:
        return float("nan")
    return float(np.polyfit(np.log(x[m]), np.log(y[m]), 1)[0])


def main():
    bench_dir = sys.argv[1]
    out = f"{bench_dir}/analysis"
    os.makedirs(out, exist_ok=True)
    df = load(bench_dir)
    df.to_csv(f"{out}/results.csv", index=False)
    ok = df[df.outcome == "ok"].copy()
    md = ["# Planner bench (SQL)", "",
          f"{len(df)} jobs; outcomes: {df.outcome.value_counts().to_dict()}", ""]

    # Scaling ladders.
    scale = ok[ok.series.str.startswith("scale_")]
    md += ["## Scaling: growth exponent (log-log slope vs queries)", "",
           "| series | max N ok | S0–2 ms | S3 ms | peak RSS S0–2 | peak RSS S3 | logical cands | MILP vars |",
           "|---|---|---|---|---|---|---|---|"]
    fig, axes = plt.subplots(2, 2, figsize=(13, 10))
    for series, g in scale.groupby("series"):
        n = g.queries.astype(float)
        e = lambda col: fit_exponent(n, g[col].astype(float))
        md.append(f"| {series} | {int(n.max())} | {e('stage0_2_ms'):.2f} | {e('stage3_ms'):.2f} | "
                  f"{e('peak_rss_stage0_2_bytes'):.2f} | {e('peak_rss_stage3_bytes'):.2f} | "
                  f"{e('logical_candidates'):.2f} | {e('milp_variables'):.2f} |")
        agg = g.groupby("queries").median(numeric_only=True)
        label = series[len("scale_"):]
        axes[0, 0].plot(agg.index, agg.stage0_2_ms / 1000, "o-", label=label)
        axes[0, 1].plot(agg.index, agg.stage3_ms / 1000, "o-", label=label)
        axes[1, 0].plot(agg.index, agg.peak_rss_stage0_2_bytes / MB, "o-", label=label)
        axes[1, 1].plot(agg.index, agg.peak_rss_stage3_bytes / MB, "o-", label=label)
    for ax, title in zip(axes.flat, ["Stages 0–2 time (s)", "Stage 3 (MILP) time (s)",
                                     "Stages 0–2 peak RSS (MiB)", "Stage 3 peak RSS (MiB)"]):
        ax.set_xscale("log", base=2); ax.set_yscale("log"); ax.set_title(title)
        ax.set_xlabel("queries"); ax.grid(True, which="both", alpha=.3)
    axes[0, 0].legend(fontsize=8)
    fig.tight_layout(); fig.savefig(f"{out}/scaling.png", dpi=120); plt.close(fig)
    walls = df[df.series.str.startswith("scale_") & (df.outcome != "ok")]
    if len(walls):
        md += ["", "First non-ok rank per scaling series:", ""]
        for series, g in walls.groupby("series"):
            first = g.sort_values("rank").iloc[0]
            md.append(f"- {series}: N={first['rank']} → {first.outcome}")
    md.append("")

    # One-factor effects.
    md += ["## One factor at a time (median over seeds)", ""]
    ofat = ok[ok.series.str.startswith("ofat_")].copy()
    if len(ofat):
        ofat["axis"] = ofat.series.str.extract(r"ofat_(.+)_n\d+_\d+")[0]
        ofat["n"] = ofat.series.str.extract(r"_n(\d+)_")[0]
        cfg_cols = [c for c in ofat.columns if c.startswith("cfg_") and c != "cfg_n_queries"]
        for (axis, n), g in ofat.groupby(["axis", "n"]):
            varying = [c for c in cfg_cols if g[c].astype(str).nunique() > 1]
            t = g.groupby("series").agg(
                **{c: (c, "first") for c in varying},
                queries=("queries", "median"), s02_ms=("stage0_2_ms", "median"),
                s3_ms=("stage3_ms", "median"),
                rss02_mib=("peak_rss_stage0_2_bytes", lambda s: s.median() / MB),
                rss3_mib=("peak_rss_stage3_bytes", lambda s: s.median() / MB),
                logical=("logical_candidates", "median"), milp_vars=("milp_variables", "median"))
            md += [f"### {axis} (N={n})", "", "```\n" + t.round(1).to_string(index=False) + "\n```", ""]

    # Regression over everything that planned.
    md += ["## log-log regression (all ok jobs)", ""]
    feats = ["queries", "distinct_group_bys", "distinct_windows", "distinct_schedules",
             "filtered_queries", "memo_groups", "logical_candidates", "physical_candidates",
             "milp_variables"]
    for target in ["stage0_2_ms", "stage3_ms", "peak_rss_stage0_2_bytes", "peak_rss_stage3_bytes"]:
        corr = {}
        for f in feats:
            if f not in ok:
                continue
            both = ok[[f, target]].dropna().astype(float)
            if len(both) > 3:
                corr[f] = np.corrcoef(np.log1p(both[f]), np.log1p(both[target]))[0, 1]
        md.append(f"- **{target}** correlation of logs: " +
                  ", ".join(f"{k} {v:.2f}" for k, v in sorted(corr.items(), key=lambda kv: -abs(kv[1]))))
    fig, axes = plt.subplots(1, 3, figsize=(16, 5))
    for ax, (x, y) in zip(axes, [("logical_candidates", "stage0_2_ms"),
                                  ("milp_variables", "stage3_ms"),
                                  ("physical_candidates", "peak_rss_stage3_bytes")]):
        ax.scatter(ok[x], ok[y], s=6, alpha=.5)
        ax.set_xscale("log"); ax.set_yscale("log"); ax.set_xlabel(x); ax.set_ylabel(y)
        ax.grid(True, which="both", alpha=.3)
    fig.tight_layout(); fig.savefig(f"{out}/drivers.png", dpi=120); plt.close(fig)
    share = ok.stage3_ms / (ok.stage0_2_ms + ok.stage3_ms)
    md += ["", f"Stage 3 share of planning time: median {share.median():.0%}, "
               f"p90 {share.quantile(.9):.0%}", ""]
    open(f"{out}/summary.md", "w").write("\n".join(md))
    print(f"wrote {out}/summary.md, scaling.png, drivers.png, results.csv")


if __name__ == "__main__":
    main()
