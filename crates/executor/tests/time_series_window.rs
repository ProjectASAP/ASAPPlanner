//! A `TimeRange` over a time-series `Scan` is a native operator: raw
//! samples are bound at the `Scan` by its `Source`, and the DAG applies the
//! window `(T - s - w, T - s]`, the `Scan`'s label matchers, an instant
//! selector's latest sample per series and Prometheus stale markers.
use asap_executor::physical_planner::{compile, promql_rows};
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::sources::{BindingDataSources, MemorySource};
use asap_executor::values::{Batch, Value};
use futures::{executor::block_on, StreamExt};
use planner_types::ir::operator::{Source, TimeShift};
use planner_types::ir::physical_export::{compile_physical_asap_dag, PhysicalASAPDAG};
use planner_types::ir::scalar::{CompareOpKind, ScalarValue};
use planner_types::ir::schema::{DataType, Field, Schema, PROMQL_SERIES_IDENTITY};
use planner_types::ir::{
    apply_materialization_timings, ExprSemantics, MaterializationAssignment, NonASAPOp, Operator,
    OperatorNode, Predicate, ScalarExpr, TimeRangeKind, TimingMemo,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

/// The Prometheus stale marker: a NaN with this bit pattern ends a series.
const STALE_MARKER: u64 = 0x7ff0_0000_0000_0002;

/// `latency_ms` rows: series identity, time, value and the `instance` label.
fn series_schema() -> Schema {
    series_schema_with_value(DataType::Float64)
}

/// [`series_schema`] with a `value` of type `value`.
fn series_schema_with_value(value: DataType) -> Schema {
    Schema::with_time_index(
        vec![
            Field::plain(PROMQL_SERIES_IDENTITY, DataType::Utf8, false),
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("value", value, false),
            Field::plain("instance", DataType::Utf8, false),
        ],
        1,
        vec![],
    )
}

/// `TimeRange(width_ms, kind)` over `offset_ms` (if not 0) over a `Scan` of
/// `latency_ms` keeping `instance="a"`, at query time, exported.
fn window_over(width_ms: u64, kind: TimeRangeKind, offset_ms: i64) -> PhysicalASAPDAG {
    window_over_rows(series_schema(), width_ms, kind, offset_ms)
}

/// [`window_over`] for a `Scan` of rows in `schema`.
fn window_over_rows(
    schema: Schema,
    width_ms: u64,
    kind: TimeRangeKind,
    offset_ms: i64,
) -> PhysicalASAPDAG {
    let instance_is_a = Predicate(ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(3)),
        op: CompareOpKind::Eq,
        right: Box::new(ScalarExpr::Literal(ScalarValue::Utf8("a".into()))),
        semantics: ExprSemantics::Promql,
    });
    let mut child = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "latency_ms".into(),
        },
        predicates: vec![instance_is_a],
        schema,
    }))
    .unwrap();
    if offset_ms != 0 {
        child = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
            shift: TimeShift {
                offset_ms,
                at: None,
            },
            child,
        }))
        .unwrap();
    }
    let range = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
        range: Duration::from_millis(width_ms),
        kind,
        child,
    }))
    .unwrap();
    let timed = apply_materialization_timings(
        &range,
        &MaterializationAssignment::default(),
        &mut TimingMemo::new(),
    )
    .unwrap();
    compile_physical_asap_dag(&timed).unwrap()
}

/// `(instance, ts, value)` samples of `latency_ms` as a provider's rows.
fn provider(samples: &[(&str, i64, f64)]) -> BindingDataSources {
    provider_in_batches(&[samples])
}

/// [`provider`] returning one batch per element of `batches`, in order.
fn provider_in_batches(batches: &[&[(&str, i64, f64)]]) -> BindingDataSources {
    let schema = Arc::new(series_schema());
    let batches = batches
        .iter()
        .map(|samples| {
            let rows = samples
                .iter()
                .map(|&(instance, ts, value)| {
                    let labels = BTreeMap::from([
                        ("__name__".to_string(), "latency_ms".to_string()),
                        ("instance".to_string(), instance.to_string()),
                    ]);
                    promql_rows::series_row(&schema, &labels, ts, value).unwrap()
                })
                .collect();
            Batch::try_new(schema.clone(), rows).unwrap()
        })
        .collect();
    let mut data_sources = BindingDataSources::default();
    data_sources
        .tables
        .register(
            Source::TimeSeries {
                metric: "latency_ms".into(),
            },
            Arc::new(MemorySource::new(schema, batches).unwrap()),
        )
        .unwrap();
    data_sources
}

/// The `(ts, value)` rows `dag`'s root outputs in a run of `scope` over
/// `samples`, bound at the `Scan`.
fn points(dag: &PhysicalASAPDAG, samples: &[(&str, i64, f64)], scope: Scope) -> Vec<(i64, f64)> {
    points_of(dag, provider(samples), scope)
}

/// The `(ts, value)` rows `dag`'s root outputs in a run of `scope` with
/// `data_sources` bound.
fn points_of(
    dag: &PhysicalASAPDAG,
    data_sources: BindingDataSources,
    scope: Scope,
) -> Vec<(i64, f64)> {
    let roots = [dag.roots[0] as u64];
    let bound = compile(dag, &BTreeSet::new(), &roots)
        .unwrap()
        .instantiate(&data_sources)
        .unwrap();
    let context = RunContext::new(scope, Limits::default()).unwrap();
    block_on(async {
        let mut output = bound.execute(&roots, context).unwrap().remove(0);
        let mut points = Vec::new();
        while let Some(batch) = output.next().await {
            for row in batch.unwrap().to_rows() {
                match (&row[1], &row[2]) {
                    (Value::Timestamp(ts), Value::Float64(v)) => points.push((*ts, *v)),
                    other => panic!("{other:?}"),
                }
            }
        }
        points
    })
}

fn at(evaluation_time_ms: i64) -> Scope {
    Scope::Query {
        evaluation_time_ms,
    }
}

// A range window `(T - w, T]` excludes a sample at its start and includes one
// at its end; the Scan's matcher drops series `b`.
#[test]
fn a_range_window_keeps_its_bounds_and_matchers() {
    let dag = window_over(1_000, TimeRangeKind::Range, 0);
    let samples = [
        ("a", 9_000, 1.0),
        ("a", 9_001, 2.0),
        ("a", 10_000, 3.0),
        ("a", 10_001, 4.0),
        ("b", 9_500, 5.0),
    ];
    assert_eq!(
        points(&dag, &samples, at(10_000)),
        [(9_001, 2.0), (10_000, 3.0)]
    );
}

// A range window under `offset 5s` keeps `(T - 5s - w, T - 5s]`.
#[test]
fn a_shifted_range_window_ends_its_offset_before_the_evaluation() {
    let dag = window_over(1_000, TimeRangeKind::Range, 5_000);
    let samples = [("a", 4_000, 1.0), ("a", 4_500, 2.0), ("a", 9_500, 3.0)];
    assert_eq!(points(&dag, &samples, at(10_000)), [(4_500, 2.0)]);
}

// An instant window keeps each series' latest sample in its lookback, and of
// two samples at that time the later one.
#[test]
fn an_instant_window_keeps_each_series_latest_sample() {
    let dag = window_over(1_000, TimeRangeKind::Instant, 0);
    let samples = [
        ("a", 9_100, 1.0),
        ("a", 9_900, 10.0),
        ("a", 9_500, 5.0),
        ("a", 9_900, 11.0),
        ("a", 10_500, 99.0),
    ];
    assert_eq!(points(&dag, &samples, at(10_000)), [(9_900, 11.0)]);
}

// An instant window over several input batches keeps each series' latest
// sample across them: of two at one time in different batches, the later
// batch's; and a stale latest sample in a later batch ends the series.
#[test]
fn an_instant_window_over_several_batches_keeps_the_latest_across_them() {
    let stale = f64::from_bits(STALE_MARKER);
    let dag = window_over(1_000, TimeRangeKind::Instant, 0);
    let tied: [&[(&str, i64, f64)]; 2] = [
        &[("a", 9_900, 10.0)],
        &[("a", 9_100, 1.0), ("a", 9_900, 11.0)],
    ];
    assert_eq!(
        points_of(&dag, provider_in_batches(&tied), at(10_000)),
        [(9_900, 11.0)]
    );
    let ended: [&[(&str, i64, f64)]; 2] = [&[("a", 9_500, 5.0)], &[("a", 9_900, stale)]];
    assert_eq!(points_of(&dag, provider_in_batches(&ended), at(10_000)), []);
}

// A range window drops a stale marker and keeps the samples around it, an
// ordinary NaN included.
#[test]
fn a_range_window_keeps_an_ordinary_nan_and_drops_stale_markers() {
    let stale = f64::from_bits(STALE_MARKER);
    let dag = window_over(1_000, TimeRangeKind::Range, 0);
    let samples = [
        ("a", 9_200, 3.0),
        ("a", 9_400, stale),
        ("a", 9_600, f64::NAN),
        ("a", 9_800, 5.0),
    ];
    let rows = points(&dag, &samples, at(10_000));
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!((rows[0], rows[2]), ((9_200, 3.0), (9_800, 5.0)));
    assert!(rows[1].1.is_nan() && rows[1].1.to_bits() != STALE_MARKER);
}

// An instant window whose series' latest sample is a stale marker has no row
// for it (the series ended), not the sample before it.
#[test]
fn an_instant_window_whose_latest_sample_is_stale_has_no_row() {
    let stale = f64::from_bits(STALE_MARKER);
    let dag = window_over(1_000, TimeRangeKind::Instant, 0);
    let ended = [("a", 9_500, 5.0), ("a", 9_900, stale)];
    assert_eq!(points(&dag, &ended, at(10_000)), []);
}

// In an ingestion run a window ends at the run's `window_end_ms`, whatever
// the shifts below it: the run builds the pane the shifts select.
#[test]
fn an_ingestion_run_builds_the_window_ending_at_its_end() {
    let dag = window_over(1_000, TimeRangeKind::Range, 5_000);
    let samples = [("a", 4_000, 1.0), ("a", 4_500, 2.0), ("a", 5_000, 3.0)];
    let ingestion = Scope::Ingestion {
        window_start_ms: 4_000,
        window_end_ms: 5_000,
    };
    assert_eq!(
        points(&dag, &samples, ingestion),
        [(4_500, 2.0), (5_000, 3.0)]
    );
}

// A time-series window whose sample value is not Float64 has no native
// operator: compiling it fails, so a compile check rejects the plan.
#[test]
fn a_time_series_window_without_a_float_sample_value_does_not_compile() {
    let dag = window_over_rows(
        series_schema_with_value(DataType::Int64),
        1_000,
        TimeRangeKind::Instant,
        0,
    );
    let error = compile(&dag, &BTreeSet::new(), &[dag.roots[0] as u64])
        .err()
        .expect("an Int64 sample value has no native time-series window");
    assert!(error.to_string().contains("Float64"), "{error}");
}
