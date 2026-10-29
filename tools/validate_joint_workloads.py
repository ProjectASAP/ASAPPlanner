#!/usr/bin/env python3
"""Reproducible Linux workload acceptance; costs/latencies are model estimates.

Build stage_pipeline, copy it to a fixed path, then pass that path here. The
binary SHA records exactly what ran even if another build updates the workspace.
Planning uses a cooperative 30s budget and an external 90s process deadline.
"""
import argparse
import concurrent.futures
import hashlib
import json
import math
import pathlib
import re
import subprocess
import time

repo = pathlib.Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description="Validate the viewer's 30 mixed ASAPQuery workloads with a fixed planner binary.")
parser.add_argument('output', type=pathlib.Path, help='directory for reports and full plan artifacts')
parser.add_argument('binary', type=pathlib.Path, help='frozen stage_pipeline executable')
args = parser.parse_args()
output_dir = args.output.resolve()
output_dir.mkdir(parents=True, exist_ok=True)
binary = args.binary.resolve()
digest = hashlib.sha256()
with binary.open('rb') as executable:
    for chunk in iter(lambda: executable.read(1024 * 1024), b''):
        digest.update(chunk)
binary_sha = digest.hexdigest()
manifest = json.loads((repo / 'tools/dag-viewer/examples.json').read_text())
workloads = []
for entry in manifest:
    if entry.get('workload', '').startswith('workloads/random-mixed-seed-862'):
        folder = (repo / 'tools/dag-viewer' / entry['workload']).parent
        for path in sorted(folder.glob('workload-*.json')):
            item = dict(entry)
            item['workload'] = str(path.relative_to(repo / 'tools/dag-viewer'))
            item['id'] = folder.name + '-' + path.stem
            workloads.append(item)
assert len(workloads) == 30, len(workloads)

def run(entry):
    name = entry['id']
    plan_path = output_dir / (name + '.json')
    timing = output_dir / (name + '.time')
    log = output_dir / (name + '.log')
    plan_path.unlink(missing_ok=True)
    command = [
        '/usr/bin/time', '-v', '-o', str(timing),
        'timeout', '90s', str(binary),
        '--workload', str(repo / 'tools/dag-viewer' / entry['workload']),
        '--milp-time-limit-s', '30',
        '--cost-model', entry['cost_model'], '--enforce-accuracy-guarantees',
        '--deployment-capabilities', json.dumps(entry['deployment_capabilities']),
        '--out', str(plan_path),
    ]
    start = time.monotonic()
    with log.open('w') as f:
        exit_code = subprocess.run(command, stdout=f, stderr=subprocess.STDOUT, cwd=repo).returncode
    result = {'workload': name, 'exit_code': exit_code, 'wall_seconds': time.monotonic() - start, 'artifact': plan_path.name}
    timing_output = timing.read_text() if timing.exists() else ''
    rss_match = re.search(r'Maximum resident set size \(kbytes\): (\d+)', timing_output)
    result['peak_planner_rss_kib'] = int(rss_match.group(1)) if rss_match else None
    if plan_path.exists():
        document = json.loads(plan_path.read_text())
        selection = document['stage3_plan_selection']
        selected = selection.get('selected')
        result['selected'] = selected
        result['error'] = selection.get('error')
        result['materialized_views_error'] = selection.get('materialized_views_error')
        report = selection.get('search', {}).get('candidate_connections', {})
        result['search'] = report
        if selected:
            accepted = selection.get('accepted', {}).get(selected, [])
            result['accepted_queries'] = len(accepted)
            result['guarantees_enforced'] = all(
                not query.get('accepted_regardless_of_guarantee', True)
                and query.get('guarantee_check_rejection') is None
                for query in accepted
            )
            costs = selection['costs'][selected]
            result['modeled_cost_per_second'] = costs.get('cost_per_second')
            result['modeled_usage'] = costs.get('usage')
        result['artifact_sha256'] = hashlib.sha256(plan_path.read_bytes()).hexdigest()
    workload = json.loads((repo / 'tools/dag-viewer' / entry['workload']).read_text())['query_workload']
    result['expected_queries'] = len(workload.get('query_batch') or []) + len(workload.get('repeating_queries') or [])
    modeled_cost = result.get('modeled_cost_per_second')
    bound = result.get('search', {}).get('lower_bound')
    result['cost_certificate_valid'] = bool(
        isinstance(modeled_cost, (int, float)) and math.isfinite(modeled_cost) and modeled_cost >= 0
        and result.get('search', {}).get('status') != 'invalid_cost_certificate'
        and (bound is None or (
            isinstance(bound, (int, float)) and math.isfinite(bound)
            and bound <= modeled_cost + 1e-8 * max(abs(modeled_cost), 1e-12)
        ))
    )
    result['passed'] = bool(
        exit_code == 0 and result.get('selected')
        and result.get('guarantees_enforced') and result['cost_certificate_valid']
        and result.get('accepted_queries') == result['expected_queries']
        and not result.get('error') and not result.get('materialized_views_error')
    )
    print(json.dumps({k: result.get(k) for k in ['workload', 'wall_seconds', 'selected', 'accepted_queries', 'modeled_cost_per_second', 'error']}), flush=True)
    return result
results = []
with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
    for result in pool.map(run, workloads):
        results.append(result)
        (output_dir / 'results.json').write_text(json.dumps({'binary_sha256': binary_sha, 'budget_seconds': 30, 'parallel_workloads': 2, 'results': results}, indent=2) + '\n')
raise SystemExit(0 if all(result['passed'] for result in results) else 1)
