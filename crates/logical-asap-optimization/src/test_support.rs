// Shared fixture helpers for the stage crates' tests (feature
// `test-support`); not every test module uses every helper.
#![allow(dead_code)]

use std::rc::Rc;

use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::OperatorNode;
use asap_types::workload::{
    BatchEntry, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload,
};

/// Pass 2 after local window composition, on a memo the test composed
/// itself (or not at all): rules 2–9 for `domain`, then
/// [`add_shared_summary_state_groups`](crate::pass2::memo::state_groups::add_shared_summary_state_groups).
/// Production runs these through
/// [`generate_logical_asap_memo_candidates`](crate::pass2::logical_memo_candidates::generate_logical_asap_memo_candidates).
pub fn sharing_rules_and_shared_state_groups(
    mut memo: crate::pass2::memo::candidates::CandidateMemo,
    domain: crate::pass2::memo::shared_configurations::PaneWidthDomain,
) -> Result<
    crate::pass2::memo::candidates::CandidateMemo,
    crate::pass2::memo::state_groups::StateGroupExpansionError,
> {
    crate::pass2::logical_memo_candidates::apply_sharing_rules(&mut memo, domain, None)?;
    crate::pass2::memo::state_groups::add_shared_summary_state_groups(&mut memo, None)?;
    Ok(memo)
}

/// The groups [`add_shared_summary_state_groups`](crate::pass2::memo::state_groups::add_shared_summary_state_groups)
/// added to an expanded memo, in order: each has one candidate, the
/// shared build (origin `SharedBuild`); no other group's candidates are
/// only that.
pub fn shared_summary_state_groups(
    memo: &crate::pass2::memo::candidates::CandidateMemo,
) -> Vec<crate::pass2::memo::MemoGroupId> {
    memo.candidates
        .iter()
        .enumerate()
        .filter(|(_, candidates)| {
            matches!(
                candidates.as_slice(),
                [only] if only.origin
                    == crate::pass2::memo::candidates::OriginOfCandidateForMemoGroup::SharedBuild
            )
        })
        .map(|(group, _)| crate::pass2::memo::MemoGroupId(group as u32))
        .collect()
}

/// The statistic classic HLL's failure probability at `precision` and
/// `relative_error` over `source` is left as, for Stage 3 tests that bind
/// it. The estimator keeps the format crate-private.
pub fn hll_classic_failure_statistic(precision: u8, relative_error: f64, source: &str) -> String {
    asap_accuracy::estimators::hll::classic_failure_statistic(precision, relative_error, source)
}

pub fn lower_promql(query: &str, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy,
                    ..Default::default()
                },
                predictability: Predictability::Unknown,
                invocations: 1,
                execute_at: None,
                range_evaluation: None,
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload::sampled_every(
            asap_frontend_promql::promql_metric_names(query).unwrap_or_default(),
            DurationMs(1_000),
        )),
    };
    asap_frontend_promql::lower_promql_workload(&workload)
        .unwrap()
        .pop()
        .unwrap()
}

// ── Shared pre-ASAP fixture builders ─────────────────────────────────────
//
// Every builder returns an `Rc<OperatorNode>` whose schema is derived by
// `OperatorNode::new_shared`, so a fixture is exactly what a front end
// would hand the planner. Added by the test migration; only add here, never
// rename or remove (several test modules share these).

use std::time::Duration;

use asap_types::ir::operator::agg_intent::AggIntent;
use asap_types::ir::operator::operator_properties::{GroupKeys, Reduction, Source};
use asap_types::ir::properties::timing::{
    apply_materialization_timings, MaterializationAssignment, NodeToTimedNodeCache,
};
use asap_types::ir::schema::{ColumnId, DataType, Field, Schema};
use asap_types::ir::{NonASAPOp, Predicate, ScalarExpr, TimeRangeKind};
use asap_types::physical::{Materialization, Medium};

/// A `TimeSeries("m")` scan over `[ts(0), value(1), labels...]`, time index 0,
/// no unique key.
pub fn metric_scan(labels: &[&str]) -> Rc<OperatorNode> {
    metric_scan_with_keys(labels, vec![])
}

/// [`metric_scan`] with explicit `unique_keys` (a `[[0]]` key makes CSE
/// willing to hoist the scan).
pub fn metric_scan_with_keys(labels: &[&str], unique_keys: Vec<Vec<ColumnId>>) -> Rc<OperatorNode> {
    let mut columns = vec![
        Field::plain("ts", DataType::Timestamp, false),
        Field::plain("value", DataType::Float64, false),
    ];
    columns.extend(
        labels
            .iter()
            .map(|n| Field::plain(*n, DataType::Utf8, true)),
    );
    scan("m", Schema::with_time_index(columns, 0, unique_keys))
}

/// A predicate-free `TimeSeries(metric)` scan with the given schema.
pub fn scan(metric: &str, schema: Schema) -> Rc<OperatorNode> {
    scan_from(
        Source::TimeSeries {
            metric: metric.into(),
        },
        schema,
    )
}

pub fn scan_from(source: Source, schema: Schema) -> Rc<OperatorNode> {
    OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Scan {
        source,
        predicates: vec![],
        schema,
    }))
    .unwrap()
}

/// A general aggregate node.
pub fn aggregate(
    reduction: Reduction,
    measures: Vec<AggIntent>,
    output_names: Vec<String>,
    having: Option<Predicate>,
    child: Rc<OperatorNode>,
) -> Rc<OperatorNode> {
    OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Aggregate {
        reduction,
        measures,
        output_names,
        filters: vec![],
        having,
        child,
    }))
    .unwrap()
}

/// `intent by (by)` — a single-measure, `HAVING`-free grouped aggregate.
pub fn agg(by: Vec<ColumnId>, intent: AggIntent, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    aggregate(Reduction::by(by), vec![intent], vec![], None, child)
}

/// `intent without (excluded)`.
pub fn without_agg(
    excluded: Vec<ColumnId>,
    intent: AggIntent,
    child: Rc<OperatorNode>,
) -> Rc<OperatorNode> {
    aggregate(
        Reduction::Reduce(GroupKeys::without(excluded)),
        vec![intent],
        vec![],
        None,
        child,
    )
}

pub fn filter(pred: ScalarExpr, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Filter {
        pred: Predicate(pred),
        child,
    }))
    .unwrap()
}

pub fn dedup(cols: Vec<ColumnId>, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::Dedup {
        cols,
        child,
    }))
    .unwrap()
}

/// An explicit range selector `child[range]`.
pub fn time_range(range: Duration, child: Rc<OperatorNode>) -> Rc<OperatorNode> {
    OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::TimeRange {
        range,
        kind: TimeRangeKind::Range,
        child,
    }))
    .unwrap()
}

/// `root` timed under the default (every sub-DAG `NotMaterialized`)
/// assignment — the shape export and the post-ASAP validators consume.
pub fn timed(root: &Rc<OperatorNode>) -> Rc<OperatorNode> {
    apply_materialization_timings(
        root,
        &MaterializationAssignment::default(),
        &mut NodeToTimedNodeCache::new(),
    )
    .expect("default materialization timings apply")
}

/// `root` timed with every sub-DAG `MatAtIngestion` where the DAG allows it.
pub fn at_ingestion(root: &Rc<OperatorNode>) -> Rc<OperatorNode> {
    apply_materialization_timings(
        root,
        &MaterializationAssignment::with_default(Materialization::MatAtIngestion(Medium::Memory)),
        &mut NodeToTimedNodeCache::new(),
    )
    .expect("at-ingestion materialization timings apply")
}

/// The Count-Min summary of a Stage 1 candidate of `query`.
pub fn cms_summary(query: &str) -> OperatorNode {
    use crate::pass1::per_computation_candidates::compose_logical_candidate;
    use crate::pass1::per_computation_candidates::generate_per_computation_candidates;
    use asap_types::ir::schema::{FieldDataType, SketchAlgorithm};
    use asap_types::ir::{ASAPOp, QueryRoot};
    let target = AccuracyTarget::max_error(0.01).failure_probability(0.01);
    let root = lower_promql(query, target);
    // Inspect each local computation choice under the built-in deployment.
    let per_computation = generate_per_computation_candidates(vec![(0, QueryRoot::Operator(root))])
        .unwrap()
        .without_unsupported_summaries(None);
    let summary = per_computation
        .targets
        .iter()
        .enumerate()
        .flat_map(|(target, choices)| {
            let inventory = &per_computation;
            (0..choices.choice_count()).filter_map(move |candidate| {
                let mut assignment = vec![0; inventory.targets.len()];
                assignment[target] = candidate;
                compose_logical_candidate(inventory, &assignment)
                    .ok()
                    .map(|roots| roots.into_iter().map(|(_, root)| root).collect::<Vec<_>>())
            })
        })
        .flat_map(|roots| match &roots[0] {
            QueryRoot::Operator(node) => OperatorNode::reachable(node),
            QueryRoot::Scalar(_) => vec![],
        })
        .find(|node| {
            matches!(
                node.asap(),
                Some(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) if *kind.algorithm() == SketchAlgorithm::Cms
            )
        })
        .map(|node| (*node).clone())
        .unwrap_or_else(|| panic!("{query}: a Count-Min summary"));
    summary
}

/// `summary` with its update weight replaced by `weight`.
pub fn weighted(
    mut summary: OperatorNode,
    weight: asap_types::ir::schema::SummaryInputExpr,
) -> Rc<OperatorNode> {
    use asap_types::ir::{ASAPOp, Operator};
    if let Operator::ASAP(ASAPOp::SummaryAgg { input, .. }) = &mut summary.operator {
        input.weight = weight;
    }
    Rc::new(summary)
}

/// A point-count estimate over `summary` carrying `guarantee`.
pub fn estimate_over(
    summary: Rc<OperatorNode>,
    guarantee: asap_types::ir::properties::AccuracyGuarantee,
) -> Rc<OperatorNode> {
    use asap_types::ir::scalar::ColumnRef;
    use asap_types::ir::schema::SketchStatistic;
    use asap_types::ir::{ASAPOp, Operator};
    Rc::new(
        OperatorNode::new(Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: summary,
            query: SketchStatistic::PointCount {
                key: ColumnRef::SampleValue,
                value: None,
            },
        }))
        .unwrap()
        .with_guarantee(Some(guarantee)),
    )
}

/// The workload entry of a query evaluated as `recurrence` says, exact and
/// with no latency bound.
pub fn entry(
    recurrence: asap_types::workload::QueryRecurrence,
) -> asap_types::workload::QueryWorkloadEntry {
    entry_with(AccuracyTarget::exact(), recurrence)
}

/// The workload entry of a query with accuracy target `accuracy`, evaluated
/// as `recurrence` says, with no latency bound.
pub fn entry_with(
    accuracy: AccuracyTarget,
    recurrence: asap_types::workload::QueryRecurrence,
) -> asap_types::workload::QueryWorkloadEntry {
    asap_types::workload::QueryWorkloadEntry {
        query: Default::default(),
        requirements: QueryRequirements {
            accuracy,
            max_latency_ms: None,
        },
        predictability: Default::default(),
        recurrence,
        range_evaluation: None,
    }
}

/// The maintained-population sub-DAG for executor tests: `root` (below its
/// projections, which are kept) replaced by `EvaluatePopulation(statistic)`
/// over a current-series population of the single selector under it,
/// grouped by `grouping`, with no matchers. The caller states the statistic
/// and grouping; nothing is recognized. No planner pass offers this sub-DAG
/// yet (#681).
pub fn population_over_selector(
    root: &Rc<OperatorNode>,
    statistic: asap_types::ir::operator::maintained_population::PopulationStatistic,
    grouping: &[&str],
) -> Rc<OperatorNode> {
    use asap_types::ir::operator::maintained_population::*;
    use asap_types::ir::{ASAPOp, Operator};
    if let Some(NonASAPOp::Project {
        cols,
        qualifier,
        child,
    }) = root.non_asap()
    {
        let child = population_over_selector(child, statistic.clone(), grouping);
        let project = Operator::NonASAP(NonASAPOp::Project {
            cols: cols.clone(),
            qualifier: qualifier.clone(),
            child,
        });
        return Rc::new(OperatorNode::with_schema(project, root.schema.clone()));
    }
    let selectors: Vec<_> = OperatorNode::reachable(root)
        .into_iter()
        .filter(|node| matches!(node.non_asap(), Some(NonASAPOp::TimeRange { .. })))
        .collect();
    let [selector] = selectors.as_slice() else {
        panic!("one selector under {root:?}");
    };
    let Some(NonASAPOp::TimeRange { range, child, .. }) = selector.non_asap() else {
        unreachable!()
    };
    let Some(NonASAPOp::Scan {
        source: Source::TimeSeries { metric },
        ..
    }) = child.non_asap()
    else {
        panic!("a time-series selector, got {child:?}");
    };
    let population = MaintainedPopulation {
        input: PopulationInput::CurrentSeries(CurrentSeriesInput {
            metric: metric.clone(),
            matchers: Vec::new(),
            grouping: grouping.iter().map(|label| label.to_string()).collect(),
            without: false,
            lookback_ms: range.as_millis() as u64,
        }),
        max_k: match statistic {
            PopulationStatistic::TopK { k } => k,
            _ => 0,
        },
        quantiles: matches!(statistic, PopulationStatistic::Quantile { .. }),
    };
    let maintained = OperatorNode::with_schema(
        Operator::ASAP(ASAPOp::MaintainPopulation {
            child: selector.clone(),
            population,
        }),
        selector.schema.clone(),
    );
    let evaluation = Operator::ASAP(ASAPOp::EvaluatePopulation {
        child: Rc::new(maintained),
        evaluation: statistic,
    });
    Rc::new(OperatorNode::with_schema(evaluation, root.schema.clone()))
}
