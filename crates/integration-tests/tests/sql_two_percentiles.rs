//! Repeating SQL percentiles over one table, every 10 s at ε = 0.01. Q1,
//! the overall p50 and p90 over the last 5 minutes: every candidate that
//! computes one of its percentiles exactly and the other from a sketch
//! compiles, is priced, and answers the exact one as `percentile_cont` does
//! and the sketched one within the sketch's bound.

mod physical_common;
mod planner_layering_common;

use asap_executor::values::Value;
use asap_frontend_sql::{lower_sql_dialect, SqlCatalog};
use asap_plan_selection::{PlanSelectionSettings, RejectionCause};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema, SketchAlgorithm};
use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode, QueryRoot};
use asap_types::workload::{PlanningWorkload, SqlDialect};
use planner_layering_common::*;
use std::rc::Rc;

const Q1: &str = "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY value), \
     percentile_cont(0.9) WITHIN GROUP (ORDER BY value) FROM metrics \
     WHERE ts > now() - INTERVAL '5' MINUTE";
/// Every query's accuracy target.
const EPSILON: f64 = 0.01;
const MINUTE_MS: i64 = 60_000;
/// The evaluation time of every run.
const EVALUATION_TIME_MS: i64 = 1_800_000_000_000;

/// `metrics(ts, city, value)`, time index `ts`.
fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "metrics",
        Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("city", DataType::Utf8, true),
                Field::plain("value", DataType::Float64, true),
            ],
            0,
            vec![],
        ),
    )
}

/// `queries`, each every 10 s at ε = [`EPSILON`], over `metrics` ingesting
/// 666.67 rows per second.
fn workload(queries: &[&str]) -> PlanningWorkload {
    let entry = |query: &&str| {
        format!(
            r#"{{"query": {query:?}, "demand": {{"fixed_interval": 10000}},
            "requirements": {{"accuracy": {{"max_error": {EPSILON}, "failure_probability": 0.001}}}},
            "predictability": {{"predictable": {{"known_at": null}}}}}}"#
        )
    };
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": {{"sql": "datafusion_sql"}}, "query_batch": null,
              "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 666.67}}}},
              "sources": {{"metrics": {{"series_count": 100}}}}}}}}"#,
        queries.iter().map(entry).collect::<Vec<_>>().join(", ")
    ))
    .unwrap()
}

/// Every stage over `queries`, with the executor's compile check.
async fn run(queries: &[&str]) -> Run {
    let workload = workload(queries);
    let mut roots = Vec::new();
    for entry in workload.query_workload.entries() {
        let root = lower_sql_dialect(
            &entry.query.0,
            &catalog(),
            SqlDialect::DataFusionSQL,
            entry.requirements.accuracy.clone(),
        )
        .await
        .unwrap_or_else(|e| panic!("{}: {e}", entry.query.0));
        roots.push(QueryRoot::Operator(root));
    }
    // Every output accepted whatever its guarantee, as `stage_pipeline`
    // plans by default: this test checks the answers against the bound.
    let settings = PlanSelectionSettings {
        ignore_accuracy_guarantees: true,
        ..PlanSelectionSettings::default()
    };
    run_stages_with_compile_check(&workload, roots, &settings)
}

/// Two rows a second over the 20 minutes before the evaluation: city
/// `c{i % 3}`, or none for every seventh row, and a heavy-tailed value from
/// a fixed linear congruential generator.
fn rows() -> Vec<(i64, Option<String>, f64)> {
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) as f64 / (1u64 << 31) as f64
    };
    (0..20 * 60 * 2)
        .map(|i: i64| {
            let ts = EVALUATION_TIME_MS - 20 * MINUTE_MS + (i + 1) * 500;
            let city = (i % 7 != 0).then(|| format!("c{}", i % 3));
            (ts, city, 10.0 / (1.0 - next()).max(1e-6).sqrt())
        })
        .collect()
}

/// The values of [`rows`] in `(T - window, T]` of the city `city` (every
/// city for `None`), sorted.
fn values_in(window_ms: i64, city: Option<&str>) -> Vec<f64> {
    let mut values: Vec<f64> = rows()
        .into_iter()
        .filter(|(ts, row_city, _)| {
            *ts > EVALUATION_TIME_MS - window_ms
                && city.is_none_or(|c| row_city.as_deref() == Some(c))
        })
        .map(|(_, _, value)| value)
        .collect();
    values.sort_by(f64::total_cmp);
    values
}

/// SQL `percentile_cont` of sorted `values`.
fn percentile_cont(q: f64, values: &[f64]) -> f64 {
    let rank = q * (values.len() - 1) as f64;
    let (low, high) = (rank.floor() as usize, rank.ceil() as usize);
    values[low] + (values[high] - values[low]) * (rank - low as f64)
}

/// Whether `answer` is the `q` percentile of sorted `values` exactly, or
/// within `algorithm`'s bound: a KLL's rank error, a DDSketch's relative
/// value error.
fn within_bound(algorithm: Option<&SketchAlgorithm>, q: f64, answer: f64, values: &[f64]) -> bool {
    let exact = percentile_cont(q, values);
    if (answer - exact).abs() <= 1e-9 * exact.abs() {
        return true;
    }
    match algorithm {
        Some(SketchAlgorithm::Kll) => {
            let n = values.len() as f64;
            let below = values.partition_point(|&v| v < answer) as f64 / n;
            let at_most = values.partition_point(|&v| v <= answer) as f64 / n;
            below - EPSILON - 1.0 / n <= q && q <= at_most + EPSILON + 1.0 / n
        }
        Some(SketchAlgorithm::DDSketch) => (answer - exact).abs() <= EPSILON * exact.abs(),
        _ => false,
    }
}

/// `root`'s answer rows at [`EVALUATION_TIME_MS`] over [`rows`], every node at
/// query time.
fn answers(root: &Rc<OperatorNode>) -> Vec<Vec<Value>> {
    let table = rows()
        .into_iter()
        .map(|(ts, city, value)| {
            vec![
                Value::Timestamp(ts),
                city.map_or(Value::Null, |city| Value::Utf8(city.into())),
                Value::Float64(value),
            ]
        })
        .collect();
    physical_common::execute_raw_rows_at(root, table, EVALUATION_TIME_MS)
}

fn as_f64(value: &Value) -> f64 {
    match value {
        Value::Float64(value) => *value,
        other => panic!("not a Float64: {other:?}"),
    }
}

/// The sketch algorithm `root` reads, if any.
fn sketch_of(root: &Rc<OperatorNode>) -> Option<SketchAlgorithm> {
    OperatorNode::reachable(root)
        .into_iter()
        .find_map(|node| match node.asap() {
            Some(
                ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }
                | ASAPOp::PaneChain {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                },
            ) => Some(kind.algorithm().clone()),
            _ => None,
        })
}

fn operator(root: &QueryRoot) -> &Rc<OperatorNode> {
    match root {
        QueryRoot::Operator(node) => node,
        QueryRoot::Scalar(_) => panic!("operator root expected"),
    }
}

/// Q1 with one percentile exact and the other from a sketch: every such
/// plan compiles and is priced (an exact aggregate split from a sketched
/// measure names its one output), and answers each percentile exactly or
/// within the sketch's bound.
#[tokio::test]
async fn q1_with_one_exact_and_one_sketched_percentile_compiles_and_matches() {
    let run = run(&[Q1]).await;
    let cannot_compile: Vec<_> = run
        .selection
        .rejected
        .iter()
        .filter(|r| r.cause == RejectionCause::ExecutorCannotCompile)
        .map(|r| format!("{}: {}", r.id, r.reason))
        .collect();
    assert!(cannot_compile.is_empty(), "{cannot_compile:#?}");
    let mixed: Vec<&Logical> = run
        .logical
        .iter()
        .filter(|c| {
            let reached = OperatorNode::reachable(operator(&c.roots[0]));
            reached
                .iter()
                .any(|n| matches!(n.non_asap(), Some(NonASAPOp::Aggregate { .. })))
                && reached
                    .iter()
                    .any(|n| matches!(n.asap(), Some(ASAPOp::SummaryEstimate { .. })))
        })
        .collect();
    assert!(!mixed.is_empty(), "no mixed exact and sketch plan for Q1");
    for candidate in &mixed {
        assert!(
            run.physical_of(candidate)
                .any(|p| run.cost(&p.id).is_some()),
            "{}: no physical candidate priced: {:#?}",
            candidate.id,
            run.selection
                .rejected
                .iter()
                .filter(|r| run.physical_of(candidate).any(|p| p.id == r.id))
                .map(|r| format!("{}: {:?} {}", r.id, r.cause, r.reason))
                .collect::<Vec<_>>()
        );
    }
    let values = values_in(5 * MINUTE_MS, None);
    for algorithm in [SketchAlgorithm::Kll, SketchAlgorithm::DDSketch] {
        let candidate = mixed
            .iter()
            .find(|c| sketch_of(operator(&c.roots[0])) == Some(algorithm.clone()))
            .unwrap_or_else(|| panic!("no mixed {algorithm:?} plan"));
        let root = operator(&candidate.roots[0]);
        // The percentile the exact aggregate computes.
        let exact_q = OperatorNode::reachable(root)
            .iter()
            .find_map(|n| match n.non_asap() {
                Some(NonASAPOp::Aggregate { measures, .. }) => match measures.as_slice() {
                    [AggIntent::Quantile { q, .. }] => Some(*q),
                    _ => None,
                },
                _ => None,
            })
            .expect("one exact percentile");
        let rows = answers(root);
        assert_eq!(rows.len(), 1, "{}", candidate.id);
        for (column, q) in [0.5, 0.9].into_iter().enumerate() {
            let answer = as_f64(&rows[0][column]);
            let bound = (q != exact_q).then_some(&algorithm);
            assert!(
                within_bound(bound, q, answer, &values),
                "{}: p{} = {answer}, exact {}",
                candidate.id,
                q * 100.0,
                percentile_cont(q, &values)
            );
        }
    }
}
