#!/usr/bin/env bash
# Regenerates tools/dag-viewer/dag.example.json, the stage_pipeline document
# the viewer opens with: the Query planner's default SQL workload over its
# default tables. Run from anywhere in the repo; edit the --table/--sql lines
# below to try your own queries (or use --promql; a run is one language).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."

# --epsilon asks for an approximate accuracy target instead of the default
# Exact, so Stage 1 has sketch candidates to offer. The viewer shows the
# selected plan, so the document lists only that candidate (display only:
# Stage 3 still selects among all of them).
cargo run -q -p asap-devtools --bin stage_pipeline -- \
  --epsilon 0.01 --max-displayed 0 \
  --table '{"name":"metrics","columns":[{"name":"ts","type":"timestamp","nullable":false},{"name":"service","type":"utf8","nullable":false},{"name":"region","type":"utf8","nullable":false},{"name":"latency","type":"float64","nullable":false},{"name":"bytes","type":"int64","nullable":false}],"time_index":0}' \
  --table '{"name":"hosts","columns":[{"name":"service","type":"utf8","nullable":false},{"name":"region","type":"utf8","nullable":false}]}' \
  --sql "SELECT service, COUNT(*) FROM metrics GROUP BY service" \
  --sql "SELECT service, AVG(latency) FROM metrics GROUP BY service" \
  --sql "SELECT metrics.service, COUNT(*) FROM metrics JOIN hosts ON metrics.service = hosts.service GROUP BY metrics.service" \
  --out tools/dag-viewer/dag.example.json

echo "wrote tools/dag-viewer/dag.example.json"
