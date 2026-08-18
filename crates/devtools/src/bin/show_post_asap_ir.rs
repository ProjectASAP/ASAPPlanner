// cargo run -p asap-devtools --bin show_post_asap_ir -- queries.txt
// (or pipe via stdin: cargo run -p asap-devtools --bin show_post_asap_ir < queries.txt)
//
// Lowers a batch of ad-hoc SQL/PromQL queries to pre-ASAP IR, then runs the
// `asap-aware-mapping` pre-ASAP → post-ASAP binding pass and prints the
// resulting **post-ASAP IR** (the sketch-bound IR: `SummaryExpr`/`SummaryNode`
// — the concrete `SummaryKind`/`SummaryParams` committed per aggregate, or
// `Logical` for whatever the pass left untouched). See `show_pre_asap_ir`
// for the sketch-agnostic IR one layer upstream.
//
// File format: one query per line, prefixed with "sql>" or "promql>".
// Blank lines and lines starting with '#' are ignored.
//
//   sql> SELECT service, COUNT(*) FROM metrics GROUP BY service
//   promql> topk(5, rate(http_requests_total[5m]))
//
// Every query lowers at ACCURACY (ε = 0.01 below) rather than `Exact` — an
// exact target only ever exercises the mergeable-accumulator arm of the
// boundary decision, never a real sketch. SQL queries run against a fixed
// `metrics(ts, service, region, latency, bytes)` catalog — the same table
// used in cross_language.rs and topk_ir.rs.

use asap_aware_mapping::implement_tree;
use asap_devtools::{lower_promql, lower_sql, SqlCatalog};
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::types::AccuracyTarget;
use std::io::Read;

const ACCURACY: AccuracyTarget = AccuracyTarget::Epsilon(0.01);

fn col(name: &str, dtype: DataType) -> Column {
    Column::new(name, dtype, false)
}

fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "metrics",
        Schema::with_time_index(
            vec![
                col("ts", DataType::Timestamp),
                col("service", DataType::Utf8),
                col("region", DataType::Utf8),
                col("latency", DataType::Float64),
                col("bytes", DataType::Int64),
            ],
            0,
            vec![],
        ),
    )
}

#[tokio::main]
async fn main() {
    let input = match std::env::args().nth(1) {
        Some(path) => {
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("failed to read {path}: {e}"))
        }
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .expect("failed to read stdin");
            buf
        }
    };

    let catalog = catalog();
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        println!("━━━ {line} ━━━");
        let l3 = if let Some(q) = line.strip_prefix("sql>") {
            lower_sql(q.trim(), &catalog, ACCURACY.clone())
                .await
                .map_err(|e| e.to_string())
        } else if let Some(q) = line.strip_prefix("promql>") {
            lower_promql(q.trim(), ACCURACY.clone()).map_err(|e| e.to_string())
        } else {
            println!("ERR: line must start with 'sql>' or 'promql>'");
            println!();
            continue;
        };
        match l3.and_then(|expr| implement_tree(&expr).map_err(|e| e.to_string())) {
            Ok(l4) => println!("{:#?}", l4.expr),
            Err(e) => println!("ERR: {e}"),
        }
        println!();
    }
}
