//! Schema-driven column resolution for the Layer-2 `relational` IR.
//!
//! The Layer-2 IR uses `ColumnRef` (name-based, optionally table-qualified);
//! the canonical IR uses positional [`ColumnId`] resolved against a per-node
//! [`Schema`]. These helpers bridge the two — the [`Binder`](crate::binder)
//! builds the schema, and [`resolve_column_refs`] turns the L2 refs (group
//! keys, dedup columns) into positional ids, qualifier-aware.

use thiserror::Error;

use asap_ir::intent_algebra::agg_intent::AggIntent;
use asap_ir::intent_algebra::expr_ir::ColumnRef;
use asap_ir::intent_algebra::expr_ir::{L2Expr, L3Expr};
use crate::relational::QueryExpr;
use asap_ir::intent_algebra::schema::{Column, ColumnId, DataType, Schema};

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
            Column::new("ts", DataType::Timestamp, false),
            Column::new("value", DataType::Float64, false),
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
        // Prefer the (table, name) match; fall back to the bare name for
        // schemas whose columns carry no qualifier.
        ColumnRef::Qualified { table, name } => schema
            .column_id_qualified(table, name)
            .or_else(|| schema.column_id(name))
            .ok_or_else(|| ResolveError::NotFound {
                name: format!("{table}.{name}"),
                available: schema.columns.iter().map(|c| c.name.clone()).collect(),
            }),
        ColumnRef::SampleValue => schema
            .column_id("value")
            .or_else(|| {
                // After an aggregate the sample value is renamed (e.g. "avg");
                // fall back to the sole non-timestamp column when unambiguous.
                let non_ts: Vec<ColumnId> = (0..schema.columns.len())
                    .filter(|&i| Some(i) != schema.time_index)
                    .collect();
                (non_ts.len() == 1).then(|| non_ts[0])
            })
            .or_else(|| {
                // A *cross-series* aggregate (`sum by (job) (…)`) emits the group
                // labels (Utf8) alongside the single numeric value column (e.g.
                // `[job:Utf8, sum:Float64]`), so the sole-non-ts fallback above is
                // ambiguous. The PromQL sample value of such a vector is that one
                // numeric column — the labels are keys, not values. Pick it when
                // it is the unique non-timestamp numeric column, so an outer
                // ranking (`topk(k, sum by (job) (…))`) resolves its sort key.
                let numeric: Vec<ColumnId> = (0..schema.columns.len())
                    .filter(|&i| Some(i) != schema.time_index)
                    .filter(|&i| {
                        matches!(schema.columns[i].dtype, DataType::Float64 | DataType::Int64)
                    })
                    .collect();
                (numeric.len() == 1).then(|| numeric[0])
            })
            .ok_or_else(|| ResolveError::NoSampleValue {
                available: schema.columns.iter().map(|c| c.name.clone()).collect(),
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

/// Resolve a Layer-2 [`L2Expr`] (name-based) into a positional [`L3Expr`] by
/// resolving every column reference against `schema`. Structural otherwise.
pub fn resolve_expr(expr: &L2Expr, schema: &Schema) -> Result<L3Expr, ResolveError> {
    let boxed = |e: &L2Expr| -> Result<Box<L3Expr>, ResolveError> {
        Ok(Box::new(resolve_expr(e, schema)?))
    };
    let each = |es: &[L2Expr]| -> Result<Vec<L3Expr>, ResolveError> {
        es.iter().map(|e| resolve_expr(e, schema)).collect()
    };
    Ok(match expr {
        L2Expr::Column(c) => L3Expr::Column(resolve_column_ref(c, schema)?),
        L2Expr::Literal(s) => L3Expr::Literal(s.clone()),
        L2Expr::Compare { left, op, right } => L3Expr::Compare {
            left: boxed(left)?,
            op: op.clone(),
            right: boxed(right)?,
        },
        L2Expr::BoolAnd(v) => L3Expr::BoolAnd(each(v)?),
        L2Expr::BoolOr(v) => L3Expr::BoolOr(each(v)?),
        L2Expr::Not(e) => L3Expr::Not(boxed(e)?),
        L2Expr::IsNull(e) => L3Expr::IsNull(boxed(e)?),
        L2Expr::IsNotNull(e) => L3Expr::IsNotNull(boxed(e)?),
        L2Expr::Cast { expr, to, try_cast } => L3Expr::Cast {
            expr: boxed(expr)?,
            to: to.clone(),
            try_cast: *try_cast,
        },
        L2Expr::InList {
            expr,
            list,
            negated,
        } => L3Expr::InList {
            expr: boxed(expr)?,
            list: each(list)?,
            negated: *negated,
        },
        L2Expr::FunctionCall { name, args } => L3Expr::FunctionCall {
            name: name.clone(),
            args: each(args)?,
        },
        L2Expr::Arith { op, left, right } => L3Expr::Arith {
            op: op.clone(),
            left: boxed(left)?,
            right: boxed(right)?,
        },
        L2Expr::Case {
            operand,
            branches,
            else_expr,
        } => L3Expr::Case {
            operand: operand.as_deref().map(&boxed).transpose()?,
            branches: branches
                .iter()
                .map(|(w, t)| Ok((resolve_expr(w, schema)?, resolve_expr(t, schema)?)))
                .collect::<Result<Vec<_>, ResolveError>>()?,
            else_expr: else_expr.as_deref().map(&boxed).transpose()?,
        },
    })
}

/// Output schema produced by an `Aggregate { by, aggs }` over `input`.
/// Mirrors `QueryExpr::output_schema_in`'s `Aggregate` arm; out-of-range `by`
/// ids are silently dropped (callers needing the strict check resolve `by`
/// via [`resolve_column_refs`], which surfaces `NotFound`).
pub fn output_schema_for_aggregate(
    input: &Schema,
    by: &[ColumnId],
    aggs: &[AggIntent],
    output_names: &[String],
) -> Schema {
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
        .unwrap_or_else(|| Column::new("value", DataType::Float64, false));
    for (i, intent) in aggs.iter().enumerate() {
        let in_col = intent
            .input_col()
            .and_then(|id| input.columns.get(id))
            .unwrap_or(&probe);
        let mut out = intent.output_column(in_col);
        if let Some(name) = output_names.get(i).filter(|s| !s.is_empty()) {
            out.name = name.clone();
        }
        out_cols.push(out);
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
        // A cross-series aggregate fully determines its output columns, so the
        // result is closed even over an open input (mirrors `output_schema_in`).
        closed: true,
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
    fn sample_value_resolves_to_sole_numeric_after_cross_series_aggregate() {
        // A cross-series aggregate output `[job:Utf8, sum:Float64]` has no `value`
        // column and two non-ts columns (ambiguous), but exactly one numeric
        // column — the sample value an outer `topk` ranks by.
        let s = Schema::new(vec![
            Column::new("job", DataType::Utf8, true),
            Column::new("sum", DataType::Float64, false),
        ]);
        assert_eq!(resolve_column_ref(&ColumnRef::SampleValue, &s), Ok(1));
    }

    #[test]
    fn sample_value_ambiguous_when_two_numeric_columns() {
        // Two numeric non-ts columns → genuinely ambiguous → NoSampleValue.
        let s = Schema::new(vec![
            Column::new("a", DataType::Float64, false),
            Column::new("b", DataType::Int64, false),
        ]);
        assert!(matches!(
            resolve_column_ref(&ColumnRef::SampleValue, &s),
            Err(ResolveError::NoSampleValue { .. })
        ));
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
        input
            .columns
            .push(Column::new("host", DataType::Utf8, false));
        let out =
            output_schema_for_aggregate(&input, &[2usize], &[AggIntent::Sum { col: None }], &[]);
        assert_eq!(out.columns.len(), 2); // host, sum
        assert_eq!(out.columns[0].name, "host");
        assert_eq!(out.columns[1].name, "sum");
        assert!(out.time_index.is_none());
        assert_eq!(out.unique_keys, vec![vec![0]]);
    }
}
