//! Planner-selected PromQL computation compiles from the timed DAG alone;
//! the deployment supplies only raw rows at the ingestion frontier.
mod common;
use asap_executor::{
    operators::Operator,
    physical_planner::{compile, promql_rows, CompiledPhysicalDAG, InputContract, Source},
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Value},
};
use common::{compile_physical_asap_dag, selected_dag};
use futures::{executor::block_on, StreamExt};
use planner_types::ir::export::{PhysicalASAPDAG, PhysicalASAPOperatorPayload};
use planner_types::{ir::schema::*, types::AccuracyTarget, workload::*};
use std::{collections::BTreeMap, rc::Rc, sync::Arc};

fn lower(query: &str) -> Rc<planner_types::ir::OperatorNode> {
    lower_with(query, AccuracyTarget::Exact)
}

fn lower_with(query: &str, accuracy: AccuracyTarget) -> Rc<planner_types::ir::OperatorNode> {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(accuracy),
                    ..Default::default()
                },
                predictability: Predictability::Unknown,
                invocations: 1,
                execute_at: None,
                time_selection: TimeSelection::default(),
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(60_000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    asap_frontend_promql::lower_promql_workload(&workload, 0)
        .unwrap()
        .remove(0)
}

/// The plan the stage pipeline selects for an exact `query`.
fn exact_dag(query: &str) -> PhysicalASAPDAG {
    let expression = lower(query);
    let root = promql_rows::with_series_identity(&expression).unwrap_or(expression);
    compile_physical_asap_dag(&selected_dag(root, AccuracyTarget::Exact)).unwrap()
}

fn population_dag(query: &str) -> PhysicalASAPDAG {
    let root = promql_rows::with_series_identity(&lower(query)).unwrap();
    let selected =
        asap_logical_optimizer::pass1::maintained_population::MaintainedPopulationStrategy::new(
            std::slice::from_ref(&root),
        )
        .candidate(&root)
        .unwrap();
    compile_physical_asap_dag(&selected).unwrap()
}

/// Raw scan nodes are the frontier; everything above them is compiled.
fn raw_inputs(dag: &PhysicalASAPDAG) -> Vec<(u64, Arc<Schema>, String)> {
    dag.nodes
        .iter()
        .filter_map(|node| match &node.payload {
            PhysicalASAPOperatorPayload::Relational {
                operator: planner_types::ir::export::NonASAPOpKind::TimeRange { .. },
            } => {
                let mut id = node.id;
                loop {
                    let n = dag.nodes.iter().find(|n| n.id == id)?;
                    if let PhysicalASAPOperatorPayload::Relational {
                        operator:
                            planner_types::ir::export::NonASAPOpKind::Scan {
                                source: planner_types::ir::operator::Source::TimeSeries { metric },
                                ..
                            },
                    } = &n.payload
                    {
                        return Some((
                            u64::from(node.id.0),
                            Arc::new(node.output_schema.clone()),
                            metric.clone(),
                        ));
                    }
                    id = dag.edges.iter().find(|e| e.consumer == id)?.producer;
                }
            }
            _ => None,
        })
        .collect()
}

type Sample = (&'static str, &'static str, &'static str, i64, f64);

/// Compile, round-trip, bind raw `(metric, job, instance, ts, value)` samples,
/// and return the root's batches.
fn execute(
    dag: &PhysicalASAPDAG,
    samples: &[Sample],
    end: i64,
) -> Result<Vec<asap_executor::runtime::SharedValue<Batch>>, String> {
    execute_relabeled(dag, samples, end, &BTreeMap::new())
}

/// [`execute`], supplying samples of each instance in `relabel` under its
/// `(__name__, instance)` instead.
fn execute_relabeled(
    dag: &PhysicalASAPDAG,
    samples: &[Sample],
    end: i64,
    relabel: &BTreeMap<&str, (&str, &str)>,
) -> Result<Vec<asap_executor::runtime::SharedValue<Batch>>, String> {
    let inputs = raw_inputs(dag);
    let program = compile(
        dag,
        inputs
            .iter()
            .map(|(id, schema, _)| (*id, InputContract::bounded(schema.clone())))
            .collect(),
        &[u64::from(dag.roots[0].0)],
    )
    .map_err(|e| e.to_string())?;
    let program: CompiledPhysicalDAG =
        serde_json::from_slice(&serde_json::to_vec(&program).unwrap()).unwrap();
    let sources = inputs
        .iter()
        .map(|(id, schema, metric)| {
            let rows = samples
                .iter()
                .filter(|sample| sample.0 == metric)
                .map(|(name, job, instance, at, value)| {
                    let (name, instance) =
                        relabel.get(instance).copied().unwrap_or((*name, *instance));
                    let labels = BTreeMap::from([
                        ("__name__".to_string(), name.to_string()),
                        ("job".into(), job.to_string()),
                        ("instance".into(), instance.to_string()),
                    ]);
                    if schema
                        .fields
                        .iter()
                        .any(|f| f.name == promql_rows::SERIES_IDENTITY_COLUMN)
                    {
                        promql_rows::series_row(schema, &labels, *at, *value).unwrap()
                    } else {
                        schema
                            .fields
                            .iter()
                            .enumerate()
                            .map(|(i, f)| match f.name.as_str() {
                                _ if Some(i) == schema.time_index => Value::Timestamp(*at),
                                "value" => Value::Float64(*value),
                                label => Value::Utf8(labels[label].clone().into()),
                            })
                            .collect()
                    }
                })
                .collect();
            let batch = Batch::try_new(schema.clone(), rows).unwrap();
            (
                *id,
                Box::new(Operator::source(schema.clone(), vec![batch]).unwrap()) as Source<'_>,
            )
        })
        .collect();
    let physical_dag = program.instantiate(sources).map_err(|e| e.to_string())?;
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: end,
            revision: 0,
        },
        Limits::default(),
    )
    .unwrap();
    block_on(async {
        let mut stream = physical_dag
            .execute(program.roots(), context)
            .map_err(|e| e.to_string())?
            .remove(0);
        let mut batches = Vec::new();
        while let Some(batch) = stream.next().await {
            batches.push(batch.map_err(|e| e.to_string())?);
        }
        Ok(batches)
    })
}

/// [`execute`], returning `(job, value)` rows of the root.
fn run(
    dag: &PhysicalASAPDAG,
    samples: &[Sample],
    end: i64,
) -> Result<BTreeMap<String, f64>, String> {
    let mut rows = BTreeMap::new();
    for batch in execute(dag, samples, end)? {
        let job = batch.schema().fields.iter().position(|f| f.name == "job");
        for row in batch.rows() {
            let key = match job.map(|i| &row[i]) {
                Some(Value::Utf8(job)) => job.to_string(),
                _ => String::new(),
            };
            let value = match row.last() {
                Some(Value::Float64(v)) => *v,
                Some(Value::Int64(v)) => *v as f64,
                other => return Err(format!("unexpected value {other:?}")),
            };
            assert!(rows.insert(key, value).is_none(), "duplicate output group");
        }
    }
    Ok(rows)
}

const SAMPLES: &[Sample] = &[
    ("m", "api", "a", 10_000, 4.),
    ("m", "api", "a", 50_000, 1.),
    ("m", "api", "b", 40_000, 7.),
    ("m", "api", "c", 30_000, 2.),
    ("m", "db", "d", 20_000, 5.),
];

fn reference(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

// Current-series aggregates read the latest member values, matching the
// backend CurrentSeriesStore formulas (PromQL quantile interpolation).
#[test]
fn population_aggregates_match_current_series_reference() {
    // Latest values: api = {a: 1, b: 7, c: 2}; db = {d: 5}.
    for (query, expected) in [
        ("sum by (job) (m)", reference(&[("api", 10.), ("db", 5.)])),
        ("count by (job) (m)", reference(&[("api", 3.), ("db", 1.)])),
        (
            "avg by (job) (m)",
            reference(&[("api", 10. / 3.), ("db", 5.)]),
        ),
        // Sorted api = [1, 2, 7]; rank 0.25 * 2 = 0.5 → 1.5.
        (
            "quantile by (job) (0.25, m)",
            reference(&[("api", 1.5), ("db", 5.)]),
        ),
    ] {
        let dag = population_dag(query);
        assert_eq!(run(&dag, SAMPLES, 60_000).unwrap(), expected, "{query}");
    }
}

// A global evaluation of an empty population is an empty vector, as in PromQL.
#[test]
fn global_population_aggregate_of_no_members_is_empty() {
    // Latest values are [1, 2, 5, 7] at 60s; every member has expired by 1000s.
    for (query, expected) in [("sum(m)", 15.), ("count(m)", 4.), ("quantile(0.5, m)", 3.5)] {
        let dag = population_dag(query);
        let live = run(&dag, SAMPLES, 60_000).unwrap();
        assert_eq!(live, reference(&[("", expected)]), "{query}");
        assert!(run(&dag, SAMPLES, 1_000_000).unwrap().is_empty(), "{query}");
    }
}

// Scalar operands on either side apply to every grouped value, including negation.
#[test]
fn scalar_literal_arithmetic_applies_to_grouped_values() {
    // sum_over_time over 5m per job: api = 4 + 1 + 7 + 2 = 14, db = 5.
    for (query, expected) in [
        (
            "sum by (job) (sum_over_time(m[5m])) * 2",
            reference(&[("api", 28.), ("db", 10.)]),
        ),
        (
            "100 - sum by (job) (sum_over_time(m[5m]))",
            reference(&[("api", 86.), ("db", 95.)]),
        ),
        (
            "-sum by (job) (sum_over_time(m[5m]))",
            reference(&[("api", -14.), ("db", -5.)]),
        ),
    ] {
        assert_eq!(
            run(&exact_dag(query), SAMPLES, 60_000).unwrap(),
            expected,
            "{query}"
        );
    }
}

// Grouped vectors match one-to-one on labels; unmatched groups are dropped and
// unchecked division by zero yields +Inf as in PromQL.
#[test]
fn grouped_vector_arithmetic_matches_labels() {
    let samples: &[Sample] = &[
        ("a", "api", "x", 10_000, 6.),
        ("a", "api", "y", 20_000, 3.),
        ("a", "db", "x", 10_000, 1.),
        ("a", "web", "x", 10_000, 1.),
        ("b", "api", "x", 10_000, 3.),
        ("b", "db", "x", 10_000, 0.),
        ("b", "cache", "x", 10_000, 1.),
    ];
    let dag =
        exact_dag("sum by (job) (sum_over_time(a[5m])) / sum by (job) (sum_over_time(b[5m]))");
    assert_eq!(
        run(&dag, samples, 60_000).unwrap(),
        reference(&[("api", 3.), ("db", f64::INFINITY)])
    );
}

// Exact observation counts finalize to the Float64 value PromQL declares,
// then roll up per job: api has 2 + 1 + 1 samples in 5m, db has 1.
#[test]
fn exact_count_finalizes_to_declared_float_value() {
    let dag = exact_dag("sum by (job) (count_over_time(m[5m]))");
    assert_eq!(
        run(&dag, SAMPLES, 60_000).unwrap(),
        reference(&[("api", 4.), ("db", 1.)])
    );
}

/// `dag` with its Binary operator replaced by `kind`.
fn with_kind(
    mut dag: PhysicalASAPDAG,
    kind: planner_types::ir::operator::BinaryOpKind,
    bool_result: bool,
) -> PhysicalASAPDAG {
    for node in &mut dag.nodes {
        if let PhysicalASAPOperatorPayload::Relational {
            operator:
                planner_types::ir::export::NonASAPOpKind::BinaryOp {
                    operator,
                    return_bool,
                },
        } = &mut node.payload
        {
            operator.kind = kind.clone();
            *return_bool = bool_result;
        }
    }
    dag
}

// A comparison Binary over grouped values keeps the groups whose comparison
// holds, with their value, on either side of the literal; `bool` yields 1 or 0.
#[test]
fn grouped_comparisons_filter_or_return_bool() {
    for (query, expected) in [
        (
            "sum by(job)(sum_over_time(m[5m])) > 10",
            reference(&[("api", 14.)]),
        ),
        (
            "sum by(job)(sum_over_time(m[5m])) > bool 10",
            reference(&[("api", 1.), ("db", 0.)]),
        ),
        (
            "10 > sum by(job)(sum_over_time(m[5m]))",
            reference(&[("db", 5.)]),
        ),
    ] {
        assert_eq!(run(&exact_dag(query), SAMPLES, 60_000).unwrap(), expected);
    }
}

// A `bool` comparison Binary over per-series evaluations matches one-to-one and
// drops the metric name; a filter keeps the surviving left value.
#[test]
fn per_series_comparisons_filter_or_return_bool() {
    use planner_types::ir::operator::BinaryOpKind::*;
    use planner_types::ir::scalar::CompareOpKind::*;

    let samples = counter("a", "api", 10., 10.)
        .chain(counter("a", "db", 10., 10.))
        .chain(counter("b", "api", 5., 5.))
        .chain(counter("b", "db", 20., 20.))
        .collect::<Vec<_>>();
    // rate: a{api} = a{db} = 50/300, b{api} = 25/300, b{db} = 100/300.
    let dag = exact_dag("rate(a[5m]) / rate(b[5m])");
    assert_eq!(
        run_series(
            &with_kind(dag.clone(), Compare(Gt), false),
            &samples,
            300_000
        )
        .unwrap(),
        series(&[("api", "x", 50. / 300.)])
    );
    assert_eq!(
        run_series(&with_kind(dag, Compare(Lt), true), &samples, 300_000).unwrap(),
        series(&[("api", "x", 0.), ("db", "x", 1.)])
    );
}

/// [`execute`], returning per-series `(identity, value)` rows of the root,
/// with NaN-aware formatting for comparison.
fn run_series(dag: &PhysicalASAPDAG, samples: &[Sample], end: i64) -> Result<String, String> {
    let mut rows = BTreeMap::new();
    for batch in execute(dag, samples, end)? {
        let schema = batch.schema();
        let column = |name: &str| schema.fields.iter().position(|f| f.name == name).unwrap();
        let (identity, value) = (column(promql_rows::SERIES_IDENTITY_COLUMN), column("value"));
        for row in batch.rows() {
            let (Value::Utf8(identity), Value::Float64(value)) = (&row[identity], &row[value])
            else {
                return Err(format!("unexpected row {row:?}"));
            };
            assert!(rows.insert(identity.to_string(), *value).is_none());
        }
    }
    Ok(format!("{rows:?}"))
}

fn series(pairs: &[(&str, &str, f64)]) -> String {
    let rows = pairs
        .iter()
        .map(|(job, instance, value)| {
            (
                format!(r#"{{"instance":"{instance}","job":"{job}"}}"#),
                *value,
            )
        })
        .collect::<BTreeMap<_, _>>();
    format!("{rows:?}")
}

/// Five counter samples per series, one per minute up to 300s, at `base + step * i`.
fn counter(
    metric: &'static str,
    job: &'static str,
    base: f64,
    step: f64,
) -> impl Iterator<Item = Sample> {
    (1..=5).map(move |i| (metric, job, "x", i * 60_000, base + step * (i - 1) as f64))
}

// Stored per-series sum and count states divide per series and drop the
// metric name, as Prometheus does.
#[test]
fn per_series_average_divides_stored_sum_by_count() {
    let dag = exact_dag("sum_over_time(m[5m]) / count_over_time(m[5m])");
    assert_eq!(
        run_series(&dag, SAMPLES, 60_000).unwrap(),
        series(&[
            ("api", "a", 2.5),
            ("api", "b", 7.),
            ("api", "c", 2.),
            ("db", "d", 5.)
        ])
    );
}

// rate(a) / rate(b) matches series on their labels without the metric name.
// Unmatched series are dropped; x/0 is +Inf and 0/0 is NaN; an empty side
// yields an empty vector.
#[test]
fn per_series_rate_ratio_matches_prometheus() {
    // Window (0, 300s]: first sample at 60s, extrapolated 60s to the start
    // (durationToZero is exactly 60s too), so rate = (last - first) * 1.25 / 300.
    // a{api} = 40 * 1.25 / 300, b{api} = 20 * 1.25 / 300, so the ratio is 2.
    let samples = counter("a", "api", 10., 10.)
        .chain(counter("a", "db", 10., 10.))
        .chain(counter("a", "web", 10., 10.))
        .chain(counter("a", "cache", 7., 0.))
        .chain(counter("b", "api", 5., 5.))
        .chain(counter("b", "web", 7., 0.))
        .chain(counter("b", "cache", 7., 0.))
        .chain(counter("b", "other", 5., 5.))
        .collect::<Vec<_>>();
    let dag = exact_dag("rate(a[5m]) / rate(b[5m])");
    assert_eq!(
        run_series(&dag, &samples, 300_000).unwrap(),
        series(&[
            ("api", "x", 2.),
            ("cache", "x", f64::NAN),
            ("web", "x", f64::INFINITY),
        ])
    );
    let only_a = samples
        .iter()
        .filter(|s| s.0 == "a")
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(run_series(&dag, &only_a, 300_000).unwrap(), series(&[]));
}

// A literal operand applies to every stored per-series value, on either side,
// and drops the metric name.
#[test]
fn per_series_scalar_arithmetic_applies_to_stored_evaluations() {
    let samples = counter("m", "api", 10., 10.).collect::<Vec<_>>();
    // rate = 40 * 1.25 / 300 = 1/6.
    for (query, expected) in [
        ("rate(m[5m]) * 2", 50. / 300. * 2.),
        ("1 - rate(m[5m])", 1. - 50. / 300.),
        // The stored sum evaluation keeps `__name__`; the arithmetic drops it.
        ("sum_over_time(m[5m]) * 2", 150. * 2.),
    ] {
        assert_eq!(
            run_series(&exact_dag(query), &samples, 300_000).unwrap(),
            series(&[("api", "x", expected)]),
            "{query}"
        );
    }
}

// A literal operand drops the metric name; series that then share a label set
// are an error, as in Prometheus, rather than duplicate output series.
#[test]
fn per_series_scalar_arithmetic_rejects_label_sets_equal_without_the_name() {
    let dag = exact_dag("sum_over_time(m[5m]) * 2");
    let samples = counter("m", "api", 10., 10.)
        .chain(counter("m", "api", 1., 1.).map(|s| (s.0, s.1, "y", s.3, s.4)))
        .collect::<Vec<_>>();
    let distinct = BTreeMap::from([("y", ("n", "y"))]);
    assert!(execute_relabeled(&dag, &samples, 300_000, &distinct).is_ok());
    // Both series are then {instance="x",job="api"}, named m and n.
    let equal = BTreeMap::from([("y", ("n", "x"))]);
    let Err(error) = execute_relabeled(&dag, &samples, 300_000, &equal) else {
        panic!("duplicate label sets were accepted");
    };
    assert!(error.contains("same labelset"), "{error}");
}

fn with_vector_match(
    mut dag: PhysicalASAPDAG,
    kind: planner_types::ir::operator::VectorMatchKind,
    labels: &[&str],
) -> PhysicalASAPDAG {
    for node in &mut dag.nodes {
        if let PhysicalASAPOperatorPayload::Relational {
            operator:
                planner_types::ir::export::NonASAPOpKind::BinaryOp {
                    operator,
                    return_bool: _,
                },
        } = &mut node.payload
        {
            operator.vector_match = Some(planner_types::ir::operator::VectorMatch {
                kind: kind.clone(),
                labels: labels.iter().map(|l| l.to_string()).collect(),
                grouping: None,
            });
        }
    }
    dag
}

// `on` and `ignoring` reduce each side to the matching labels, which become
// the result's labels; a duplicate match group on either side is an error.
#[test]
fn per_series_vector_matching_follows_on_and_ignoring() {
    use planner_types::ir::operator::VectorMatchKind;
    let samples = counter("a", "api", 10., 10.)
        .chain(counter("b", "api", 5., 5.))
        .chain(counter("b", "db", 5., 5.))
        .collect::<Vec<_>>();
    let dag = exact_dag("rate(a[5m]) / rate(b[5m])");
    for (kind, labels) in [
        (VectorMatchKind::On, ["job"]),
        (VectorMatchKind::Ignoring, ["instance"]),
    ] {
        let dag = with_vector_match(dag.clone(), kind, &labels);
        assert_eq!(
            run_series(&dag, &samples, 300_000).unwrap(),
            format!(
                "{:?}",
                BTreeMap::from([(r#"{"job":"api"}"#.to_string(), 2.)])
            )
        );
        let mut duplicate = samples.clone();
        duplicate.extend(counter("b", "api", 5., 5.).map(|s| (s.0, s.1, "y", s.3, s.4)));
        let error = run_series(&dag, &duplicate, 300_000).unwrap_err();
        assert!(error.contains("duplicate series"), "{error}");
        let mut duplicate = samples.clone();
        duplicate.extend(counter("a", "api", 5., 5.).map(|s| (s.0, s.1, "y", s.3, s.4)));
        let error = run_series(&dag, &duplicate, 300_000).unwrap_err();
        assert!(error.contains("many-to-one"), "{error}");
    }
}

// Current-series sums and averages are compensated like Prometheus, and an
// overflowing running sum does not turn the average into +Inf.
#[test]
fn population_sums_and_averages_are_compensated() {
    let cancel: &[Sample] = &[
        ("m", "api", "a", 50_000, 1e100),
        ("m", "api", "b", 50_000, 1.),
        ("m", "api", "c", 50_000, -1e100),
    ];
    let huge: &[Sample] = &[
        ("m", "api", "a", 50_000, 1.7e308),
        ("m", "api", "b", 50_000, 1.7e308),
    ];
    for (query, samples, expected) in [
        ("sum by (job) (m)", cancel, 1.),
        ("avg by (job) (m)", cancel, 1. / 3.),
        ("avg by (job) (m)", huge, 1.7e308),
    ] {
        let dag = population_dag(query);
        assert_eq!(
            run(&dag, samples, 60_000).unwrap(),
            reference(&[("api", expected)]),
            "{query}"
        );
    }
}

// A bare count over stored Count-Min state compiles to a Planner evaluation that
// returns the sketch's total update weight, including colliding items.
#[test]
fn stored_count_min_bare_count_compiles_to_a_evaluation() {
    use asap_executor::summary_kernels::CountMinSketchAccumulator;
    use asap_logical_optimizer::{Replacement, ReplacementStrategy, TargetSubDAG};
    let root = lower_with("count(up)", AccuracyTarget::Epsilon(0.02));
    let dag = asap_logical_optimizer::ASAPStrategies::default()
        .replacements(&TargetSubDAG::new(&root))
        .into_iter()
        .find_map(|candidate| match candidate.replacement {
            Replacement::SubDAG(node) => {
                let dag = compile_physical_asap_dag(&node).ok()?;
                let bare_count = dag.nodes.iter().any(|n| {
                    matches!(
                        &n.payload,
                        PhysicalASAPOperatorPayload::SummaryEstimate {
                            query: SketchStatistic::PointCount { value: None, .. }
                        }
                    )
                });
                let count_min = dag.nodes.iter().any(|n| {
                    matches!(&n.payload, PhysicalASAPOperatorPayload::SummaryAgg {
                        family: FieldDataType::Sketch(kind, _), ..
                    } if kind.algorithm() == &SketchAlgorithm::Cms)
                });
                (bare_count && count_min).then_some(dag)
            }
            _ => None,
        })
        .expect("Planner lists a Count-Min candidate for count(up)");
    let state = dag
        .nodes
        .iter()
        .find(|n| matches!(n.payload, PhysicalASAPOperatorPayload::SummaryAgg { .. }))
        .unwrap();
    let PhysicalASAPOperatorPayload::SummaryAgg {
        family: FieldDataType::Sketch(kind, _),
        ..
    } = &state.payload
    else {
        unreachable!()
    };
    let SketchParams::Cms { width, depth } = kind.params() else {
        unreachable!()
    };
    let schema = Arc::new(state.output_schema.clone());
    let program = compile(
        &dag,
        BTreeMap::from([(
            u64::from(state.id.0),
            InputContract::bounded(schema.clone()),
        )]),
        &[u64::from(dag.roots[0].0)],
    )
    .unwrap();
    let program: CompiledPhysicalDAG =
        serde_json::from_slice(&serde_json::to_vec(&program).unwrap()).unwrap();
    let mut sketch = CountMinSketchAccumulator::new(*depth as usize, *width as usize);
    sketch.inner.update("a", 3.0);
    sketch.inner.update("b", 7.0);
    let row = schema
        .fields
        .iter()
        .map(|field| match &field.dtype {
            FieldDataType::Plain(_) => panic!("unexpected stored column {field:?}"),
            family => Value::Summary {
                family: family.clone(),
                state: Arc::new(sketch.clone()),
            },
        })
        .collect();
    let batch = Batch::try_new(schema.clone(), vec![row]).unwrap();
    let physical_dag = program
        .instantiate(BTreeMap::from([(
            u64::from(state.id.0),
            Box::new(Operator::source(schema, vec![batch]).unwrap()) as Source<'_>,
        )]))
        .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 0,
        },
        Limits::default(),
    )
    .unwrap();
    let values = block_on(async {
        let mut stream = physical_dag
            .execute(program.roots(), context)
            .unwrap()
            .remove(0);
        let mut values = Vec::new();
        while let Some(batch) = stream.next().await {
            values.extend(batch.unwrap().rows().iter().map(|row| row[0].clone()));
        }
        values
    });
    assert!(
        matches!(values.as_slice(), [Value::Int64(10)]),
        "{values:?}"
    );
}
