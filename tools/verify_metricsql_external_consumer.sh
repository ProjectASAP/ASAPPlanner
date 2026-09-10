#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
consumer_dir=$(mktemp -d "${TMPDIR:-/tmp}/asap-metricsql-consumer.XXXXXX")
mkdir -p "$consumer_dir/src"

cat > "$consumer_dir/Cargo.toml" <<EOF
[package]
name = "asap-metricsql-external-consumer"
version = "0.1.0"
edition = "2021"

[dependencies]
asap-frontend-metricsql = { path = "$repo_root/crates/frontend-metricsql" }
asap-types = { path = "$repo_root/crates/types" }
EOF

cat > "$consumer_dir/src/main.rs" <<'EOF'
fn main() {
    asap_frontend_metricsql::lower_metricsql(
        "sum(rate(requests_total[5m]))",
        asap_types::types::AccuracyTarget::Exact,
    )
    .expect("a downstream stable crate can parse and lower MetricsQL");
}
EOF

CARGO_TARGET_DIR="$consumer_dir/target" cargo +stable check --manifest-path "$consumer_dir/Cargo.toml"
