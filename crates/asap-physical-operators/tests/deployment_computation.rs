//! Planner-selected PromQL computation compiles from the timed DAG alone;
//! the deployment supplies only raw rows at the ingestion frontier.
use asap_physical_operators::{
    operators::Operator,
    physical_planner::{compile, promql_rows, CompiledPhysicalDag, InputContract, Source},
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Value},
};
use futures::{executor::block_on, StreamExt};
use planner_types::{post_asap::*, pre_asap::QueryExpr, types::AccuracyTarget, workload::*};
use std::{collections::BTreeMap, rc::Rc, sync::Arc};

fn lower(query: &str) -> QueryExpr {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(AccuracyTarget::Exact),
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

/// The first exact summary candidate, as Planner selection would hand it over.
fn exact_dag(query: &str) -> PostAsapDag {
    use asap_aware_mapping::{Replacement, ReplacementStrategy, TargetSubDAG};
    let expression = lower(query);
    let root = Rc::new(promql_rows::with_series_identity(&expression).unwrap_or(expression));
    asap_aware_mapping::SketchAlgorithmStrategy::new(&asap_aware_mapping::DefaultCostModel)
        .replacements(&TargetSubDAG::new(&root))
        .into_iter()
        .find_map(|candidate| match candidate.replacement {
            Replacement::Summary(node) => {
                let dag = compile_post_asap_dag(&node).ok()?;
                dag.nodes
                    .iter()
                    .all(|n| !matches!(&n.payload, PostAsapOperatorPayload::SummaryAgg { family, .. } if !matches!(family, SummaryFamilyType::ExactAggregate(..))))
                    .then_some(dag)
            }
            _ => None,
        })
        .unwrap()
}

fn population_dag(query: &str) -> PostAsapDag {
    let root = Rc::new(promql_rows::with_series_identity(&lower(query)).unwrap());
    let selected = asap_aware_mapping::maintained_population::MaintainedPopulationStrategy::new(
        std::slice::from_ref(&root),
    )
    .candidate(&root)
    .unwrap();
    compile_post_asap_dag(&selected).unwrap()
}

/// Raw scan nodes are the frontier; everything above them is compiled.
fn raw_inputs(dag: &PostAsapDag) -> Vec<(u64, Arc<SummarySchema>, String)> {
    dag.nodes
        .iter()
        .filter_map(|node| match &node.payload {
            PostAsapOperatorPayload::Fallback {
                expression: QueryExpr::TimeRange { child, .. },
            } => match child.as_ref() {
                QueryExpr::Scan {
                    source: planner_types::pre_asap::Source::TimeSeries { metric },
                    ..
                } => Some((
                    u64::from(node.id.0),
                    Arc::new(node.output_schema.clone()),
                    metric.clone(),
                )),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

type Sample = (&'static str, &'static str, &'static str, i64, f64);

/// Compile, round-trip, bind raw `(metric, job, instance, ts, value)` samples,
/// and return `(job, value)` rows of the root.
fn run(dag: &PostAsapDag, samples: &[Sample], end: i64) -> Result<BTreeMap<String, f64>, String> {
    let inputs = raw_inputs(dag);
    let program = compile(
        dag,
        inputs
            .iter()
            .map(|(id, schema, _)| (*id, InputContract::bounded(schema.clone())))
            .collect(),
        &[u64::from(dag.root.0)],
    )
    .map_err(|e| e.to_string())?;
    let program: CompiledPhysicalDag =
        serde_json::from_slice(&serde_json::to_vec(&program).unwrap()).unwrap();
    let sources = inputs
        .iter()
        .map(|(id, schema, metric)| {
            let rows = samples
                .iter()
                .filter(|sample| sample.0 == metric)
                .map(|(name, job, instance, at, value)| {
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
    let graph = program.instantiate(sources).map_err(|e| e.to_string())?;
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: end,
            revision: 0,
        },
        Limits::default(),
    )
    .unwrap();
    block_on(async {
        let mut stream = graph
            .execute(program.roots(), context)
            .map_err(|e| e.to_string())?
            .remove(0);
        let mut rows = BTreeMap::new();
        while let Some(batch) = stream.next().await {
            let batch = batch.map_err(|e| e.to_string())?;
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
    })
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

// A global readout of an empty population is an empty vector, as in PromQL.
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
    let mut dag = exact_dag("sum by (job) (count_over_time(m[5m]))");
    let finalize = dag
        .nodes
        .iter()
        .find(|node| {
            matches!(
                node.payload,
                PostAsapOperatorPayload::Value {
                    operation: ValueOperation::FinalizeExactAccumulator
                }
            )
        })
        .unwrap()
        .clone();
    let root = dag.nodes.iter().find(|n| n.id == dag.root).unwrap().clone();
    let mut edge = dag
        .edges
        .iter()
        .find(|e| e.producer == finalize.id)
        .unwrap()
        .clone();
    // Read the rolled-up exact state the same way the query path does.
    let mut read = finalize.clone();
    read.id = PostAsapNodeId(root.id.0 + 1);
    read.output_schema = root.output_schema.clone();
    read.output_schema.fields.last_mut().unwrap().dtype =
        SummaryFamilyType::Plain(planner_types::pre_asap::DataType::Float64);
    edge.producer = root.id;
    edge.consumer = read.id;
    edge.intermediate_schema = root.output_schema.clone();
    edge.data_state = root.output_state;
    dag.root = read.id;
    dag.nodes.push(read);
    dag.edges.push(edge);
    assert_eq!(
        run(&dag, SAMPLES, 60_000).unwrap(),
        reference(&[("api", 4.), ("db", 1.)])
    );
}

// Comparisons need filter/bool semantics that `Binary` does not carry, so
// they fail at compile time instead of emitting 0/1 values.
#[test]
fn row_comparison_fails_closed() {
    let mut dag = exact_dag("sum by (job) (sum_over_time(m[5m])) * 2");
    for node in &mut dag.nodes {
        if let PostAsapOperatorPayload::Binary { operator } = &mut node.payload {
            operator.kind = planner_types::pre_asap::BinaryOpKind::Compare(
                planner_types::pre_asap::CompareOpKind::Gt,
            );
        }
    }
    let error = run(&dag, SAMPLES, 60_000).unwrap_err();
    assert!(error.contains("comparison"), "{error}");
}
