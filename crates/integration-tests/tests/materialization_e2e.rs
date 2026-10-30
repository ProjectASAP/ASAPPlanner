//! Materialization decides placement, not answers: the same selected DAG
//! timed with its states `NotMaterialized` (everything at query time) and
//! `MatAtIngestion` (materialized views refreshed at ingestion time)
//! executes to the same result.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use asap_executor::physical_planner::{compile, promql_rows};
use asap_executor::runtime::Scope;
use asap_executor::values::{Batch, SchemaRef, Value};
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::planning::{
    explicit_local_candidates, explicit_summary_candidate, selected_dag,
};
use asap_types::ir::operator::Source;
use asap_types::ir::physical_export::{
    compile_physical_asap_dag, PhysicalASAPDAG, PhysicalASAPOperatorPayload,
};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::properties::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::{
    apply_materialization_timings, split_shared_by_timing, ASAPOp, MaterializationAssignment,
    NodeToTimedNodeCache, NonASAPOp, OperatorNode,
};
use asap_types::physical::{Materialization, Medium};

use crate::physical_common;

const AT_INGESTION: Materialization = Materialization::MatAtIngestion(Medium::Memory);

/// The logical DAG the planner pipeline selects for `query`, optionally with
/// a PromQL series identity (per-entity state needs one).
fn selected(query: &str, series_identity: bool) -> Rc<OperatorNode> {
    selected_with(query, series_identity, AccuracyTarget::max_error(0.01))
}

/// [`selected`] for a quantile query, whose target names its metric (a
/// rank error).
fn selected_quantile(query: &str, series_identity: bool) -> Rc<OperatorNode> {
    selected_with(
        query,
        series_identity,
        AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Rank),
    )
}

fn selected_with(query: &str, series_identity: bool, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    let mut root = lower_promql(query, accuracy.clone()).unwrap();
    if series_identity {
        root = promql_rows::with_series_identity(&root).unwrap();
    }
    selected_dag(root, accuracy)
}

/// The first logical candidate for `query` that builds a sketch: plan
/// selection picks the exact plan while every summary runs at query time.
fn sketch_candidate(query: &str) -> Rc<OperatorNode> {
    let accuracy = AccuracyTarget::max_error(0.01);
    let root = lower_promql(query, accuracy.clone()).unwrap();
    explicit_local_candidates(root)
        .into_iter()
        .find(|candidate| {
            OperatorNode::reachable(candidate).iter().any(|node| {
                matches!(
                    node.asap(),
                    Some(ASAPOp::SummaryAgg {
                        family: asap_types::ir::schema::FieldDataType::Sketch(..),
                        ..
                    })
                )
            })
        })
        .expect("a sketch candidate")
}

/// The two placements under test: nothing stored, and every state stored at
/// ingestion time where the DAG allows it.
fn placements() -> [MaterializationAssignment; 2] {
    [
        MaterializationAssignment::default(),
        MaterializationAssignment::with_default(AT_INGESTION),
    ]
}

/// `root` timed under `assignment`, as Stage 2 times a plan: a node read at
/// two timings (Pass 1 composes identical inputs as one memo group's node,
/// e.g. the scan of two quantiles of one metric) is split first, one copy
/// per timing.
fn timed_dag(root: &Rc<OperatorNode>, assignment: &MaterializationAssignment) -> PhysicalASAPDAG {
    let (split, assignment) = split_shared_by_timing(std::slice::from_ref(root), assignment);
    let timed =
        apply_materialization_timings(&split[0], &assignment, &mut NodeToTimedNodeCache::new())
            .unwrap();
    compile_physical_asap_dag(&timed).unwrap()
}

fn states(dag: &PhysicalASAPDAG) -> Vec<u64> {
    dag.nodes
        .iter()
        .filter(|node| node.materialization == AT_INGESTION)
        .map(|node| node.id as u64)
        .collect()
}

fn floats(result: &[Vec<Batch>]) -> Vec<f64> {
    result[0]
        .iter()
        .flat_map(|batch| batch.to_rows())
        .flatten()
        .filter_map(|value| match value {
            Value::Float64(value) => Some(value),
            _ => None,
        })
        .collect()
}

/// Execute a timed DAG over one raw-source batch per `Scan`
/// (`batch(schema, metric)`): each materialized view refreshed in an
/// ingestion run over `ingestion`, then the query at `evaluation_time_ms`
/// reading the views' states. Returns the root's values and the views.
fn execute(
    dag: &PhysicalASAPDAG,
    batch: &dyn Fn(&SchemaRef, &str) -> Batch,
    ingestion: Scope,
    evaluation_time_ms: i64,
) -> (Vec<f64>, Vec<u64>) {
    let mut dag = dag.clone();
    let views = dag
        .set_retention(&HashMap::new(), &HashMap::new(), None)
        .unwrap();
    let raw = |dag: &PhysicalASAPDAG| {
        physical_common::raw_data_store(dag, |source, schema| {
            let Source::TimeSeries { metric } = source else {
                panic!("a time series source")
            };
            batch(schema, metric).to_rows()
        })
    };
    // Each view's ingestion run refreshes it from the raw data.
    let mut store = raw(&dag);
    for view in &views {
        let refresh = compile(&dag, &[], &[view.node_id as u64]).unwrap();
        let states = physical_common::execute(&refresh, raw(&dag), ingestion.clone()).remove(0);
        store = store.with_materialized_view(view.node_id, states).unwrap();
    }
    let query = compile(&dag, &views, &[dag.roots[0] as u64]).unwrap();
    let result = physical_common::execute(&query, store, Scope::Query { evaluation_time_ms });
    (
        floats(&result),
        views.iter().map(|view| view.node_id as u64).collect(),
    )
}

/// `(metric, job, seconds, value)` samples as one batch per metric.
fn job_samples<'a>(
    samples: &'a [(&'a str, &'a str, i64, f64)],
) -> impl Fn(&SchemaRef, &str) -> Batch + 'a {
    move |schema, name| {
        let rows = samples
            .iter()
            .filter(|sample| sample.0 == name)
            .map(|(metric, job, seconds, value)| {
                let labels = BTreeMap::from([
                    ("__name__".to_string(), metric.to_string()),
                    ("job".to_string(), job.to_string()),
                ]);
                promql_rows::series_row(schema, &labels, seconds * 1000, *value).unwrap()
            })
            .collect();
        Batch::try_new(schema.clone(), rows).unwrap()
    }
}

/// Values 1..=100 at 300 s, one row each, in every Float64 column of
/// `schema`.
fn values_1_to_100(schema: &SchemaRef, _metric: &str) -> Batch {
    let rows = (1..=100)
        .map(|value| {
            schema
                .fields
                .iter()
                .map(|field| match field.dtype {
                    asap_types::ir::schema::FieldDataType::Plain(
                        asap_types::ir::schema::DataType::Float64,
                    ) => Value::Float64(f64::from(value)),
                    asap_types::ir::schema::FieldDataType::Plain(
                        asap_types::ir::schema::DataType::Timestamp,
                    ) => Value::Timestamp(300_000),
                    _ => panic!("unexpected field {field:?}"),
                })
                .collect()
        })
        .collect();
    Batch::try_new(schema.clone(), rows).unwrap()
}

/// `MatAtIngestion` puts the quantile's state in a materialized view;
/// `NotMaterialized` has no view and reads the raw source at
/// query time; both answer alike.
#[test]
fn materialization_decides_precompute_contents() {
    let root = sketch_candidate("quantile(0.99, latency)");
    let mut answers = Vec::new();
    for assignment in placements() {
        let dag = timed_dag(&root, &assignment);
        let ingestion = Scope::Ingestion {
            window_start_ms: 0,
            window_end_ms: 300_000,
        };
        let (values, frontier) = execute(&dag, &values_1_to_100, ingestion, 300_000);
        assert_eq!(frontier, states(&dag));
        answers.push(values);
    }
    assert_eq!(answers[1].len(), 1);
    assert!(!states(&timed_dag(&root, &placements()[1])).is_empty());
    assert_eq!(answers[0], answers[1]);
}

/// Mixed placement in one DAG: one summary `MatAtIngestion` (a materialized view)
/// and the other `NotMaterialized` (built from the raw source at query
/// time) answer like the all-query-time placement.
#[test]
fn mixed_placement_answers_like_query_time() {
    let accuracy = AccuracyTarget::max_error(0.01);
    let root = lower_promql(
        "quantile(0.99, latency) + quantile(0.5, latency)",
        accuracy.clone(),
    )
    .unwrap();
    let sketches = |candidate: &Rc<OperatorNode>| -> Vec<Rc<OperatorNode>> {
        OperatorNode::reachable(candidate)
            .into_iter()
            .filter(|node| {
                matches!(
                    node.asap(),
                    Some(ASAPOp::SummaryAgg {
                        family: asap_types::ir::schema::FieldDataType::Sketch(..),
                        ..
                    })
                )
            })
            .collect()
    };
    let root = explicit_summary_candidate(root, asap_types::ir::schema::SketchAlgorithm::Kll);
    assert_eq!(
        sketches(&root).len(),
        2,
        "two explicitly selected KLL builds"
    );
    let mut mixed = MaterializationAssignment::default();
    mixed.set(&sketches(&root)[0], AT_INGESTION);
    let mut answers = Vec::new();
    for assignment in [MaterializationAssignment::default(), mixed] {
        let dag = timed_dag(&root, &assignment);
        let ingestion = Scope::Ingestion {
            window_start_ms: 0,
            window_end_ms: 300_000,
        };
        let (values, frontier) = execute(&dag, &values_1_to_100, ingestion, 300_000);
        assert_eq!(frontier, states(&dag));
        answers.push((values, frontier.len()));
    }
    // Only the assigned sketch is stored.
    assert_eq!(answers[0].1, 0);
    assert_eq!(answers[1].1, 1);
    assert_eq!(answers[1].0.len(), 1);
    assert_eq!(answers[0].0, answers[1].0);
}

/// Whether the DAG has an ingestion-time `BinaryOp`.
fn ingestion_binary(dag: &PhysicalASAPDAG) -> bool {
    dag.nodes.iter().any(|node| {
        matches!(
            node.payload,
            PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::BinaryOp { .. })
        ) && node.output_state.timing == ExecutionTiming::IngestionTime
    })
}

const INGESTION_WINDOW: Scope = Scope::Ingestion {
    window_start_ms: 240_000,
    window_end_ms: 300_000,
};

/// Prometheus drops series without a match: arithmetic over different
/// selectors keeps only label sets present on both sides (none when
/// disjoint), at either placement, and never becomes aligned maintenance.
#[test]
fn arithmetic_over_different_selectors_matches_prometheus() {
    // The selected root reads the outer `sum`'s exact state through its
    // finalize.
    let root = selected("sum(sum_over_time(m[1m]) + sum_over_time(n[1m]))", true);
    assert!(matches!(
        root.asap(),
        Some(ASAPOp::FinalizeExactAccumulator { .. })
    ));
    let disjoint = [
        ("m", "a", 250, 1.0),
        ("m", "a", 290, 2.0),
        ("n", "b", 250, 5.0),
    ];
    // Only job a is on both sides: m_a + n_a = (1 + 2) + 7; m{job="b"} is dropped.
    let overlapping = [
        ("m", "a", 250, 1.0),
        ("m", "a", 290, 2.0),
        ("m", "b", 250, 5.0),
        ("n", "a", 250, 7.0),
    ];
    for assignment in placements() {
        let dag = timed_dag(&root, &assignment);
        assert!(!ingestion_binary(&dag));
        let (values, _) = execute(&dag, &job_samples(&disjoint), INGESTION_WINDOW, 300_000);
        assert!(values.is_empty(), "{values:?}");
        let (values, _) = execute(&dag, &job_samples(&overlapping), INGESTION_WINDOW, 300_000);
        assert_eq!(values, [10.0]);
    }
    let root = selected_quantile(
        "quantile(0.9, sum_over_time(m[1m]) + sum_over_time(n[1m]))",
        true,
    );
    assert!(!ingestion_binary(&timed_dag(&root, &placements()[1])));
}

/// Arithmetic over one selector is aligned maintenance when stored at
/// ingestion time, and adds each series' two evaluations before the
/// quantile at either placement.
#[test]
#[ignore = "the two identical selectors stay separate until Pass 2 merges identical sub-DAGs (#580)"]
fn arithmetic_over_one_selector_executes() {
    let root = selected_quantile(
        "quantile(0.9, sum_over_time(m[1m]) + sum_over_time(m[1m]))",
        true,
    );
    let samples = [
        ("m", "a", 250, 1.0),
        ("m", "a", 290, 2.0),
        ("m", "b", 250, 5.0),
    ];
    let [not_materialized, at_ingestion] = placements().map(|a| timed_dag(&root, &a));
    assert!(!ingestion_binary(&not_materialized));
    assert!(
        ingestion_binary(&at_ingestion),
        "one selector shares its key set"
    );
    for dag in [not_materialized, at_ingestion] {
        let (values, _) = execute(&dag, &job_samples(&samples), INGESTION_WINDOW, 300_000);
        // job a: 3 + 3 = 6; job b: 5 + 5 = 10 (mispairing a with b gives 8 and 8).
        // KLL at epsilon 0.01 returns an input value within 0.01 of rank 0.9;
        // of two values only the larger is.
        assert_eq!(values, [10.0]);
    }
}
