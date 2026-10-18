//! Raw samples in, labeled answers out: how a deployment runtime feeds a
//! plan's inputs and reads its PromQL outputs, without the executor's row
//! encoding.
//!
//! [`input_rows_from_samples`] turns the samples a runtime received into
//! the input rows of one raw-sample window of the installable DAG: the
//! window's `Scan` runs over them (its metric and label matchers), and the
//! rows in the window at the evaluation time are bound to its `TimeRange`.
//! [`series_labels_of_output_row`] reads the labels of an answer row back.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::{executor::block_on, StreamExt};
use planner_types::ir::operator::Source as ScanSource;
use planner_types::ir::physical_export::{PhysicalASAPDAG, RawSampleWindow};
use planner_types::ir::schema::Schema;
use planner_types::ir::{NonASAPOp, Operator};

use crate::physical_planner::bind_with_data_sources;
use crate::physical_planner::promql_rows::{
    decode_series_identity, series_row, SERIES_IDENTITY_COLUMN,
};
use crate::runtime::{Limits, RunContext, Scope};
use crate::sources::{DataSources, MemorySource};
use crate::values::{Batch, Value};
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

/// The input rows of `window` of `dag` at evaluation time
/// `evaluation_time_ms`, from `samples`: the samples of the window's
/// metric inside the window, selected by its `Scan` (label matchers
/// included), in the `TimeRange`'s schema. The runtime binds them to
/// `window.time_range_node_id`. `samples` may hold any series and times;
/// what the window does not read is dropped. Bad input when the window's
/// nodes are not a time-series `Scan` and a `TimeRange` of `dag`.
pub fn input_rows_from_samples(
    dag: &PhysicalASAPDAG,
    window: &RawSampleWindow,
    evaluation_time_ms: i64,
    samples: &[Sample],
) -> Result<Batch, Error> {
    let node = |id| {
        dag.nodes
            .iter()
            .find(|n| n.id == id)
            .ok_or_else(|| Error::Invalid(format!("window names unknown node {id}")))
    };
    let scan = node(window.scan_node_id)?;
    let Operator::NonASAP(NonASAPOp::Scan {
        source: source @ ScanSource::TimeSeries { metric },
        ..
    }) = &scan.payload
    else {
        return Err(Error::Invalid(
            "window's scan is not a time-series Scan".into(),
        ));
    };
    let time_range = node(window.time_range_node_id)?;
    if !matches!(
        time_range.payload,
        Operator::NonASAP(NonASAPOp::TimeRange { .. })
    ) {
        return Err(Error::Invalid(
            "window's time range is not a TimeRange".into(),
        ));
    }
    let (start, end) = window.bounds_ms(evaluation_time_ms);
    let scan_schema = Arc::new(scan.output_schema.clone());
    let rows = samples
        .iter()
        .filter(|s| start < s.timestamp_ms && s.timestamp_ms <= end)
        .filter(|s| s.labels.get("__name__") == Some(metric))
        .map(|s| series_row(&scan_schema, &s.labels, s.timestamp_ms, s.value))
        .collect::<Result<Vec<_>, _>>()?;
    let mut sources = DataSources::default();
    sources.register(
        source.clone(),
        Arc::new(MemorySource::new(
            Arc::clone(&scan_schema),
            vec![Batch::try_new(Arc::clone(&scan_schema), rows)?],
        )?),
    )?;
    let scan_id = window.scan_node_id as u64;
    let selecting = bind_with_data_sources(dag, BTreeMap::new(), &[scan_id], &sources)?;
    let scope = Scope::Query {
        evaluation_time_ms,
        revision: 0,
    };
    let selected = block_on(async {
        let context = RunContext::new(scope, Limits::default())?;
        let mut output = selecting.execute(&[scan_id], context)?.remove(0);
        let mut rows = Vec::new();
        while let Some(batch) = output.next().await {
            rows.extend(batch?.rows().iter().cloned());
        }
        Ok::<_, Error>(rows)
    })?;
    if time_range.output_schema.fields != scan.output_schema.fields {
        return Err(Error::Invalid(
            "window's TimeRange changes its Scan's columns".into(),
        ));
    }
    Batch::try_new(Arc::new(time_range.output_schema.clone()), selected)
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
