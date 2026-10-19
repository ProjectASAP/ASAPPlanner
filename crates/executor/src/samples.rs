//! Raw samples in, labeled answers out: how a deployment runtime feeds a
//! plan's inputs and reads its PromQL outputs, without the executor's row
//! encoding.
//!
//! A [`RawSampleSelector`] is prepared once per raw-sample window of the
//! installable DAG. It says which metric's samples the window reads
//! ([`RawSampleSelector::metric`]), and turns the samples a runtime
//! received into the window's input rows ([`RawSampleSelector::window_batch`]):
//! those in the window, kept by the window's `Scan` predicates (its label
//! matchers) and, for an instant selector, only each series' latest one.
//! [`series_labels_of_output_row`] reads the labels of an answer row back.

use std::collections::BTreeMap;
use std::sync::Arc;

use planner_types::ir::operator::Source as ScanSource;
use planner_types::ir::physical_export::{PhysicalASAPDAG, RawSampleWindow};
use planner_types::ir::schema::Schema;
use planner_types::ir::{NonASAPOp, Operator, OperatorNode, TimeRangeKind};
use std::rc::Rc;

use crate::expressions::CompiledExpression;
use crate::physical_planner::promql_rows::{
    decode_series_identity, series_batch, SERIES_IDENTITY_COLUMN,
};
use crate::values::{Batch, SchemaRef, Value};
use crate::Error;

/// One raw sample, e.g. one Prometheus Remote Write sample.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    /// Every label of the series, `__name__` (the metric) included.
    pub labels: BTreeMap<String, String>,
    /// When the sample was taken, in Unix milliseconds.
    pub timestamp_ms: i64,
    /// The sampled value.
    pub value: f64,
}

/// One raw-sample window of an installable DAG, prepared to turn samples
/// into its input rows: built once per window ([`RawSampleSelector::new`]),
/// then called for each window end ([`RawSampleSelector::window_batch`]).
pub struct RawSampleSelector {
    /// The metric the window's `Scan` reads (`__name__`).
    metric: String,
    /// The `Scan`'s output schema, which the rows are encoded in.
    scan_schema: SchemaRef,
    /// The `TimeRange`'s output schema, which the rows are returned in.
    time_range_schema: SchemaRef,
    /// The `Scan`'s predicates (label matchers), compiled over its schema.
    predicates: Vec<CompiledExpression>,
    /// Width of the window: a range, or an instant selector's lookback.
    width_ms: i64,
    /// Whether the window is an instant selector, which keeps only each
    /// series' latest sample in its lookback.
    latest_sample_per_series: bool,
}

impl RawSampleSelector {
    /// The selector of `window` of `dag`. Bad input when the window's nodes
    /// are not a time-series `Scan` and a `TimeRange` of `dag` with the same
    /// columns, or a predicate does not compile.
    pub fn new(dag: &PhysicalASAPDAG, window: &RawSampleWindow) -> Result<Self, Error> {
        let node = |id| {
            dag.nodes
                .iter()
                .find(|n| n.id == id)
                .ok_or_else(|| Error::Invalid(format!("window names unknown node {id}")))
        };
        let (scan, time_range) = (node(window.scan_node_id)?, node(window.time_range_node_id)?);
        let Operator::NonASAP(NonASAPOp::Scan {
            source: ScanSource::TimeSeries { metric },
            ..
        }) = &scan.payload
        else {
            return Err(Error::Invalid(
                "window's scan is not a time-series Scan".into(),
            ));
        };
        let Operator::NonASAP(NonASAPOp::TimeRange { kind, .. }) = &time_range.payload else {
            return Err(Error::Invalid(
                "window's time range is not a TimeRange".into(),
            ));
        };
        if time_range.output_schema.fields != scan.output_schema.fields {
            return Err(Error::Invalid(
                "window's TimeRange changes its Scan's columns".into(),
            ));
        }
        let scan_schema = Arc::new(scan.output_schema.clone());
        // The Scan's predicates over its own columns. A Scan has no inputs,
        // so its predicates name no operator and compile from the payload.
        let Operator::NonASAP(NonASAPOp::Scan { predicates, .. }) = &scan.payload else {
            unreachable!("matched above");
        };
        let predicates = predicates
            .iter()
            .map(|predicate| {
                if !predicate.0.operator_refs().is_empty() {
                    return Err(Error::Invalid(
                        "a Scan predicate reads another operator".into(),
                    ));
                }
                let expression = predicate.0.map_operator_refs(&mut |_| -> Rc<OperatorNode> {
                    unreachable!("checked: no operator references")
                });
                CompiledExpression::compile(&expression, &scan_schema)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            metric: metric.clone(),
            scan_schema,
            time_range_schema: Arc::new(time_range.output_schema.clone()),
            predicates,
            width_ms: window.width_ms,
            latest_sample_per_series: *kind == TimeRangeKind::Instant,
        })
    }

    /// The metric whose samples the window reads; a runtime routes only
    /// those samples to [`Self::window_batch`].
    pub fn metric(&self) -> &str {
        &self.metric
    }

    /// The window's input batch (Arrow columns) for the window ending at `window_end_ms`:
    /// the samples of [`Self::metric`] in `(window_end_ms - width,
    /// window_end_ms]` that the `Scan`'s predicates keep, and, for an
    /// instant selector, only each series' latest one; in the `TimeRange`'s
    /// schema, bound to the window's `TimeRange`. `samples` may hold other
    /// series and times. Bad input when a sample's labels cannot fill the
    /// schema.
    pub fn window_batch(&self, window_end_ms: i64, samples: &[Sample]) -> Result<Batch, Error> {
        let start_ms = window_end_ms - self.width_ms;
        let mut kept: Vec<&Sample> = samples
            .iter()
            .filter(|s| start_ms < s.timestamp_ms && s.timestamp_ms <= window_end_ms)
            .filter(|s| s.labels.get("__name__").map(String::as_str) == Some(self.metric()))
            .collect();
        if self.latest_sample_per_series {
            let mut latest: BTreeMap<&BTreeMap<String, String>, &Sample> = BTreeMap::new();
            for sample in kept {
                let entry = latest.entry(&sample.labels).or_insert(sample);
                if sample.timestamp_ms >= entry.timestamp_ms {
                    *entry = sample;
                }
            }
            kept = latest.into_values().collect();
        }
        // Columns first, with each series' labels and identity built once;
        // then each predicate (label matcher) once per distinct label value.
        let mut batch = series_batch(
            &self.scan_schema,
            Arc::clone(&self.time_range_schema),
            &kept,
        )?;
        for predicate in &self.predicates {
            if batch.num_rows() == 0 {
                break;
            }
            let keep = predicate.evaluate_batch(&batch)?;
            let keep = keep
                .as_any()
                .downcast_ref::<arrow::array::BooleanArray>()
                .ok_or_else(|| Error::Invalid("Scan predicate is not boolean".into()))?;
            batch = batch.filter(keep)?;
        }
        Ok(batch)
    }
}

/// The series identity of `labels` (every label of a series, `__name__`
/// included): the text a plan's series identity column holds, for a
/// runtime that builds a pane's columns itself
/// ([`crate::values::Batch::try_from_columns`]). The inverse of
/// [`series_labels_of_output_row`] on that column.
pub fn series_identity(labels: &BTreeMap<String, String>) -> Result<String, Error> {
    crate::physical_planner::promql_rows::encode_series_identity(labels)
}

/// The labels of the series a PromQL answer row belongs to: its series
/// identity, when the schema keeps one, plus every other non-empty text
/// column (a label the query grouped by). Bad input when the identity is
/// not canonically encoded.
pub fn series_labels_of_output_row(
    schema: &Schema,
    row: &[Value],
) -> Result<BTreeMap<String, String>, Error> {
    let mut labels = BTreeMap::new();
    for (field, cell) in schema.fields.iter().zip(row) {
        let Value::Utf8(text) = cell else { continue };
        if field.name == SERIES_IDENTITY_COLUMN {
            labels.extend(decode_series_identity(text)?);
        } else if !text.is_empty() {
            labels.insert(field.name.clone(), text.to_string());
        }
    }
    Ok(labels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use planner_types::ir::operator::Source;
    use planner_types::ir::physical_export::compile_physical_asap_dag;
    use planner_types::ir::scalar::{CompareOpKind, ScalarValue};
    use planner_types::ir::schema::{DataType, Field, PROMQL_SERIES_IDENTITY};
    use planner_types::ir::{
        apply_materialization_timings, ExprSemantics, MaterializationAssignment, OperatorNode,
        Predicate, ScalarExpr, TimingMemo,
    };
    use std::rc::Rc;
    use std::time::Duration;

    /// `TimeRange(width_ms, kind)` over a `Scan` of `source` keeping
    /// `instance="a"`, timed and exported, with its raw-sample window.
    fn window_over(
        source: Source,
        width_ms: u64,
        kind: TimeRangeKind,
    ) -> (PhysicalASAPDAG, RawSampleWindow) {
        let schema = Schema::with_time_index(
            vec![
                Field::plain(PROMQL_SERIES_IDENTITY, DataType::Utf8, false),
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
                Field::plain("instance", DataType::Utf8, false),
            ],
            1,
            vec![],
        );
        let instance_is_a = Predicate(ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Column(3)),
            op: CompareOpKind::Eq,
            right: Box::new(ScalarExpr::Literal(ScalarValue::Utf8("a".into()))),
            semantics: ExprSemantics::Promql,
        });
        let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
            source,
            predicates: vec![instance_is_a],
            schema,
        }))
        .unwrap();
        let range = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_millis(width_ms),
            kind,
            child: scan,
        }))
        .unwrap();
        let timed: Rc<OperatorNode> = apply_materialization_timings(
            &range,
            &MaterializationAssignment::default(),
            &mut TimingMemo::new(),
        )
        .unwrap();
        let dag = compile_physical_asap_dag(&timed).unwrap();
        let window = RawSampleWindow {
            time_range_node_id: dag.roots[0],
            width_ms: width_ms as i64,
            end_offset_ms: 0,
            scan_node_id: dag.roots[0] - 1,
        };
        (dag, window)
    }

    fn sample(metric: &str, instance: &str, timestamp_ms: i64, value: f64) -> Sample {
        Sample {
            labels: BTreeMap::from([
                ("__name__".to_string(), metric.to_string()),
                ("instance".to_string(), instance.to_string()),
            ]),
            timestamp_ms,
            value,
        }
    }

    /// Each row's timestamp and value.
    fn points(batch: &Batch) -> Vec<(i64, f64)> {
        batch
            .to_rows()
            .iter()
            .map(|row| match (&row[1], &row[2]) {
                (Value::Timestamp(ts), Value::Float64(v)) => (*ts, *v),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    fn latency_ms() -> Source {
        Source::TimeSeries {
            metric: "latency_ms".into(),
        }
    }

    /// A range window `(end - width, end]` excludes a sample at its start
    /// and includes one at its end; the Scan's matcher drops series `b`
    /// and the metric routing drops another metric.
    #[test]
    fn a_range_window_keeps_its_bounds_matchers_and_metric() {
        let (dag, window) = window_over(latency_ms(), 1_000, TimeRangeKind::Range);
        let selector = RawSampleSelector::new(&dag, &window).unwrap();
        assert_eq!(selector.metric(), "latency_ms");
        let samples = [
            sample("latency_ms", "a", 9_000, 1.0),
            sample("latency_ms", "a", 9_001, 2.0),
            sample("latency_ms", "a", 10_000, 3.0),
            sample("latency_ms", "a", 10_001, 4.0),
            sample("latency_ms", "b", 9_500, 5.0),
            sample("cpu_seconds", "a", 9_500, 6.0),
        ];
        let rows = selector.window_batch(10_000, &samples).unwrap();
        assert_eq!(points(&rows), [(9_001, 2.0), (10_000, 3.0)]);
    }

    /// An instant selector keeps each series' latest sample in its lookback.
    #[test]
    fn an_instant_window_keeps_each_series_latest_sample() {
        let (dag, window) = window_over(latency_ms(), 1_000, TimeRangeKind::Instant);
        let selector = RawSampleSelector::new(&dag, &window).unwrap();
        let samples = [
            sample("latency_ms", "a", 9_100, 1.0),
            sample("latency_ms", "a", 9_900, 10.0),
            sample("latency_ms", "a", 9_500, 5.0),
        ];
        let rows = selector.window_batch(10_000, &samples).unwrap();
        assert_eq!(points(&rows), [(9_900, 10.0)]);
    }

    /// A window over a table `Scan` has no metric to route samples by.
    #[test]
    fn a_window_over_a_table_scan_is_refused() {
        let table = Source::Table {
            table_ref: "metrics".into(),
        };
        let (dag, window) = window_over(table, 1_000, TimeRangeKind::Range);
        assert!(matches!(
            RawSampleSelector::new(&dag, &window),
            Err(Error::Invalid(_))
        ));
    }

    /// The selector's batch has the rows the row encoding (`series_row`)
    /// gives, with each label column stored once per distinct series, not
    /// once per sample (the matcher's filter keeps the dictionary).
    #[test]
    fn selected_columns_equal_the_row_encoding_with_one_label_per_series() {
        use crate::physical_planner::promql_rows::series_row;
        use arrow::array::AsArray;
        let (dag, window) = window_over(latency_ms(), 10_000, TimeRangeKind::Range);
        let selector = RawSampleSelector::new(&dag, &window).unwrap();
        let samples = (0..30)
            .map(|i| {
                sample(
                    "latency_ms",
                    ["a", "b", "a"][i % 3],
                    1_000 + i as i64,
                    i as f64,
                )
            })
            .collect::<Vec<_>>();
        let batch = selector.window_batch(10_000, &samples).unwrap();
        let expected = samples
            .iter()
            .filter(|s| s.labels["instance"] == "a")
            .map(|s| series_row(&selector.scan_schema, &s.labels, s.timestamp_ms, s.value).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(format!("{:?}", batch.to_rows()), format!("{expected:?}"));
        for column in [0, 3] {
            let dictionary = batch
                .column(column)
                .as_dictionary::<arrow::datatypes::UInt32Type>();
            assert_eq!(dictionary.values().len(), 2, "column {column}");
        }
    }
}
