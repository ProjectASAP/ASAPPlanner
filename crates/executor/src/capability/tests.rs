use super::*;

// Only a query-time build applies a summary filter.
#[test]
fn filtered_summary_runs_only_at_query_time() {
    let filter = planner_types::ir::Predicate::<usize>(planner_types::ir::ScalarExpr::Literal(
        planner_types::ir::scalar::ScalarValue::Boolean(true),
    ));
    assert!(validate_ingestion_time_summary(Some(&filter)).is_err());
    assert!(validate_ingestion_time_summary::<usize>(None).is_ok());
}
