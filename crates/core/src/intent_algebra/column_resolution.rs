//! Schema-driven column resolution for the Layer-2 `relational` IR.
//!
//! The Layer-2 IR uses `ColumnRef::Named(String)` / `Aggregate.keys:
//! Vec<String>`; the canonical IR uses positional [`ColumnId`] resolved
//! against a per-node [`Schema`]. These helpers bridge the two — the
//! [`Binder`](super::binder) builds the schema, and [`resolve_named_keys`]
//! turns the L2 names into positional ids.

use thiserror::Error;

use crate::intent_algebra::agg_intent::AggIntent;
use crate::intent_algebra::query_expr::ColumnRef;
use crate::intent_algebra::relational::QueryExpr;
use crate::intent_algebra::schema::{Column, ColumnId, DataType, Schema};

/// Errors returned by the resolution helpers.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("column `{name}` not found in schema (have: {available:?})")]
    NotFound {
        name: String,
        available: Vec<String>,
    },
    #[error("ColumnRef::SampleValue has no `value` column in schema (have: {available:?})")]
    NoSampleValue { available: Vec<String> },
    #[error("ColumnRef::Wildcard cannot be resolved to a single ColumnId")]
    WildcardNotPositional,
}

/// Synthesize the conventional PromQL leaf schema `(ts, value)` for a metric.
pub fn infer_source_schema(_metric_or_table: &str) -> Schema {
    Schema::with_time_index(
        vec![
            Column {
                name: "ts".into(),
                dtype: DataType::Timestamp,
                nullable: false,
            },
            Column {
                name: "value".into(),
                dtype: DataType::Float64,
                nullable: false,
            },
        ],
        0,
        Vec::new(),
    )
}

/// Synthesise the root schema by walking to the outermost `Source` leaf.
pub fn infer_schema_for_root(expr: &QueryExpr) -> Schema {
    match expr.source_name() {
        Some(name) => infer_source_schema(name),
        None => Schema::default(),
    }
}

/// Resolve a single [`ColumnRef`] to a positional [`ColumnId`].
pub fn resolve_column_ref(col: &ColumnRef, schema: &Schema) -> Result<ColumnId, ResolveError> {
    match col {
        ColumnRef::Named(name) => schema
            .column_id(name)
            .ok_or_else(|| ResolveError::NotFound {
                name: name.clone(),
                available: schema.columns.iter().map(|c| c.name.clone()).collect(),
            }),
        ColumnRef::SampleValue => {
            schema
                .column_id("value")
                .ok_or_else(|| ResolveError::NoSampleValue {
                    available: schema.columns.iter().map(|c| c.name.clone()).collect(),
                })
        }
        ColumnRef::Wildcard => Err(ResolveError::WildcardNotPositional),
    }
}

/// Resolve every entry, short-circuiting on the first error.
pub fn resolve_column_refs(
    cols: &[ColumnRef],
    schema: &Schema,
) -> Result<Vec<ColumnId>, ResolveError> {
    cols.iter().map(|c| resolve_column_ref(c, schema)).collect()
}

/// Resolve a list of named GROUP BY keys (`Aggregate.keys`) to `ColumnId`s.
pub fn resolve_named_keys(keys: &[String], schema: &Schema) -> Result<Vec<ColumnId>, ResolveError> {
    keys.iter()
        .map(|name| {
            schema
                .column_id(name)
                .ok_or_else(|| ResolveError::NotFound {
                    name: name.clone(),
                    available: schema.columns.iter().map(|c| c.name.clone()).collect(),
                })
        })
        .collect()
}

/// Output schema produced by an `Aggregate { by, aggs }` over `input`.
/// Mirrors `QueryExpr::output_schema_in`'s `Aggregate` arm; out-of-range `by`
/// ids are silently dropped (callers needing the strict check resolve `by`
/// via [`resolve_named_keys`], which surfaces `NotFound`).
pub fn output_schema_for_aggregate(input: &Schema, by: &[ColumnId], aggs: &[AggIntent]) -> Schema {
    let mut out_cols: Vec<Column> = Vec::with_capacity(by.len() + aggs.len());
    for &id in by {
        if let Some(c) = input.columns.get(id) {
            out_cols.push(c.clone());
        }
    }
    let value_col_idx = input
        .column_id("value")
        .or_else(|| (0..input.columns.len()).find(|i| !by.contains(i)));
    let probe = value_col_idx
        .and_then(|i| input.columns.get(i))
        .cloned()
        .unwrap_or(Column {
            name: "value".into(),
            dtype: DataType::Float64,
            nullable: false,
        });
    for intent in aggs {
        out_cols.push(intent.output_column(&probe));
    }
    let unique_keys = if by.is_empty() {
        Vec::new()
    } else {
        vec![(0..by.len()).collect()]
    };
    Schema {
        columns: out_cols,
        time_index: None,
        unique_keys,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_schema_has_ts_and_value() {
        let s = infer_source_schema("m");
        assert_eq!(s.columns.len(), 2);
        assert_eq!(s.time_index, Some(0));
        assert!(!s.has_unique_key());
    }

    #[test]
    fn resolve_sample_value() {
        let s = infer_source_schema("m");
        assert_eq!(resolve_column_ref(&ColumnRef::SampleValue, &s), Ok(1));
    }

    #[test]
    fn resolve_unknown_name_errors() {
        let s = infer_source_schema("m");
        let err = resolve_column_ref(&ColumnRef::Named("host".into()), &s).unwrap_err();
        assert!(matches!(err, ResolveError::NotFound { .. }));
    }

    #[test]
    fn aggregate_strips_time_and_keeps_unique_keys() {
        let mut input = infer_source_schema("m");
        input.columns.push(Column {
            name: "host".into(),
            dtype: DataType::Utf8,
            nullable: false,
        });
        let out = output_schema_for_aggregate(&input, &[2usize], &[AggIntent::Sum]);
        assert_eq!(out.columns.len(), 2); // host, sum
        assert_eq!(out.columns[0].name, "host");
        assert_eq!(out.columns[1].name, "sum");
        assert!(out.time_index.is_none());
        assert_eq!(out.unique_keys, vec![vec![0]]);
    }
}
