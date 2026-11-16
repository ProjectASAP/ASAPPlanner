"""Generate the sketch-heavy workload set: the PlanningWorkload files in this
directory, its tables.json, and its entry in ../index.json.

Each workload targets one sketch family the planner can select (UnivMon for
a frequency L2, Count-Min or Count Sketch with a heap for top-k, KMV for
distinct counts), sits next to one, or is a shape that should plan a sketch
but does not yet (`blocked-*`). File names are
<subset>-<shape>__<parameters>, for example
heavy-hitters-cmsheap-topk10__u1000-i1-1hz-5m-every60s-l1eps0.01-d0.05:
top-10 URLs by request rate over 1,000 URLs with one instance each, sampled
at 1 Hz, a 5 min window, evaluated every 60 s, at an L1-normalized error of
0.01 with failure probability 0.05. All metric, table and label names are
synthetic.

Run from anywhere: python3 tools/dag-viewer/workloads/sketch-heavy/gen.py
"""
import json, os

HERE = os.path.dirname(os.path.abspath(__file__))
INDEX = os.path.join(HERE, "..", "index.json")
SET_ID = os.path.basename(HERE)
SQL = {"sql": "datafusion_sql"}
CLICKS = {"name": "clicks", "time_index": 0, "columns": [
    {"name": "ts", "type": "timestamp", "nullable": False},
    {"name": "url", "type": "utf8", "nullable": False},
    {"name": "user_id", "type": "utf8", "nullable": False},
    {"name": "client_ip", "type": "utf8", "nullable": False},
    {"name": "region", "type": "utf8", "nullable": False},
    {"name": "service", "type": "utf8", "nullable": False},
    {"name": "bytes", "type": "float64", "nullable": False}]}

def target(metric, eps, delta):
    return {"accuracy_metric": metric, "max_error": eps, "failure_probability": delta, "accuracy_scope": "whole_result"}

def q(text, every_s, acc=None):
    r = {"query": text, "demand": {"fixed_interval_at": {"interval": every_s * 1000, "evaluation_phase": 0}},
         "predictability": {"predictable": {"known_at": None}}}
    r["requirements"] = {"accuracy": acc} if acc else {}
    return r

def wl(lang, queries, sources, rows_per_second):
    return {"query_workload": {"language": lang, "repeating_queries": queries},
            "data_workload": {"arrival": {"ingesting": {"rows_per_second": float(rows_per_second)}}, "sources": sources}}

def http(urls, instances, hz):
    """Request counter per (url, instance): one series each."""
    return {"http_requests_total": {"series_count": urls * instances, "metric_type": "counter",
            "sample_interval": 1000 // hz, "distinct_values_per_label": {"url": urls, "instance": instances}}}

def http_bytes(urls, instances, hz):
    return {"http_response_bytes": {"series_count": urls * instances, "metric_type": "gauge",
            "sample_interval": 1000 // hz, "column_signs": {"value": "non_negative"},
            "distinct_values_per_label": {"url": urls, "instance": instances}}}

def dns(resolvers, hz):
    """One series per resolver; each sample's value is the numeric id of the
    client that sent one DNS query (an item stream, the ASAP value model)."""
    return {"dns_query_client": {"series_count": resolvers, "metric_type": "gauge", "sample_interval": 1000 // hz,
            "column_signs": {"value": "non_negative"}, "distinct_values_per_label": {"resolver": resolvers}}}

def clicks(**extra):
    s = {"column_signs": {"bytes": "non_negative"},
         "distinct_values_per_label": {"url": 1000000, "user_id": 5000000, "client_ip": 2000000,
                                        "region": 8, "service": 40}}
    s.update(extra)
    return {"clicks": s}

W = {}  # set -> {name: workload}
def add(s, name, w):
    W.setdefault(s, {})[name] = w

# ---------------------------------------------------------------- heavy hitters
TOPK = "topk({k}, sum by (url) (rate(http_requests_total[{w}])))"
for urls, inst, hz, win, every, eps, delta in [
        (1000, 1, 1, "5m", 10, 0.01, 0.05), (1000, 1, 1, "5m", 60, 0.01, 0.05), (1000, 1, 10, "1h", 60, 0.01, 0.05),
        (1000, 1, 10, "24h", 300, 0.002, 0.01), (2500, 1, 1, "1h", 60, 0.01, 0.05), (2500, 1, 10, "5m", 10, 0.002, 0.05),
        (5000, 1, 1, "5m", 60, 0.01, 0.05), (5000, 1, 10, "1h", 10, 0.01, 0.05), (5000, 1, 1, "24h", 300, 0.002, 0.05),
        (1000, 2, 1, "1h", 60, 0.01, 0.05), (2500, 2, 10, "5m", 10, 0.01, 0.05), (2500, 2, 1, "24h", 60, 0.002, 0.05)]:
    add("heavy-hitters", f"cmsheap-topk10__u{urls}-i{inst}-{hz}hz-{win}-every{every}s-l1eps{eps}-d{delta}",
        wl("promql", [q(TOPK.format(k=10, w=win), every, target("absolute_stream_l1", eps, delta))],
           http(urls, inst, hz), urls * inst * hz))
for urls, hz, win, every, eps, delta in [
        (100, 1, "5m", 60, 0.1, 0.1), (100, 10, "5m", 10, 0.05, 0.1), (100, 1, "1h", 60, 0.02, 0.1),
        (300, 1, "5m", 60, 0.05, 0.1), (300, 10, "1h", 60, 0.1, 0.1), (1000, 1, "5m", 60, 0.05, 0.1)]:
    add("heavy-hitters", f"csheap-topk10__u{urls}-i1-{hz}hz-{win}-every{every}s-l2eps{eps}-d{delta}",
        wl("promql", [q(TOPK.format(k=10, w=win), every, target("absolute_stream_l2", eps, delta))],
           http(urls, 1, hz), urls * hz))
# Controls: exact wins or the target cannot be certified.
add("heavy-hitters", "control-topk10-10-instances-per-url__u1000-i10-10hz-1h-every60s-l1eps0.01-d0.05",
    wl("promql", [q(TOPK.format(k=10, w="1h"), 60, target("absolute_stream_l1", 0.01, 0.05))], http(1000, 10, 10), 100000))
add("heavy-hitters", "control-topk10-sum-over-time__u1000-i1-1hz-1h-every60s-l1eps0.01-d0.05",
    wl("promql", [q("topk(10, sum by (url) (sum_over_time(http_response_bytes[1h])))", 60, target("absolute_stream_l1", 0.01, 0.05))],
       http_bytes(1000, 1, 1), 1000))
add("heavy-hitters", "control-topk10-delta-too-small__u5000-i1-1hz-1h-every60s-l1eps0.01-d0.01",
    wl("promql", [q(TOPK.format(k=10, w="1h"), 60, target("absolute_stream_l1", 0.01, 0.01))], http(5000, 1, 1), 5000))
add("heavy-hitters", "control-topk10-10000-urls__u10000-i1-1hz-1h-every60s-l1eps0.01-d0.05",
    wl("promql", [q(TOPK.format(k=10, w="1h"), 60, target("absolute_stream_l1", 0.01, 0.05))], http(10000, 1, 1), 10000))

# ---------------------------------------------------------- frequency moments
L2 = "l2_over_time(dns_query_client[{w}])"
for res, hz, win, every, eps, delta in [
        (1, 1, "1h", 60, 0.1, 0.05), (1, 10, "24h", 60, 0.05, 0.05), (1, 10, "6h", 10, 0.2, 0.05),
        (2, 10, "24h", 300, 0.1, 0.05), (4, 1, "1h", 10, 0.1, 0.05), (4, 10, "6h", 60, 0.05, 0.05),
        (4, 10, "24h", 300, 0.1, 0.1), (8, 1, "24h", 60, 0.1, 0.1), (8, 10, "24h", 10, 0.2, 0.1),
        (8, 10, "1h", 60, 0.05, 0.1), (2, 1, "5m", 60, 0.1, 0.05), (8, 10, "5m", 10, 0.1, 0.1)]:
    add("frequency-moments", f"univmon-l2__r{res}-{hz}hz-{win}-every{every}s-releps{eps}-d{delta}",
        wl("promql", [q(L2.format(w=win), every, target("relative", eps, delta))], dns(res, hz), res * hz))
add("frequency-moments", "control-l2-16-resolvers__r16-10hz-24h-every60s-releps0.1-d0.1",
    wl("promql", [q(L2.format(w="24h"), 60, target("relative", 0.1, 0.1))], dns(16, 10), 160))

# ------------------------------------------------------------------ distinct
DQ_ALL = "SELECT COUNT(DISTINCT user_id) FROM clicks WHERE ts >= now() - INTERVAL '{s} seconds'"
DQ_GRP = "SELECT {g}, COUNT(DISTINCT user_id) FROM clicks WHERE ts >= now() - INTERVAL '{s} seconds' GROUP BY {g}"
for grp, rps, secs, every, eps in [
        (None, 100000, 3600, 60, 0.05), (None, 100000, 86400, 300, 0.02), (None, 2000000, 3600, 10, 0.05),
        (None, 2000000, 86400, 60, 0.02), ("region", 100000, 3600, 60, 0.05), ("region", 2000000, 86400, 300, 0.05),
        ("region", 2000000, 3600, 60, 0.02), ("service", 100000, 86400, 300, 0.05), ("service", 2000000, 3600, 60, 0.05)]:
    text = DQ_ALL.format(s=secs) if grp is None else DQ_GRP.format(g=grp, s=secs)
    add("distinct", f"sql-distinct-users{'-by-' + grp if grp else ''}__{rps}rps-{secs}s-every{every}s-releps{eps}-d0.05",
        wl(SQL, [q(text, every, target("relative", eps, 0.05))], clicks(), rps))
for res, hz, win, every, eps in [(1, 10, "24h", 60, 0.05), (4, 10, "6h", 60, 0.05), (8, 1, "24h", 300, 0.02)]:
    add("distinct", f"promql-distinct-clients__r{res}-{hz}hz-{win}-every{every}s-releps{eps}-d0.1",
        wl("promql", [q(f"distinct_over_time(dns_query_client[{win}])", every, target("relative", eps, 0.1))], dns(res, hz), res * hz))

# --------------------------------------------------------------------- mixed
X1h = "sum by (url) (rate(http_requests_total[1h]))"
for urls in [1000, 5000]:
    add("mixed", f"topk-and-total__u{urls}-i1-1hz-1h-every60s",
        wl("promql", [q(f"topk(10, {X1h})", 60, target("absolute_stream_l1", 0.01, 0.05)),
                      q("sum(rate(http_requests_total[1h]))", 60)], http(urls, 1, 1), urls))
    add("mixed", f"topk-dashboard-and-alert__u{urls}-i1-1hz-1h-every10s-and-60s",
        wl("promql", [q(f"topk(10, {X1h})", 10, target("absolute_stream_l1", 0.01, 0.05)),
                      q(f"topk(10, {X1h})", 60, target("absolute_stream_l1", 0.002, 0.05))], http(urls, 1, 1), urls))
    add("mixed", f"topk-by-url-and-by-instance__u{urls // 10}-i10-1hz-1h-every60s",
        wl("promql", [q(f"topk(10, {X1h})", 60, target("absolute_stream_l1", 0.01, 0.05)),
                      q("topk(10, sum by (instance) (rate(http_requests_total[1h])))", 60, target("absolute_stream_l1", 0.01, 0.05))],
           http(urls // 10, 10, 1), urls))
    add("mixed", f"control-topk10-and-topk50__u{urls}-i1-1hz-1h-every60s",
        wl("promql", [q(f"topk(10, {X1h})", 60, target("absolute_stream_l1", 0.01, 0.05)),
                      q(f"topk(50, {X1h})", 60, target("absolute_stream_l1", 0.01, 0.05))], http(urls, 1, 1), urls))
for res in [1, 4, 8]:
    R = target("relative", 0.1, 0.1)
    add("mixed", f"l2-and-count__r{res}-10hz-24h-every60s",
        wl("promql", [q(L2.format(w="24h"), 60, R), q("count_over_time(dns_query_client[24h])", 60)], dns(res, 10), res * 10))
    add("mixed", f"l2-and-distinct__r{res}-10hz-24h-every60s",
        wl("promql", [q(L2.format(w="24h"), 60, R), q("distinct_over_time(dns_query_client[24h])", 60, target("relative", 0.05, 0.1))],
           dns(res, 10), res * 10))
    add("mixed", f"l2-24h-and-15m__r{res}-10hz-every60s",
        wl("promql", [q(L2.format(w="24h"), 60, R), q(L2.format(w="15m"), 60, R)], dns(res, 10), res * 10))
    add("mixed", f"repeat-rate-and-l2__r{res}-10hz-24h-every60s",
        wl("promql", [q("l2_over_time(dns_query_client[24h]) / count_over_time(dns_query_client[24h])", 60, R),
                      q(L2.format(w="24h"), 60, R)], dns(res, 10), res * 10))
src = {}
src.update(http(1000, 1, 1)); src.update(dns(4, 10))
add("mixed", "edge-dashboard__u1000-i1-1hz-r4-10hz",
    wl("promql", [q(f"topk(10, {X1h})", 60, target("absolute_stream_l1", 0.01, 0.05)),
                  q("sum(rate(http_requests_total[5m]))", 10),
                  q(L2.format(w="24h"), 60, target("relative", 0.1, 0.1)),
                  q("distinct_over_time(dns_query_client[24h])", 300, target("relative", 0.02, 0.1))], src, 1040))

# ------------------------------------------- blocked (shapes no sketch serves yet)
L1T = target("absolute_stream_l1", 0.01, 0.05)
add("blocked", "sql-topk-count-urls__1000urls-100000rps-3600s-every60s-l1eps0.01-d0.05",
    wl(SQL, [q("SELECT url, COUNT(*) AS c FROM clicks WHERE ts >= now() - INTERVAL '3600 seconds' GROUP BY url ORDER BY c DESC LIMIT 10", 60, L1T)],
       clicks(distinct_values_per_label={"url": 1000, "user_id": 5000000, "client_ip": 2000000, "region": 8, "service": 40}), 100000))
add("blocked", "sql-topk-bytes-urls__1000urls-100000rps-3600s-every60s-l1eps0.01-d0.05",
    wl(SQL, [q("SELECT url, SUM(bytes) AS b FROM clicks WHERE ts >= now() - INTERVAL '3600 seconds' GROUP BY url ORDER BY b DESC LIMIT 10", 60, L1T)],
       clicks(series_count=10000, distinct_values_per_label={"url": 1000, "user_id": 5000000, "client_ip": 2000000, "region": 8, "service": 40}), 100000))
add("blocked", "sql-l2-client-ip__100000rps-3600s-every60s-releps0.1-d0.1",
    wl(SQL, [q("SELECT SQRT(SUM(CAST(c AS DOUBLE) * CAST(c AS DOUBLE))) FROM (SELECT client_ip, COUNT(*) AS c FROM clicks WHERE ts >= now() - INTERVAL '3600 seconds' GROUP BY client_ip)", 60, target("relative", 0.1, 0.1))],
       clicks(), 100000))
add("blocked", "sql-entropy-client-ip__100000rps-3600s-every60s-abseps0.1-d0.1",
    wl(SQL, [q("SELECT -SUM(p * LN(p)) FROM (SELECT COUNT(*) * 1.0 / SUM(COUNT(*)) OVER () AS p FROM clicks WHERE ts >= now() - INTERVAL '3600 seconds' GROUP BY client_ip)", 60, target("absolute", 0.1, 0.1))],
       clicks(), 100000))
add("blocked", "promql-entropy__r4-10hz-24h-every60s-abseps0.1-d0.1",
    wl("promql", [q("entropy_over_time(dns_query_client[24h])", 60, target("absolute", 0.1, 0.1))], dns(4, 10), 40))
add("blocked", "sql-distinct-users-no-delta-hll-shape__2000000rps-86400s-every60s-releps0.02",
    wl(SQL, [q(DQ_ALL.format(s=86400), 60, {"accuracy_metric": "relative", "max_error": 0.02})], clicks(), 2000000))
add("blocked", "sql-point-count-one-url__100000rps-3600s-every60s-l1eps0.001-d0.05",
    wl(SQL, [q("SELECT COUNT(*) FROM clicks WHERE url = 'url-00042' AND ts >= now() - INTERVAL '3600 seconds'", 60, target("absolute_stream_l1", 0.001, 0.05))],
       clicks(), 100000))


SET = {
    "id": SET_ID,
    "name": "Sketch-heavy",
    "description": (
        "Workloads where a sketch family is the cheapest plan that meets the accuracy target, with controls "
        "next to them. heavy-hitters: topk(10, sum by (url) (rate(http_requests_total[W]))) over 100 to 10,000 "
        "URLs at L1- or L2-normalized targets (Count-Min or Count Sketch with a heap). frequency-moments: "
        "l2_over_time over 1 to 16 value streams (UnivMon). distinct: SQL COUNT(DISTINCT user_id) at 100,000 "
        "or 2,000,000 rows/s and PromQL distinct_over_time (KMV). mixed: several queries that could share one "
        "sketch. blocked: SQL top-k, L2, entropy, point counts and HLL-shaped distinct counts, which no sketch "
        "serves yet."),
    "source": (
        "Synthetic, written by gen.py in this directory (metric, table and label names are made up). Each "
        "workload declares only the data facts the shape implies: series count, sample interval, metric type, "
        "distinct values per grouping label and column signs; no stream norms. SQL workloads read the clicks "
        "table in tables.json. Planned with the default deployment (raw data retained) and cost model, "
        "accuracy guarantees enforced."),
    "accuracy_guarantees": "enforced",
}


def file_name(subset, name):
    return f"{subset}-{name}.json"


if __name__ == "__main__":
    for old in os.listdir(HERE):
        if old.endswith(".json"):
            os.remove(os.path.join(HERE, old))
    with open(os.path.join(HERE, "tables.json"), "w") as f:
        json.dump([CLICKS], f, indent=1)
        f.write("\n")
    entries = []
    for subset, workloads in W.items():
        for name, workload in workloads.items():
            with open(os.path.join(HERE, file_name(subset, name)), "w") as f:
                json.dump(workload, f, indent=1)
                f.write("\n")
            entries.append({"id": f"{subset}-{name}", "file": f"{SET_ID}/{file_name(subset, name)}"})
    with open(INDEX) as f:
        index = json.load(f)
    index["sets"] = [s for s in index["sets"] if s["id"] != SET_ID] + [dict(SET, workloads=entries)]
    with open(INDEX, "w") as f:
        json.dump(index, f, indent=2, ensure_ascii=False)
        f.write("\n")
    print(len(entries), "workloads in", {s: len(v) for s, v in W.items()})
