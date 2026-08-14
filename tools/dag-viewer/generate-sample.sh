#!/usr/bin/env bash
# Regenerates tools/dag-viewer/dag.json from a fixed set of example queries.
# Run from anywhere in the repo; edit the --sql/--promql lines below to try
# your own queries instead.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."

cargo run -p asap-lower --example dag_export -- \
  --sql "SELECT service, COUNT(*) FROM metrics GROUP BY service" --name q1 \
  --sql "SELECT service, AVG(latency) FROM metrics GROUP BY service" --name q2 \
  --promql "topk(5, rate(http_requests_total[5m]))" --name q3 \
  > tools/dag-viewer/dag.json

echo "wrote tools/dag-viewer/dag.json"
