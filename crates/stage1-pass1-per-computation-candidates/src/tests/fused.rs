use super::{memo_printout, selector, state_group, sum, topk};
use crate::pass1_memo;
use crate::test_fixtures::aggregate_group;
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::{ASAPOp, Operator, TimeRangeKind};
use asap_types::stages::Memo;
use asap_types::summary_type::{parameter_list, SketchAlgorithm, SummaryInputExpr, WeightDomain};
use asap_types::test_support::memo_builders::{candidates_of, query};
use asap_types::workload::DataWorkload;

/// `topk(5, sum by (job) (sum_over_time(m[5m])))`: the top-k gets heaps
/// over the range's raw data samples, keyed by `job` (the ranked item),
/// weighted by the sample value with the open requirement that the samples
/// of `m` are non-negative (`RawDataSamplesOfMetric`). Each heap build
/// lists the absorbed sums' groups in `fused_groups`, and the top-k's own
/// heaps over its one-row-per-item input are pruned.
#[test]
fn whole_expression_topk_heaps_read_the_raw_samples() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Range, "m", &["job"]);
    let per_series = aggregate_group(&mut memo, rows, Reduction::PerEntity, vec![sum()], None);
    let by_job = aggregate_group(
        &mut memo,
        per_series,
        Reduction::by(vec![2]),
        vec![sum()],
        None,
    );
    let ranked = aggregate_group(
        &mut memo,
        by_job,
        Reduction::by(vec![]),
        vec![topk(5)],
        None,
    );
    query(&mut memo, ranked, None);
    let memo = pass1_memo(memo, None).unwrap();
    for algorithm in [
        SketchAlgorithm::CmsWithHeap,
        SketchAlgorithm::CountSketchWithHeap,
    ] {
        let (state, update) = state_group(&memo, algorithm);
        assert_eq!(
            *memo.groups()[state.0 as usize].expression.children()[0],
            rows
        );
        assert_eq!(
            update.item,
            Some(SummaryInputExpr::Column(ColumnRef::Named("job".into())))
        );
        assert_eq!(
            update.weight_domain,
            WeightDomain::RawDataSamplesOfMetric { metric: "m".into() }
        );
        for build in candidates_of(&memo, state) {
            let mut fused = build.fused_groups.clone();
            fused.sort();
            assert_eq!(fused, vec![per_series, by_job]);
        }
        assert_eq!(
            candidates_of(&memo, state).len(),
            parameter_list(algorithm).len()
        );
    }
}

/// `topk(5, sum by (job) (sum_over_time(m[5m])))`: the whole-expression
/// heap's candidates, built through the readout and output path every
/// summary way takes. The top-k group gets one `Project` per heap
/// algorithm, ranking the estimate read from that heap's state; only the
/// state's builds list the absorbed groups.
#[test]
fn whole_expression_topk_heap_candidates() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Range, "m", &["job"]);
    let per_series = aggregate_group(&mut memo, rows, Reduction::PerEntity, vec![sum()], None);
    let by_job = aggregate_group(
        &mut memo,
        per_series,
        Reduction::by(vec![2]),
        vec![sum()],
        None,
    );
    let ranked = aggregate_group(
        &mut memo,
        by_job,
        Reduction::by(vec![]),
        vec![topk(5)],
        None,
    );
    query(&mut memo, ranked, None);
    let memo = pass1_memo(memo, None).unwrap();
    // The top-k group's own expression, then one ranked `Project` per heap.
    let ranked_candidates = candidates_of(&memo, ranked);
    let kinds: Vec<&str> = ranked_candidates
        .iter()
        .map(|c| c.expression.operator.kind_name())
        .collect();
    assert_eq!(
        kinds,
        ["Aggregate", "Project", "Project"],
        "{}",
        memo_printout(&memo)
    );
    for (project, algorithm) in ranked_candidates[1..].iter().zip([
        SketchAlgorithm::CmsWithHeap,
        SketchAlgorithm::CountSketchWithHeap,
    ]) {
        assert!(project.fused_groups.is_empty());
        let estimate = *project.expression.operator.children()[0];
        let [estimate_candidate] = candidates_of(&memo, estimate) else {
            panic!("one readout candidate: {}", memo_printout(&memo));
        };
        assert!(matches!(
            estimate_candidate.expression.operator,
            Operator::ASAP(ASAPOp::SummaryEstimate { .. })
        ));
        assert!(estimate_candidate.fused_groups.is_empty());
        let (state, _) = state_group(&memo, algorithm);
        assert_eq!(*estimate_candidate.expression.operator.children()[0], state);
        assert_eq!(
            candidates_of(&memo, state).len(),
            parameter_list(algorithm).len()
        );
        for build in candidates_of(&memo, state) {
            assert_eq!(build.fused_groups, [by_job, per_series]);
        }
    }
    // Scan, range, the two sums, the top-k, the sums' two accumulator
    // states, and per heap its state and readout: no other group.
    assert_eq!(memo.groups().len(), 11, "{}", memo_printout(&memo));
}

/// A top-k over a `count` ranks the counted items with unit weight, proven
/// non-negative.
#[test]
fn count_ranked_topk_counts_with_unit_weight() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let counts = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![2]),
        vec![AggIntent::Count],
        None,
    );
    let ranked = aggregate_group(
        &mut memo,
        counts,
        Reduction::by(vec![]),
        vec![topk(5)],
        None,
    );
    query(&mut memo, ranked, None);
    let memo = pass1_memo(memo, None).unwrap();
    let (_, update) = state_group(&memo, SketchAlgorithm::CmsWithHeap);
    assert_eq!(update.weight, SummaryInputExpr::Constant(1.0));
    assert!(matches!(
        update.weight_domain,
        WeightDomain::NonNegative { .. }
    ));
}

/// The ranked item over several group keys is their tuple; the top-k's own
/// partition keys partition the heap and stay out of the tuple.
#[test]
fn ranked_item_is_the_keys_outside_the_partition() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Range, "m", &["job", "host"]);
    let per_series = aggregate_group(&mut memo, rows, Reduction::PerEntity, vec![sum()], None);
    let by_both = aggregate_group(
        &mut memo,
        per_series,
        Reduction::by(vec![2, 3]),
        vec![sum()],
        None,
    );
    let ranked = aggregate_group(
        &mut memo,
        by_both,
        Reduction::by(vec![0]),
        vec![topk(5)],
        None,
    );
    query(&mut memo, ranked, None);
    let memo = pass1_memo(memo, None).unwrap();
    let (_, update) = state_group(&memo, SketchAlgorithm::CountSketchWithHeap);
    assert_eq!(
        update.item,
        Some(SummaryInputExpr::Column(ColumnRef::Named("host".into())))
    );
}
