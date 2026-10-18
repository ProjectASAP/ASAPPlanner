//! Raw samples in, labeled answers out: how a deployment runtime feeds a
//! plan's inputs and reads its PromQL outputs, without the executor's row
//! encoding.
//!
//! A PromQL input row carries the sample's full label set as one series
//! identity column, plus its timestamp, its value and any label columns
//! the plan reads; [`input_rows_from_samples`] builds those rows for an
//! input node of the installable DAG (its `Scan`, or the `TimeRange` of a
//! stored output's pane). [`series_labels_of_output_row`] reads the labels
//! of an answer row back.

use std::collections::BTreeMap;
use std::sync::Arc;

use planner_types::ir::physical_export::PhysicalASAPDAGNode;
use planner_types::ir::schema::Schema;

use crate::physical_planner::promql_rows::{
    decode_series_identity, series_row, SERIES_IDENTITY_COLUMN,
};
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

/// The executor's input rows for `samples` fed to `input_node`: one row per
/// sample, in the node's output schema. `input_node` is a node of the
/// installable DAG the runtime binds as an input: a `Scan`, or the
/// `TimeRange` of a stored output's pane (given only that pane's samples).
/// Bad input when the schema has no series identity column or a sample's
/// labels cannot fill it.
pub fn input_rows_from_samples(
    input_node: &PhysicalASAPDAGNode,
    samples: &[Sample],
) -> Result<Batch, Error> {
    let schema = Arc::new(input_node.output_schema.clone());
    let rows = samples
        .iter()
        .map(|s| series_row(&schema, &s.labels, s.timestamp_ms, s.value))
        .collect::<Result<Vec<_>, _>>()?;
    Batch::try_new(schema, rows)
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
    use planner_types::ir::physical_export::PhysicalASAPDAGNode;
    use planner_types::ir::properties::execution_data_state::DataPrimitive;
    use planner_types::ir::properties::{ExecutionDataState, ExecutionTiming};
    use planner_types::ir::schema::{DataType, Field, PROMQL_SERIES_IDENTITY};
    use planner_types::ir::{NonASAPOp, Operator};
    use planner_types::physical::Materialization;

    /// A sample becomes one input row of the node's schema, and the labels
    /// of a row with its series identity read back as the sample's.
    #[test]
    fn a_sample_round_trips_through_its_input_row() {
        let mut schema = Schema::new(vec![
            Field::plain(PROMQL_SERIES_IDENTITY, DataType::Utf8, false),
            Field::plain("timestamp", DataType::Timestamp, false),
            Field::plain("value", DataType::Float64, false),
        ]);
        schema.time_index = Some(1);
        let node = PhysicalASAPDAGNode {
            id: 0,
            payload: Operator::NonASAP(NonASAPOp::Scan {
                source: planner_types::ir::operator::Source::TimeSeries {
                    metric: "latency_ms".into(),
                },
                predicates: vec![],
                schema: schema.clone(),
            }),
            output_state: ExecutionDataState {
                timing: ExecutionTiming::QueryTime,
                primitive: DataPrimitive::Raw,
            },
            materialization: Materialization::NotMaterialized,
            output_schema: schema.clone(),
            guarantee: None,
        };
        let labels = BTreeMap::from([
            ("__name__".to_string(), "latency_ms".to_string()),
            ("instance".to_string(), "a".to_string()),
        ]);
        let sample = Sample {
            labels: labels.clone(),
            timestamp_ms: 5_000,
            value: 120.0,
        };
        let batch = input_rows_from_samples(&node, &[sample]).unwrap();
        assert_eq!(batch.rows().len(), 1);
        assert_eq!(
            series_labels_of_output_row(&schema, &batch.rows()[0]).unwrap(),
            labels
        );
    }
}
