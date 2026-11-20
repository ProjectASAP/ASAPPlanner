#!/usr/bin/env python3
"""SQL planner-bench workload generator.

A *config* is a dict of axis values (see BASELINE). `workload(config, seed)`
renders it into a PlanningWorkload JSON over one wide table `t` (or
`n_tables` copies). `manifest.py` decides which configs to run.
"""
import json
import random

# Dimension columns and their declared distinct counts, low to high.
DIMS = {"a": 3, "b": 10, "c": 50, "d": 200, "e": 1000, "f": 5000}
DIM_ORDER = list(DIMS)
UID = "uid"  # high-cardinality column COUNT DISTINCT reads
UID_DISTINCT = 1_000_000
SERIES_COUNT = 100_000
SAMPLE_INTERVAL_MS = 10_000

MIN = 60_000
WINDOW_POOLS = {
    # Each a multiple of the previous: rollup/sharing-friendly.
    "nested": [1 * MIN, 5 * MIN, 10 * MIN, 30 * MIN, 60 * MIN, 360 * MIN, 1440 * MIN],
    # Pairwise coprime minutes: little pane sharing.
    "coprime": [7 * MIN, 11 * MIN, 13 * MIN, 17 * MIN, 19 * MIN, 23 * MIN, 29 * MIN],
}
INTERVAL_POOLS = {
    "aligned": [10_000, 30_000, 60_000, 300_000],
    "coprime": [7_000, 11_000, 13_000, 17_000],
}
QUANTILES = [0.5, 0.9, 0.95, 0.99]
TOPK = [5, 10, 20]

# Accuracy targets (requirements.accuracy JSON); None = exact (field omitted).
EPS_TIGHT = {"max_error": 0.01, "failure_probability": 0.001}
EPS_MID = {"max_error": 0.05, "failure_probability": 0.01}
EPS_LOOSE = {"max_error": 0.1}
ACCURACY_MODES = {
    "uniform": [EPS_TIGHT],
    "mixed": [None, EPS_TIGHT, EPS_MID, EPS_LOOSE],
    "exact": [None],
    "ignore": [EPS_TIGHT],  # targets present, run with --ignore-accuracy-guarantees
}

# Aggregate families. "safe" ones appear in committed corpora; the others
# are kept only if the probe run shows they lower and plan.
AGG_FAMILIES = ["sum", "count", "quantile", "count_distinct", "topk", "avg", "min", "max"]

BASELINE = {
    "n_queries": 16,
    "agg_mix": ["sum", "count", "quantile", "count_distinct", "topk"],
    "aggs_per_query": 1,
    "gb_width": 2,          # max GROUP BY columns per query (0..6)
    "gb_structure": "random",  # same | nested | random
    "n_windows": 3,         # distinct windows drawn from the pool
    "window_pool": "nested",
    "n_intervals": 1,
    "interval_pool": "aligned",
    "phases": "zero",       # zero | random
    "filter_frac": 0.0,     # fraction of queries with a filter
    "filter_kind": "eq",    # eq | in
    "filter_values": 2,     # distinct literal values filters draw from
    "accuracy": "uniform",
    "capabilities": "every",  # every | default
    "n_tables": 1,
}


def _agg_sql(family, rng):
    if family == "sum":
        return "SUM(value)"
    if family == "count":
        return "COUNT(*)"
    if family == "avg":
        return "AVG(value)"
    if family == "min":
        return "MIN(value)"
    if family == "max":
        return "MAX(value)"
    if family == "quantile":
        return f"percentile_cont({rng.choice(QUANTILES)}) WITHIN GROUP (ORDER BY value)"
    if family == "count_distinct":
        return f"COUNT(DISTINCT {UID})"
    raise ValueError(family)


def _group_by(config, i, rng, fixed):
    w = config["gb_width"]
    s = config["gb_structure"]
    if w == 0:
        return []
    if s == "same":
        return fixed
    if s == "nested":
        # Prefix chain over a fixed column order: a ⊂ ab ⊂ abc ...
        return fixed[: (i % w) + 1]
    if s == "random":
        k = rng.randint(1, w)
        return sorted(rng.sample(DIM_ORDER, k), key=DIM_ORDER.index)
    raise ValueError(s)


def _duration_sql(ms):
    if ms % (60 * MIN) == 0:
        return f"INTERVAL '{ms // (60 * MIN)}' HOUR"
    return f"INTERVAL '{ms // MIN}' MINUTE"


def _query(config, i, rng, fixed_gb, windows, table):
    gb = _group_by(config, i, rng, fixed_gb)
    family = rng.choice(config["agg_mix"])
    window = rng.choice(windows)
    where = [f"ts > now() - {_duration_sql(window)}"]
    if rng.random() < config["filter_frac"]:
        # Filter on a column outside the GROUP BY when there is one.
        pool = [c for c in DIM_ORDER if c not in gb] or DIM_ORDER
        col = rng.choice(pool)
        values = [f"'{col}{v}'" for v in range(1, config["filter_values"] + 1)]
        if config["filter_kind"] == "in" and len(values) > 1:
            pick = sorted(rng.sample(values, rng.randint(2, len(values))))
            where.append(f"{col} IN ({', '.join(pick)})")
        else:
            where.append(f"{col} = {rng.choice(values)}")
    where_sql = " AND ".join(where)
    gb_sql = f" GROUP BY {', '.join(gb)}" if gb else ""
    sel = ", ".join(gb + [""]) if gb else ""
    if family == "topk":
        # Top-k groups by count or sum; needs a grouping column.
        key = gb or [rng.choice(DIM_ORDER[1:])]
        measure = rng.choice(["COUNT(*)", "SUM(value)"])
        return (
            f"SELECT {', '.join(key)}, {measure} AS m FROM {table} WHERE {where_sql} "
            f"GROUP BY {', '.join(key)} ORDER BY m DESC LIMIT {rng.choice(TOPK)}"
        )
    n = config["aggs_per_query"]
    aggs = [_agg_sql(family, rng)]
    others = [f for f in config["agg_mix"] if f != "topk"]
    # The mix may admit fewer than n distinct aggregates: bounded tries.
    for _ in range(20 * n):
        if len(aggs) >= n or not others:
            break
        a = _agg_sql(rng.choice(others), rng)
        if a not in aggs:
            aggs.append(a)
    return f"SELECT {sel}{', '.join(aggs)} FROM {table} WHERE {where_sql}{gb_sql}"


def workload(config, seed):
    """The PlanningWorkload dict and the --sql-catalog-table JSON strings."""
    rng = random.Random(seed)
    windows = rng.sample(WINDOW_POOLS[config["window_pool"]], config["n_windows"])
    intervals = rng.sample(INTERVAL_POOLS[config["interval_pool"]], config["n_intervals"])
    w = config["gb_width"]
    fixed_gb = sorted(rng.sample(DIM_ORDER, w), key=DIM_ORDER.index) if w else []
    tables = [f"t{j}" if config["n_tables"] > 1 else "t" for j in range(config["n_tables"])]
    accuracy_pool = ACCURACY_MODES[config["accuracy"]]
    entries, seen = [], set()
    attempts = 0
    while len(entries) < config["n_queries"]:
        attempts += 1
        if attempts > 50 * config["n_queries"]:
            break  # the axis values admit fewer distinct queries than asked
        i = len(entries)
        table = tables[i % len(tables)]
        q = _query(config, i, rng, fixed_gb, windows, table)
        interval = rng.choice(intervals)
        phase = 0 if config["phases"] == "zero" else rng.randrange(0, interval, 1000)
        acc = rng.choice(accuracy_pool)
        key = (q, interval, phase, json.dumps(acc))
        if key in seen:
            continue
        seen.add(key)
        entries.append({
            "query": q,
            "demand": {"fixed_interval_at": {"interval": interval, "evaluation_phase": phase}},
            "requirements": {} if acc is None else {"accuracy": acc},
            "predictability": {"predictable": {"known_at": None}},
        })
    per_table_rate = SERIES_COUNT / (SAMPLE_INTERVAL_MS / 1000)
    sources = {
        t: {
            "series_count": SERIES_COUNT,
            "sample_interval": SAMPLE_INTERVAL_MS,
            "column_signs": {"value": "non_negative"},
            "max_distinct_per_evaluation": UID_DISTINCT,
            "distinct_values_per_label": dict(DIMS, **{UID: UID_DISTINCT}),
        }
        for t in tables
    }
    wl = {
        "query_workload": {
            "language": {"sql": "datafusion_sql"},
            "query_batch": None,
            "repeating_queries": entries,
        },
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": per_table_rate * len(tables)}},
            "sources": sources,
        },
    }
    catalog = [
        json.dumps({
            "name": t,
            "columns": [{"name": "ts", "type": "timestamp"}]
            + [{"name": c, "type": "utf8"} for c in DIM_ORDER + [UID]]
            + [{"name": "value", "type": "float64"}],
            "time_index": 0,
        })
        for t in tables
    ]
    return wl, catalog


if __name__ == "__main__":
    wl, cat = workload(BASELINE, 1)
    for e in wl["query_workload"]["repeating_queries"]:
        print(e["demand"]["fixed_interval_at"]["interval"], e["query"])
