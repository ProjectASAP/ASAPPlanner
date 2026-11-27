use super::{quantile, selector, sum};
use crate::computation_targets::computation_targets;
use crate::test_fixtures::aggregate_group;
use crate::{pass1_memo, PerComputationCandidateError};
use asap_types::deployment_capabilities::{
    CandidateExclusion, DeploymentCapabilities, DeploymentExclusion, ExclusionSource,
    SupportedSummary,
};
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, Operator, TimeRangeKind};
use asap_types::stages::{Memo, MemoGroupId};
use asap_types::summary_type::{
    ExactKind, SketchAlgorithm, StatisticKind, SummaryFamily, SummaryUpdate,
};
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
    let targets = computation_targets(&memo, &[]).unwrap();
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
    assert!(computation_targets(&memo, &[]).unwrap().is_empty());
    let memo = pass1_memo(memo, &[]).unwrap();
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
    assert_eq!(computation_targets(&memo, &[]).unwrap().len(), 1);
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
        pass1_memo(memo, &[]),
        Err(PerComputationCandidateError::ContainsAsap)
    ));
}

/// One global aggregate of `measure` over the instant selector `m`
/// (`quantile(0.9, m)`), planned by Pass 1 under `exclusions`; returns the
/// memo and the aggregate's group.
fn pass1_under(measure: AggIntent, exclusions: &[DeploymentExclusion]) -> (Memo, MemoGroupId) {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &[]);
    let aggregate = aggregate_group(&mut memo, rows, Reduction::by(vec![]), vec![measure], None);
    query(&mut memo, aggregate, None);
    (pass1_memo(memo, exclusions).unwrap(), aggregate)
}

/// The summary types of the state builds Pass 1 added to `memo`.
fn built_summary_types(memo: &Memo) -> Vec<FieldDataType> {
    groups_where(memo, |e| {
        matches!(e, Operator::ASAP(ASAPOp::SummaryAgg { .. }))
    })
    .into_iter()
    .filter_map(|g| match &memo.groups()[g.0 as usize].expression {
        Operator::ASAP(ASAPOp::SummaryAgg { summary_type, .. }) => Some(summary_type.clone()),
        _ => None,
    })
    .collect()
}

/// Under exact-only capabilities (`supported_summaries: []`) every sketch
/// family is excluded outright, so Pass 1 offers no summary for a
/// quantile: the aggregate keeps only its own exact expression, as Stage 2
/// would have pruned every build.
#[test]
fn exact_only_capabilities_offer_no_summary() {
    let exact_only = DeploymentCapabilities {
        supported_summaries: Some(vec![]),
        ..Default::default()
    };
    let (memo, aggregate) = pass1_under(quantile(0.9), &exact_only.exclusions());
    assert_eq!(candidates_of(&memo, aggregate).len(), 1);
    assert!(groups_where(&memo, |e| matches!(e, Operator::ASAP(_))).is_empty());
}

/// Exact accumulators are left to Stage 2: with every `Sum` state
/// excluded, Pass 1 still offers a `sum` its accumulator.
#[test]
fn an_excluded_exact_family_is_left_to_stage_2() {
    let no_sum_state = [DeploymentExclusion {
        source: ExclusionSource::ExcludedCandidate { index: 0 },
        exclusion: CandidateExclusion {
            summary_family: Some(SummaryFamily::Exact(ExactKind::Sum)),
            ..Default::default()
        },
    }];
    let (memo, _) = pass1_under(sum(), &no_sum_state);
    assert!(built_summary_types(&memo).contains(&FieldDataType::exact(ExactKind::Sum)));
}

/// A KLL listed without its quantile readout cannot answer a quantile, so
/// Pass 1 offers a quantile no KLL build (and, with every other algorithm
/// unlisted, no summary at all). A readout the measure does not read
/// (KLL's `Total`) keeps the build.
#[test]
fn an_excluded_readout_of_the_measure_skips_the_summary() {
    let kll_with = |readouts| DeploymentCapabilities {
        supported_summaries: Some(vec![SupportedSummary {
            summary_type: SketchAlgorithm::KLL,
            readouts,
        }]),
        ..Default::default()
    };
    let (memo, aggregate) = pass1_under(quantile(0.9), &kll_with(vec![]).exclusions());
    assert!(built_summary_types(&memo).is_empty());
    assert_eq!(candidates_of(&memo, aggregate).len(), 1);

    let (memo, _) = pass1_under(
        quantile(0.9),
        &kll_with(vec![StatisticKind::Quantile]).exclusions(),
    );
    assert!(built_summary_types(&memo).contains(&FieldDataType::Sketch(SketchAlgorithm::KLL)));
}

/// Only an exclusion of every state of a family, or of the readout the
/// measure reads, skips a summary in Pass 1. KLL not mergeable across
/// windows still has builds Stage 2 may keep, so Pass 1 offers them.
#[test]
fn a_partial_exclusion_keeps_the_summary() {
    let kll_not_mergeable = DeploymentCapabilities {
        state_not_mergeable_across_windows: vec![SummaryFamily::Sketch(SketchAlgorithm::KLL)],
        ..Default::default()
    };
    let (memo, _) = pass1_under(quantile(0.9), &kll_not_mergeable.exclusions());
    assert!(built_summary_types(&memo).contains(&FieldDataType::Sketch(SketchAlgorithm::KLL)));
}
