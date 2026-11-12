//! Steps 4 and 5: dead-candidate elimination and Pass 2's pruned list.
use std::collections::BTreeSet;

use asap_types::accuracy::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::stages::{CandidateForMemoGroupId, Memo, RootPruneReason};
use asap_types::summary_type::parameter_list;
use asap_types::summary_type::{SketchAlgorithm, SketchParams};
use asap_types::workload::DataWorkload;

use crate::test_fixtures::*;
use asap_types::test_support::memo_builders::*;
use asap_types::test_support::memo_fixtures::{arithmetic, range};

/// The concepts doc example "ratio of quantiles":
/// `quantile_over_time(0.99, m[5m]) / quantile_over_time(0.5, m[5m])`,
/// relative error 0.05, every 60 s; Pass 1 offers KLL and DDSketch to both
/// quantiles. The ratio carries only relative error, so each KLL readout is
/// pruned by the metric rule and every KLL build through it. The readouts
/// of DDSketch get the levels 0.02 … 0.001 (cap 0.045), so the builds with
/// alpha 0.1 and 0.05 miss every level they could serve. Chain groups are
/// added only for the surviving builds.
#[test]
fn dead_candidates_are_pruned_and_listed_before_chain_groups() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan_group(&mut memo, "m", &["job"], vec![]);
    let window = group(&mut memo, range(300_000, input));
    let algorithms = [SketchAlgorithm::Kll, SketchAlgorithm::DDSketch];
    let p99 = quantile_over_time_readouts(&mut memo, window, 0.99, &algorithms);
    let p50 = quantile_over_time_readouts(&mut memo, window, 0.5, &algorithms);
    let ratio = group(
        &mut memo,
        arithmetic(ArithmeticOpKind::Div, p99.result, p50.result),
    );
    let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    query(&mut memo, ratio, Some(target));
    let logical = pass2(memo, &[entry(60_000)]);
    let memo = &logical.memo;
    let (kll_state, ddsketch_state) = (p99.by_algorithm[0].state, p99.by_algorithm[1].state);
    // Each KLL readout: the metric rule at the ratio, directly.
    for quantile in [&p99, &p50] {
        let kll_readout = quantile.by_algorithm[0].candidate_of_result;
        let pruned = pruned_entry(&logical, kll_readout).expect("the KLL readout is pruned");
        assert!(pruned.causes.iter().all(|cause| matches!(
            &cause.root,
            RootPruneReason::MetricRule { producer_metric: AccuracyMetric::Rank, accepted_metrics, .. }
                if accepted_metrics == &vec![AccuracyMetric::Relative]
        )));
    }
    // Every KLL build: the metric rule on its edge into a KLL readout,
    // whose output accepts only relative error.
    for index_in_group in 0..parameter_list(SketchAlgorithm::Kll).len() {
        let build = CandidateForMemoGroupId {
            group: kll_state,
            index_in_group,
        };
        let pruned = pruned_entry(&logical, build).expect("every KLL build is pruned");
        assert!(!pruned.causes.is_empty());
        assert!(pruned.causes.iter().all(|cause| matches!(
            &cause.root,
            RootPruneReason::MetricRule { producer_metric: AccuracyMetric::Rank, accepted_metrics, .. }
                if accepted_metrics == &vec![AccuracyMetric::Relative]
        )));
    }
    // DDSketch: alpha 0.1 and 0.05 pruned below every required accuracy;
    // the levels of the readouts are 0.02 … 0.001.
    let pruned_alphas: Vec<f64> = logical
        .pruned
        .iter()
        .filter(|pruned| pruned.candidate.group == ddsketch_state)
        .map(|pruned| {
            assert!(pruned
                .causes
                .iter()
                .all(|cause| cause.through_pruned_candidates.is_empty()
                    && matches!(cause.root, RootPruneReason::BelowRequiredAccuracy { .. })));
            match memo
                .candidate(pruned.candidate)
                .logical_properties
                .parameters()
            {
                Some(SketchParams::DDSketch { alpha }) => *alpha,
                other => panic!("{other:?}"),
            }
        })
        .collect();
    assert_eq!(pruned_alphas, vec![0.1, 0.05]);
    let readout_group = p99.by_algorithm[1].readout.group;
    let mut levels: Vec<f64> = requirements_of(memo, readout_group)
        .into_iter()
        .map(|required| required.expect("a DDSketch readout's level").max_error)
        .collect();
    levels.sort_by(f64::total_cmp);
    assert_eq!(levels, vec![0.001, 0.002, 0.005, 0.01, 0.02]);
    // Chain builds exist only for the five surviving alphas.
    let chains = groups_where(memo, |e| {
        matches!(e, Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { summary_type, .. })
            if *summary_type == FieldDataType::Sketch(SketchAlgorithm::DDSketch))
    });
    assert_eq!(chains.len(), 1);
    assert_eq!(candidates_of(memo, chains[0]).len(), 5);
    assert!(groups_where(memo, |e| matches!(
        e,
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { summary_type, .. })
            if *summary_type == FieldDataType::Sketch(SketchAlgorithm::Kll)
    ))
    .is_empty());
    // No logical edge names a pruned candidate.
    let pruned: BTreeSet<_> = logical.pruned.iter().map(|p| p.candidate).collect();
    assert!(logical
        .edges
        .iter()
        .all(|edge| !pruned.contains(&edge.producer_candidate_id)
            && !pruned.contains(&edge.consumer_candidate_id)));
    let _ = kll_state;
}
