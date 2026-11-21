#!/usr/bin/env python3
"""Run one node's shard of planner_bench jobs.

usage: runner.py <bench_dir> <shard.jsonl> <results.jsonl>
    [--workers 4] [--mem-gb 3.2] [--timeout-s 1200]
    [--solo-mem-gb 14] [--solo-timeout-s 3600]

Tier 1 runs jobs `--workers` at a time, each killed above `--mem-gb` RSS or
after `--timeout-s`. Jobs that hit a limit, and jobs of a series whose
smaller rank already hit one, go to tier 2: one at a time with the solo
limits. A tier-2 failure skips the series' larger ranks. Every job writes
one result line (appended, flushed), so a rerun resumes: jobs whose id is
already in the results with a final outcome are skipped.

The RSS limit is a watchdog polling /proc/<pid>/status every 50 ms, not a
cgroup: a spike shorter than that can overshoot, so the tier-1 caps leave
headroom under the node's 15 GB.
"""
import argparse
import json
import os
import subprocess
import threading
import time

BIN = "planner_bench"
EVERY_CAPS = "every-summary-family.json"


def rss_bytes(pid):
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) * 1024
    except OSError:
        pass
    return 0


def run_job(bench_dir, job, mem_bytes, timeout_s):
    catalog = json.load(open(f"{bench_dir}/catalogs/{job['id']}.json"))
    cmd = [f"{bench_dir}/{BIN}", "--workload", f"{bench_dir}/workloads/{job['id']}.json"]
    for t in catalog:
        cmd += ["--sql-catalog-table", t]
    if job["config"]["capabilities"] == "every":
        cmd += ["--deployment-capabilities", f"{bench_dir}/{EVERY_CAPS}"]
    if job["config"]["accuracy"] == "ignore":
        cmd.append("--ignore-accuracy-guarantees")
    started = time.monotonic()
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    peak, killed = 0, None
    while proc.poll() is None:
        rss = rss_bytes(proc.pid)
        peak = max(peak, rss)
        if rss > mem_bytes:
            killed = "memory"
        elif time.monotonic() - started > timeout_s:
            killed = "timeout"
        if killed:
            proc.kill()
            break
        time.sleep(0.05)
    out, err = proc.communicate()
    wall = time.monotonic() - started
    result = {"id": job["id"], "series": job["series"], "rank": job["rank"], "seed": job["seed"],
              "config": job["config"], "generated_queries": job["generated_queries"],
              "wall_s": wall, "watchdog_peak_rss_bytes": peak, "exit_code": proc.returncode,
              "mem_limit_bytes": mem_bytes, "timeout_s": timeout_s}
    if killed:
        result["outcome"] = f"killed_{killed}"
    elif proc.returncode != 0:
        result["outcome"] = "crashed"
        result["stderr"] = err[-4000:]
    else:
        try:
            result["bench"] = json.loads(out.strip().splitlines()[-1])
            result["outcome"] = result["bench"]["status"]
        except (ValueError, IndexError):
            result["outcome"] = "bad_output"
            result["stderr"] = err[-4000:]
    return result


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("bench_dir")
    ap.add_argument("shard")
    ap.add_argument("results")
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--mem-gb", type=float, default=3.2)
    ap.add_argument("--timeout-s", type=float, default=1200)
    ap.add_argument("--solo-mem-gb", type=float, default=14)
    ap.add_argument("--solo-timeout-s", type=float, default=3600)
    args = ap.parse_args()

    jobs = [json.loads(l) for l in open(args.shard)]
    jobs.sort(key=lambda j: (j["rank"], j["series"], j["seed"]))
    done = {}
    if os.path.exists(args.results):
        for line in open(args.results):
            r = json.loads(line)
            done[r["id"]] = r
    lock = threading.Lock()
    tier1_failed = {}  # series -> smallest failing rank in tier 1
    solo_failed = {}   # series -> smallest failing rank in tier 2
    for r in done.values():
        if r.get("tier") == 2 and r["outcome"].startswith("killed"):
            solo_failed[r["series"]] = min(r["rank"], solo_failed.get(r["series"], 1 << 60))
        if r.get("tier") == 1 and r["outcome"].startswith("killed"):
            tier1_failed[r["series"]] = min(r["rank"], tier1_failed.get(r["series"], 1 << 60))
    out = open(args.results, "a")

    def record(result):
        with lock:
            out.write(json.dumps(result) + "\n")
            out.flush()

    def final(job_id):
        r = done.get(job_id)
        return r is not None and not (r.get("tier") == 1 and r["outcome"].startswith("killed"))

    def past(failed, job):
        return job["rank"] > failed.get(job["series"], 1 << 60)

    # Tier 1.
    queue = [j for j in jobs if not final(j["id"]) and j["id"] not in done]
    deferred = [j for j in jobs if j["id"] in done and not final(j["id"])]

    def worker():
        while True:
            with lock:
                if not queue:
                    return
                job = queue.pop(0)
                if past(tier1_failed, job):
                    deferred.append(job)
                    continue
            r = run_job(args.bench_dir, job, args.mem_gb * 2**30, args.timeout_s)
            r["tier"] = 1
            record(r)
            if r["outcome"].startswith("killed"):
                with lock:
                    deferred.append(job)
                    tier1_failed[job["series"]] = min(job["rank"],
                                                      tier1_failed.get(job["series"], 1 << 60))

    threads = [threading.Thread(target=worker) for _ in range(args.workers)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    # Tier 2: one at a time, ascending rank so a failure skips the rest.
    deferred.sort(key=lambda j: (j["rank"], j["series"], j["seed"]))
    for job in deferred:
        if past(solo_failed, job):
            record({"id": job["id"], "series": job["series"], "rank": job["rank"],
                    "seed": job["seed"], "config": job["config"],
                    "generated_queries": job["generated_queries"],
                    "tier": 2, "outcome": "skipped_after_smaller_failed"})
            continue
        r = run_job(args.bench_dir, job, args.solo_mem_gb * 2**30, args.solo_timeout_s)
        r["tier"] = 2
        record(r)
        if r["outcome"].startswith("killed"):
            solo_failed[job["series"]] = min(job["rank"], solo_failed.get(job["series"], 1 << 60))
    print("shard done")


if __name__ == "__main__":
    main()
