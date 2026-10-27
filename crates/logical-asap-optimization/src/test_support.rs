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

/// The statistic classic HLL's failure probability at `precision` and
/// `relative_error` over `source` is left as, for Stage 3 tests that bind
/// it. The estimator keeps the format crate-private.
pub fn hll_classic_failure_statistic(precision: u8, relative_error: f64, source: &str) -> String {
    crate::accuracy::estimators::hll::classic_failure_statistic(precision, relative_error, source)
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
    apply_materialization_timings, MaterializationAssignment, TimingMemo,
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
        &mut TimingMemo::new(),
    )
    .expect("default materialization timings apply")
}

/// `root` timed with every sub-DAG `MatAtIngestion` where the DAG allows it.
pub fn at_ingestion(root: &Rc<OperatorNode>) -> Rc<OperatorNode> {
    apply_materialization_timings(
        root,
        &MaterializationAssignment::with_default(Materialization::MatAtIngestion(Medium::Memory)),
        &mut TimingMemo::new(),
    )
    .expect("at-ingestion materialization timings apply")
}

/// The Count-Min summary of a Stage 1 candidate of `query`.
pub fn cms_summary(query: &str) -> OperatorNode {
    use crate::deployment_capabilities::DeploymentCapabilities;
    use crate::per_computation_candidates::compose_logical_candidate;
    use crate::per_computation_candidates::generate_per_computation_candidates;
    use asap_types::ir::schema::{FieldDataType, SketchAlgorithm};
    use asap_types::ir::{ASAPOp, QueryRoot};
    let target = AccuracyTarget::max_error(0.01).failure_probability(0.01);
    let root = lower_promql(query, target);
    // Inspect each local computation choice under the built-in deployment.
    let per_computation = generate_per_computation_candidates(vec![(0, QueryRoot::Operator(root))])
        .unwrap()
        .without_unsupported_summaries(&DeploymentCapabilities::default());
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

/// The maintained-population sub-DAG of a PromQL instant aggregation or
/// top-k over current series: `EvaluatePopulation ▸ MaintainPopulation`
/// over the selector, under `root`'s own projections. No planner pass offers
/// it yet (#681); executor tests use it to run the population operators.
/// `None` when `root` is not such a query.
pub fn current_series_population(root: &Rc<OperatorNode>) -> Option<Rc<OperatorNode>> {
    use asap_types::ir::operator::maintained_population::*;
    use asap_types::ir::properties::AccuracyGuarantee;
    use asap_types::ir::scalar::{CompareOpKind, ScalarValue};
    use asap_types::ir::{ASAPOp, Operator};
    if let Some(NonASAPOp::Project {
        cols,
        qualifier,
        child,
    }) = root.non_asap()
    {
        let child = current_series_population(child)?;
        let guarantee = child.guarantee.clone();
        return Some(Rc::new(
            OperatorNode::with_schema(
                Operator::NonASAP(NonASAPOp::Project {
                    cols: cols.clone(),
                    qualifier: qualifier.clone(),
                    child,
                }),
                root.schema.clone(),
            )
            .with_guarantee(guarantee),
        ));
    }
    let (source, grouping, evaluation, value_column) = match root.non_asap()? {
        NonASAPOp::Aggregate {
            child,
            reduction: Reduction::Reduce(grouping),
            measures,
            filters,
            having: None,
            ..
        } if filters.iter().all(Option::is_none) => {
            let (col, evaluation) = match measures.as_slice() {
                [AggIntent::Quantile { q, col, .. }] => {
                    (*col, PopulationStatistic::Quantile { q: *q })
                }
                [AggIntent::TopK { k, .. }] => (None, PopulationStatistic::TopK { k: *k }),
                [AggIntent::Sum { col }] => (*col, PopulationStatistic::Sum),
                [AggIntent::Count { .. }] => (None, PopulationStatistic::Count),
                [AggIntent::Avg { col }] => (*col, PopulationStatistic::Average),
                _ => return None,
            };
            (child, grouping, evaluation, col)
        }
        NonASAPOp::Limit {
            n: Some(n),
            offset: 0,
            partition_by,
            child,
        } => {
            let Some(NonASAPOp::Sort {
                child,
                keys,
                partition_by: sorted_by,
            }) = child.non_asap()
            else {
                return None;
            };
            let [key] = keys.as_slice() else {
                return None;
            };
            if sorted_by != partition_by || key.ascending {
                return None;
            }
            let ScalarExpr::Column(col) = &key.expr else {
                return None;
            };
            // `topk` sorts the selector's rows through a projection that keeps
            // every column in place.
            let child = match child.non_asap() {
                Some(NonASAPOp::Project {
                    cols,
                    qualifier: None,
                    child: rows,
                }) if cols.len() == rows.schema.fields.len()
                    && cols.iter().enumerate().all(|(i, item)| {
                        item.alias.is_none() && matches!(item.expr, ScalarExpr::Column(c) if c == i)
                    }) =>
                {
                    rows
                }
                _ => child,
            };
            (
                child,
                partition_by,
                PopulationStatistic::TopK { k: *n },
                Some(*col),
            )
        }
        _ => return None,
    };
    // A bare selector carries its metric's sample interval as the horizon
    // after which a series that stops reporting leaves the population.
    let (selector, lookback_ms) = match source.non_asap() {
        Some(NonASAPOp::TimeRange { range, child, .. }) => {
            (child.as_ref(), u64::try_from(range.as_millis()).ok()?)
        }
        _ => (source.as_ref(), 300_000),
    };
    let Some(NonASAPOp::Scan {
        source: Source::TimeSeries { metric },
        predicates,
        schema,
    }) = selector.non_asap()
    else {
        return None;
    };
    if value_column.is_some_and(|c| schema.fields.get(c).is_none_or(|f| f.name != "value")) {
        return None;
    }
    let label = |col: usize| -> Option<String> {
        let field = schema.fields.get(col)?;
        (field.dtype == DataType::Utf8).then(|| field.name.clone())
    };
    let mut matchers = Vec::new();
    for predicate in predicates {
        let ScalarExpr::Compare {
            left, op, right, ..
        } = &predicate.0
        else {
            return None;
        };
        let (ScalarExpr::Column(col), ScalarExpr::Literal(ScalarValue::Utf8(value))) =
            (left.as_ref(), right.as_ref())
        else {
            return None;
        };
        let operation = match op {
            CompareOpKind::Eq => CurrentSeriesMatch::Equal,
            CompareOpKind::Ne => CurrentSeriesMatch::NotEqual,
            CompareOpKind::Regex => CurrentSeriesMatch::Regex,
            CompareOpKind::NotRegex => CurrentSeriesMatch::NotRegex,
            _ => return None,
        };
        matchers.push(CurrentSeriesMatcher {
            label: label(*col)?,
            value: value.clone(),
            operation,
        });
    }
    matchers.sort();
    matchers.dedup();
    let mut labels = grouping
        .keys()
        .iter()
        .map(|c| label(*c))
        .collect::<Option<Vec<_>>>()?;
    labels.sort();
    labels.dedup();
    let population = MaintainedPopulation {
        input: PopulationInput::CurrentSeries(CurrentSeriesInput {
            metric: metric.clone(),
            matchers,
            grouping: labels,
            without: grouping.is_without(),
            lookback_ms,
        }),
        max_k: match evaluation {
            PopulationStatistic::TopK { k } => k,
            _ => 0,
        },
        quantiles: matches!(evaluation, PopulationStatistic::Quantile { .. }),
    };
    let input = Rc::new(
        (**source)
            .clone()
            .with_guarantee(Some(AccuracyGuarantee::exact("source samples"))),
    );
    let maintained = Rc::new(
        OperatorNode::with_schema(
            Operator::ASAP(ASAPOp::MaintainPopulation {
                child: input,
                population,
            }),
            source.schema.clone(),
        )
        .with_guarantee(Some(AccuracyGuarantee::exact(
            "exact members under the declared population semantics",
        ))),
    );
    Some(Rc::new(
        OperatorNode::with_schema(
            Operator::ASAP(ASAPOp::EvaluatePopulation {
                child: maintained,
                evaluation,
            }),
            root.schema.clone(),
        )
        .with_guarantee(Some(AccuracyGuarantee::exact(
            "exact current-population evaluation",
        ))),
    ))
}

#[cfg(test)]
mod population_tests {
    use super::{current_series_population, lower_promql};
    use asap_types::ir::operator::maintained_population::{PopulationInput, PopulationStatistic};
    use asap_types::ir::physical_export::compile_physical_asap_dag as export_timed;
    use asap_types::ir::properties::timing::{
        apply_materialization_timings, MaterializationAssignment, TimingMemo,
    };
    use asap_types::ir::{ASAPOp, Operator, OperatorNode};
    use std::rc::Rc;

    /// Time `root` under the default materialization assignment (which runs
    /// the data-state / population-contract validation) and export it.
    fn compile_physical_asap_dag(root: &Rc<OperatorNode>) -> Result<(), String> {
        root.validate_structure().map_err(|e| e.to_string())?;
        let timed = apply_materialization_timings(
            root,
            &MaterializationAssignment::default(),
            &mut TimingMemo::new(),
        )
        .map_err(|e| format!("{e:?}"))?;
        export_timed(&timed).map_err(|e| format!("{e:?}"))?;
        Ok(())
    }

    fn lower(q: &str) -> Rc<OperatorNode> {
        lower_promql(q, asap_types::ir::properties::AccuracyTarget::exact())
    }

    // Population timing is a materialization choice: the population follows
    // its assignment (`NotMaterialized` by default), while its evaluation
    // always runs at query time.
    #[test]
    fn population_timing_follows_its_materialization() {
        use asap_types::ir::properties::ExecutionTiming::{IngestionTime, QueryTime};
        use asap_types::physical::{Materialization, Medium};
        let candidate = current_series_population(&lower("topk(5,a)")).unwrap();
        let Some(ASAPOp::EvaluatePopulation {
            child: population, ..
        }) = candidate.asap()
        else {
            unreachable!()
        };
        let on_disk = Materialization::MatAtIngestion(Medium::Disk);
        let mut at_ingestion = MaterializationAssignment::default();
        at_ingestion.set(population, on_disk);
        for (assignment, timing, materialization) in [
            (
                MaterializationAssignment::default(),
                QueryTime,
                Materialization::NotMaterialized,
            ),
            (at_ingestion, IngestionTime, on_disk),
        ] {
            let timed =
                apply_materialization_timings(&candidate, &assignment, &mut TimingMemo::new())
                    .unwrap();
            let dag = export_timed(&timed).unwrap();
            let [population, evaluation] = &dag.nodes[dag.nodes.len() - 2..] else {
                unreachable!()
            };
            assert_eq!(population.output_state.timing, timing);
            assert_eq!(population.materialization, materialization);
            assert_eq!(evaluation.output_state.timing, QueryTime);
        }
        let mut evaluation_at_ingestion = (*candidate).clone();
        evaluation_at_ingestion.timing = Some(IngestionTime);
        assert!(compile_physical_asap_dag(&Rc::new(evaluation_at_ingestion)).is_err());
    }

    // An evaluation cannot reinterpret arbitrary rows as maintained state or
    // exceed its producer's contract.
    #[test]
    fn malformed_population_dags_fail_closed() {
        let candidate = current_series_population(&lower("topk(5,a)")).unwrap();
        compile_physical_asap_dag(&candidate).expect("the unmodified candidate is legal");
        let mut bad = (*candidate).clone();
        let Operator::ASAP(ASAPOp::EvaluatePopulation { evaluation, .. }) = &mut bad.operator
        else {
            unreachable!()
        };
        *evaluation = PopulationStatistic::TopK { k: 6 };
        assert!(compile_physical_asap_dag(&Rc::new(bad.clone())).is_err());
        let Operator::ASAP(ASAPOp::EvaluatePopulation { child, evaluation }) = &mut bad.operator
        else {
            unreachable!()
        };
        *evaluation = PopulationStatistic::TopK { k: 5 };
        let producer = Rc::make_mut(child);
        let Operator::ASAP(ASAPOp::MaintainPopulation { population, .. }) = &mut producer.operator
        else {
            unreachable!()
        };
        let PopulationInput::CurrentSeries(spec) = &mut population.input else {
            unreachable!()
        };
        spec.metric = "b".into();
        assert!(compile_physical_asap_dag(&Rc::new(bad)).is_err());
    }
}
