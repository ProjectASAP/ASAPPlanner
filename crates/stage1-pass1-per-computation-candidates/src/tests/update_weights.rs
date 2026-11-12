use super::{selector, state_group, sum};
use crate::pass1_memo;
use crate::test_fixtures::aggregate_group;
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::{ASAPOp, Operator, TimeRangeKind};
use asap_types::stages::Memo;
use asap_types::summary_type::{SketchAlgorithm, SummaryInputExpr};
use asap_types::test_support::memo_builders::{groups_where, query};
use asap_types::workload::DataWorkload;

/// A Count-Min count adds one per row (bounded by the row count); a sum's
/// state reads the raw value column (bounded by the source's norm).
#[test]
fn update_weights_follow_the_measure() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let counts = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![2]),
        vec![AggIntent::Count],
        None,
    );
    query(&mut memo, counts, None);
    let memo = pass1_memo(memo, None).unwrap();
    let (_, update) = state_group(&memo, SketchAlgorithm::Cms);
    assert_eq!(update.weight, SummaryInputExpr::Constant(1.0));
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let sums = aggregate_group(&mut memo, rows, Reduction::by(vec![2]), vec![sum()], None);
    query(&mut memo, sums, None);
    let memo = pass1_memo(memo, None).unwrap();
    let state = groups_where(
        &memo,
        |e| matches!(e, Operator::ASAP(ASAPOp::SummaryAgg { input, .. }) if input.weight == SummaryInputExpr::Column(ColumnRef::SampleValue)),
    );
    assert_eq!(state.len(), 1);
}
