//! A bounded PromQL source row carries the entire label set, not just labels
//! mentioned by the query. The source adapter owns this lossless encoding.
use super::*;
use planner_types::pre_asap::{Column, DataType, Source as LogicalSource};
use std::rc::Rc;

/// Not a legal PromQL label name, so it cannot shadow a user label.
pub const SERIES_IDENTITY_COLUMN: &str = "$promql_series_identity";

/// Canonical, reversible identity. JSON object encoding preserves label names,
/// empty values and escaping; sorting makes ingestion order irrelevant.
pub fn encode_series_identity(labels: &BTreeMap<String, String>) -> Result<String, Error> {
    serde_json::to_string(labels).map_err(|error| invalid(error.to_string()))
}

pub fn decode_series_identity(encoded: &str) -> Result<BTreeMap<String, String>, Error> {
    let labels: BTreeMap<String, String> =
        serde_json::from_str(encoded).map_err(|error| invalid(error.to_string()))?;
    if encode_series_identity(&labels)? != encoded {
        return Err(invalid("series identity is not canonically encoded"));
    }
    Ok(labels)
}

/// Resolve the row representation before candidate search. `closed` describes
/// physical columns here: the final column contains every dynamic source label.
/// It does not assert that the query's projected labels are the full label set.
///
/// This realization supports explicit `by` grouping and per-series computation.
/// Operators that rewrite or implicitly match dynamic label sets require their
/// own realization; they must not accidentally treat the opaque identity as a
/// user label or silently discard it.
pub fn with_series_identity(root: &QueryExpr) -> Result<QueryExpr, Error> {
    let mut root = root.clone();
    fn visit(node: &mut QueryExpr) -> Result<(), Error> {
        use planner_types::pre_asap::Reduction;
        match node {
            QueryExpr::Scan {
                source: LogicalSource::TimeSeries { .. },
                schema,
                ..
            } => {
                if schema
                    .columns
                    .iter()
                    .any(|column| column.name == SERIES_IDENTITY_COLUMN)
                {
                    return Err(invalid(
                        "source already contains a physical series identity",
                    ));
                }
                if schema.closed {
                    return Err(invalid(
                        "dynamic series identity requires an open PromQL source",
                    ));
                }
                schema
                    .columns
                    .push(Column::new(SERIES_IDENTITY_COLUMN, DataType::Utf8, false));
                schema.closed = true;
                Ok(())
            }
            QueryExpr::TimeRange { child, .. } | QueryExpr::Limit { child, .. } => {
                visit(Rc::make_mut(child))
            }
            QueryExpr::Aggregate {
                child, reduction, ..
            } => {
                if matches!(reduction, Reduction::Reduce(keys) if keys.is_without()) {
                    return Err(invalid(
                        "dynamic without grouping requires label-set projection",
                    ));
                }
                visit(Rc::make_mut(child))
            }
            QueryExpr::Sort {
                child,
                partition_by,
                ..
            } => {
                if partition_by.is_without() {
                    return Err(invalid(
                        "dynamic without ranking requires label-set projection",
                    ));
                }
                visit(Rc::make_mut(child))
            }
            _ => Err(invalid(
                "operator has no dynamic series-identity realization",
            )),
        }
    }
    visit(&mut root)?;
    root.output_schema()
        .map_err(|error| invalid(error.to_string()))?;
    Ok(root)
}

/// Construct source rows only from full identities. The named label columns
/// are projections of that same identity and cannot independently redefine it.
pub fn series_row(
    schema: &Schema,
    labels: &BTreeMap<String, String>,
    timestamp: i64,
    value: f64,
) -> Result<Vec<crate::values::Value>, Error> {
    use crate::values::Value;
    let identity = encode_series_identity(labels)?;
    let mut found = false;
    let row = schema
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            if field.name == SERIES_IDENTITY_COLUMN {
                if field.dtype != SummaryFamilyType::Plain(DataType::Utf8)
                    || field.nullable
                    || found
                {
                    return Err(invalid("invalid series identity column"));
                }
                found = true;
                Ok(Value::Utf8(identity.clone().into()))
            } else if Some(index) == schema.time_index {
                Ok(Value::Timestamp(timestamp))
            } else if field.name == "value"
                && field.dtype == SummaryFamilyType::Plain(DataType::Float64)
            {
                Ok(Value::Float64(value))
            } else if field.dtype == SummaryFamilyType::Plain(DataType::Utf8) {
                Ok(labels.get(&field.name).map_or_else(
                    || Value::Utf8("".into()),
                    |value| Value::Utf8(value.clone().into()),
                ))
            } else {
                Err(invalid("unsupported PromQL source column"))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !found {
        return Err(invalid("source lacks its full series identity"));
    }
    Ok(row)
}
