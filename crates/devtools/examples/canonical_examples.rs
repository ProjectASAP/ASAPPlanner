// cargo run -p asap-lower --example canonical_examples
//
// One-off: pretty-print the QueryExpr for one canonical query per variant,
// plus custom Join/SetOp/Dedup/CTE probes, to eyeball the actual shape.

use asap_devtools::lower_promql;
use asap_frontend_sql::{lower_sql_dialect, SqlCatalog};
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::types::AccuracyTarget;
use asap_types::workload::SqlDialect;

fn col(name: &str, dtype: DataType) -> Column {
    Column::new(name, dtype, false)
}

fn packets_catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "packets",
        Schema::new(vec![
            col("srcip", DataType::Utf8),
            col("dstip", DataType::Utf8),
            col("srcport", DataType::Int64),
            col("dstport", DataType::Int64),
            col("proto", DataType::Utf8),
            col("time", DataType::Float64),
            col("pkt_len", DataType::Int64),
        ]),
    )
}

fn bgp_catalog() -> SqlCatalog {
    let updates = Schema::new(vec![
        col("timestamp", DataType::Timestamp),
        col("peer_asn", DataType::Int64),
        col("prefix", DataType::Utf8),
    ]);
    let rib = Schema::new(vec![
        col("snapshot_ts", DataType::Timestamp),
        col("collector", DataType::Utf8),
        col("prefix", DataType::Utf8),
    ]);
    SqlCatalog::new()
        .with_table("bgp_updates", updates)
        .with_table("bgp_rib_state", rib)
}

#[tokio::main]
async fn main() {
    let promql_examples: &[(&str, &str)] = &[
        ("Scan", "up"),
        ("BinaryOp + PromqlScalarBridge", "up > 1"),
        ("QueryTimestamp", "time()"),
        ("Aggregate", "sum(up)"),
        (
            "PromqlRelabel",
            r#"label_replace(up, "foo", "$1", "bar", "(.*)")"#,
        ),
        ("PromqlSeriesSample", "limitk(3, up)"),
        ("PromqlInfoEnrich", "info(up)"),
        ("Sort + Limit (topk)", "topk(3, up)"),
        ("PromqlSubquery", "avg_over_time(up[5m:1m])"),
        ("TimeRange", "rate(http_requests_total[5m])"),
        ("TimeShift", "up offset 5m"),
        (
            "Concat",
            r#"histogram_quantiles(rate(http_request_duration_seconds_bucket[5m]), "le", 0.5, 0.9)"#,
        ),
        ("PromqlVectorFromScalar", "vector(1)"),
        ("PromqlScalarFromVector", "scalar(up)"),
    ];
    for (label, q) in promql_examples {
        println!("=== {label} === promql> {q}");
        match lower_promql(q, AccuracyTarget::Exact) {
            Ok(qe) => println!("{qe:#?}"),
            Err(e) => println!("ERR: {e}"),
        }
        println!();
    }

    let packets = packets_catalog();
    let bgp = bgp_catalog();
    let sql_examples: &[(&str, &str, &str)] = &[
        ("Filter + Scan", "packets", "SELECT * FROM packets WHERE proto = 'tcp'"),
        ("Project", "packets", "SELECT srcip, dstip FROM packets"),
        ("SQLWindowFunc", "packets", "SELECT srcip, LAG(time) OVER (PARTITION BY srcip ORDER BY time) FROM packets"),
        ("Join (custom)", "bgp", "SELECT u.prefix FROM bgp_updates u JOIN bgp_rib_state r ON u.prefix = r.prefix"),
        ("SetOp / UNION (custom)", "packets", "SELECT srcip FROM packets UNION SELECT dstip FROM packets"),
        ("SetOp / UNION ALL (custom)", "packets", "SELECT srcip FROM packets UNION ALL SELECT dstip FROM packets"),
        ("Dedup, row-level (custom)", "packets", "SELECT DISTINCT srcip, dstip FROM packets"),
        ("CTE (custom)", "packets", "WITH totals AS (SELECT srcip, COUNT(*) AS cnt FROM packets GROUP BY srcip) SELECT * FROM totals WHERE cnt > 10"),
        ("CTE referenced twice / diamond (custom)", "packets", "WITH a AS (SELECT srcip, COUNT(*) AS c FROM packets GROUP BY srcip) SELECT x.srcip FROM a x JOIN a y ON x.srcip = y.srcip"),
        ("Nested subquery, no CTE syntax (custom)", "packets", "SELECT * FROM (SELECT srcip, COUNT(*) AS cnt FROM packets GROUP BY srcip) t WHERE cnt > 10"),
    ];
    for (label, which_catalog, q) in sql_examples {
        let catalog = if *which_catalog == "bgp" {
            &bgp
        } else {
            &packets
        };
        println!("=== {label} === sql> {q}");
        match lower_sql_dialect(q, catalog, SqlDialect::DataFusionSQL, AccuracyTarget::Exact).await
        {
            Ok(qe) => println!("{qe:#?}"),
            Err(e) => println!("ERR: {e}"),
        }
        println!();
    }
}
