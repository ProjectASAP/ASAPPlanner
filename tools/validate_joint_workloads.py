#!/usr/bin/env python3
"""Reproducible Linux workload acceptance; costs/latencies are model estimates.

Build asap_planner, copy it to a fixed path, then pass that path here. The
binary SHA records exactly what ran even if another build updates the workspace.
Each workload is planned through Stage 3 as its workload set says (deployment
capabilities, cost model, accuracy guarantees); each planning process is
stopped after 90 s.
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
parser.add_argument('binary', type=pathlib.Path, help='frozen asap_planner executable')
args = parser.parse_args()
output_dir = args.output.resolve()
output_dir.mkdir(parents=True, exist_ok=True)
binary = args.binary.resolve()
digest = hashlib.sha256()
with binary.open('rb') as executable:
    for chunk in iter(lambda: executable.read(1024 * 1024), b''):
        digest.update(chunk)
binary_sha = digest.hexdigest()
viewer = repo / 'tools/dag-viewer'
index = json.loads((viewer / 'workloads/index.json').read_text())
workloads = []
for workload_set in index['sets']:
    if workload_set['id'].startswith('random-mixed-seed-862'):
        for entry in workload_set['workloads']:
            path = viewer / 'workloads' / entry['file']
            workloads.append({
                'id': path.parent.name + '-' + path.stem,
                'workload': str(path.relative_to(viewer)),
                'deployment_capabilities': workload_set['deployment_capabilities'],
                'cost_model': workload_set['cost_model'],
                'accuracy_guarantees': workload_set['accuracy_guarantees'],
            })
assert len(workloads) == 30, len(workloads)

def run(entry):
    name = entry['id']
    plan_path = output_dir / (name + '.json')
    timing = output_dir / (name + '.time')
    log = output_dir / (name + '.log')
    capabilities = output_dir / (name + '.capabilities.json')
    capabilities.write_text(json.dumps(entry['deployment_capabilities']))
    plan_path.unlink(missing_ok=True)
    command = [
        '/usr/bin/time', '-v', '-o', str(timing),
        'timeout', '90s', str(binary),
        '--workload', str(viewer / entry['workload']),
        '--run-until', 'stage3', '--format', 'json',
        '--cost-inputs', str(viewer / 'cost-inputs' / (entry['cost_model'] + '.json')),
        '--deployment-capabilities', str(capabilities),
        '--out', str(plan_path),
    ]
    if entry['accuracy_guarantees'] != 'enforced':
        command.append('--ignore-accuracy-guarantees')
    start = time.monotonic()
    with log.open('w') as f:
        exit_code = subprocess.run(command, stdout=f, stderr=subprocess.STDOUT, cwd=repo).returncode
    result = {'workload': name, 'exit_code': exit_code, 'wall_seconds': time.monotonic() - start, 'artifact': plan_path.name}
    timing_output = timing.read_text() if timing.exists() else ''
    rss_match = re.search(r'Maximum resident set size \(kbytes\): (\d+)', timing_output)
    result['peak_planner_rss_kib'] = int(rss_match.group(1)) if rss_match else None
    if plan_path.exists():
        document = json.loads(plan_path.read_text())
        # Stage 3's `Selection`: the selected plan is P1, with its cost.
        selection = document['stage3_plan_selection'] or {}  # null when Pass 1 rejected a query
        selected = 'P1' if selection.get('cost') else None
        result['selected'] = selected
        result['error'] = selection.get('error') or (document.get('stage1_logical_asap_optimization') or {}).get('error')
        result['materialized_views_error'] = selection.get('materialized_views_error')
        report = selection.get('search_report') or {}
        result['search'] = report
        if selected:
            accepted = selection.get('accepted_accuracy_per_query') or []
            result['accepted_queries'] = len(accepted)
            result['guarantees_enforced'] = all(
                not query.get('accepted_regardless_of_guarantee', True)
                and query.get('guarantee_check_rejection') is None
                for query in accepted
            )
            result['modeled_cost_per_second'] = selection['cost'].get('cost_per_second')
            result['modeled_usage'] = selection['cost'].get('usage')
        result['artifact_sha256'] = hashlib.sha256(plan_path.read_bytes()).hexdigest()
    workload = json.loads((viewer / entry['workload']).read_text())['query_workload']
    result['expected_queries'] = len(workload.get('query_batch') or []) + len(workload.get('repeating_queries') or [])
    modeled_cost = result.get('modeled_cost_per_second')
    bound = result.get('search', {}).get('cost_lower_bound_per_second')
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
        (output_dir / 'results.json').write_text(json.dumps({'binary_sha256': binary_sha, 'deadline_seconds': 90, 'parallel_workloads': 2, 'results': results}, indent=2) + '\n')
raise SystemExit(0 if all(result['passed'] for result in results) else 1)
