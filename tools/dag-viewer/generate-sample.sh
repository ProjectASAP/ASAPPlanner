#!/usr/bin/env bash
# Regenerates tools/dag-viewer/dag.example.json from real example queries.
# Run from anywhere in the repo; edit the --sql/--promql lines below to try
# your own queries instead.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."

# This checked-in sample intentionally omits deployment-owned physical
# evidence. The exporter therefore emits the raw DAG only; costed post-ASAP
# examples must pass a complete --planner-cost-json document and never fall
# back to structural node counts.
cargo run -p asap-devtools --bin dag_export -- \
  --post-asap --progress \
  --epsilon 0.01 \
  --sql "SELECT service, COUNT(*) FROM metrics GROUP BY service" --name q1 \
  --sql "SELECT service, AVG(latency) FROM metrics GROUP BY service" --name q2 \
  --promql "topk(5, rate(http_requests_total[5m]))" --name q3 \
  --promql "topk(10, rate(http_requests_total[5m]))" --name q4 \
  --sql "SELECT metrics.service, COUNT(*) FROM metrics JOIN hosts ON metrics.service = hosts.service GROUP BY metrics.service" --name q6 \
  > tools/dag-viewer/dag.example.json

echo "wrote tools/dag-viewer/dag.example.json"
