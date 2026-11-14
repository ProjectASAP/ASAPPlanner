//! Decision-tree levels 3 and 4: the (τ, ℓ) each logical candidate is
//! offered, and the prune rules that remove some of them.
use std::collections::BTreeMap;

use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::operator::arguments::{AtModifier, Reduction, Source};
use asap_types::ir::properties::ExecutionTiming::QueryTime;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::stages::{Memo, PhysicalASAPMemoCandidates, PruneReason, ValueSign};
use asap_types::summary_type::{
    EntityIdentity, ExactKind, SketchAlgorithm, SummaryInputExpr, SummaryUpdate,
};
use asap_types::test_support::memo_builders::{group, query, scan_group};
use asap_types::test_support::memo_fixtures::{range, shift, state};
use asap_types::workload::{DataArrival, DataWorkload, SourceFacts};

use super::{
    any_across_evaluations, generate, offered, physical_properties_list, properties, pruned,
    pruned_with,
};
use crate::test_fixtures::{
    candidate_id, edge, example, ingesting, logical, memo, retaining_everything, sketch_group,
    GRID_60S, IT_ACROSS, IT_NONE, QT_ACROSS, QT_NONE, QT_ONE,
};

/// Query A over ingesting data, everything retained. Time-dependent
/// candidates (the last minute, the build over it, the merge, the windows,
/// the readout) run only at query time; on the 60 s grid they may be
/// materialized across evaluations. The scan and the chain are
/// time-independent, so ingestion time is offered too, and so is
/// `AcrossEvaluations` without a grid: kept up to date at ingestion time,
/// their result is final at any time (`PhysicalCandidate::delivers_on`). A
/// summary build is not offered (IT, None): fed at ingestion time
/// it keeps its state, so the chain is offered (IT, AcrossEvaluations)
/// only; the scan's (IT, None) feeds the chain built at ingestion time.
#[test]
fn offers_each_candidate_its_timings_and_lifetimes() {
    let example = example();
    let physical = generate(&example.logical, &ingesting(None), &retaining_everything());
    let query_time = physical_properties_list(&[QT_NONE, QT_ONE, QT_ACROSS]);
    for candidate in [
        example.last_minute,
        example.build,
        example.merge,
        example.windows,
        example.readout,
    ] {
        assert_eq!(offered(&physical, candidate), query_time, "{candidate:?}");
        assert_eq!(pruned(&physical, candidate), vec![], "{candidate:?}");
    }
    assert_eq!(
        offered(&physical, example.scan),
        physical_properties_list(&[QT_NONE, QT_ONE, QT_ACROSS, IT_NONE, IT_ACROSS])
    );
    assert_eq!(
        offered(&physical, example.chain),
        physical_properties_list(&[QT_NONE, QT_ONE, QT_ACROSS, IT_ACROSS])
    );
    assert_eq!(pruned(&physical, example.chain), vec![]);
}

/// No summary build (`SummaryAgg`, chain,
/// `MaintainPopulation`) is offered (IngestionTime, None), offered or
/// pruned; the scan, which keeps nothing between batches, is.
#[test]
fn no_summary_build_is_offered_at_ingestion_time_without_a_lifetime() {
    let example = example();
    let physical = generate(&example.logical, &ingesting(None), &retaining_everything());
    let memo = &example.logical.memo;
    let ingestion_time_none = properties(IT_NONE);
    let alternatives = physical
        .physical_candidates
        .iter()
        .map(|candidate| {
            (
                candidate.logical_candidate_id,
                candidate.physical_properties,
            )
        })
        .chain(physical.pruned_candidates.iter().map(|pruned| {
            (
                pruned.candidate.logical_candidate_id,
                pruned.candidate.physical_properties,
            )
        }));
    for (logical_candidate, physical_properties) in alternatives {
        let builds = matches!(
            memo.candidate(logical_candidate).expression.operator,
            Operator::ASAP(
                ASAPOp::SummaryAgg { .. }
                    | ASAPOp::SummaryTumblingWindowChain { .. }
                    | ASAPOp::MaintainPopulation { .. }
            )
        );
        assert!(
            !(builds && physical_properties == ingestion_time_none),
            "{logical_candidate:?}"
        );
    }
    assert!(offered(&physical, example.scan).contains(&ingestion_time_none));
}

/// Data at rest (or of unknown arrival) has no ingestion pipeline: every
/// ingestion-time alternative is pruned.
#[test]
fn without_an_ingestion_pipeline_nothing_runs_at_ingestion_time() {
    let example = example();
    for arrival in [DataArrival::AtRest, DataArrival::Unknown] {
        let data = DataWorkload {
            arrival,
            ..DataWorkload::default()
        };
        let physical = generate(&example.logical, &data, &retaining_everything());
        assert_eq!(
            pruned(&physical, example.scan),
            pruned_with(&[IT_NONE, IT_ACROSS], PruneReason::NoIngestionPipeline)
        );
        assert_eq!(
            pruned(&physical, example.chain),
            pruned_with(&[IT_ACROSS], PruneReason::NoIngestionPipeline)
        );
        assert!(physical
            .physical_candidates
            .iter()
            .all(|candidate| candidate.physical_properties.timing() == QueryTime));
    }
}

/// Without raw data retained, the scan cannot run at query time, nor keep
/// its raw rows across evaluations; what reads it at
/// query time then has no buildable producer, so only the ingestion-time
/// chain serves the query.
#[test]
fn without_raw_data_only_ingestion_time_builds_read_the_scan() {
    let example = example();
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..retaining_everything()
    };
    let physical = generate(&example.logical, &ingesting(None), &capabilities);
    assert_eq!(
        pruned(&physical, example.scan),
        pruned_with(
            &[QT_NONE, QT_ONE, QT_ACROSS, IT_ACROSS],
            PruneReason::RawDataNotRetained
        )
    );
    assert_eq!(
        offered(&physical, example.scan),
        physical_properties_list(&[IT_NONE])
    );
    assert_eq!(offered(&physical, example.last_minute), vec![]);
    assert_eq!(offered(&physical, example.build), vec![]);
    assert_eq!(
        offered(&physical, example.chain),
        physical_properties_list(&[IT_ACROSS])
    );
    assert_eq!(
        offered(&physical, example.merge),
        physical_properties_list(&[QT_NONE, QT_ONE, QT_ACROSS])
    );
    assert_eq!(
        pruned(&physical, example.build),
        pruned_with(
            &[QT_NONE, QT_ONE, QT_ACROSS],
            PruneReason::InputHasNoBuildableProducer
        )
    );
}

/// The default deployment retains no query-time result across
/// evaluations: every (QueryTime, AcrossEvaluations) is pruned, and
/// `OneEvaluation` stays.
#[test]
fn query_time_results_across_evaluations_need_the_deployment_to_retain_them() {
    let example = example();
    let physical = generate(
        &example.logical,
        &ingesting(None),
        &DeploymentCapabilities::default(),
    );
    for candidate in [example.build, example.merge, example.readout] {
        assert_eq!(
            pruned(&physical, candidate),
            pruned_with(&[QT_ACROSS], PruneReason::QueryTimeResultsNotRetained),
            "{candidate:?}"
        );
        assert_eq!(
            offered(&physical, candidate),
            physical_properties_list(&[QT_NONE, QT_ONE])
        );
    }
}

/// `m[1m] @ 1000`: the `@ T` result is time-independent, so it and its
/// time-dependent input, the range under it, are offered ingestion time.
/// The pinned result is also offered `AcrossEvaluations` without a grid,
/// and built at ingestion time it reads the range's (IT, None); the
/// range, read under the anchor, is not materialized across evaluations.
/// A query reads its output at query time, so the pinned (IT, None) is
/// pruned.
#[test]
fn an_at_t_subtree_is_offered_ingestion_time() {
    let mut memo = memo();
    let scan = scan_group(&mut memo, "m", &[], vec![]);
    let last_minute = group(&mut memo, range(60_000, scan));
    let pinned = group(
        &mut memo,
        shift(0, Some(AtModifier::Timestamp(1_000)), last_minute),
    );
    query(&mut memo, pinned, None);
    let [scan, last_minute, pinned] =
        [scan, last_minute, pinned].map(|group| candidate_id(group, 0));
    let evaluation_grid_of_group = vec![None; memo.groups().len()];
    let logical = logical(
        memo,
        vec![
            edge(scan, last_minute, 0, true),
            edge(last_minute, pinned, 0, true),
        ],
        evaluation_grid_of_group,
    );
    let physical = generate(&logical, &ingesting(None), &retaining_everything());
    assert_eq!(
        offered(&physical, last_minute),
        physical_properties_list(&[QT_NONE, QT_ONE, IT_NONE])
    );
    assert_eq!(pruned(&physical, last_minute), vec![]);
    assert_eq!(
        offered(&physical, pinned),
        physical_properties_list(&[QT_NONE, QT_ONE, QT_ACROSS, IT_ACROSS])
    );
    assert_eq!(
        pruned(&physical, pinned),
        pruned_with(&[IT_NONE], PruneReason::ReachesNoQueryOutput)
    );
}

/// `x @ 1700000000 [5m]` with no known horizon
/// end reads `x` back without bound, so the scan under the `@ T` is not
/// materialized across evaluations; it still runs at query time.
#[test]
fn a_subtree_under_an_absolute_anchor_is_not_materialized_across_evaluations() {
    let mut memo = memo();
    let scan = scan_group(&mut memo, "x", &[], vec![]);
    let pinned = group(
        &mut memo,
        shift(0, Some(AtModifier::Timestamp(1_700_000_000_000)), scan),
    );
    let window = group(&mut memo, range(300_000, pinned));
    query(&mut memo, window, None);
    let [scan, pinned, window] = [scan, pinned, window].map(|group| candidate_id(group, 0));
    let mut evaluation_grid_of_group = vec![GRID_60S; memo.groups().len()];
    evaluation_grid_of_group[pinned.group.0 as usize] = None;
    evaluation_grid_of_group[window.group.0 as usize] = None;
    let logical = logical(
        memo,
        vec![edge(scan, pinned, 0, true), edge(pinned, window, 0, true)],
        evaluation_grid_of_group,
    );
    let physical = generate(&logical, &ingesting(None), &retaining_everything());
    let scan_offered = offered(&physical, scan);
    assert!(scan_offered.contains(&properties(QT_NONE)));
    assert!(!any_across_evaluations(&scan_offered));
    assert!(!any_across_evaluations(
        pruned(&physical, scan)
            .iter()
            .map(|(properties, _)| properties)
    ));
}

/// A rate state built over a SQL table (rows without a series order) is
/// offered ingestion time only when the data bounds how late a row
/// arrives; over PromQL samples, taken to arrive in order, it always is.
#[test]
fn a_counter_state_runs_at_ingestion_time_over_ordered_rows() {
    // A scan's schema with its series identity realized, as for a PromQL
    // scan.
    let mut scratch = memo();
    let rows = scan_group(&mut scratch, "rows", &[], vec![]);
    let schema = scratch.groups()[rows.0 as usize]
        .group_properties
        .schema
        .clone();
    let rate_state = |memo: &mut Memo, source: Source| {
        let scan = group(
            memo,
            Operator::NonASAP(NonASAPOp::Scan {
                source,
                predicates: vec![],
                schema: schema.clone(),
            }),
        );
        let state = group(
            memo,
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                child: scan,
                summary_type: FieldDataType::exact(ExactKind::Rate),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::PerEntity,
                filter: None,
                tumbling_window_width_ms: 60_000,
            }),
        );
        query(memo, state, None);
        (candidate_id(scan, 0), candidate_id(state, 0))
    };
    let mut memo = memo();
    let (table, table_state) = rate_state(
        &mut memo,
        Source::Table {
            table_ref: "events".into(),
        },
    );
    let (series, series_state) = rate_state(
        &mut memo,
        Source::TimeSeries {
            metric: "c_total".into(),
        },
    );
    let evaluation_grid_of_group = vec![None; memo.groups().len()];
    let logical = logical(
        memo,
        vec![
            edge(table, table_state, 0, true),
            edge(series, series_state, 0, true),
        ],
        evaluation_grid_of_group,
    );
    let at_ingestion = |physical: &PhysicalASAPMemoCandidates, state| {
        offered(physical, state).contains(&properties(IT_ACROSS))
    };
    let unbounded = generate(&logical, &ingesting(None), &retaining_everything());
    assert!(!at_ingestion(&unbounded, table_state));
    assert!(at_ingestion(&unbounded, series_state));
    let bounded = generate(&logical, &ingesting(Some(2_000)), &retaining_everything());
    assert!(at_ingestion(&bounded, table_state));
    assert!(at_ingestion(&bounded, series_state));
}

/// `topk(5, count by (job) (m))` as a Count-Min heap keyed by `job` and
/// weighted by the sample value: pruned in every alternative unless the
/// data declares `m`'s values non-negative, and then offered.
#[test]
fn a_count_min_build_needs_weights_proven_non_negative() {
    let heap = |column_signs: BTreeMap<String, ValueSign>| {
        let data = DataWorkload {
            sources: [(
                "m".to_string(),
                SourceFacts {
                    column_signs,
                    ..SourceFacts::default()
                },
            )]
            .into(),
            ..DataWorkload::default()
        };
        let mut memo = Memo::new(&data);
        let scan = scan_group(&mut memo, "m", &["job"], vec![]);
        let state = sketch_group(
            &mut memo,
            Operator::ASAP(ASAPOp::SummaryAgg {
                child: scan,
                summary_type: FieldDataType::Sketch(SketchAlgorithm::CmsWithHeap),
                input: SummaryUpdate {
                    item: Some(SummaryInputExpr::EntityIdentity(
                        EntityIdentity::PromqlLabelSet { excluding: vec![] },
                    )),
                    weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
                },
                reduction: Reduction::Reduce(Default::default()),
                filter: None,
            }),
            SketchAlgorithm::CmsWithHeap,
            1,
        );
        query(&mut memo, state, None);
        let (scan, state) = (candidate_id(scan, 0), candidate_id(state, 0));
        let evaluation_grid_of_group = vec![None; memo.groups().len()];
        let logical = logical(
            memo,
            vec![edge(scan, state, 0, true)],
            evaluation_grid_of_group,
        );
        let physical = generate(&logical, &ingesting(None), &retaining_everything());
        (offered(&physical, state), pruned(&physical, state))
    };
    let (kept, unproven) = heap(BTreeMap::new());
    assert_eq!(kept, vec![]);
    assert_eq!(
        unproven,
        pruned_with(
            &[QT_NONE, QT_ONE, QT_ACROSS, IT_ACROSS],
            PruneReason::CountMinWeightsNotProvenNonNegative
        )
    );
    let (kept, unproven) = heap(BTreeMap::from([(
        "value".to_string(),
        ValueSign::NonNegative,
    )]));
    assert_eq!(
        kept,
        physical_properties_list(&[QT_NONE, QT_ONE, QT_ACROSS, IT_ACROSS])
    );
    assert_eq!(unproven, vec![]);
}

/// `(x[1m]) @ T` lowers as `TimeShift@T(TimeRange 1m (Scan))`: the scan is
/// read back without bound through the range just as the range is, so
/// neither is materialized across evaluations, offered or pruned.
#[test]
fn every_group_below_an_absolute_anchor_is_not_materialized_across_evaluations() {
    let mut memo = memo();
    let scan = scan_group(&mut memo, "x", &[], vec![]);
    let last_minute = group(&mut memo, range(60_000, scan));
    let pinned = group(
        &mut memo,
        shift(
            0,
            Some(AtModifier::Timestamp(1_700_000_000_000)),
            last_minute,
        ),
    );
    query(&mut memo, pinned, None);
    let [scan, last_minute, pinned] =
        [scan, last_minute, pinned].map(|group| candidate_id(group, 0));
    let evaluation_grid_of_group = vec![GRID_60S; memo.groups().len()];
    let logical = logical(
        memo,
        vec![
            edge(scan, last_minute, 0, true),
            edge(last_minute, pinned, 0, true),
        ],
        evaluation_grid_of_group,
    );
    let physical = generate(&logical, &ingesting(None), &retaining_everything());
    for candidate in [scan, last_minute] {
        let alternatives = offered(&physical, candidate);
        let pruned_alternatives = pruned(&physical, candidate);
        assert!(
            !any_across_evaluations(
                alternatives
                    .iter()
                    .chain(pruned_alternatives.iter().map(|(properties, _)| properties))
            ),
            "{candidate:?}: {alternatives:?}"
        );
    }
    assert!(offered(&physical, scan).contains(&properties(QT_NONE)));
}

/// `max_over_time(m[1m] @ 90)` on a fixed interval without a phase (no
/// known grid): the `Max` state over the pinned minute does not depend on
/// the evaluation time, so its one result is materialized across
/// evaluations and read at any time; built at ingestion time, it reads
/// the pinned minute's ingestion-time alternative, which is then kept.
#[test]
fn a_time_independent_group_without_a_grid_is_materialized_across_evaluations() {
    let mut memo = memo();
    let scan = scan_group(&mut memo, "m", &[], vec![]);
    let last_minute = group(&mut memo, range(60_000, scan));
    let pinned = group(
        &mut memo,
        shift(0, Some(AtModifier::Timestamp(90_000)), last_minute),
    );
    let max = group(
        &mut memo,
        state(FieldDataType::exact(ExactKind::Max), pinned),
    );
    query(&mut memo, max, None);
    let [scan, last_minute, pinned, max] =
        [scan, last_minute, pinned, max].map(|group| candidate_id(group, 0));
    let evaluation_grid_of_group = vec![None; memo.groups().len()];
    let logical = logical(
        memo,
        vec![
            edge(scan, last_minute, 0, true),
            edge(last_minute, pinned, 0, true),
            edge(pinned, max, 0, true),
        ],
        evaluation_grid_of_group,
    );
    let physical = generate(&logical, &ingesting(None), &retaining_everything());
    assert_eq!(
        offered(&physical, max),
        physical_properties_list(&[QT_NONE, QT_ONE, QT_ACROSS, IT_ACROSS])
    );
    assert!(offered(&physical, pinned).contains(&properties(IT_NONE)));
    assert_eq!(pruned(&physical, pinned), vec![]);
}
