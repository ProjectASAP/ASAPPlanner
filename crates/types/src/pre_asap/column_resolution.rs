//! Schema-driven column resolution.
//!
//! Front ends (issue #179) emit `ColumnRef` (name-based, optionally
//! table-qualified); the canonical tree uses positional [`ColumnId`] resolved
//! against a per-node [`Schema`]. These helpers bridge the two — the
//! [`Binder`](super::binder) builds the schema, and [`resolve_column_refs`]
//! turns name-based refs (group keys, dedup columns) into positional ids,
//! qualifier-aware.

use std::rc::Rc;

use thiserror::Error;

use super::agg_intent::AggIntent;
use super::expr_ir::ColumnRef;
use super::query_expr::{
    aggregate_output_schema, GroupKeys, QueryExpr, QueryExprError, Reduction, ResolvedQueryExpr,
    UnresolvedQueryExpr,
};
use super::schema::{ColumnId, DataType, Schema};

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
                // After an aggregate the sample value is renamed (e.g. "avg", or
                // "sum" alongside group labels in `[job:Utf8, sum:Float64]`).
                // Fall back to the unique non-timestamp *numeric* column — the
                // sample value is always numeric, and the labels are keys, not
                // values. Requiring numeric (rather than "the sole non-ts column
                // of any type") avoids binding `SampleValue` to a label column in
                // a `[ts, host:Utf8]`-shaped schema (#70), and still resolves an
                // outer ranking's sort key (`topk(k, sum by (job) (…))`).
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

/// Resolve PromQL aggregation grouping keys with the language's absent-label
/// semantics (issue #53): a key not present in a **closed** schema is provably
/// absent from every row, so all rows carry its empty value, grouping by it is
/// the identity partition, and Prometheus omits the (empty) label from the
/// aggregation output — so the key is **dropped** rather than rejected. This
/// is what makes `sum(sum by (k) (m)) by (j)` lower: the inner cross-series
/// aggregate freezes the schema to the closed `[k, sum]`, which provably lacks
/// `j`.
///
/// Against an **open** schema an unresolved key is still an error: the label
/// may exist at runtime, and PromQL leaves seed every referenced label via the
/// Binder, so an unresolved key over an open schema indicates a resolution bug,
/// not an absent label. (SQL is unaffected — its `GROUP BY` resolves through
/// the strict [`resolve_column_refs`], and DataFusion has already validated
/// the columns anyway.)
pub fn resolve_group_keys_promql(
    cols: &[ColumnRef],
    schema: &Schema,
) -> Result<Vec<ColumnId>, ResolveError> {
    cols.iter()
        .filter_map(|c| match resolve_column_ref(c, schema) {
            Err(ResolveError::NotFound { .. }) if schema.closed => None,
            other => Some(other),
        })
        .collect()
}

/// Resolve a name-based scalar [`UnresolvedQueryExpr`] (one of `QueryExpr`'s scalar
/// variants, issue #205) into a positional [`ResolvedQueryExpr`] by resolving every
/// column reference against `schema`. Structural otherwise. `expr` must be
/// one of the scalar variants — an operator variant here is a construction
/// bug, not a shape this needs to handle silently.
pub fn resolve_expr(
    expr: &UnresolvedQueryExpr,
    schema: &Schema,
) -> Result<ResolvedQueryExpr, ResolveError> {
    let rc = |e: &UnresolvedQueryExpr| -> Result<Rc<ResolvedQueryExpr>, ResolveError> {
        Ok(Rc::new(resolve_expr(e, schema)?))
    };
    let each = |es: &[UnresolvedQueryExpr]| -> Result<Vec<ResolvedQueryExpr>, ResolveError> {
        es.iter().map(|e| resolve_expr(e, schema)).collect()
    };
    Ok(match expr {
        QueryExpr::Column(c) => QueryExpr::Column(resolve_column_ref(c, schema)?),
        QueryExpr::Literal(s) => QueryExpr::Literal(s.clone()),
        QueryExpr::QueryTimestamp => QueryExpr::QueryTimestamp,
        QueryExpr::Compare { left, op, right } => QueryExpr::Compare {
            left: rc(left)?,
            op: op.clone(),
            right: rc(right)?,
        },
        QueryExpr::BoolAnd(v) => QueryExpr::BoolAnd(each(v)?),
        QueryExpr::BoolOr(v) => QueryExpr::BoolOr(each(v)?),
        QueryExpr::Not(e) => QueryExpr::Not(rc(e)?),
        QueryExpr::IsNull(e) => QueryExpr::IsNull(rc(e)?),
        QueryExpr::IsNotNull(e) => QueryExpr::IsNotNull(rc(e)?),
        QueryExpr::Cast { expr, to, try_cast } => QueryExpr::Cast {
            expr: rc(expr)?,
            to: to.clone(),
            try_cast: *try_cast,
        },
        QueryExpr::InList {
            expr,
            list,
            negated,
        } => QueryExpr::InList {
            expr: rc(expr)?,
            list: each(list)?,
            negated: *negated,
        },
        QueryExpr::FunctionCall { name, args } => QueryExpr::FunctionCall {
            name: name.clone(),
            args: each(args)?,
        },
        QueryExpr::Arithmetic { op, left, right } => QueryExpr::Arithmetic {
            op: op.clone(),
            left: rc(left)?,
            right: rc(right)?,
        },
        QueryExpr::Case {
            operand,
            branches,
            else_expr,
        } => QueryExpr::Case {
            operand: operand.as_deref().map(&rc).transpose()?,
            branches: branches
                .iter()
                .map(|(w, t)| Ok((resolve_expr(w, schema)?, resolve_expr(t, schema)?)))
                .collect::<Result<Vec<_>, ResolveError>>()?,
            else_expr: else_expr.as_deref().map(&rc).transpose()?,
        },
        other => unreachable!("resolve_expr called on a non-scalar QueryExpr variant: {other:?}"),
    })
}

/// Output schema produced by an `Aggregate { by, measures }` over `input`.
/// Mirrors `QueryExpr::output_schema_in`'s `Aggregate` arm; out-of-range `by`
/// ids are silently dropped (callers needing the strict check resolve `by`
/// via [`resolve_column_refs`], which surfaces `NotFound`).
pub fn output_schema_for_aggregate(
    input: &Schema,
    by: &GroupKeys,
    measures: &[AggIntent],
    output_names: &[String],
) -> Result<Schema, QueryExprError> {
    // Delegate to the single canonical derivation so HAVING resolution can never
    // drift from `QueryExpr::output_schema_in` (issue #41). HAVING is SQL-only
    // and cross-series (SQL has no `without`), but detect the child-independent
    // per-entity case anyway (a lone `rate`/`increase`/`*_over_time` intent) so
    // the two agree on every shared input — the range-window child marker the
    // canonical arm also keys off is not visible here, and never co-occurs with
    // HAVING.
    let per_entity =
        by.is_empty() && !by.is_without() && measures.len() == 1 && measures[0].is_per_series();
    let reduction = if per_entity {
        Reduction::PerEntity
    } else {
        Reduction::Reduce(by.clone())
    };
    aggregate_output_schema(input, &reduction, measures, output_names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pre_asap::schema::Column;

    /// The conventional PromQL leaf shape: `(ts: Timestamp, value: Float64)`.
    fn ts_value_schema() -> Schema {
        Schema::with_time_index(
            vec![
                Column::new("ts", DataType::Timestamp, false),
                Column::new("value", DataType::Float64, false),
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
            Column::new("job", DataType::Utf8, true),
            Column::new("sum", DataType::Float64, false),
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
                Column::new("ts", DataType::Timestamp, false),
                Column::new("host", DataType::Utf8, true),
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
        let s = ts_value_schema();
        let err = resolve_column_ref(&ColumnRef::Named("host".into()), &s).unwrap_err();
        assert!(matches!(err, ResolveError::NotFound { .. }));
    }

    #[test]
    fn group_keys_promql_drops_absent_key_in_closed_schema() {
        // The output of a nested cross-series aggregate: closed `[group, sum]`.
        // `by (job)` — `job` is provably absent → dropped, not rejected (#53).
        let s = Schema {
            columns: vec![
                Column::new("group", DataType::Utf8, true),
                Column::new("sum", DataType::Float64, false),
            ],
            time_index: None,
            unique_keys: vec![],
            closed: true,
        };
        assert_eq!(
            resolve_group_keys_promql(&[ColumnRef::Named("job".into())], &s),
            Ok(vec![])
        );
        // Present keys still resolve positionally; absent ones drop around them.
        assert_eq!(
            resolve_group_keys_promql(
                &[
                    ColumnRef::Named("job".into()),
                    ColumnRef::Named("group".into())
                ],
                &s
            ),
            Ok(vec![0])
        );
    }

    #[test]
    fn group_keys_promql_still_errors_on_open_schema() {
        // An open schema can't prove absence — an unresolved key there is a
        // resolution bug (the Binder seeds every referenced label), not an
        // absent label. Keep the strict error.
        let open = ts_value_schema(); // closed: false
        assert!(matches!(
            resolve_group_keys_promql(&[ColumnRef::Named("job".into())], &open),
            Err(ResolveError::NotFound { .. })
        ));
    }

    #[test]
    fn aggregate_strips_time_and_keeps_unique_keys() {
        let mut input = ts_value_schema();
        input
            .columns
            .push(Column::new("host", DataType::Utf8, false));
        let out = output_schema_for_aggregate(
            &input,
            &GroupKeys::by(vec![2]),
            &[AggIntent::Sum { col: None }],
            &[],
        )
        .expect("valid group-by column");
        assert_eq!(out.columns.len(), 2); // host, sum
        assert_eq!(out.columns[0].name, "host");
        assert_eq!(out.columns[1].name, "sum");
        assert!(out.time_index.is_none());
        assert_eq!(out.unique_keys, vec![vec![0]]);
    }

    #[test]
    fn having_schema_agrees_with_canonical_for_a_per_series_reduction() {
        // Issue #41: `output_schema_for_aggregate` (HAVING resolution) and the
        // canonical `QueryExpr::output_schema_in` must produce identical schemas
        // for the same aggregate. Before the dedup this diverged on a per-series
        // reduction — the HAVING mirror lacked the per-series branch and would
        // collapse `[ts, value]` to a single `rate` column.
        use crate::pre_asap::query_expr::Source;
        use std::time::Duration;

        let leaf_schema = Schema::with_time_index(
            vec![
                Column::new("ts", DataType::Timestamp, false),
                Column::new("value", DataType::Float64, false),
            ],
            0,
            vec![],
        );
        let scan = QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: leaf_schema.clone(),
        };
        // Aggregate{ reduction: PerEntity, [Rate], child: TimeRange{ Scan } } —
        // a per-series reduction (label-preserving).
        let agg = QueryExpr::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Rate],
            output_names: vec![],
            having: None,
            child: Rc::new(QueryExpr::TimeRange {
                range: Duration::from_secs(300),
                child: Rc::new(scan),
            }),
        };
        let canonical = agg.output_schema().expect("canonical schema");

        // The HAVING-resolution derivation gets only the input schema (the
        // TimeRange passes the leaf schema through).
        let having_side =
            output_schema_for_aggregate(&leaf_schema, &GroupKeys::none(), &[AggIntent::Rate], &[])
                .unwrap();

        assert_eq!(
            canonical, having_side,
            "the two aggregate-schema derivations must agree (issue #41)"
        );
        // Sanity: it really is the label-preserving per-series shape, not `[rate]`.
        assert!(having_side.columns.iter().any(|c| c.name == "value"));
        assert!(having_side.time_index.is_some());
    }
}
