//! Schema-driven column resolution.
//!
//! Front ends emit `ColumnRef` (name-based, optionally table-qualified); the
//! IR uses positional [`ColumnId`] resolved against a per-node [`Schema`].
//! These helpers turn name-based refs into positional ids, qualifier-aware.
//! Front-end name resolution (`asap_frontend_common::resolve`) calls them.

use thiserror::Error;

use super::expr_ir::ColumnRef;
use crate::ir::schema::{ColumnId, DataType, FieldDataType, Schema};

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
    #[error("column `{name}` matches several unqualified columns")]
    Ambiguous { name: String },
}

/// Resolve a single [`ColumnRef`] to a positional [`ColumnId`].
pub fn resolve_column_ref(col: &ColumnRef, schema: &Schema) -> Result<ColumnId, ResolveError> {
    match col {
        // A bare name must be unique: after a join `k` could be `a.k` or `b.k`.
        ColumnRef::Named(name) => match schema.fields.iter().filter(|c| &c.name == name).count() {
            0 => Err(ResolveError::NotFound {
                name: name.clone(),
                available: schema.fields.iter().map(|c| c.name.clone()).collect(),
            }),
            1 => Ok(schema.column_id(name).expect("one field has this name")),
            _ => Err(ResolveError::Ambiguous { name: name.clone() }),
        },
        // Prefer the (table, name) match; fall back to the bare name for
        // schemas whose columns carry no qualifier, but only when that name
        // is unique: after a join `b.k` must not bind to `a.k`.
        ColumnRef::Qualified { table, name } => {
            if let Some(id) = schema.column_id_qualified(table, name) {
                return Ok(id);
            }
            match schema.fields.iter().filter(|c| &c.name == name).count() {
                0 => Err(ResolveError::NotFound {
                    name: format!("{table}.{name}"),
                    available: schema.fields.iter().map(|c| c.name.clone()).collect(),
                }),
                1 => Ok(schema.column_id(name).expect("one field has this name")),
                _ => Err(ResolveError::Ambiguous {
                    name: format!("{table}.{name}"),
                }),
            }
        }
        ColumnRef::SampleValue => schema
            .column_id("value")
            .or_else(|| {
                // After an aggregate the sample value is renamed (e.g. "avg", or
                // "sum" alongside group labels in `[job:Utf8, sum:Float64]`).
                // Fall back to the unique non-timestamp *numeric* column — the
                // sample value is always numeric, and the labels are keys, not
                // values. Requiring numeric (rather than "the sole non-ts column
                // of any type") avoids binding `SampleValue` to a label column in
                // a `[ts, host:Utf8]`-shaped schema (#70), and still resolves an
                // outer ranking's sort key (`topk(k, sum by (job) (…))`).
                let numeric: Vec<ColumnId> = (0..schema.fields.len())
                    .filter(|&i| Some(i) != schema.time_index)
                    .filter(|&i| {
                        matches!(
                            schema.fields[i].dtype,
                            FieldDataType::Plain(DataType::Float64 | DataType::Int64)
                        )
                    })
                    .collect();
                (numeric.len() == 1).then(|| numeric[0])
            })
            .ok_or_else(|| ResolveError::NoSampleValue {
                available: schema.fields.iter().map(|c| c.name.clone()).collect(),
            }),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::schema::Field;

    /// The conventional PromQL leaf shape: `(ts: Timestamp, value: Float64)`.
    fn ts_value_schema() -> Schema {
        Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
            ],
            0,
            Vec::new(),
        )
    }

    #[test]
    fn resolve_sample_value() {
        let s = ts_value_schema();
        assert_eq!(resolve_column_ref(&ColumnRef::SampleValue, &s), Ok(1));
    }

    #[test]
    fn sample_value_resolves_to_sole_numeric_after_cross_series_aggregate() {
        // A cross-series aggregate output `[job:Utf8, sum:Float64]` has no `value`
        // column and two non-ts columns (ambiguous), but exactly one numeric
        // column — the sample value an outer `topk` ranks by.
        let s = Schema::new(vec![
            Field::plain("job", DataType::Utf8, true),
            Field::plain("sum", DataType::Float64, false),
        ]);
        assert_eq!(resolve_column_ref(&ColumnRef::SampleValue, &s), Ok(1));
    }

    #[test]
    fn sample_value_does_not_bind_a_label_column() {
        // `[ts:Timestamp, host:Utf8]` has no `value` column and its sole non-ts
        // column is a *label* (Utf8), not a sample value. `SampleValue` must not
        // bind to it (#70) — resolution fails cleanly instead of picking a label.
        let s = Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("host", DataType::Utf8, true),
            ],
            0,
            vec![],
        );
        assert!(matches!(
            resolve_column_ref(&ColumnRef::SampleValue, &s),
            Err(ResolveError::NoSampleValue { .. })
        ));
    }

    #[test]
    fn sample_value_ambiguous_when_two_numeric_columns() {
        // Two numeric non-ts columns → genuinely ambiguous → NoSampleValue.
        let s = Schema::new(vec![
            Field::plain("a", DataType::Float64, false),
            Field::plain("b", DataType::Int64, false),
        ]);
        assert!(matches!(
            resolve_column_ref(&ColumnRef::SampleValue, &s),
            Err(ResolveError::NoSampleValue { .. })
        ));
    }

    #[test]
    fn resolve_unknown_name_errors() {
        let s = ts_value_schema();
        let err = resolve_column_ref(&ColumnRef::Named("host".into()), &s).unwrap_err();
        assert!(matches!(err, ResolveError::NotFound { .. }));
    }
}
