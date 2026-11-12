use std::time::Duration;

use super::{quantile, topk};
use crate::candidates::{add_measure_way, Pending, SingleMeasure};
use crate::computation_targets::HowToComputeAggregateMeasure;
use crate::test_fixtures::aggregate_group;
use crate::PerComputationCandidateError;
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::schema::DataType;
use asap_types::ir::{NonASAPOp, Operator, TimeRangeKind};
use asap_types::stages::{Memo, MemoGroupId};
use asap_types::summary_type::SketchAlgorithm;
use asap_types::test_support::memo_builders::{group, scan_group};
use asap_types::test_support::memo_fixtures::scan;
use asap_types::workload::DataWorkload;

/// Offers `measure` by `way` to `result_group`, expecting `Unsupported`, and
/// checks the memo is unchanged: its groups, each group's candidates and
/// its hash-consing index (all in the memo's `Debug`).
fn assert_unsupported_way_adds_nothing(
    memo: &mut Memo,
    measure: &SingleMeasure<'_>,
    way: HowToComputeAggregateMeasure,
    result_group: MemoGroupId,
) {
    let groups = memo.groups().len();
    let before = format!("{memo:?}");
    let added = add_measure_way(memo, Pending::new(memo), measure, &way, result_group, None);
    assert!(
        matches!(added, Err(PerComputationCandidateError::Unsupported(_))),
        "{added:?}"
    );
    assert_eq!(memo.groups().len(), groups);
    assert_eq!(format!("{memo:?}"), before);
}

/// A heap top-k whose estimate lacks a column of the result group (it
/// partitions `host` rows; the group ranks `job`): `ranked_as_topk` finds no
/// `job` in the pending estimate, so neither the heap's state group nor its
/// estimate group is added.
#[test]
fn a_heap_estimate_without_the_groups_columns_adds_nothing() {
    let mut memo = Memo::new(&DataWorkload::default());
    let by_job = scan_group(&mut memo, "m", &["job"], vec![]);
    let by_host = scan_group(&mut memo, "n", &["host"], vec![]);
    let ranked = aggregate_group(
        &mut memo,
        by_job,
        Reduction::by(vec![2]),
        vec![topk(5)],
        None,
    );
    let reduction = Reduction::by(vec![2]);
    let intent = topk(5);
    let measure = SingleMeasure {
        input: by_host,
        unfiltered_input: by_host,
        reduction: &reduction,
        intent: &intent,
        filter: None,
        raw_data_samples_of: None,
    };
    assert_unsupported_way_adds_nothing(
        &mut memo,
        &measure,
        HowToComputeAggregateMeasure::Summary(SketchAlgorithm::CountSketchWithHeap),
        ranked,
    );
}

/// A readout of an open per-series aggregate would need a renaming
/// `Project`, which drops the labels an open schema does not list: the
/// KLL state and readout groups are not added.
#[test]
fn a_readout_an_open_group_cannot_rename_adds_nothing() {
    let mut memo = Memo::new(&DataWorkload::default());
    let scanned = group(
        &mut memo,
        scan("latency", &[("job", DataType::Utf8)], vec![]),
    );
    let rows = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(300),
            kind: TimeRangeKind::Range,
            child: scanned,
        }),
    );
    let intent = quantile(0.9);
    let per_series = aggregate_group(
        &mut memo,
        rows,
        Reduction::PerEntity,
        vec![intent.clone()],
        None,
    );
    let measure = SingleMeasure {
        input: rows,
        unfiltered_input: rows,
        reduction: &Reduction::PerEntity,
        intent: &intent,
        filter: None,
        raw_data_samples_of: None,
    };
    assert_unsupported_way_adds_nothing(
        &mut memo,
        &measure,
        HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Kll),
        per_series,
    );
}
