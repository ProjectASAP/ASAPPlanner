//! Cross-frontend evaluation-time semantics (issues #46 and #184).

use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_integration_tests::fixtures::lower_promql_root;
use asap_types::ir::{NonASAPOp, ScalarExpr};
use asap_types::pre_asap::schema::{DataType, Field, Schema};
use asap_types::types::AccuracyTarget;

/// PromQL exposes its evaluation time as Unix seconds, whereas SQL exposes
/// `CURRENT_TIMESTAMP` as a timestamp. They represent related clock concepts
/// but must remain distinguishable in the shared IR and type inference.
#[tokio::test]
async fn promql_eval_time_and_sql_current_timestamp_remain_distinct() {
    let promql = lower_promql_root("time()", AccuracyTarget::Exact).expect("lower PromQL time()");
    assert!(
        matches!(
            promql,
            asap_types::ir::QueryRoot::Scalar(ScalarExpr::EvalTimestamp)
        ),
        "expected a bare evaluation-time scalar, got {promql:?}"
    );
    assert_eq!(
        ScalarExpr::EvalTimestamp
            .scalar_type(&Schema::default())
            .unwrap()
            .0,
        DataType::Float64
    );

    let catalog = SqlCatalog::new().with_table(
        "metrics",
        Schema::new(vec![Field::plain("value", DataType::Float64, false)]),
    );
    let sql = lower_sql(
        "SELECT CURRENT_TIMESTAMP FROM metrics",
        &catalog,
        AccuracyTarget::Exact,
    )
    .await
    .expect("lower SQL CURRENT_TIMESTAMP");
    let Some(NonASAPOp::Project { cols, child, .. }) = sql.non_asap() else {
        panic!("expected SQL projection, got {sql:?}");
    };
    assert!(matches!(&cols[0].expr, ScalarExpr::CurrentTimestamp));
    let (sql_dtype, _) = cols[0]
        .expr
        .scalar_type(&child.schema)
        .expect("SQL CURRENT_TIMESTAMP type");
    assert_eq!(sql_dtype, DataType::Timestamp);
    assert_eq!(sql.schema.fields[0].dtype, DataType::Timestamp);
}
