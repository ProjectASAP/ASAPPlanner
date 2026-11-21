//! Deployment capabilities: what each one left out expands into, how an
//! exclusion matches a physical candidate, and the JSON a caller writes.

use super::*;
use crate::ir::operator::arguments::{Reduction, Source};
use crate::ir::operator::AggIntent;
use crate::ir::operator::OperatorKind;
use crate::ir::properties::ExecutionTiming::{self, IngestionTime, QueryTime};
use crate::ir::scalar::ColumnRef;
use crate::ir::schema::{FieldDataType, Schema};
use crate::ir::{ASAPOp, NonASAPOp, Operator};
use crate::stages::{Lifetime, PhysicalProperties, StorageMedium};
use crate::summary_type::{
    ExactKind, ExactParams, SketchParameterRange, SketchParams, SketchStatistic, StatisticKind,
    SummaryFamily, SummaryTypeCapability, SummaryUpdate,
};

// ---------------------------------------------------------------------------
// Fixtures: operators over child ids `usize`, and the five (τ, ℓ) Stage 2
// offers.

fn properties(timing: ExecutionTiming, lifetime: Lifetime) -> PhysicalProperties {
    PhysicalProperties::new(timing, lifetime).unwrap()
}
fn qt_none() -> PhysicalProperties {
    properties(QueryTime, Lifetime::None)
}
fn qt_one() -> PhysicalProperties {
    properties(QueryTime, Lifetime::OneEvaluation(StorageMedium::Memory))
}
fn qt_across() -> PhysicalProperties {
    properties(
        QueryTime,
        Lifetime::AcrossEvaluations(StorageMedium::Memory),
    )
}
fn it_none() -> PhysicalProperties {
    properties(IngestionTime, Lifetime::None)
}
fn it_across() -> PhysicalProperties {
    properties(
        IngestionTime,
        Lifetime::AcrossEvaluations(StorageMedium::Memory),
    )
}

/// A scan of the metric `m`.
fn scan() -> Operator<usize> {
    Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "m".to_string(),
        },
        predicates: vec![],
        schema: Schema::default(),
    })
}

/// An `Aggregate` of `measures` with `reduction` over child 0.
fn aggregate(reduction: Reduction, measures: Vec<AggIntent>) -> Operator<usize> {
    Operator::NonASAP(NonASAPOp::Aggregate {
        reduction,
        output_names: vec![String::new(); measures.len()],
        measures,
        filters: vec![],
        having: None,
        child: 0,
    })
}

/// A per-series `Aggregate` of the one `measure`.
fn per_series(measure: AggIntent) -> Operator<usize> {
    aggregate(Reduction::PerEntity, vec![measure])
}

/// A per-series `SummaryAgg` of `summary_type` over the sample value.
fn build(summary_type: FieldDataType) -> Operator<usize> {
    Operator::ASAP(ASAPOp::SummaryAgg {
        child: 0,
        summary_type,
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
    })
}

/// A per-series tumbling-window chain of `summary_type`, 60 s windows.
fn chain(summary_type: FieldDataType) -> Operator<usize> {
    Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: 0,
        summary_type,
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
        tumbling_window_width_ms: 60_000,
    })
}

/// A merge of child 0's states, keeping its groups.
fn merge() -> Operator<usize> {
    Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![0],
        group_by: None,
    })
}

/// A readout of `statistic` from child 0's state.
fn estimate(statistic: SketchStatistic) -> Operator<usize> {
    Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: 0,
        query: statistic,
    })
}

fn exact(kind: ExactKind) -> FieldDataType {
    FieldDataType::exact(kind)
}
fn kll() -> FieldDataType {
    FieldDataType::Sketch(SketchAlgorithm::Kll)
}

/// `operator` as a candidate at `physical_properties`, with θ `parameters`.
fn described<'a>(
    operator: &'a Operator<usize>,
    parameters: Option<&'a SketchParams>,
    physical_properties: PhysicalProperties,
) -> CandidateDescription<'a, usize> {
    CandidateDescription {
        operator,
        parameters,
        physical_properties,
        read_summary_type: None,
    }
}

/// Whether `exclusion` matches `operator` at `physical_properties`, no θ.
fn matches(
    exclusion: &CandidateExclusion,
    operator: &Operator<usize>,
    physical_properties: PhysicalProperties,
) -> bool {
    exclusion.matches(&described(operator, None, physical_properties))
}

fn total() -> SketchStatistic {
    SketchStatistic::PointCount {
        key: ColumnRef::SampleValue,
        value: None,
    }
}

// ---------------------------------------------------------------------------
// Matching.

/// Timing and operator together: only the query-time scan matches.
#[test]
fn an_exclusion_matches_on_timing_and_operator() {
    let exclusion = CandidateExclusion {
        timing: Some(QueryTime),
        operator: Some(OperatorKind::Scan),
        ..Default::default()
    };
    assert!(matches(&exclusion, &scan(), qt_none()));
    assert!(!matches(&exclusion, &scan(), it_none()));
    assert!(!matches(
        &exclusion,
        &per_series(AggIntent::Max { col: None }),
        qt_none()
    ));
}

/// A field left unset matches any value: with no timing, both scans match.
#[test]
fn an_unset_field_matches_any_value() {
    let exclusion = CandidateExclusion {
        operator: Some(OperatorKind::Scan),
        ..Default::default()
    };
    assert!(matches(&exclusion, &scan(), qt_none()));
    assert!(matches(&exclusion, &scan(), it_none()));
}

/// `kept_across_evaluations` reads the lifetime, whatever the timing: the
/// raw-data and query-time-result capabilities expand into it.
#[test]
fn kept_across_evaluations_matches_the_lifetime() {
    let kept = CandidateExclusion {
        kept_across_evaluations: Some(true),
        ..Default::default()
    };
    assert!(matches(&kept, &scan(), it_across()));
    assert!(matches(&kept, &scan(), qt_across()));
    assert!(!matches(&kept, &scan(), qt_one()));
    assert!(!matches(&kept, &scan(), it_none()));
    let not_kept = CandidateExclusion {
        kept_across_evaluations: Some(false),
        ..Default::default()
    };
    assert!(matches(&not_kept, &scan(), qt_one()));
    assert!(!matches(&not_kept, &scan(), it_across()));
}

/// `per_entity_reduction` matches only operators that carry a
/// `Reduction`; a scan has none, so it never matches `Some(_)`.
#[test]
fn per_entity_reduction_matches_only_operators_with_a_reduction() {
    let max_over_time = per_series(AggIntent::Max { col: None });
    let by_job = aggregate(Reduction::by(vec![1]), vec![AggIntent::Sum { col: None }]);
    let per_entity_aggregate = CandidateExclusion {
        operator: Some(OperatorKind::Aggregate),
        per_entity_reduction: Some(true),
        ..Default::default()
    };
    assert!(matches(&per_entity_aggregate, &max_over_time, qt_none()));
    assert!(!matches(&per_entity_aggregate, &by_job, qt_none()));
    let per_entity = CandidateExclusion {
        per_entity_reduction: Some(true),
        ..Default::default()
    };
    assert!(matches(&per_entity, &build(kll()), qt_one()));
    assert!(!matches(&per_entity, &scan(), qt_none()));
    let grouped = CandidateExclusion {
        per_entity_reduction: Some(false),
        ..Default::default()
    };
    assert!(matches(&grouped, &by_job, qt_none()));
}

/// A family and a θ range match the operators that build state only: the
/// `SummaryAgg` and the chain, never a merge, although the merge over a
/// KLL chain also carries the chain's θ (as the selected plans of a KLL
/// quantile do, on their `TimeRange` and `SummaryMerge`).
#[test]
fn summary_family_and_parameter_range_match_state_builds_only() {
    let k547 = SketchParams::Kll { k: 547 };
    let k1404 = SketchParams::Kll { k: 1404 };
    let kll_build = build(kll());
    let kll_chain = chain(kll());
    let kll_merge = merge();
    let nodes = [
        described(&kll_build, Some(&k547), qt_one()),
        described(&kll_build, Some(&k1404), qt_one()),
        described(&kll_chain, Some(&k547), it_across()),
        described(&kll_merge, Some(&k547), qt_one()),
    ];
    let family = CandidateExclusion {
        summary_family: Some(SummaryFamily::Sketch(SketchAlgorithm::Kll)),
        ..Default::default()
    };
    assert_eq!(
        nodes.map(|node| family.matches(&node)),
        [true, true, true, false]
    );
    let below_1404 = CandidateExclusion {
        sketch_parameter_range: Some(SketchParameterRange::KllK {
            at_least: None,
            at_most: Some(1403),
        }),
        ..Default::default()
    };
    assert_eq!(
        nodes.map(|node| below_1404.matches(&node)),
        [true, false, true, false]
    );
    let exact_sum = build(FieldDataType::ExactAggregate(
        ExactKind::Sum,
        ExactParams::Sum,
    ));
    let exclusions_of = |exclusion: CandidateExclusion| matches(&exclusion, &exact_sum, qt_one());
    assert!(exclusions_of(CandidateExclusion {
        summary_family: Some(SummaryFamily::Exact(ExactKind::Sum)),
        ..Default::default()
    }));
    assert!(!exclusions_of(CandidateExclusion {
        summary_family: Some(SummaryFamily::Exact(ExactKind::Count)),
        ..Default::default()
    }));
    assert!(!exclusions_of(below_1404));
}

/// An aggregate's measure matches an `Aggregate` one of whose measures is
/// of that kind, so a target whose query-time `rate` is excluded is not
/// given an `Aggregate[rate]` instead; a state build of a `Rate` state is
/// not an aggregate.
#[test]
fn aggregate_measure_matches_an_aggregate_computing_it() {
    let query_time_rate = CandidateExclusion {
        timing: Some(QueryTime),
        aggregate_measure: Some("rate".to_string()),
        ..Default::default()
    };
    assert!(matches(
        &query_time_rate,
        &per_series(AggIntent::Rate),
        qt_none()
    ));
    assert!(matches(
        &query_time_rate,
        &aggregate(
            Reduction::PerEntity,
            vec![AggIntent::Sum { col: None }, AggIntent::Rate]
        ),
        qt_none()
    ));
    assert!(!matches(
        &query_time_rate,
        &per_series(AggIntent::Sum { col: None }),
        qt_none()
    ));
    assert!(!matches(
        &query_time_rate,
        &per_series(AggIntent::Rate),
        it_none()
    ));
    assert!(!matches(
        &query_time_rate,
        &build(exact(ExactKind::Counter)),
        qt_one()
    ));
    // A measure with arguments is named by its kind alone.
    let quantile = CandidateExclusion {
        aggregate_measure: Some("quantile".to_string()),
        ..Default::default()
    };
    assert!(matches(
        &quantile,
        &per_series(AggIntent::Quantile { col: None, q: 0.99 }),
        qt_none()
    ));
}

/// Whether one of the exclusions `capabilities` expand into matches
/// `operator` at `physical_properties`, no θ: what Stage 2 prunes it for.
fn excluded_by(
    capabilities: &DeploymentCapabilities,
    operator: &Operator<usize>,
    physical_properties: PhysicalProperties,
) -> bool {
    capabilities
        .exclusions()
        .iter()
        .any(|e| matches(&e.exclusion, operator, physical_properties))
}

/// A family not mergeable across windows expands into its tumbling-window
/// chains (whose windows a `SummaryMerge` combines) and its builds kept
/// across evaluations (each evaluation merges in the rows that arrived
/// since). Together they rule out a rate state merged across windows, at
/// either timing, and keep a query-time rate build within one evaluation.
#[test]
fn a_family_not_mergeable_across_windows_expands_into_its_chains_and_kept_builds() {
    let not_mergeable = DeploymentCapabilities {
        state_not_mergeable_across_windows: vec![SummaryFamily::Exact(ExactKind::Counter)],
        query_time_results_retained: true,
        ..Default::default()
    };
    let rate = Some(SummaryFamily::Exact(ExactKind::Counter));
    let exclusion = |exclusion| DeploymentExclusion {
        source: ExclusionSource::StateNotMergeableAcrossWindows,
        exclusion,
    };
    assert_eq!(
        not_mergeable.exclusions(),
        vec![
            exclusion(CandidateExclusion {
                summary_family: rate,
                operator: Some(OperatorKind::SummaryTumblingWindowChain),
                ..Default::default()
            }),
            exclusion(CandidateExclusion {
                summary_family: rate,
                kept_across_evaluations: Some(true),
                ..Default::default()
            }),
        ]
    );
    let rate_build = build(exact(ExactKind::Counter));
    let rate_chain = chain(exact(ExactKind::Counter));
    let excluded =
        |operator: &Operator<usize>, properties| excluded_by(&not_mergeable, operator, properties);
    assert!(excluded(&rate_chain, it_across()));
    assert!(excluded(&rate_chain, qt_across()));
    assert!(excluded(&rate_build, qt_across()));
    assert!(excluded(&rate_build, it_across()));
    assert!(!excluded(&rate_build, qt_one()));
    assert!(!excluded(&rate_build, qt_none()));
    assert!(!excluded(&chain(exact(ExactKind::Sum)), it_across()));
    assert!(!excluded(&per_series(AggIntent::Rate), qt_none()));
}

/// The counter semantics a deployment lacks expand into the counter
/// functions (by their JSON `kind`) and counter states that need them:
/// without extrapolation `rate`, `increase`, `delta` and the `Rate` and
/// `Increase` states; without reset handling `rate`, `increase`, `irate`
/// (`"i_rate"`), `resets` and the `Rate`, `Increase` and `IRate` states.
/// `irate` does not extrapolate, so only reset handling rules it out.
#[test]
fn counter_semantics_left_out_expand_into_the_functions_and_states_that_need_them() {
    for (measure, name) in [
        (AggIntent::Rate, "rate"),
        (AggIntent::Increase, "increase"),
        (AggIntent::Delta, "delta"),
        (AggIntent::IRate, "i_rate"),
        (AggIntent::Resets, "resets"),
    ] {
        assert_eq!(measure.kind_name(), name);
    }
    let without = |reset_handling, extrapolation| DeploymentCapabilities {
        counter_semantics: CounterSemanticsSupport {
            reset_handling,
            extrapolation,
        },
        query_time_results_retained: true,
        ..Default::default()
    };
    let measure = |source, name: &str| DeploymentExclusion {
        source,
        exclusion: CandidateExclusion {
            aggregate_measure: Some(name.to_string()),
            ..Default::default()
        },
    };
    let state = |source, kind| DeploymentExclusion {
        source,
        exclusion: CandidateExclusion {
            summary_family: Some(SummaryFamily::Exact(kind)),
            ..Default::default()
        },
    };
    let not_extrapolated = ExclusionSource::CounterNotExtrapolated;
    assert_eq!(
        without(true, false).exclusions(),
        vec![
            measure(not_extrapolated, "rate"),
            measure(not_extrapolated, "increase"),
            measure(not_extrapolated, "delta"),
            state(not_extrapolated, ExactKind::Counter),
        ]
    );
    let resets_not_handled = ExclusionSource::CounterResetsNotHandled;
    assert_eq!(
        without(false, true).exclusions(),
        vec![
            measure(resets_not_handled, "rate"),
            measure(resets_not_handled, "increase"),
            measure(resets_not_handled, "i_rate"),
            measure(resets_not_handled, "resets"),
            state(resets_not_handled, ExactKind::Counter),
            state(resets_not_handled, ExactKind::IRate),
        ]
    );

    // What each expansion rules out, function by function.
    let functions = [
        per_series(AggIntent::Rate),
        per_series(AggIntent::Increase),
        per_series(AggIntent::Delta),
        per_series(AggIntent::IRate),
        per_series(AggIntent::Resets),
        per_series(AggIntent::Sum { col: None }),
        build(exact(ExactKind::Counter)),
        chain(exact(ExactKind::Counter)),
        build(exact(ExactKind::IRate)),
        build(exact(ExactKind::Sum)),
        scan(),
    ];
    let ruled_out = |capabilities: &DeploymentCapabilities| {
        functions
            .iter()
            .map(|f| excluded_by(capabilities, f, qt_one()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ruled_out(&without(true, false)),
        [true, true, true, false, false, false, true, true, false, false, false]
    );
    assert_eq!(
        ruled_out(&without(false, true)),
        [true, true, false, true, true, false, true, true, true, false, false]
    );
}

/// A readout matches a `SummaryEstimate` of that statistic reading a state
/// of that algorithm, and nothing else.
#[test]
fn readout_matches_an_estimate_by_statistic_and_the_state_it_reads() {
    let kll_total = CandidateExclusion {
        readout: Some(SketchReadout {
            summary_type: SketchAlgorithm::Kll,
            statistic: StatisticKind::Total,
        }),
        ..Default::default()
    };
    let total_readout = estimate(total());
    let quantile_readout = estimate(SketchStatistic::Quantile { q: 0.99 });
    let cms = FieldDataType::Sketch(SketchAlgorithm::Cms);
    let reading = |operator, state| CandidateDescription {
        read_summary_type: state,
        ..described(operator, None, qt_none())
    };
    let kll_state = kll();
    assert!(kll_total.matches(&reading(&total_readout, Some(&kll_state))));
    assert!(!kll_total.matches(&reading(&quantile_readout, Some(&kll_state))));
    assert!(!kll_total.matches(&reading(&total_readout, Some(&cms))));
    assert!(!kll_total.matches(&reading(&total_readout, None)));
    assert!(!matches(&kll_total, &build(kll()), qt_one()));
}

// ---------------------------------------------------------------------------
// Expansion.

/// The query-time-results exclusion of the default, the one Stage 2 applies
/// today as `QueryTimeResultsNotRetained`.
fn query_time_results_exclusion() -> DeploymentExclusion {
    DeploymentExclusion {
        source: ExclusionSource::QueryTimeResultsNotRetained,
        exclusion: CandidateExclusion {
            timing: Some(QueryTime),
            kept_across_evaluations: Some(true),
            ..Default::default()
        },
    }
}

/// The default allows everything the planner builds: its only exclusion is
/// the query-time results kept across evaluations it does not retain, so
/// planning with the defaults prunes what it prunes today.
#[test]
fn default_capabilities_expand_into_the_query_time_results_exclusion_only() {
    assert_eq!(
        DeploymentCapabilities::default().exclusions(),
        vec![query_time_results_exclusion()]
    );
    let retaining_everything = DeploymentCapabilities {
        query_time_results_retained: true,
        ..Default::default()
    };
    assert_eq!(retaining_everything.exclusions(), vec![]);
}

/// Each capability left out expands into its exclusions, in the
/// documented order, and the caller's own exclusions come last with their
/// index.
#[test]
fn each_capability_left_out_expands_into_its_exclusions() {
    let stored_only = CandidateExclusion {
        timing: Some(QueryTime),
        operator: Some(OperatorKind::Scan),
        ..Default::default()
    };
    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        query_time_results_retained: false,
        counter_semantics: CounterSemanticsSupport {
            reset_handling: false,
            extrapolation: false,
        },
        state_not_mergeable_across_windows: vec![SummaryFamily::Exact(ExactKind::Counter)],
        excluded_candidates: vec![stored_only.clone()],
        ..Default::default()
    };
    let exclusion = |source, exclusion| DeploymentExclusion { source, exclusion };
    // The counter and mergeability expansions, item by item, are their own
    // tests; here only their place in the order.
    let sources: Vec<ExclusionSource> =
        capabilities.exclusions().iter().map(|e| e.source).collect();
    let mut expected_sources = vec![
        ExclusionSource::RawDataNotRetained,
        ExclusionSource::RawDataNotRetained,
        ExclusionSource::QueryTimeResultsNotRetained,
    ];
    expected_sources.extend([ExclusionSource::CounterResetsNotHandled; 6]);
    expected_sources.extend([ExclusionSource::CounterNotExtrapolated; 4]);
    expected_sources.extend([ExclusionSource::StateNotMergeableAcrossWindows; 2]);
    expected_sources.push(ExclusionSource::ExcludedCandidate { index: 0 });
    assert_eq!(sources, expected_sources);
    let exclusions = capabilities.exclusions();
    let first_three_and_last: Vec<DeploymentExclusion> = exclusions[..3]
        .iter()
        .chain(exclusions.last())
        .cloned()
        .collect();
    assert_eq!(
        first_three_and_last,
        vec![
            exclusion(
                ExclusionSource::RawDataNotRetained,
                CandidateExclusion {
                    timing: Some(QueryTime),
                    operator: Some(OperatorKind::Scan),
                    ..Default::default()
                }
            ),
            exclusion(
                ExclusionSource::RawDataNotRetained,
                CandidateExclusion {
                    operator: Some(OperatorKind::Scan),
                    kept_across_evaluations: Some(true),
                    ..Default::default()
                }
            ),
            query_time_results_exclusion(),
            exclusion(ExclusionSource::ExcludedCandidate { index: 0 }, stored_only),
        ]
    );
}

/// `supported_summaries` expands into one family exclusion per sketch
/// algorithm it does not list, and one readout exclusion per statistic a
/// listed algorithm answers (its capability table row) but the deployment
/// does not.
#[test]
fn supported_summaries_expand_into_family_and_readout_exclusions() {
    let capabilities = DeploymentCapabilities {
        supported_summaries: Some(vec![SupportedSummary {
            summary_type: SketchAlgorithm::Kll,
            readouts: vec![StatisticKind::Quantile],
        }]),
        query_time_results_retained: true,
        ..Default::default()
    };
    let exclusions = capabilities.exclusions();
    let family = |algorithm| DeploymentExclusion {
        source: ExclusionSource::SummaryNotSupported,
        exclusion: CandidateExclusion {
            summary_family: Some(SummaryFamily::Sketch(algorithm)),
            ..Default::default()
        },
    };
    let readout = |statistic| DeploymentExclusion {
        source: ExclusionSource::ReadoutNotSupported,
        exclusion: CandidateExclusion {
            readout: Some(SketchReadout {
                summary_type: SketchAlgorithm::Kll,
                statistic,
            }),
            ..Default::default()
        },
    };
    let unlisted: Vec<_> = SketchAlgorithm::ALL
        .iter()
        .filter(|&&algorithm| algorithm != SketchAlgorithm::Kll)
        .map(|&algorithm| family(algorithm))
        .collect();
    let unsupported_readouts: Vec<_> = SummaryTypeCapability::of(&kll())
        .readouts()
        .iter()
        .map(|&(statistic, _)| statistic)
        .filter(|&statistic| statistic != StatisticKind::Quantile)
        .map(readout)
        .collect();
    for expected in unlisted.iter().chain(&unsupported_readouts) {
        assert!(exclusions.contains(expected), "missing {expected:?}");
    }
    assert!(!exclusions.contains(&family(SketchAlgorithm::Kll)));
    assert!(!exclusions.contains(&readout(StatisticKind::Quantile)));
    assert_eq!(
        exclusions.len(),
        unlisted.len() + unsupported_readouts.len()
    );
}

// ---------------------------------------------------------------------------
// Validation and JSON.

/// An exclusion that cannot be applied is bad input, named by its index:
/// one that sets no field (it would exclude every candidate), an empty
/// range, an α that is not a number in (0, 1), a family its θ range
/// contradicts, or a measure no aggregate has.
#[test]
fn invalid_excluded_candidates_are_rejected() {
    let data = crate::workload::DataWorkload::default();
    let valid = CandidateExclusion {
        operator: Some(OperatorKind::Scan),
        ..Default::default()
    };
    let with = |bad: CandidateExclusion| DeploymentCapabilities {
        excluded_candidates: vec![valid.clone(), bad],
        ..Default::default()
    };
    let range = |range| CandidateExclusion {
        sketch_parameter_range: Some(range),
        ..Default::default()
    };
    let invalid = [
        CandidateExclusion::default(),
        range(SketchParameterRange::KllK {
            at_least: Some(1404),
            at_most: Some(547),
        }),
        range(SketchParameterRange::DDSketchAlpha {
            at_least: Some(f64::NAN),
            at_most: None,
        }),
        range(SketchParameterRange::DDSketchAlpha {
            at_least: None,
            at_most: Some(1.0),
        }),
        CandidateExclusion {
            summary_family: Some(SummaryFamily::Sketch(SketchAlgorithm::DDSketch)),
            sketch_parameter_range: Some(SketchParameterRange::KllK {
                at_least: None,
                at_most: Some(1403),
            }),
            ..Default::default()
        },
        CandidateExclusion {
            summary_family: Some(SummaryFamily::Exact(ExactKind::Sum)),
            sketch_parameter_range: Some(SketchParameterRange::KmvK {
                at_least: Some(4323),
                at_most: None,
            }),
            ..Default::default()
        },
        CandidateExclusion {
            aggregate_measure: Some("rte".to_string()),
            ..Default::default()
        },
    ];
    for bad in invalid {
        let result = with(bad.clone()).validate(&data);
        assert!(
            matches!(
                result,
                Err(DeploymentCapabilitiesError::InvalidExclusion { index: 1, .. })
            ),
            "{bad:?}: {result:?}"
        );
    }
    assert_eq!(DeploymentCapabilities::default().validate(&data), Ok(()));
    let valid_only = DeploymentCapabilities {
        excluded_candidates: vec![valid],
        ..Default::default()
    };
    assert_eq!(valid_only.validate(&data), Ok(()));
}

/// An exclusion whose fields match no operator in common contradicts
/// itself and matches nothing, so it is bad input: a state family, θ range
/// (state builds only) with another operator, an
/// `aggregate_measure` (an `Aggregate` only) with another operator, a
/// `readout` (a `SummaryEstimate` only) with another operator or with a
/// build-only field, and `per_entity_reduction` with an operator that has
/// no reduction. Fields that do share an operator are accepted.
#[test]
fn self_contradicting_exclusions_are_rejected() {
    let data = crate::workload::DataWorkload::default();
    let kll_readout = SketchReadout {
        summary_type: SketchAlgorithm::Kll,
        statistic: StatisticKind::Quantile,
    };
    let with_operator = |operator, exclusion: CandidateExclusion| CandidateExclusion {
        operator: Some(operator),
        ..exclusion
    };
    let family = CandidateExclusion {
        summary_family: Some(SummaryFamily::Exact(ExactKind::Counter)),
        ..Default::default()
    };
    let parameters = CandidateExclusion {
        sketch_parameter_range: Some(SketchParameterRange::KllK {
            at_least: None,
            at_most: Some(1403),
        }),
        ..Default::default()
    };
    let measure = CandidateExclusion {
        aggregate_measure: Some("rate".to_string()),
        ..Default::default()
    };
    let readout = CandidateExclusion {
        readout: Some(kll_readout),
        ..Default::default()
    };
    let per_entity = CandidateExclusion {
        per_entity_reduction: Some(true),
        ..Default::default()
    };
    let contradicting = [
        // The review's repro: `{"operator": "SummaryMerge", "summary_family":
        // {"exact": "Counter"}}`.
        with_operator(OperatorKind::SummaryMerge, family.clone()),
        with_operator(OperatorKind::SummaryEstimate, parameters.clone()),
        with_operator(OperatorKind::SummaryAgg, measure.clone()),
        with_operator(OperatorKind::SummaryAgg, readout.clone()),
        CandidateExclusion {
            readout: Some(kll_readout),
            ..family.clone()
        },
        CandidateExclusion {
            readout: Some(kll_readout),
            ..parameters.clone()
        },
        with_operator(OperatorKind::Scan, per_entity.clone()),
    ];
    for bad in contradicting {
        let capabilities = DeploymentCapabilities {
            excluded_candidates: vec![bad.clone()],
            ..Default::default()
        };
        let result = capabilities.validate(&data);
        assert!(
            matches!(
                result,
                Err(DeploymentCapabilitiesError::InvalidExclusion { index: 0, .. })
            ),
            "{bad:?}: {result:?}"
        );
    }
    let consistent = [
        with_operator(OperatorKind::SummaryTumblingWindowChain, family.clone()),
        with_operator(OperatorKind::SummaryAgg, parameters),
        with_operator(OperatorKind::Aggregate, measure.clone()),
        with_operator(OperatorKind::SummaryEstimate, readout),
        with_operator(OperatorKind::SummaryAgg, per_entity.clone()),
        CandidateExclusion {
            per_entity_reduction: Some(true),
            ..measure
        },
        CandidateExclusion {
            per_entity_reduction: Some(true),
            ..family
        },
    ];
    for good in consistent {
        let capabilities = DeploymentCapabilities {
            excluded_candidates: vec![good.clone()],
            ..Default::default()
        };
        assert_eq!(capabilities.validate(&data), Ok(()), "{good:?}");
    }
}

/// Only an exclusion that sets `summary_family` and nothing else rules a
/// family out outright, as `supported_summaries` does for an algorithm it
/// does not list; any further field narrows it.
#[test]
fn a_family_is_excluded_outright_only_by_a_family_only_exclusion() {
    let kll = SummaryFamily::Sketch(SketchAlgorithm::Kll);
    let family_only = CandidateExclusion {
        summary_family: Some(kll),
        ..Default::default()
    };
    assert_eq!(family_only.family_excluded_outright(), Some(kll));
    let narrower = CandidateExclusion {
        operator: Some(OperatorKind::SummaryTumblingWindowChain),
        ..family_only
    };
    assert_eq!(narrower.family_excluded_outright(), None);
    let scans = CandidateExclusion {
        operator: Some(OperatorKind::Scan),
        ..Default::default()
    };
    assert_eq!(scans.family_excluded_outright(), None);
    let exact_only = DeploymentCapabilities {
        supported_summaries: Some(vec![]),
        ..Default::default()
    };
    let outright: Vec<SummaryFamily> = exact_only
        .exclusions()
        .iter()
        .filter_map(|e| e.exclusion.family_excluded_outright())
        .collect();
    let every_sketch: Vec<SummaryFamily> = SketchAlgorithm::ALL
        .iter()
        .map(|&algorithm| SummaryFamily::Sketch(algorithm))
        .collect();
    assert_eq!(outright, every_sketch);
}

/// The exclusions of an arm with no query-time scan, written as the caller
/// writes them, plus a KLL size range: they load field by field and
/// round-trip; a misspelled or removed field is refused; `{}` is the
/// default; and the new capabilities fill their defaults field by field.
#[test]
fn excluded_candidates_json_loads_field_by_field() {
    let json = r#"{"excluded_candidates": [
        {"timing": "query_time", "operator": "Scan"},
        {"operator": "Aggregate", "per_entity_reduction": true},
        {"timing": "query_time", "operator": "SummaryAgg"},
        {"timing": "query_time", "operator": "SummaryTumblingWindowChain"},
        {"summary_family": {"sketch": "Kll"}, "sketch_parameter_range": {"kll_k": {"at_most": 1403}}},
        {"timing": "query_time", "aggregate_measure": "rate"},
        {"summary_family": {"exact": "Counter"}, "operator": "SummaryTumblingWindowChain"},
        {"readout": {"summary_type": "Kll", "statistic": "Total"}},
        {"operator": "Scan", "kept_across_evaluations": true}]}"#;
    let loaded: DeploymentCapabilities = serde_json::from_str(json).unwrap();
    let exclusion = |exclusion: CandidateExclusion| exclusion;
    assert_eq!(
        loaded.excluded_candidates,
        vec![
            exclusion(CandidateExclusion {
                timing: Some(QueryTime),
                operator: Some(OperatorKind::Scan),
                ..Default::default()
            }),
            exclusion(CandidateExclusion {
                operator: Some(OperatorKind::Aggregate),
                per_entity_reduction: Some(true),
                ..Default::default()
            }),
            exclusion(CandidateExclusion {
                timing: Some(QueryTime),
                operator: Some(OperatorKind::SummaryAgg),
                ..Default::default()
            }),
            exclusion(CandidateExclusion {
                timing: Some(QueryTime),
                operator: Some(OperatorKind::SummaryTumblingWindowChain),
                ..Default::default()
            }),
            exclusion(CandidateExclusion {
                summary_family: Some(SummaryFamily::Sketch(SketchAlgorithm::Kll)),
                sketch_parameter_range: Some(SketchParameterRange::KllK {
                    at_least: None,
                    at_most: Some(1403),
                }),
                ..Default::default()
            }),
            exclusion(CandidateExclusion {
                timing: Some(QueryTime),
                aggregate_measure: Some("rate".to_string()),
                ..Default::default()
            }),
            exclusion(CandidateExclusion {
                summary_family: Some(SummaryFamily::Exact(ExactKind::Counter)),
                operator: Some(OperatorKind::SummaryTumblingWindowChain),
                ..Default::default()
            }),
            exclusion(CandidateExclusion {
                readout: Some(SketchReadout {
                    summary_type: SketchAlgorithm::Kll,
                    statistic: StatisticKind::Total,
                }),
                ..Default::default()
            }),
            exclusion(CandidateExclusion {
                operator: Some(OperatorKind::Scan),
                kept_across_evaluations: Some(true),
                ..Default::default()
            }),
        ]
    );
    let written = serde_json::to_string(&loaded).unwrap();
    assert_eq!(
        serde_json::from_str::<DeploymentCapabilities>(&written).unwrap(),
        loaded
    );
    for refused in [
        r#"{"excluded_candidates": [{"timing": "query_time", "operater": "Scan"}]}"#,
        // Counter semantics and mergeability are capabilities, not
        // exclusion fields; θ ranges are `sketch_parameter_range`.
        r#"{"excluded_candidates": [{"counter_semantics": "extrapolation"}]}"#,
        r#"{"excluded_candidates": [{"summary_family": {"exact": "Counter"}, "merged_across_windows": true}]}"#,
        r#"{"excluded_candidates": [{"sketch_parameters": {"kll_k": {"at_most": 1403}}}]}"#,
    ] {
        assert!(
            serde_json::from_str::<DeploymentCapabilities>(refused).is_err(),
            "{refused}"
        );
    }
    assert_eq!(
        serde_json::from_str::<DeploymentCapabilities>("{}").unwrap(),
        DeploymentCapabilities::default()
    );
    let counters: DeploymentCapabilities = serde_json::from_str(
        r#"{"counter_semantics": {"extrapolation": false},
            "state_not_mergeable_across_windows": [{"exact": "Counter"}]}"#,
    )
    .unwrap();
    assert_eq!(
        counters.counter_semantics,
        CounterSemanticsSupport {
            reset_handling: true,
            extrapolation: false,
        }
    );
    assert_eq!(
        counters.state_not_mergeable_across_windows,
        vec![SummaryFamily::Exact(ExactKind::Counter)]
    );
    assert!(serde_json::from_str::<DeploymentCapabilities>(
        r#"{"counter_semantics": {"extrapolates": false}}"#
    )
    .is_err());
}

/// The default retains raw data and keeps no query-time result across
/// evaluations; a JSON object with only some fields keeps the defaults for
/// the rest, and an unknown field is refused.
#[test]
fn default_capabilities_and_json_fill_the_rest() {
    let default = DeploymentCapabilities::default();
    assert!(default.raw_data_retained && !default.query_time_results_retained);
    assert_eq!(default.supported_summaries, None);
    let parsed: DeploymentCapabilities = serde_json::from_str(
        r#"{"supported_summaries": [{"summary_type": "Kll", "readouts": ["Quantile"]}]}"#,
    )
    .unwrap();
    assert_eq!(
        parsed.supported_summaries,
        Some(vec![SupportedSummary {
            summary_type: SketchAlgorithm::Kll,
            readouts: vec![StatisticKind::Quantile],
        }])
    );
    assert!(parsed.raw_data_retained && !parsed.query_time_results_retained);
    assert!(serde_json::from_str::<DeploymentCapabilities>(r#"{"raw_data": false}"#).is_err());
}

/// A source prints as the capabilities' JSON names it, the caller's own
/// exclusion by its index; only the default deployment's own source (no
/// query-time result kept across evaluations) is in the default
/// deployment.
#[test]
fn exclusion_sources_print_as_their_json_names() {
    let every_source = [
        ExclusionSource::SummaryNotSupported,
        ExclusionSource::ReadoutNotSupported,
        ExclusionSource::RawDataNotRetained,
        ExclusionSource::QueryTimeResultsNotRetained,
        ExclusionSource::CounterResetsNotHandled,
        ExclusionSource::CounterNotExtrapolated,
        ExclusionSource::StateNotMergeableAcrossWindows,
        ExclusionSource::ExcludedCandidate { index: 2 },
    ];
    for source in every_source {
        let printed = source.to_string();
        match source {
            ExclusionSource::ExcludedCandidate { .. } => {
                assert_eq!(printed, "excluded_candidates[2]")
            }
            _ => assert_eq!(serde_json::to_value(source).unwrap(), printed.as_str()),
        }
    }
    assert_eq!(
        ExclusionSource::CounterNotExtrapolated.to_string(),
        "counter_not_extrapolated"
    );
    let default_sources: Vec<ExclusionSource> = DeploymentCapabilities::default()
        .exclusions()
        .iter()
        .map(|e| e.source)
        .collect();
    for source in every_source {
        assert_eq!(
            source.in_default_deployment(),
            default_sources.contains(&source),
            "{source}"
        );
    }
}

/// The error names the exclusion once; the planner adds the
/// "deployment capabilities" context.
#[test]
fn an_invalid_exclusion_error_names_its_index_once() {
    let error = DeploymentCapabilitiesError::InvalidExclusion {
        index: 0,
        reason: "it sets no field".to_string(),
    };
    assert_eq!(error.to_string(), "excluded candidate 0: it sets no field");
}
