// cargo run -p asap-control-lower --example topk_ir
//
// Lowers every topk-shaped query from the design discussion and prints the
// resulting L3 IR. Used for interactive exploration; not a test.

use asap_ir::intent_algebra::schema::{Column, DataType, Schema};
use asap_ir::types::AccuracyTarget;
use asap_control_lower::{lower_promql, lower_sql, SqlCatalog};

fn col(name: &str, dtype: DataType) -> Column {
    Column::new(name, dtype, false)
}

fn catalog() -> SqlCatalog {
    SqlCatalog::new()
        .with_table(
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
        .with_table(
            "hosts",
            Schema::new(vec![
                col("service", DataType::Utf8),
                col("region", DataType::Utf8),
            ]),
        )
}

async fn show_sql(label: &str, query: &str) {
    println!("━━━ {label} ━━━");
    println!("{query}");
    match lower_sql(query, &catalog(), AccuracyTarget::Exact).await {
        Ok(qe) => println!("{qe:#?}"),
        Err(e) => println!("ERR: {e}"),
    }
    println!();
}

fn show_promql(label: &str, query: &str) {
    println!("━━━ {label} ━━━");
    println!("{query}");
    match lower_promql(query, AccuracyTarget::Exact) {
        Ok(qe) => println!("{qe:#?}"),
        Err(e) => println!("ERR: {e}"),
    }
    println!();
}

#[tokio::main]
async fn main() {
    // ── SQL ──────────────────────────────────────────────────────────────────

    show_sql(
        "S1 — count ranked, alias",
        "SELECT service, COUNT(*) AS cnt \
         FROM metrics GROUP BY service \
         ORDER BY cnt DESC LIMIT 5",
    )
    .await;

    show_sql(
        "S2 — count ranked, inline",
        "SELECT service, COUNT(*) \
         FROM metrics GROUP BY service \
         ORDER BY COUNT(*) DESC LIMIT 5",
    )
    .await;

    show_sql(
        "S3 — count ranked, with OFFSET",
        "SELECT service, COUNT(*) AS cnt \
         FROM metrics GROUP BY service \
         ORDER BY cnt DESC LIMIT 5 OFFSET 10",
    )
    .await;

    show_sql(
        "S4 — SUM ranked",
        "SELECT service, SUM(bytes) AS total \
         FROM metrics GROUP BY service \
         ORDER BY total DESC LIMIT 5",
    )
    .await;

    show_sql(
        "S5 — AVG ranked",
        "SELECT service, AVG(latency) AS avg_lat \
         FROM metrics GROUP BY service \
         ORDER BY avg_lat DESC LIMIT 5",
    )
    .await;

    show_sql(
        "S6 — MAX ranked",
        "SELECT service, MAX(latency) AS max_lat \
         FROM metrics GROUP BY service \
         ORDER BY max_lat DESC LIMIT 5",
    )
    .await;

    show_sql(
        "S7 — no aggregation",
        "SELECT ts, service, latency \
         FROM metrics \
         ORDER BY latency DESC LIMIT 5",
    )
    .await;

    show_sql(
        "S8 — partitioned count topk (window function)",
        "SELECT service, region, cnt FROM (\
            SELECT service, region, COUNT(*) AS cnt, \
                   ROW_NUMBER() OVER (PARTITION BY region ORDER BY COUNT(*) DESC) AS rn \
            FROM metrics GROUP BY service, region\
         ) t WHERE rn <= 5",
    )
    .await;

    show_sql(
        "S9 — partitioned AVG topk (window function)",
        "SELECT service, region, avg_lat FROM (\
            SELECT service, region, AVG(latency) AS avg_lat, \
                   ROW_NUMBER() OVER (PARTITION BY region ORDER BY AVG(latency) DESC) AS rn \
            FROM metrics GROUP BY service, region\
         ) t WHERE rn <= 5",
    )
    .await;

    // ── PromQL ───────────────────────────────────────────────────────────────

    show_promql(
        "P1 — topk over count_over_time",
        "topk(5, count_over_time(http_requests_total[5m]))",
    );

    show_promql(
        "P2 — topk over instant vector",
        "topk(5, http_requests_total)",
    );

    show_promql(
        "P3 — topk over rate",
        "topk(5, rate(http_requests_total[5m]))",
    );

    show_promql(
        "P4 — topk over avg_over_time",
        "topk(5, avg_over_time(http_requests_total[5m]))",
    );

    show_promql(
        "P5 — topk over sum-by aggregation",
        "topk(5, sum by (service) (http_requests_total))",
    );

    show_promql(
        "P6 — bottomk over instant vector",
        "bottomk(5, http_requests_total)",
    );

    show_promql(
        "P8 — topk with by, instant vector",
        "topk by (service) (5, http_requests_total)",
    );

    show_promql(
        "P9 — topk with by, over rate",
        "topk by (service) (5, rate(http_requests_total[5m]))",
    );

    show_promql(
        "P10 — topk with by, over count_over_time",
        "topk by (service) (5, count_over_time(http_requests_total[5m]))",
    );
}
