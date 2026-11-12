use super::{quantile, selector, sum};
use crate::computation_targets::computation_targets;
use crate::test_fixtures::aggregate_group;
use crate::{pass1_memo, PerComputationCandidateError};
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator, TimeRangeKind};
use asap_types::stages::{Memo, MemoGroupId};
use asap_types::summary_type::{ExactKind, SummaryUpdate};
use asap_types::test_support::memo_builders::{candidates_of, group, groups_where, query};
use asap_types::workload::DataWorkload;

/// Each measure of a multi-measure aggregate is its own target; a measure
/// whose only realization is its exact accumulator is still a target.
#[test]
fn multi_measure_aggregate_has_one_target_per_measure() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let both = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![2]),
        vec![sum(), quantile(0.9)],
        None,
    );
    query(&mut memo, both, None);
    let targets = computation_targets(&memo, None).unwrap();
    let measures: Vec<(MemoGroupId, usize)> = targets
        .iter()
        .map(|target| (target.aggregate_group, target.measure_index))
        .collect();
    assert_eq!(measures, vec![(both, 0), (both, 1)]);
}

/// An aggregate with no realization besides exact execution (no
/// accumulator, no summary: `stddev`) is not a target.
#[test]
fn aggregate_without_realization_is_not_a_target() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &[]);
    let stddev = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![]),
        vec![AggIntent::StdDev {
            col: None,
            population: true,
        }],
        None,
    );
    query(&mut memo, stddev, None);
    assert!(computation_targets(&memo, None).unwrap().is_empty());
    let memo = pass1_memo(memo, None).unwrap();
    assert_eq!(candidates_of(&memo, stddev).len(), 1);
}

/// An aggregate several queries read, or a scalar root reads twice, is one
/// target per measure: Pass 1 walks the groups.
#[test]
fn an_aggregate_read_by_several_roots_is_one_target() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &[]);
    let p = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![]),
        vec![quantile(0.9)],
        None,
    );
    query(&mut memo, p, None);
    query(&mut memo, p, None);
    assert_eq!(computation_targets(&memo, None).unwrap().len(), 1);
}

/// A memo whose groups already hold ASAP operators fails at the stage
/// boundary.
#[test]
fn asap_input_is_rejected() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &[]);
    let state = group(
        &mut memo,
        Operator::ASAP(ASAPOp::SummaryAgg {
            child: rows,
            summary_type: FieldDataType::exact(ExactKind::Sum),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
        }),
    );
    query(&mut memo, state, None);
    assert!(matches!(
        pass1_memo(memo, None),
        Err(PerComputationCandidateError::ContainsAsap)
    ));
}

/// The summaries the deployment does not support are not offered.
#[test]
fn unsupported_summaries_are_not_offered() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Range, "m", &[]);
    let p = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![]),
        vec![quantile(0.9)],
        None,
    );
    query(&mut memo, p, None);
    let memo = pass1_memo(memo, Some(&[])).unwrap();
    assert_eq!(candidates_of(&memo, p).len(), 1);
    assert!(groups_where(&memo, |e| matches!(e, Operator::ASAP(_))).is_empty());
}
