//! Samples in, answers out: the raw table of each time series the plan's
//! `Scan`s read, built column by column from the harness's samples, and the
//! series and value of each answer row.
//!
//! A time series is the table `(ts, value, $promql_series_identity,
//! *labels)` over its samples (the columns its `Scan`s read): the series
//! identity is the canonical JSON of every label, `__name__` included,
//! e.g. `{"__name__":"m","s":"s_0"}`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use aqpbm_harness::data::{Sample, SeriesLabels};
use asap_executor::arrow::array::{
    ArrayRef, DictionaryArray, Float64Array, StringArray, TimestampMillisecondArray, UInt32Array,
};
use asap_executor::arrow::datatypes::UInt32Type;
use asap_executor::values::{Batch, BatchColumn, SchemaRef, Value};
use asap_planner::PhysicalASAPDAG;
use asap_types::ir::operator::Source;
use asap_types::ir::schema::{DataType, FieldDataType, Schema, PROMQL_SERIES_IDENTITY};
use asap_types::ir::NonASAPOp;
use asap_types::plan::physical_asap_dag::PhysicalASAPOperatorPayload;

/// One time series' raw table: what each of its columns holds.
pub(super) struct TimeSeriesTable {
    /// `Source::TimeSeries` of `metric`.
    pub(super) source: Source,
    pub(super) metric: String,
    /// The union of its `Scan`s' columns, by name.
    pub(super) schema: SchemaRef,
    columns: Vec<TimeSeriesColumn>,
}

/// What one column of a time series' raw table holds.
enum TimeSeriesColumn {
    /// The sample's time, ms.
    Time,
    /// The sample's value.
    Value,
    /// The canonical JSON of the series' labels.
    SeriesIdentity,
    /// The value of this label; empty when the series lacks it.
    Label(String),
}

/// The raw table of each time series `dag`'s `Scan`s read, in metric order.
/// `Err` for a source the lane cannot feed: a SQL table, or a column that
/// is neither the time, the value, the series identity nor a text label.
pub(super) fn time_series_tables(dag: &PhysicalASAPDAG) -> Result<Vec<TimeSeriesTable>, String> {
    let mut schemas: BTreeMap<&str, Schema> = BTreeMap::new();
    for node in &dag.nodes {
        let PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { source, schema, .. }) =
            &node.payload
        else {
            continue;
        };
        let Source::TimeSeries { metric } = source else {
            return Err(format!(
                "node {} scans {source:?}, not a time series",
                node.id
            ));
        };
        let known = schemas.entry(metric).or_insert_with(|| schema.clone());
        for field in &schema.fields {
            if !known.fields.iter().any(|known| known.name == field.name) {
                known.fields.push(field.clone());
            }
        }
    }
    schemas
        .into_iter()
        .map(|(metric, schema)| {
            let columns = schema
                .fields
                .iter()
                .enumerate()
                .map(|(index, field)| match (&field.dtype, field.name.as_str()) {
                    _ if Some(index) == schema.time_index => Ok(TimeSeriesColumn::Time),
                    (FieldDataType::Plain(DataType::Float64), "value") => {
                        Ok(TimeSeriesColumn::Value)
                    }
                    (FieldDataType::Plain(DataType::Utf8), PROMQL_SERIES_IDENTITY) => {
                        Ok(TimeSeriesColumn::SeriesIdentity)
                    }
                    (FieldDataType::Plain(DataType::Utf8), label) => {
                        Ok(TimeSeriesColumn::Label(label.to_string()))
                    }
                    (dtype, name) => Err(format!(
                        "time series {metric}: column {name} of type {dtype:?} is not a label"
                    )),
                })
                .collect::<Result<_, _>>()?;
            Ok(TimeSeriesTable {
                source: Source::TimeSeries {
                    metric: metric.to_string(),
                },
                metric: metric.to_string(),
                schema: Arc::new(schema),
                columns,
            })
        })
        .collect()
}

/// Each series' identity text, computed once per series: the harness shares
/// one label map among a series' samples, so the map's address names the
/// series. The map is kept alive here, so its address is never reused by
/// another series.
#[derive(Default)]
pub(super) struct SeriesIdentities {
    by_address: HashMap<*const SeriesLabels, (Arc<SeriesLabels>, Arc<str>)>,
}

impl SeriesIdentities {
    fn identity(&mut self, labels: &Arc<SeriesLabels>) -> Arc<str> {
        let (_, identity) = self
            .by_address
            .entry(Arc::as_ptr(labels))
            .or_insert_with(|| {
                let json = serde_json::to_string(labels.as_ref()).expect("labels are JSON");
                (Arc::clone(labels), json.into())
            });
        Arc::clone(identity)
    }
}

impl TimeSeriesTable {
    /// The rows of `samples` (all of this table's metric) in the table's
    /// schema. Every text column is dictionary-encoded over the chunk's
    /// series: one key per row, one text per series.
    pub(super) fn batch(
        &self,
        samples: &[&Sample],
        identities: &mut SeriesIdentities,
    ) -> Result<Batch, asap_executor::Error> {
        // Each row's series as an index into this chunk's series.
        let mut series: Vec<&Arc<SeriesLabels>> = Vec::new();
        let mut index_of: HashMap<*const SeriesLabels, u32> = HashMap::new();
        let keys: UInt32Array = samples
            .iter()
            .map(|sample| {
                *index_of
                    .entry(Arc::as_ptr(&sample.labels))
                    .or_insert_with(|| {
                        series.push(&sample.labels);
                        (series.len() - 1) as u32
                    })
            })
            .collect();
        let text_column = |texts: StringArray| -> Result<BatchColumn, asap_executor::Error> {
            let column = DictionaryArray::<UInt32Type>::try_new(keys.clone(), Arc::new(texts))
                .map_err(|error| asap_executor::Error::Invalid(error.to_string()))?;
            Ok(BatchColumn::Plain(Arc::new(column) as ArrayRef))
        };
        let columns = self
            .columns
            .iter()
            .map(|column| match column {
                TimeSeriesColumn::Time => Ok(BatchColumn::Plain(Arc::new(
                    TimestampMillisecondArray::from_iter_values(
                        samples.iter().map(|sample| sample.ts_ms),
                    ),
                ))),
                TimeSeriesColumn::Value => Ok(BatchColumn::Plain(Arc::new(
                    Float64Array::from_iter_values(samples.iter().map(|sample| sample.value)),
                ))),
                TimeSeriesColumn::SeriesIdentity => {
                    let texts: Vec<Arc<str>> = series
                        .iter()
                        .map(|labels| identities.identity(labels))
                        .collect();
                    text_column(texts.iter().map(|text| Some(text.as_ref())).collect())
                }
                TimeSeriesColumn::Label(label) => text_column(
                    series
                        .iter()
                        .map(|labels| Some(labels.get(label).map_or("", String::as_str)))
                        .collect(),
                ),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Batch::try_from_columns(self.schema.clone(), columns)
    }
}

/// Each answer row of `batches` (one query root's output) as its series
/// labels and value: the series identity's labels, when the output keeps
/// one, and every other non-empty text column (a label the query grouped
/// by); the value is the row's first Float64 column. Example: a row
/// `(ts, 12.5, {"s":"s_0"})` of `sum_over_time(m[5m])` is `{s="s_0"} 12.5`.
pub(super) fn answers_of(batches: &[Batch]) -> Result<BTreeMap<SeriesLabels, f64>, String> {
    let mut answers = BTreeMap::new();
    for batch in batches {
        let schema = batch.schema();
        for row in batch.to_rows() {
            let mut labels = SeriesLabels::new();
            let mut value = None;
            for (field, cell) in schema.fields.iter().zip(&row) {
                match cell {
                    Value::Utf8(text) if field.name == PROMQL_SERIES_IDENTITY => {
                        let identity: SeriesLabels = serde_json::from_str(text)
                            .map_err(|error| format!("series identity {text}: {error}"))?;
                        labels.extend(identity);
                    }
                    Value::Utf8(text) if !text.is_empty() => {
                        labels.insert(field.name.clone(), text.to_string());
                    }
                    Value::Float64(number) if value.is_none() => value = Some(*number),
                    _ => {}
                }
            }
            let value = value.ok_or_else(|| format!("an answer row without a value: {row:?}"))?;
            answers.insert(labels, value);
        }
    }
    Ok(answers)
}
