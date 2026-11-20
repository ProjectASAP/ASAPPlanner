#!/usr/bin/env bash
# usage: cluster.sh deploy|launch|status|collect|stop <bench_dir> [probe_dir]
#
# deploy:  copy planner_bench, the capabilities file, runner.py and each
#          node's shard (with only its workloads) to /scratch/bench on node<i>.
# launch:  start the runner detached (setsid nohup) on every node; it keeps
#          running after this shell and SSH session end. Node 0 runs the
#          probe jobs first when deployed with a probe_dir.
# status:  results so far per node, by outcome.
# collect: pull every node's results.jsonl and runner.log into <bench_dir>/results/.
# stop:    kill the runner and any planner_bench on every node.
set -euo pipefail
cmd=$1
bench=$2
probe=${3:-}
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
bin=${PLANNER_BENCH_BIN:-$repo/target/release/planner_bench}
remote=/scratch/bench
host() { echo "milindsr@node$1.scratch1.cloudmigration-PG0.apt.emulab.net"; }
ssh_() { ssh -o StrictHostKeyChecking=no -o BatchMode=yes "$@"; }

case $cmd in
deploy)
  for i in 0 1 2 3; do
    stage=$(mktemp -d)
    mkdir -p "$stage/workloads" "$stage/catalogs"
    cp "$bin" "$here/runner.py" "$repo/tools/dag-viewer/deployment-capabilities/every-summary-family.json" "$stage/"
    cp "$bench/shard$i.jsonl" "$stage/shard.jsonl"
    python3 - "$bench" "$stage" "$stage/shard.jsonl" <<'EOF'
import json, shutil, sys
src, dst, shard = sys.argv[1:]
for line in open(shard):
    j = json.loads(line)["id"]
    shutil.copy(f"{src}/workloads/{j}.json", f"{dst}/workloads/")
    shutil.copy(f"{src}/catalogs/{j}.json", f"{dst}/catalogs/")
EOF
    if [[ $i == 0 && -n $probe ]]; then
      cp "$probe/shard0.jsonl" "$stage/probe.jsonl"
      cp "$probe"/workloads/* "$stage/workloads/"
      cp "$probe"/catalogs/* "$stage/catalogs/"
    fi
    ssh_ "$(host $i)" "mkdir -p $remote && python3 --version"
    tar -C "$stage" -czf - . | ssh_ "$(host $i)" "tar -C $remote -xzf -"
    rm -rf "$stage"
    echo "node$i deployed: $(wc -l < "$bench/shard$i.jsonl") jobs"
  done
  ;;
launch)
  for i in 0 1 2 3; do
    ssh_ "$(host $i)" "cd $remote && if pgrep -f '^python3 runner.py' >/dev/null; then echo 'node$i: already running'; exit 0; fi; \
      setsid nohup bash -c '[ -f probe.jsonl ] && python3 runner.py $remote probe.jsonl probe_results.jsonl; \
      python3 runner.py $remote shard.jsonl results.jsonl' > runner.log 2>&1 < /dev/null & echo node$i launched"
  done
  ;;
status)
  for i in 0 1 2 3; do
    echo "== node$i"
    ssh_ "$(host $i)" "cd $remote && uptime && free -g | sed -n 2p && wc -l < shard.jsonl && \
      for f in probe_results.jsonl results.jsonl; do [ -f \$f ] && echo \$f && python3 -c \"
import json,collections,sys
print(dict(collections.Counter(json.loads(l)['outcome'] for l in open(sys.argv[1]))))\" \$f; done; tail -2 runner.log" || true
  done
  ;;
collect)
  mkdir -p "$bench/results"
  for i in 0 1 2 3; do
    for f in results.jsonl probe_results.jsonl runner.log; do
      scp -q -o StrictHostKeyChecking=no "$(host $i):$remote/$f" "$bench/results/node${i}_$f" 2>/dev/null || true
    done
  done
  ls -la "$bench/results"
  ;;
stop)
  for i in 0 1 2 3; do
    ssh_ "$(host $i)" "pkill -f '^python3 runner.py'; pkill -f '^/scratch/bench/planner_bench'; echo node$i stopped" || true
  done
  ;;
*) echo "unknown command $cmd" >&2; exit 2 ;;
esac
