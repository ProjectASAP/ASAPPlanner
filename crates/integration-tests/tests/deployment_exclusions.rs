//! Deployment exclusions end to end: a deployment's capabilities (and the
//! caller's own `excluded_candidates`) expand into exclusions Stage 2
//! applies as one filter, so Stage 3 never selects an excluded candidate
//! and reports when no plan is left without them.
//!
//! Every plan and cost below was observed at the base commit with a probe
//! that pruned the same candidates in Stage 2 (a throwaway patch, since
//! deleted); the counts of pruned candidates are the probe's.

use asap_planner::{
    CandidateExclusion, CounterSemanticsSupport, DeploymentCapabilities, ErrorSelectingPlan,
    ExclusionSource, OperatorKind, PlanningModels, PlanningRun, RejectionCause, Selection,
    SelectionError, SketchAlgorithm, SketchParameterRange, SketchParams, SummaryFamily,
};
use asap_planner_cli::planning_stages::{lower_promql_roots, run_planning_stages_over};
use asap_types::deployment_capabilities::CandidateDescription;
use asap_types::ir::flat::NodeId;
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::ExecutionTiming::{self, IngestionTime, QueryTime};
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPDAGNode};
use asap_types::stages::{PhysicalProperties, PruneReason};
use asap_types::summary_type::ExactKind;
use asap_types::workload::PlanningWorkload;

use crate::planning_common::cost_inputs_without_disk_cost;

// ---------------------------------------------------------------------------
// Workloads.

/// One repeating PromQL `query` every `interval_ms`, with `requirements`
/// (the inside of the JSON object), over `arrival` and `sources`.
fn workload(
    query: &str,
    interval_ms: u32,
    requirements: &str,
    arrival: &str,
    sources: &str,
) -> PlanningWorkload {
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [
              {{"query": {query:?}, "demand": {{"fixed_interval_at": {{"interval": {interval_ms}, "evaluation_phase": 0}}}},
                "requirements": {{{requirements}}}, "predictability": {{"predictable": {{"known_at": null}}}}}}]}},
            "data_workload": {{"arrival": {arrival}, "sources": {sources}}}}}"#
    ))
    .unwrap()
}

const INGESTING_1000: &str = r#"{"ingesting": {"rows_per_second": 1000.0}}"#;
/// The gauge `m`: 1,000 series at 1 Hz.
const GAUGE_M: &str =
    r#"{"m": {"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000}}"#;
/// The counter `c`: 1,000 series at 1 Hz.
const COUNTER_C: &str =
    r#"{"c": {"series_count": 1000, "metric_type": "counter", "sample_interval": 1000}}"#;

/// `quantile_over_time(0.99, m[1h])` every 60 s over one series at 10 Hz,
/// with a per-value Rank target of 0.005 at δ = 0.01. One series, so δ is
/// not split per readout value and Pass 2 keeps KLL at k = 547, 1,404 and
/// 2,863 (five physical candidates each).
fn kll_workload() -> PlanningWorkload {
    workload(
        "quantile_over_time(0.99, m[1h])",
        60_000,
        r#""accuracy": {"accuracy_metric": "rank", "max_error": 0.005, "failure_probability": 0.01, "accuracy_scope": "per_value"}"#,
        r#"{"ingesting": {"rows_per_second": 10.0}}"#,
        r#"{"m": {"series_count": 1, "metric_type": "gauge", "sample_interval": 100}}"#,
    )
}

/// `sum_over_time(m[5m])` every 300 s over `GAUGE_M`, arriving.
fn sum_workload() -> PlanningWorkload {
    workload("sum_over_time(m[5m])", 300_000, "", INGESTING_1000, GAUGE_M)
}

/// `quantile_over_time(0.5, m[1m])` and `(0.9, m[5m])` every 10 s over
/// `GAUGE_M` (the shared-scan workload of `stage3_objective`).
fn shared_scan_workload() -> PlanningWorkload {
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [
              {{"query": "quantile_over_time(0.5, m[1m])", "demand": {{"fixed_interval_at": {{"interval": 10000, "evaluation_phase": 0}}}},
                "requirements": {{}}, "predictability": {{"predictable": {{"known_at": null}}}}}},
              {{"query": "quantile_over_time(0.9, m[5m])", "demand": {{"fixed_interval_at": {{"interval": 10000, "evaluation_phase": 0}}}},
                "requirements": {{}}, "predictability": {{"predictable": {{"known_at": null}}}}}}]}},
            "data_workload": {{"arrival": {INGESTING_1000}, "sources": {GAUGE_M}}}}}"#
    ))
    .unwrap()
}

/// `function(c[range])` every 60 s over `COUNTER_C`, arriving.
fn counter_workload(function: &str, range: &str) -> PlanningWorkload {
    workload(
        &format!("{function}(c[{range}])"),
        60_000,
        "",
        INGESTING_1000,
        COUNTER_C,
    )
}

// ---------------------------------------------------------------------------
// Planning and reading the result.

/// The facade's stages over `workload` with `capabilities`.
fn planned(workload: &PlanningWorkload, capabilities: &DeploymentCapabilities) -> PlanningRun {
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.clone().unwrap();
    let roots = lower_promql_roots(workload).unwrap();
    // Disk at no cost: these plans differ in which candidates a
    // deployment excludes, and the raw data a query-time scan keeps would
    // make scan twins near-equal in cost, a tie the solver may break
    // either way.
    let cost_inputs = cost_inputs_without_disk_cost();
    let models = PlanningModels::builtin()
        .with_cost_inputs(&cost_inputs)
        .with_deployment_capabilities(capabilities);
    run_planning_stages_over(roots, &entries, &data, models)
        .unwrap()
        .run
}

/// The capabilities of the default deployment plus the caller's
/// `excluded_candidates`.
fn excluding(excluded_candidates: Vec<CandidateExclusion>) -> DeploymentCapabilities {
    DeploymentCapabilities {
        excluded_candidates,
        ..Default::default()
    }
}

fn selected(run: &PlanningRun) -> (&PhysicalASAPDAG, &Selection) {
    let (plan, selection) = run.selection.as_ref().expect("a plan is selected");
    (plan, selection)
}

/// The plan's operators and timings, in node order.
fn operators(plan: &PhysicalASAPDAG) -> Vec<(OperatorKind, ExecutionTiming)> {
    plan.nodes
        .iter()
        .map(|node| (OperatorKind::of(&node.payload), node.output_state.timing))
        .collect()
}

/// The summary type and θ of each state build of the plan, in node order.
fn builds(plan: &PhysicalASAPDAG) -> Vec<(FieldDataType, Option<SketchParams>)> {
    plan.nodes
        .iter()
        .filter_map(|node| match &node.payload {
            Operator::ASAP(
                ASAPOp::SummaryAgg { summary_type, .. }
                | ASAPOp::SummaryTumblingWindowChain { summary_type, .. },
            ) => Some((summary_type.clone(), node.parameters.clone())),
            _ => None,
        })
        .collect()
}

/// How many of Stage 2's pruned alternatives `source` excluded.
fn pruned_by(run: &PlanningRun, source: ExclusionSource) -> usize {
    run.physical_asap_candidates
        .as_ref()
        .expect("Stage 2 ran")
        .pruned_candidates
        .iter()
        .filter(|pruned| pruned.reason == PruneReason::ExcludedByDeployment(source))
        .count()
}

/// `node` of `plan` as Stage 2 described its candidate: its payload,
/// `parameters`, `output_state.timing` and `lifetime`, and for a
/// `SummaryEstimate` the state field type of its input node's schema.
fn description_of_plan_node<'a>(
    plan: &'a PhysicalASAPDAG,
    node: &'a PhysicalASAPDAGNode,
) -> CandidateDescription<'a, NodeId> {
    let read_summary_type = match &node.payload {
        Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => plan
            .nodes
            .iter()
            .find(|state| state.id == *summary_input)
            .and_then(|state| state.output_schema.summary_state_type()),
        _ => None,
    };
    CandidateDescription {
        operator: &node.payload,
        parameters: node.parameters.as_ref(),
        physical_properties: PhysicalProperties::new(node.output_state.timing, node.lifetime)
            .expect("a plan node's timing and lifetime go together"),
        read_summary_type,
    }
}

/// Whether some node of `plan` matches `exclusion`.
fn some_node_matches(plan: &PhysicalASAPDAG, exclusion: &CandidateExclusion) -> bool {
    plan.nodes
        .iter()
        .any(|node| exclusion.matches(&description_of_plan_node(plan, node)))
}

/// Asserts the selected plan costs `expected` per second, as observed (six
/// decimals), at the default costs with disk at no cost ([`planned`]).
fn assert_cost(selection: &Selection, expected: f64) {
    let cost = selection
        .selected_plan_cost_per_second
        .expect("cost model 1");
    assert!(
        (cost - expected).abs() <= 5e-7,
        "cost {cost} per second, expected {expected}"
    );
}

/// The one rejection of a run whose deployment left no plan.
fn only_rejection(run: &PlanningRun) -> &asap_planner::ErrorSelectingPlan {
    match &run.selection {
        Err(SelectionError::Selection(error)) => error,
        other => panic!(
            "expected no plan, got {:?}",
            other.as_ref().map(|(plan, _)| plan)
        ),
    }
}

/// Asserts no plan is left and the one rejection is
/// `ExcludedByDeployment`, whose reason contains each of `named`.
fn assert_excluded_by_deployment(run: &PlanningRun, named: &[&str]) {
    let ErrorSelectingPlan::NoValidCandidate(rejections) = only_rejection(run) else {
        panic!("expected NoValidCandidate, got {:?}", only_rejection(run));
    };
    assert_eq!(rejections.len(), 1, "{rejections:?}");
    assert_eq!(rejections[0].cause, RejectionCause::ExcludedByDeployment);
    for name in named {
        assert!(
            rejections[0].reason.contains(name),
            "reason {:?} does not name {name:?}",
            rejections[0].reason
        );
    }
    assert!(!RejectionCause::ExcludedByDeployment.failed_a_check());
}

fn kll_at_most(k: u32) -> CandidateExclusion {
    CandidateExclusion {
        sketch_parameter_range: Some(SketchParameterRange::KllK {
            at_least: None,
            at_most: Some(k),
        }),
        ..Default::default()
    }
}

/// The four exclusions of an arm with no query-time scan: no query-time
/// scan, no per-series aggregate, no query-time build or chain.
fn stored_arm() -> Vec<CandidateExclusion> {
    vec![
        CandidateExclusion {
            timing: Some(QueryTime),
            operator: Some(OperatorKind::Scan),
            ..Default::default()
        },
        CandidateExclusion {
            operator: Some(OperatorKind::Aggregate),
            per_entity_reduction: Some(true),
            ..Default::default()
        },
        CandidateExclusion {
            timing: Some(QueryTime),
            operator: Some(OperatorKind::SummaryAgg),
            ..Default::default()
        },
        CandidateExclusion {
            timing: Some(QueryTime),
            operator: Some(OperatorKind::SummaryTumblingWindowChain),
            ..Default::default()
        },
    ]
}

const KLL_PLAN: [(OperatorKind, ExecutionTiming); 5] = [
    (OperatorKind::Scan, QueryTime),
    (OperatorKind::TimeRange, QueryTime),
    (OperatorKind::SummaryAgg, QueryTime),
    (OperatorKind::SummaryEstimate, QueryTime),
    (OperatorKind::Project, QueryTime),
];

/// The KLL quantile over stored windows: a chain built at ingestion, its
/// windows merged at query time.
const STORED_KLL_PLAN: [(OperatorKind, ExecutionTiming); 6] = [
    (OperatorKind::Scan, IngestionTime),
    (OperatorKind::SummaryTumblingWindowChain, IngestionTime),
    (OperatorKind::TimeRange, QueryTime),
    (OperatorKind::SummaryMerge, QueryTime),
    (OperatorKind::SummaryEstimate, QueryTime),
    (OperatorKind::Project, QueryTime),
];

fn kll(k: u32) -> (FieldDataType, Option<SketchParams>) {
    (
        FieldDataType::Sketch(SketchAlgorithm::Kll),
        Some(SketchParams::Kll { k }),
    )
}

// ---------------------------------------------------------------------------
// Caller exclusions.

/// An exclusion that matches no candidate (no KMV answers a quantile or a
/// sum) changes neither the plan nor its cost, and prunes nothing.
#[test]
fn an_exclusion_matching_nothing_changes_nothing() {
    let nothing = excluding(vec![CandidateExclusion {
        sketch_parameter_range: Some(SketchParameterRange::KmvK {
            at_least: None,
            at_most: Some(106_263),
        }),
        ..Default::default()
    }]);
    for workload in [kll_workload(), sum_workload(), shared_scan_workload()] {
        let default = planned(&workload, &DeploymentCapabilities::default());
        let restricted = planned(&workload, &nothing);
        let (default_plan, default_selection) = selected(&default);
        let (plan, selection) = selected(&restricted);
        assert_eq!(plan, default_plan);
        assert_eq!(
            selection.selected_plan_cost_per_second,
            default_selection.selected_plan_cost_per_second
        );
        assert_eq!(
            pruned_by(&restricted, ExclusionSource::ExcludedCandidate { index: 0 }),
            0
        );
    }
}

/// The answer-10 example: a KLL `k` range moves the quantile's state to a
/// larger `k`, and its cost never falls as more is excluded. Unrestricted,
/// the quantile reads stored windows of KLL at k = 1,404 (sealed
/// sketches, priced at what they retain, cost less to keep than a
/// query-time k = 547 build costs to run); excluding k up to 1,403 changes
/// nothing;
/// excluding k up to 2,862 rules every stored size out, and the quantile is
/// built at query time at k = 2,863.
#[test]
fn a_kll_k_range_moves_the_quantile_to_a_larger_k() {
    let workload = kll_workload();
    let cases = [
        (None, STORED_KLL_PLAN.as_slice(), 1404, 0.057933),
        (
            Some(kll_at_most(1403)),
            STORED_KLL_PLAN.as_slice(),
            1404,
            0.057933,
        ),
        (Some(kll_at_most(2862)), KLL_PLAN.as_slice(), 2863, 0.062491),
    ];
    for (exclusion, plan_operators, k, cost) in cases {
        let run = planned(&workload, &excluding(exclusion.iter().cloned().collect()));
        let (plan, selection) = selected(&run);
        assert_eq!(operators(plan), plan_operators, "k = {k}");
        assert_eq!(builds(plan), vec![kll(k)]);
        assert_cost(selection, cost);
        if let Some(exclusion) = &exclusion {
            assert!(!some_node_matches(plan, exclusion), "k = {k}");
        }
    }
}

/// Excluding every KLL size, by family or by a range covering the largest,
/// leaves the exact per-series quantile, at a higher cost.
#[test]
fn excluding_every_kll_size_selects_a_plan_without_kll() {
    let workload = kll_workload();
    let by_family = CandidateExclusion {
        summary_family: Some(SummaryFamily::Sketch(SketchAlgorithm::Kll)),
        ..Default::default()
    };
    for exclusion in [by_family, kll_at_most(2863)] {
        let run = planned(&workload, &excluding(vec![exclusion.clone()]));
        let (plan, selection) = selected(&run);
        assert_eq!(
            operators(plan),
            [
                (OperatorKind::Scan, QueryTime),
                (OperatorKind::TimeRange, QueryTime),
                (OperatorKind::Aggregate, QueryTime),
            ]
        );
        assert!(matches!(
            &plan.nodes[2].payload,
            Operator::NonASAP(NonASAPOp::Aggregate { reduction: Reduction::PerEntity, measures, .. })
                if measures == &[AggIntent::Quantile { col: None, q: 0.99 }]
        ));
        assert_eq!(builds(plan), vec![]);
        assert_cost(selection, 0.072584);
        assert!(!some_node_matches(plan, &exclusion));
    }
}

/// An arm with no query-time scan: without query-time scans, per-series
/// aggregates and query-time builds, the sum is read from an ingestion-time
/// chain of exact sums; eight candidates are excluded.
#[test]
fn the_stored_arm_exclusions_select_an_ingestion_time_chain() {
    let workload = sum_workload();
    let unrestricted = planned(&workload, &DeploymentCapabilities::default());
    let (plan, selection) = selected(&unrestricted);
    assert_eq!(
        operators(plan),
        [
            (OperatorKind::Scan, QueryTime),
            (OperatorKind::TimeRange, QueryTime),
            (OperatorKind::Aggregate, QueryTime),
        ]
    );
    assert_cost(selection, 0.060534);

    let stored = planned(&workload, &excluding(stored_arm()));
    let (plan, selection) = selected(&stored);
    assert_eq!(
        operators(plan),
        [
            (OperatorKind::Scan, IngestionTime),
            (OperatorKind::SummaryTumblingWindowChain, IngestionTime),
            (OperatorKind::TimeRange, QueryTime),
            (OperatorKind::SummaryMerge, QueryTime),
            (OperatorKind::FinalizeExactAccumulator, QueryTime),
        ]
    );
    assert_eq!(
        builds(plan),
        vec![(FieldDataType::exact(ExactKind::Sum), None)]
    );
    assert_cost(selection, 0.105237);
    let excluded: usize = (0..4)
        .map(|index| pruned_by(&stored, ExclusionSource::ExcludedCandidate { index }))
        .sum();
    assert_eq!(excluded, 8);
}

/// Data at rest has no ingestion pipeline, so only query-time scans can
/// feed the sum. Excluding them leaves no plan: the rejection is
/// `ExcludedByDeployment`, naming the exclusion and the two candidates it
/// pruned, never a plan with a scan, and never the generic `Infeasible`.
#[test]
fn an_excluded_candidate_is_never_selected_even_when_it_is_the_only_feasible_one() {
    let workload = workload(
        "sum_over_time(m[5m])",
        300_000,
        "",
        r#""at_rest""#,
        r#"{"m": {"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000, "rows_held": 300000000}}"#,
    );
    let unrestricted = planned(&workload, &DeploymentCapabilities::default());
    let (plan, selection) = selected(&unrestricted);
    assert_eq!(operators(plan)[0], (OperatorKind::Scan, QueryTime));
    assert_cost(selection, 0.060534);

    let no_scan = planned(
        &workload,
        &excluding(vec![CandidateExclusion {
            timing: Some(QueryTime),
            operator: Some(OperatorKind::Scan),
            ..Default::default()
        }]),
    );
    assert_eq!(
        pruned_by(&no_scan, ExclusionSource::ExcludedCandidate { index: 0 }),
        2
    );
    assert_excluded_by_deployment(
        &no_scan,
        &[
            "excluded_candidates[0] (2 candidates)",
            "may be infeasible without",
        ],
    );
}

/// Stage 2 records each matched build with the caller's exclusion, by its
/// index: the five KLL k = 547 alternatives, then the ten at k = 547 and
/// 1,404. None is left for Stage 3 to report as not costable, and nothing
/// is excluded without an exclusion.
#[test]
fn excluded_candidates_are_pruned_in_stage2_with_their_index() {
    let workload = kll_workload();
    for (exclusions, count) in [
        (vec![], 0),
        (vec![kll_at_most(1403)], 5),
        (vec![kll_at_most(2862)], 10),
    ] {
        let run = planned(&workload, &excluding(exclusions));
        assert_eq!(
            pruned_by(&run, ExclusionSource::ExcludedCandidate { index: 0 }),
            count
        );
        let (_, selection) = selected(&run);
        assert!(selection
            .pruned_candidates
            .iter()
            .all(|pruned| pruned.reason != PruneReason::UsageNotCostable));
    }
}

/// The exclusions reach Stage 2 through `PlanningModels` with the
/// deployment's other capabilities: the same run through
/// `run_planning_stages_over` honours `DeploymentCapabilities::validate`,
/// so an exclusion that sets no field is refused before planning.
#[test]
fn an_exclusion_setting_no_field_is_refused_before_planning() {
    let run = planned(
        &kll_workload(),
        &excluding(vec![CandidateExclusion::default()]),
    );
    assert!(
        matches!(
            run.selection,
            Err(SelectionError::InvalidDeploymentCapabilities(
                asap_types::deployment_capabilities::DeploymentCapabilitiesError::InvalidExclusion {
                    index: 0,
                    ..
                }
            ))
        ),
        "{:?}",
        run.selection.as_ref().map(|(plan, _)| plan)
    );
}

// ---------------------------------------------------------------------------
// Capabilities a target may lack (the new dimensions).

/// A target without mergeable counter state: `rate(c[5m])` no longer reads
/// an ingestion-time chain of counter states merged across windows, but
/// builds and finalizes the counter state within each evaluation.
/// `increase` the same. No node of the new plans keeps a counter state
/// across windows.
#[test]
fn a_target_without_mergeable_rate_state_builds_rate_within_one_evaluation() {
    let not_mergeable = DeploymentCapabilities {
        state_not_mergeable_across_windows: vec![SummaryFamily::Exact(ExactKind::Counter)],
        ..Default::default()
    };
    // A counter state merged across windows: a chain of it, or a build of
    // it kept across evaluations.
    let merged_state = |kind| {
        [
            CandidateExclusion {
                summary_family: Some(SummaryFamily::Exact(kind)),
                operator: Some(OperatorKind::SummaryTumblingWindowChain),
                ..Default::default()
            },
            CandidateExclusion {
                summary_family: Some(SummaryFamily::Exact(kind)),
                kept_across_evaluations: Some(true),
                ..Default::default()
            },
        ]
    };
    let cases = [
        ("rate", "5m", ExactKind::Counter, 0.274034, 0.367670),
        ("increase", "1h", ExactKind::Counter, 1.374738, 3.953712),
    ];
    for (function, range, kind, unrestricted_cost, restricted_cost) in cases {
        let workload = counter_workload(function, range);
        let unrestricted = planned(&workload, &DeploymentCapabilities::default());
        let (plan, selection) = selected(&unrestricted);
        // Over an hour, a 1 min level merged from the chain and a cover
        // over both (`multi_level_window_chains.md`); over 5 min, the chain.
        let windows: &[(OperatorKind, ExecutionTiming)] = match range {
            "1h" => &[
                (OperatorKind::SummaryMergeWindows, IngestionTime),
                (OperatorKind::TimeRangeCover, QueryTime),
            ],
            _ => &[(OperatorKind::TimeRange, QueryTime)],
        };
        let expected: Vec<(OperatorKind, ExecutionTiming)> = [
            (OperatorKind::Scan, IngestionTime),
            (OperatorKind::SummaryTumblingWindowChain, IngestionTime),
        ]
        .into_iter()
        .chain(windows.iter().copied())
        .chain([
            (OperatorKind::SummaryMerge, QueryTime),
            (OperatorKind::FinalizeExactAccumulator, QueryTime),
        ])
        .collect();
        assert_eq!(operators(plan), expected, "{function}");
        assert_eq!(builds(plan), vec![(FieldDataType::exact(kind), None)]);
        assert_cost(selection, unrestricted_cost);

        let restricted = planned(&workload, &not_mergeable);
        let (plan, selection) = selected(&restricted);
        assert_eq!(
            operators(plan),
            [
                (OperatorKind::Scan, QueryTime),
                (OperatorKind::TimeRange, QueryTime),
                (OperatorKind::SummaryAgg, QueryTime),
                (OperatorKind::FinalizeExactAccumulator, QueryTime),
            ],
            "{function}"
        );
        assert_eq!(builds(plan), vec![(FieldDataType::exact(kind), None)]);
        assert_cost(selection, restricted_cost);
        for merged in merged_state(kind) {
            assert!(!some_node_matches(plan, &merged), "{function}");
        }
        assert_eq!(
            pruned_by(&restricted, ExclusionSource::StateNotMergeableAcrossWindows),
            3,
            "{function}"
        );
    }
}

/// Why an aggregate's measure is a dimension of its own: excluding every
/// counter state alone lets the planner rebuild the query-time rate as a
/// per-series `Aggregate[rate]` over raw rows; excluding that measure too
/// leaves no plan, reported as excluded by the deployment.
#[test]
fn excluding_rate_state_alone_rebuilds_it_as_an_aggregate_unless_its_measure_is_excluded() {
    let workload = counter_workload("rate", "5m");
    let rate_state = CandidateExclusion {
        summary_family: Some(SummaryFamily::Exact(ExactKind::Counter)),
        ..Default::default()
    };
    let state_only = planned(&workload, &excluding(vec![rate_state.clone()]));
    let (plan, selection) = selected(&state_only);
    assert_eq!(
        operators(plan),
        [
            (OperatorKind::Scan, QueryTime),
            (OperatorKind::TimeRange, QueryTime),
            (OperatorKind::Aggregate, QueryTime),
        ]
    );
    assert!(matches!(
        &plan.nodes[2].payload,
        Operator::NonASAP(NonASAPOp::Aggregate { reduction: Reduction::PerEntity, measures, .. })
            if measures == &[AggIntent::Rate]
    ));
    assert_cost(selection, 0.447669);
    assert_eq!(
        pruned_by(&state_only, ExclusionSource::ExcludedCandidate { index: 0 }),
        5
    );

    let query_time_rate = CandidateExclusion {
        timing: Some(QueryTime),
        aggregate_measure: Some("rate".to_string()),
        ..Default::default()
    };
    let neither = planned(&workload, &excluding(vec![rate_state, query_time_rate]));
    assert_excluded_by_deployment(
        &neither,
        &["excluded_candidates[0]", "excluded_candidates[1]"],
    );
}

/// A target whose counter functions do not extrapolate plans no `rate`
/// (every counter state and `Aggregate[rate]` needs extrapolation), but plans
/// `irate` as before; one that does not handle counter resets plans
/// neither.
#[test]
fn counter_semantics_a_target_lacks_rule_out_the_functions_that_need_them() {
    let without = |reset_handling, extrapolation| DeploymentCapabilities {
        counter_semantics: CounterSemanticsSupport {
            reset_handling,
            extrapolation,
        },
        ..Default::default()
    };
    let rate = counter_workload("rate", "5m");
    let irate = counter_workload("irate", "5m");

    let no_extrapolation = planned(&rate, &without(true, false));
    assert_eq!(
        pruned_by(&no_extrapolation, ExclusionSource::CounterNotExtrapolated),
        7
    );
    assert_excluded_by_deployment(
        &no_extrapolation,
        &["counter_not_extrapolated (7 candidates)"],
    );

    let default_irate = planned(&irate, &DeploymentCapabilities::default());
    let irate_without_extrapolation = planned(&irate, &without(true, false));
    let (default_plan, _) = selected(&default_irate);
    let (plan, selection) = selected(&irate_without_extrapolation);
    assert_eq!(plan, default_plan);
    assert_eq!(
        builds(plan),
        vec![(FieldDataType::exact(ExactKind::IRate), None)]
    );
    assert_cost(selection, 0.367670);
    assert_eq!(
        pruned_by(
            &irate_without_extrapolation,
            ExclusionSource::CounterNotExtrapolated
        ),
        0
    );

    let irate_without_resets = planned(&irate, &without(false, true));
    assert_eq!(
        pruned_by(
            &irate_without_resets,
            ExclusionSource::CounterResetsNotHandled
        ),
        4
    );
    assert_excluded_by_deployment(&irate_without_resets, &["counter_resets_not_handled"]);
    let rate_without_resets = planned(&rate, &without(false, true));
    assert_excluded_by_deployment(&rate_without_resets, &["counter_resets_not_handled"]);
}
