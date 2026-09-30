//! A retained PromQL subtree (`Fallback`) compiles from its typed expression.
//! The deployment supplies only its selector's raw series; expected values are
//! hand-computed with Prometheus semantics.
use asap_physical_operators::{
    operators::Operator,
    physical_planner::{compile, promql_fallback, promql_rows, CompiledPhysicalDag, InputContract},
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Value},
};
use futures::{executor::block_on, StreamExt};
use planner_types::{
    post_asap::{execution_data_state::lift_plain, *},
    pre_asap::QueryExpr,
    types::AccuracyTarget,
    workload::*,
};
use std::{collections::BTreeMap, rc::Rc};

/// Bare selectors look back one ingestion interval: 60s.
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
    let expression = asap_frontend_promql::lower_promql_workload(&workload, 0)
        .unwrap()
        .remove(0);
    promql_rows::with_series_identity(&expression).unwrap()
}

/// The whole query retained as one pre-ASAP node.
fn fallback_dag(expression: QueryExpr) -> PostAsapDag {
    let schema = lift_plain(&expression.output_schema().unwrap());
    compile_post_asap_dag(&Rc::new(SummaryNode {
        expr: SummaryExpr::KeepPreAsap(Rc::new(expression)),
        schema,
        guarantee: None,
    }))
    .unwrap()
}

/// `(job, seconds, value)`; every sample belongs to metric `m`.
type Sample = (&'static str, i64, f64);

fn compile_query(query: &str) -> Result<CompiledPhysicalDag, String> {
    let expression = lower(query);
    let dag = fallback_dag(expression.clone());
    let root = u64::from(dag.root.0);
    let inputs = promql_fallback::raw_series(&expression)
        .map_err(|e| e.to_string())?
        .map(|(_, schema)| {
            (
                promql_fallback::raw_series_input(root),
                InputContract::bounded(schema),
            )
        })
        .into_iter()
        .collect();
    let program = compile(&dag, inputs, &[root]).map_err(|e| e.to_string())?;
    Ok(serde_json::from_slice(&serde_json::to_vec(&program).unwrap()).unwrap())
}

/// Evaluate at `at` seconds; returns `(job or "", timestamp ms, value)` rows in order.
fn run(query: &str, samples: &[Sample], at: i64) -> Result<Vec<(String, i64, f64)>, String> {
    let expression = lower(query);
    let program = compile_query(query)?;
    let mut sources = BTreeMap::new();
    if let Some((_, schema)) = promql_fallback::raw_series(&expression).unwrap() {
        let rows = samples
            .iter()
            .map(|(job, seconds, value)| {
                let labels = BTreeMap::from([
                    ("__name__".to_string(), "m".to_string()),
                    ("job".into(), job.to_string()),
                ]);
                promql_rows::series_row(&schema, &labels, seconds * 1000, *value).unwrap()
            })
            .collect();
        let batch = Batch::try_new(schema.clone(), rows).unwrap();
        sources.insert(
            promql_fallback::raw_series_input(program.roots()[0]),
            Box::new(Operator::source(schema, vec![batch]).unwrap()) as _,
        );
    }
    let graph = program.instantiate(sources).map_err(|e| e.to_string())?;
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: at * 1000,
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
        let mut rows = Vec::new();
        while let Some(batch) = stream.next().await {
            let batch = batch.map_err(|e| e.to_string())?;
            let schema = batch.schema().clone();
            for row in batch.rows() {
                let mut job = String::new();
                let mut time = -1;
                let mut value = None;
                for (field, cell) in schema.fields.iter().zip(row) {
                    match (field.name.as_str(), cell) {
                        (promql_rows::SERIES_IDENTITY_COLUMN, Value::Utf8(id)) => {
                            job = promql_rows::decode_series_identity(id).unwrap()["job"].clone()
                        }
                        ("job", Value::Utf8(label)) => job = label.to_string(),
                        (_, Value::Timestamp(t)) => time = *t,
                        (_, Value::Float64(v)) => value = Some(*v),
                        (_, Value::Int64(v)) => value = Some(*v as f64),
                        other => return Err(format!("unexpected cell {other:?}")),
                    }
                }
                rows.push((job, time, value.ok_or("missing value")?));
            }
        }
        Ok(rows)
    })
}

fn values(query: &str, samples: &[Sample], at: i64) -> Vec<(String, f64)> {
    run(query, samples, at)
        .unwrap_or_else(|e| panic!("{query}: {e}"))
        .into_iter()
        .map(|(job, _, value)| (job, value))
        .collect()
}

fn one(query: &str, samples: &[Sample], at: i64) -> f64 {
    match values(query, samples, at).as_slice() {
        [(_, value)] => *value,
        other => panic!("{query}: expected one sample, got {other:?}"),
    }
}

const COUNTER: &[Sample] = &[
    ("a", 60, 10.),
    ("a", 120, 20.),
    ("a", 180, 5.),
    ("a", 240, 15.),
];

// rate/increase correct the reset at 180s and extrapolate half an interval at
// most; delta treats the same samples as a gauge.
#[test]
fn range_functions_follow_prometheus_extrapolation_and_resets() {
    // Reset-corrected increase is 25 over 180s of samples; 60s on each side extrapolates.
    let increase = 25. * (180. + 60. + 60.) / 180.;
    assert!((one("increase(m[5m])", COUNTER, 300) - increase).abs() < 1e-9);
    assert!((one("rate(m[5m])", COUNTER, 300) - increase / 300.).abs() < 1e-12);
    let delta = 5. * (180. + 60. + 60.) / 180.;
    assert!((one("delta(m[5m])", COUNTER, 300) - delta).abs() < 1e-9);
    // Fewer than two samples yield no rate.
    assert!(values("rate(m[2m])", COUNTER, 300).is_empty());
    for (query, expected) in [
        ("sum_over_time(m[5m])", 50.),
        ("avg_over_time(m[5m])", 12.5),
        ("min_over_time(m[5m])", 5.),
        ("max_over_time(m[5m])", 20.),
        ("count_over_time(m[5m])", 4.),
    ] {
        assert_eq!(one(query, COUNTER, 300), expected, "{query}");
    }
}

// Ranges are left-open: a sample at `t - range` is excluded, one at `t` is included.
#[test]
fn ranges_exclude_their_start_and_offsets_shift_them() {
    let samples = &[("a", 60, 1.), ("a", 90, 1.), ("a", 120, 1.), ("a", 150, 1.)];
    assert_eq!(one("count_over_time(m[1m])", samples, 120), 2.);
    // offset 1m reads (60s, 120s] at 180s; output keeps the evaluation time.
    let rows = run("count_over_time(m[1m] offset 1m)", samples, 180).unwrap();
    assert_eq!(rows, vec![("a".into(), 180_000, 2.)]);
}

// A bare selector takes the latest sample within the lookback; a stale marker
// hides the series rather than exposing an older value.
#[test]
fn instant_selection_uses_lookback_and_stale_markers() {
    let stale = f64::from_bits(0x7ff0_0000_0000_0002);
    let samples = &[("a", 0, 1.), ("a", 30, 2.), ("b", 30, 3.), ("b", 50, stale)];
    assert_eq!(values("m", samples, 60), vec![("a".into(), 2.)]);
    // The lookback (30s, 90s] excludes the sample at 30s.
    assert!(values("m", samples, 90).is_empty());
    // Range functions skip stale markers.
    assert_eq!(
        values("sum_over_time(m[1m])", samples, 60),
        vec![("a".into(), 2.), ("b".into(), 3.)]
    );
}

// NaN samples follow Prometheus: min/max skip them, sums propagate them.
#[test]
fn nan_samples() {
    let samples = &[("a", 10, f64::NAN), ("a", 20, 3.), ("a", 30, 1.)];
    assert_eq!(one("max_over_time(m[1m])", samples, 60), 3.);
    assert_eq!(one("min_over_time(m[1m])", samples, 60), 1.);
    assert!(one("sum_over_time(m[1m])", samples, 60).is_nan());
}

// Aggregation over no series is an empty vector, not one zero or null row;
// sort_desc orders the selected series.
#[test]
fn cross_series_aggregates_and_empty_inputs() {
    let samples = &[("a", 50, 1.), ("b", 40, 2.), ("b", 55, 4.)];
    assert_eq!(values("sum(m)", samples, 60), vec![(String::new(), 5.)]);
    assert_eq!(values("count(m)", samples, 60), vec![(String::new(), 2.)]);
    assert_eq!(
        values("max by (job) (m)", samples, 60),
        vec![("a".into(), 1.), ("b".into(), 4.)]
    );
    assert_eq!(
        values("sort_desc(m)", samples, 60),
        vec![("b".into(), 4.), ("a".into(), 1.)]
    );
    // topk by (job) keeps the top series of each job, not one overall.
    let jobs = &[("a", 50, 1.), ("b", 50, 2.)];
    let mut top = values("topk by (job) (1, m)", jobs, 60);
    top.sort_by(|x, y| x.0.cmp(&y.0));
    assert_eq!(top, vec![("a".into(), 1.), ("b".into(), 2.)]);
    assert_eq!(values("topk(1, m)", jobs, 60), vec![("b".into(), 2.)]);
    for query in ["sum(m)", "count(m)", "max(m)", "sum by (job) (rate(m[5m]))"] {
        assert!(values(query, &[], 60).is_empty(), "{query}");
    }
}

// scalar() is the single series' value and NaN otherwise; vector() needs no input.
#[test]
fn scalar_and_vector_bridges() {
    assert_eq!(one("scalar(m)", &[("a", 50, 7.)], 60), 7.);
    assert!(one("scalar(m)", &[("a", 50, 7.), ("b", 50, 8.)], 60).is_nan());
    assert!(one("scalar(m)", &[], 60).is_nan());
    assert_eq!(
        run("vector(3)", &[], 60).unwrap(),
        vec![(String::new(), 60_000, 3.)]
    );
    assert_eq!(
        values("2 - m", &[("a", 50, 7.)], 60),
        vec![("a".into(), -5.)]
    );
    assert_eq!(
        values("m * 2", &[("a", 50, 7.)], 60),
        vec![("a".into(), 14.)]
    );
}

// Subquery steps are absolute multiples of the resolution in the left-open
// range; each step evaluates the operand, and the outer function reduces them.
#[test]
fn subqueries_evaluate_their_operand_on_the_aligned_grid() {
    // Steps 60..300: selections 1, 7, 3, (none at 240s), 4.
    let samples = &[
        ("a", 50, 1.),
        ("a", 110, 7.),
        ("a", 170, 3.),
        ("a", 290, 4.),
    ];
    assert_eq!(one("max_over_time(m[5m:1m])", samples, 300), 7.);
    assert_eq!(one("count_over_time(m[5m:1m])", samples, 300), 4.);
    // At 190s the steps are 60, 120, 180 (not 70, 130, 190): counts 1 + 2 + 2.
    let samples = &[("a", 30, 1.), ("a", 90, 1.), ("a", 150, 1.), ("a", 185, 1.)];
    assert_eq!(
        one("sum_over_time(count_over_time(m[2m])[3m:1m])", samples, 190),
        5.
    );
    // offset 1m moves the grid to (-50s, 130s]: steps 0, 60, 120 count 0 + 1 + 2.
    assert_eq!(
        one(
            "sum_over_time(count_over_time(m[2m])[3m:1m] offset 1m)",
            samples,
            190
        ),
        3.
    );
    assert!(compile_query("max_over_time(m[5m:1m] @ 100)").is_err());
}

// Subquery work is bounded by the query: at most 100000 steps.
#[test]
fn dense_subquery_grids_are_rejected() {
    assert!(compile_query("max_over_time(m[100s:1ms])").is_ok());
    assert!(compile_query("max_over_time(m[30d:1ms])").is_err());
}

// The deployment must supply the selector's raw rows under the documented slot
// with the exact selector schema; unsupported shapes stay rejected.
#[test]
fn raw_series_contract_is_explicit() {
    let expression = lower("rate(m[5m])");
    let dag = fallback_dag(expression.clone());
    let root = u64::from(dag.root.0);
    let (selector, schema) = promql_fallback::raw_series(&expression).unwrap().unwrap();
    assert!(matches!(selector, QueryExpr::TimeRange { .. }));
    let missing = compile(&dag, BTreeMap::new(), &[root]).err().unwrap();
    assert!(missing.to_string().contains("raw series input"));
    let mut wrong = (*schema).clone();
    wrong.fields.pop();
    let wrong = compile(
        &dag,
        BTreeMap::from([(
            promql_fallback::raw_series_input(root),
            InputContract::bounded(std::sync::Arc::new(wrong)),
        )]),
        &[root],
    );
    assert!(wrong.is_err());
    // A consumed bare selector is raw range rows for its consumer; it is not
    // turned into instant selection.
    let selector = lower("m");
    let schema = lift_plain(&selector.output_schema().unwrap());
    let node = |id, payload| PostAsapDagNode {
        id: PostAsapNodeId(id),
        payload,
        output_state: ExecutionDataState::QUERY_ROWS,
        output_schema: schema.clone(),
        guarantee: None,
    };
    let consumed = PostAsapDag {
        nodes: vec![
            node(
                0,
                PostAsapOperatorPayload::Fallback {
                    expression: selector.clone(),
                },
            ),
            node(
                1,
                PostAsapOperatorPayload::Value {
                    operation: ValueOperation::Limit {
                        n: 1,
                        offset: 0,
                        partition_by: Default::default(),
                    },
                },
            ),
        ],
        edges: vec![PostAsapDagEdge {
            producer: PostAsapNodeId(0),
            consumer: PostAsapNodeId(1),
            role: EdgeRole::Input,
            intermediate_schema: schema.clone(),
            data_state: ExecutionDataState::QUERY_ROWS,
            grouping: GroupingEdgeCompatibility::NotApplicable,
            window: WindowEdgeCompatibility::NotApplicable,
        }],
        root: PostAsapNodeId(1),
    };
    let raw = promql_fallback::raw_series(&selector).unwrap().unwrap().1;
    assert!(compile(
        &consumed,
        BTreeMap::from([(
            promql_fallback::raw_series_input(0),
            InputContract::bounded(raw)
        )]),
        &[1],
    )
    .is_err());
    // Implicit subquery resolution belongs to the deployment's evaluation interval.
    assert!(compile_query("max_over_time(m[5m:])").is_err());
}
