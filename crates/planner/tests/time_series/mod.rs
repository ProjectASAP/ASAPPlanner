//! A stand-in runtime's raw data samples, bound at the plan's `Scan`s with
//! public APIs only: one `TableProvider` per time series, a table `(ts,
//! value, *labels)` over the samples it received. The DAG's own
//! `TimeRange`s apply each window, instant lookback and stale marker.
#![allow(dead_code)] // Each test uses part of it.

use std::collections::BTreeMap;
use std::sync::Arc;

use asap_executor::sources::{DataSources, MemorySource};
use asap_executor::values::{Batch, Value};
use asap_types::ir::operator::Source;
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPOperatorPayload};
use asap_types::ir::schema::{DataType, FieldDataType, Schema, PROMQL_SERIES_IDENTITY};
use asap_types::ir::NonASAPOp;

/// One raw data sample as it arrives, e.g. one Prometheus Remote Write sample.
#[derive(Debug, Clone, PartialEq)]
pub struct RawDataSample {
    /// Every label of the series, `__name__` (the metric) included.
    pub labels: BTreeMap<String, String>,
    pub timestamp_ms: i64,
    pub value: f64,
}

/// The series identity column's text: the canonical JSON of every label
/// ([`PROMQL_SERIES_IDENTITY`]).
pub fn series_identity(labels: &BTreeMap<String, String>) -> String {
    serde_json::to_string(labels).unwrap()
}

/// The deployment's catalog for `dag`: each metric its time-series `Scan`s
/// read, bound to a provider over its samples among `samples` (routed by
/// `__name__`), with every column those `Scan`s read.
pub fn time_series_tables(dag: &PhysicalASAPDAG, samples: &[RawDataSample]) -> DataSources {
    // Each metric's columns: the union of its Scans' columns, by name.
    let mut schemas: BTreeMap<&str, Schema> = BTreeMap::new();
    for node in &dag.nodes {
        let PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries { metric },
            schema,
            ..
        }) = &node.payload
        else {
            continue;
        };
        let known = schemas.entry(metric).or_insert_with(|| schema.clone());
        for field in &schema.fields {
            if !known.fields.iter().any(|f| f.name == field.name) {
                known.fields.push(field.clone());
            }
        }
    }
    let mut tables = DataSources::default();
    for (metric, schema) in schemas {
        let schema = Arc::new(schema);
        let rows = samples
            .iter()
            .filter(|s| s.labels.get("__name__").map(String::as_str) == Some(metric))
            .map(|sample| row(&schema, sample))
            .collect();
        let batch = Batch::try_new(schema.clone(), rows).unwrap();
        tables
            .register(
                Source::TimeSeries {
                    metric: metric.to_string(),
                },
                Arc::new(MemorySource::new(schema, vec![batch]).unwrap()),
            )
            .unwrap();
    }
    tables
}

/// `sample` as a row of `schema`: its identity, time, value, and each label
/// column's value (empty when the series lacks the label).
fn row(schema: &Schema, sample: &RawDataSample) -> Vec<Value> {
    schema
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| match field.name.as_str() {
            PROMQL_SERIES_IDENTITY => Value::Utf8(series_identity(&sample.labels).into()),
            _ if Some(index) == schema.time_index => Value::Timestamp(sample.timestamp_ms),
            "value" if field.dtype == FieldDataType::Plain(DataType::Float64) => {
                Value::Float64(sample.value)
            }
            label => Value::Utf8(sample.labels.get(label).cloned().unwrap_or_default().into()),
        })
        .collect()
}

/// The labels of the series an answer row belongs to, read from the output
/// schema's label columns: the series identity's labels, when the schema
/// keeps one, and every other non-empty text column (a label the query
/// grouped by).
pub fn series_labels(schema: &Schema, row: &[Value]) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    for (field, cell) in schema.fields.iter().zip(row) {
        let Value::Utf8(text) = cell else { continue };
        if field.name == PROMQL_SERIES_IDENTITY {
            let identity: BTreeMap<String, String> = serde_json::from_str(text).unwrap();
            labels.extend(identity);
        } else if !text.is_empty() {
            labels.insert(field.name.clone(), text.to_string());
        }
    }
    labels
}
