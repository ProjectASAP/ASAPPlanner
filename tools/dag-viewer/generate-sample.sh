#!/usr/bin/env bash
# Regenerates tools/dag-viewer/dag.example.json, the stage_pipeline document
# the viewer opens with: the Query planner's default workload (SQL over its
# default tables, and PromQL). Run from anywhere in the repo; edit the
# --table/--sql/--promql lines below to try your own queries.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."

# --epsilon asks for an approximate accuracy target instead of the default
# Exact, so Stage 1 has sketch candidates to offer. The document lists the
# first 4 candidates and the selected one (display only: Stage 3 still
# selects among all of them).
cargo run -q -p asap-devtools --bin stage_pipeline -- \
  --epsilon 0.01 --max-displayed 4 \
  --table '{"name":"metrics","columns":[{"name":"ts","type":"timestamp","nullable":false},{"name":"service","type":"utf8","nullable":false},{"name":"region","type":"utf8","nullable":false},{"name":"latency","type":"float64","nullable":false},{"name":"bytes","type":"int64","nullable":false}],"time_index":0}' \
  --table '{"name":"hosts","columns":[{"name":"service","type":"utf8","nullable":false},{"name":"region","type":"utf8","nullable":false}]}' \
  --sql "SELECT service, COUNT(*) FROM metrics GROUP BY service" --name count_by_service \
  --sql "SELECT service, AVG(latency) FROM metrics GROUP BY service" --name avg_latency \
  --promql "topk(5, rate(http_requests_total[5m]))" --name q3 \
  --sql "SELECT metrics.service, COUNT(*) FROM metrics JOIN hosts ON metrics.service = hosts.service GROUP BY metrics.service" --name q6 \
  --promql "topk(10, rate(http_requests_total[5m]))" --name q4 \
  --out tools/dag-viewer/dag.example.json

echo "wrote tools/dag-viewer/dag.example.json"
