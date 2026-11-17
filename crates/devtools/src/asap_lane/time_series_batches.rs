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

use aqpbm_core::harness::data::{Sample, SampleChunk, SeriesLabels};
use asap_executor::arrow::array::{
    ArrayRef, DictionaryArray, Float64Array, StringArray, TimestampMillisecondArray, UInt32Array,
};
use asap_executor::arrow::datatypes::UInt32Type;
use asap_executor::values::{Batch, BatchColumn, SchemaRef, Value};
use asap_planner::PhysicalASAPDAG;
use asap_types::ir::operator::Source;
use asap_types::ir::schema::{DataType, FieldDataType, PROMQL_SERIES_IDENTITY};

/// One time series' raw table: what each of its columns holds, and the
/// series seen so far, each with an ordinal that is its dictionary key in
/// every text column.
pub(super) struct TimeSeriesTable {
    /// `Source::TimeSeries` of its metric.
    pub(super) source: Source,
    /// The union of its `Scan`s' columns.
    pub(super) schema: SchemaRef,
    columns: Vec<TimeSeriesColumn>,
    /// The series seen so far, by ordinal.
    series: Vec<Arc<SeriesLabels>>,
    /// Each column's dictionary texts, one per series of `series` (`None`
    /// for a column that is not text); rebuilt when a series is added.
    texts: Option<Vec<Option<ArrayRef>>>,
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

/// The raw table of each time series `dag`'s `Scan`s read, in the order of
/// its first `Scan` ([`PhysicalASAPDAG::raw_data_schemas`]). `Err` for a
/// source the lane cannot feed: a SQL table, or a column that is neither
/// the time, the value, the series identity nor a text label.
pub(super) fn time_series_tables(dag: &PhysicalASAPDAG) -> Result<Vec<TimeSeriesTable>, String> {
    dag.raw_data_schemas()
        .into_iter()
        .map(|(source, schema)| {
            if !matches!(source, Source::TimeSeries { .. }) {
                return Err(format!("the plan scans {source:?}, not a time series"));
            }
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
                        "{source:?}: column {name} of type {dtype:?} is not a label"
                    )),
                })
                .collect::<Result<_, _>>()?;
            Ok(TimeSeriesTable {
                source,
                schema: Arc::new(schema),
                columns,
                series: Vec::new(),
                texts: None,
            })
        })
        .collect()
}

/// Where each series' samples go: its table and its ordinal there, or
/// nowhere (a metric the plan does not read), looked up once per series.
/// The harness shares one label map among a series' samples, so the map's
/// address names the series; the map is kept alive here, so its address is
/// never reused by another series.
#[derive(Default)]
pub(super) struct SampleRouter {
    by_address: HashMap<*const SeriesLabels, Option<(usize, u32)>>,
    /// Every series seen, keeping its address taken.
    seen: Vec<Arc<SeriesLabels>>,
}

impl SampleRouter {
    /// `chunk`'s samples per table of `tables`, each with its series'
    /// ordinal, in chunk order; a sample of a metric no table reads is
    /// dropped.
    pub(super) fn route<'c>(
        &mut self,
        chunk: &'c SampleChunk,
        tables: &mut [TimeSeriesTable],
    ) -> Vec<Vec<(u32, &'c Sample)>> {
        let mut routed = vec![Vec::new(); tables.len()];
        for sample in &chunk.samples {
            let route = *self
                .by_address
                .entry(Arc::as_ptr(&sample.labels))
                .or_insert_with(|| {
                    self.seen.push(Arc::clone(&sample.labels));
                    let metric = sample.labels.get("__name__")?;
                    let table = tables.iter().position(|table| {
                        matches!(&table.source, Source::TimeSeries { metric: read } if read == metric)
                    })?;
                    Some((table, tables[table].add_series(&sample.labels)))
                });
            if let Some((table, ordinal)) = route {
                routed[table].push((ordinal, sample));
            }
        }
        routed
    }
}

impl TimeSeriesTable {
    /// Adds a series; its ordinal.
    fn add_series(&mut self, labels: &Arc<SeriesLabels>) -> u32 {
        self.series.push(Arc::clone(labels));
        self.texts = None;
        (self.series.len() - 1) as u32
    }

    /// Each text column's dictionary texts, one per series, built when a
    /// series was added since the last call.
    fn texts(&mut self) -> &[Option<ArrayRef>] {
        let series = &self.series;
        self.texts.get_or_insert_with(|| {
            self.columns
                .iter()
                .map(|column| {
                    let texts: StringArray = match column {
                        TimeSeriesColumn::Time | TimeSeriesColumn::Value => return None,
                        TimeSeriesColumn::SeriesIdentity => series
                            .iter()
                            .map(|labels| {
                                Some(
                                    serde_json::to_string(labels.as_ref())
                                        .expect("labels are JSON"),
                                )
                            })
                            .collect(),
                        TimeSeriesColumn::Label(label) => series
                            .iter()
                            .map(|labels| Some(labels.get(label).map_or("", String::as_str)))
                            .collect(),
                    };
                    Some(Arc::new(texts) as ArrayRef)
                })
                .collect()
        })
    }

    /// The rows of `samples` (this table's, each with its series' ordinal)
    /// in the table's schema. Every text column is dictionary-encoded: the
    /// series' ordinal is the key, and the dictionary holds every series
    /// seen so far, shared by every batch until a series is added.
    pub(super) fn batch(
        &mut self,
        samples: &[(u32, &Sample)],
    ) -> Result<Batch, asap_executor::Error> {
        let keys = UInt32Array::from_iter_values(samples.iter().map(|(ordinal, _)| *ordinal));
        let schema = self.schema.clone();
        let mut columns = Vec::with_capacity(self.columns.len());
        // The texts are `Arc`s: cloning them shares the dictionary.
        let texts = self.texts().to_vec();
        for (column, texts) in self.columns.iter().zip(texts) {
            columns.push(match (column, texts) {
                (TimeSeriesColumn::Time, _) => {
                    BatchColumn::Plain(Arc::new(TimestampMillisecondArray::from_iter_values(
                        samples.iter().map(|(_, sample)| sample.ts_ms),
                    )))
                }
                (TimeSeriesColumn::Value, _) => BatchColumn::Plain(Arc::new(
                    Float64Array::from_iter_values(samples.iter().map(|(_, sample)| sample.value)),
                )),
                (_, texts) => {
                    let texts = texts.expect("a text column has its texts");
                    let column = DictionaryArray::<UInt32Type>::try_new(keys.clone(), texts)
                        .map_err(|error| asap_executor::Error::Invalid(error.to_string()))?;
                    BatchColumn::Plain(Arc::new(column))
                }
            });
        }
        Batch::try_from_columns(schema, columns)
    }
}

/// Each answer row of `batches` (one query root's output) as its series
/// labels and value: the series identity's labels, when the output keeps
/// one, and every other non-empty text column (a label the query grouped
/// by); the value is the row's first Float64 column. Example: a row
/// `(ts, 12.5, {"s":"s_0"})` of `sum_over_time(m[5m])` is `{s="s_0"} 12.5`.
/// `Err` for two rows of one series, which a query answer never has.
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
            if let Some(earlier) = answers.insert(labels.clone(), value) {
                return Err(format!(
                    "two answers for series {labels:?}: {earlier} and {value}"
                ));
            }
        }
    }
    Ok(answers)
}
