// cargo run -p asap-devtools --bin show_post_asap_ir -- queries.txt
// (or pipe via stdin: cargo run -p asap-devtools --bin show_post_asap_ir < queries.txt)
//
// Lowers a batch of ad-hoc SQL/PromQL queries to pre-ASAP IR, then runs the
// `asap-aware-mapping` pre-ASAP → post-ASAP binding pass and prints the
// resulting **post-ASAP IR** (the sketch-bound IR: `SummaryExpr`/`SummaryNode`
// — the concrete `SummaryKind`/`SummaryParams` committed per aggregate, or
// `KeepPreAsap` for whatever the pass left untouched). See `show_pre_asap_ir`
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

use asap_aware_mapping::replacement::keep_pre_asap;
use asap_aware_mapping::{
    Replacement, ReplacementStrategy, ReplacementSubDAG, SketchAlgorithmStrategy, TargetSubDAG,
};
use asap_devtools::{lower_promql_with_data_ingestion_interval, lower_sql, SqlCatalog};
use asap_types::pre_asap::query_expr::QueryExpr;
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::types::AccuracyTarget;
use std::io::Read;
use std::rc::Rc;

const ACCURACY: AccuracyTarget = AccuracyTarget::Epsilon(0.01);

/// `SketchAlgorithmStrategy::replacements` returns every candidate. This
/// debug tool prints all of them so callers can inspect the planner's choices.
/// If the strategy has none, preserve the single pre-ASAP fallback output.
fn bind_all(expr: &QueryExpr) -> Result<Vec<Rc<asap_types::post_asap::SummaryNode>>, String> {
    let root = Rc::new(expr.clone());
    let target = TargetSubDAG::new(&root);
    let candidates = SketchAlgorithmStrategy::default_cost_model()
        .replacements(&target)
        .into_iter()
        .filter_map(|candidate| match candidate {
            ReplacementSubDAG {
                replacement: Replacement::Summary(node),
                ..
            } => Some(node),
            _ => None,
        })
        .collect::<Vec<_>>();

    if candidates.is_empty() {
        Ok(vec![keep_pre_asap(&root).map_err(|e| e.to_string())?])
    } else {
        Ok(candidates)
    }
}

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
    let mut args = std::env::args().skip(1);
    assert_eq!(
        args.next().as_deref(),
        Some("--data-ingestion-interval-ms"),
        "usage: show_post_asap_ir --data-ingestion-interval-ms <ms> [queries.txt]"
    );
    let interval_ms = args
        .next()
        .expect("missing interval")
        .parse()
        .expect("interval must be an unsigned integer");
    let input = match args.next() {
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
            lower_promql_with_data_ingestion_interval(q.trim(), ACCURACY.clone(), interval_ms)
                .map_err(|e| e.to_string())
        } else {
            println!("ERR: line must start with 'sql>' or 'promql>'");
            println!();
            continue;
        };
        match l3.and_then(|expr| bind_all(&expr)) {
            Ok(candidates) => {
                for (index, candidate) in candidates.iter().enumerate() {
                    println!("--- candidate {} ---", index + 1);
                    println!("{:#?}", candidate.expr);
                }
            }
            Err(e) => println!("ERR: {e}"),
        }
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_all_returns_every_sketch_candidate() {
        let expr = lower_promql_with_data_ingestion_interval(
            "quantile(0.99, rate(http_requests_total[5m]))",
            ACCURACY.clone(),
            1_000,
        )
        .expect("query lowers to pre-ASAP IR");
        let root = Rc::new(expr.clone());
        let expected = SketchAlgorithmStrategy::default_cost_model()
            .replacements(&TargetSubDAG::new(&root))
            .len();

        assert!(expected > 1, "fixture exposes alternative bindings");
        assert_eq!(bind_all(&expr).expect("binding succeeds").len(), expected);
    }
}
