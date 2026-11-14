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

// θ is checked with the kernel, not with the family: a Count Sketch with
// heap of even depth is refused (its estimate is a median of rows),
// while its family stays native.
#[test]
fn weighted_frequency_parameters_are_checked_with_the_kernel() {
    let family = SummaryFamilyType::Sketch(SketchAlgorithm::CountSketchWithHeap);
    let mut update = SummaryUpdate::column(planner_types::ir::scalar::ColumnRef::SampleValue);
    update.item = Some(planner_types::summary_type::SummaryInputExpr::Column(
        planner_types::ir::scalar::ColumnRef::Named("host".into()),
    ));
    let theta = |depth, heap_size| SketchParams::CountSketchWithHeap {
        width: 32,
        depth,
        heap_size,
    };
    assert!(validate_native_family(&family).is_ok());
    assert!(validate_summary_kernel(&family, Some(&theta(5, 8)), &update).is_ok());
    assert!(validate_summary_kernel(&family, Some(&theta(4, 8)), &update).is_err());
    assert!(validate_summary_kernel(&family, Some(&theta(5, 0)), &update).is_err());
    assert!(validate_summary_kernel(&family, None, &update).is_err());
}
