// cargo run -p asap-lower --bin show_ir -- queries.txt
// (or pipe via stdin: cargo run -p asap-lower --bin show_ir < queries.txt)
//
// Lowers a batch of ad-hoc SQL/PromQL queries to L3 IR and prints them.
// File format: one query per line, prefixed with "sql>" or "promql>".
// Blank lines and lines starting with '#' are ignored.
//
//   sql> SELECT service, COUNT(*) FROM metrics GROUP BY service
//   promql> topk(5, rate(http_requests_total[5m]))
//
// SQL queries run against a fixed `metrics(ts, service, region, latency,
// bytes)` catalog — the same table used in cross_language.rs and topk_ir.rs.

use asap_ir::intent_algebra::schema::{Column, DataType, Schema};
use asap_ir::types::AccuracyTarget;
use asap_lower::{lower_promql, lower_sql, SqlCatalog};
use std::io::Read;

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
        if let Some(q) = line.strip_prefix("sql>") {
            match lower_sql(q.trim(), &catalog, AccuracyTarget::Exact).await {
                Ok(qe) => println!("{qe:#?}"),
                Err(e) => println!("ERR: {e}"),
            }
        } else if let Some(q) = line.strip_prefix("promql>") {
            match lower_promql(q.trim(), AccuracyTarget::Exact) {
                Ok(qe) => println!("{qe:#?}"),
                Err(e) => println!("ERR: {e}"),
            }
        } else {
            println!("ERR: line must start with 'sql>' or 'promql>'");
        }
        println!();
    }
}
