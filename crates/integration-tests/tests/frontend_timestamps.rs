//! Cross-frontend evaluation-time semantics (issues #46 and #184).

use asap_frontend_promql::lower_promql;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::pre_asap::QueryExpr;
use asap_types::types::AccuracyTarget;

/// PromQL exposes its evaluation time as Unix seconds, whereas SQL exposes
/// `CURRENT_TIMESTAMP` as a timestamp. They represent related clock concepts
/// but must remain distinguishable in the shared IR and type inference.
#[tokio::test]
async fn promql_eval_time_and_sql_current_timestamp_remain_distinct() {
    let promql = lower_promql("time()", AccuracyTarget::Exact).expect("lower PromQL time()");
    assert!(matches!(promql, QueryExpr::EvalTimestamp));
    let promql_schema = promql.output_schema().expect("PromQL time() schema");
    assert_eq!(promql_schema.columns[0].dtype, DataType::Float64);

    let catalog = SqlCatalog::new().with_table(
        "metrics",
        Schema::new(vec![Column::new("value", DataType::Float64, false)]),
    );
    let sql = lower_sql(
        "SELECT CURRENT_TIMESTAMP FROM metrics",
        &catalog,
        AccuracyTarget::Exact,
    )
    .await
    .expect("lower SQL CURRENT_TIMESTAMP");
    let QueryExpr::Project { cols, .. } = sql else {
        panic!("expected SQL projection, got {sql:?}");
    };
    assert!(matches!(&cols[0].expr, QueryExpr::CurrentTimestamp));
    let sql_schema = cols[0]
        .expr
        .output_schema()
        .expect("SQL CURRENT_TIMESTAMP schema");
    assert_eq!(sql_schema.columns[0].dtype, DataType::Timestamp);
}
