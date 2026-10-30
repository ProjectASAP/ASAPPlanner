//! Name resolution as an explicit pass.
//!
//! [`resolve_schema_with_inherited`] produces the complete, self-contained
//! [`Schema`] every `ColumnId` in a schemaless leaf's scope indexes into, so
//! positional resolution in [`resolve`](crate::resolve) is total.
//!
//! Every schema is derived purely from the query's own usage. That is the
//! honest state for the observability domain (metric label sets are
//! open-ended). A registry-backed source catalog is tracked in #1112.

use asap_types::ir::operator::{AggIntent, GroupKeys, Reduction};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, Schema};

use crate::unresolved::{UnresolvedOp, UnresolvedScalar};

/// The complete [`Schema`] in scope for a query rooted at `tree`: the time
/// axis, the synthetic `value` column, and one column per distinct name
/// referenced anywhere in the tree, plus the `inherited` label names
/// referenced by an **enclosing** scope rather than by `tree` itself. This is
/// how an independently-bound `BinaryOp` side still sees an outer aggregate's
/// group keys — the `__name__` / `job` in `sum by (__name__)(a or b)`, which
/// appear in neither side's own matchers (issue #52).
pub(crate) fn resolve_schema_with_inherited(tree: &UnresolvedOp, inherited: &[String]) -> Schema {
    let mut columns = default_leaf_columns();
    for name in collect_referenced_columns(tree).iter().chain(inherited) {
        if !columns.iter().any(|c| c.name == *name) {
            columns.push(Field::plain(name.clone(), DataType::Utf8, true));
        }
    }
    let time_index = columns.iter().position(|c| c.name == "ts");
    Schema {
        fields: columns,
        time_index,
        unique_keys: Vec::new(),
        // Usage-derived (schemaless PromQL): the metric's full label set is
        // open and runtime-only, so this lists only what the query references.
        closed: false,
    }
}

/// The conventional PromQL leaf shape: `(ts: Timestamp, value: Float64)`.
fn default_leaf_columns() -> Vec<Field> {
    vec![
        Field::plain("ts", DataType::Timestamp, false),
        Field::plain("value", DataType::Float64, false),
    ]
}

/// Push a `ColumnRef`'s bare name (the schema-seedable identifier). `Qualified`
/// collapses to its `name`; `SampleValue`/`Wildcard` carry no name.
fn push_ref_name(c: &ColumnRef, out: &mut Vec<String>) {
    match c {
        ColumnRef::Named(n) => out.push(n.clone()),
        ColumnRef::Qualified { name, .. } => out.push(name.clone()),
        ColumnRef::SampleValue | ColumnRef::Wildcard => {}
    }
}

/// Every distinct column name referenced anywhere in `tree` that resolves
/// positionally — every place a front end puts a name-based reference:
/// `Scan.predicates`, `Aggregate`'s `reduction`/`having`/per-measure `col`,
/// `Dedup.cols`, `PromqlSeriesSample.by`, `Filter.pred`, `Project.cols`,
/// `Sort`/`Limit`/`SQLWindowFunc` keys, `Join.pred`, `PromqlRelabel.value`,
/// `Concat.discriminator_unique_key`. Operators referenced from scalar
/// positions (`scalar(v)`, subqueries) are walked too, as the old
/// `PromqlScalarFromVector` operator child was. Sorted and deduplicated.
pub(crate) fn collect_referenced_columns(tree: &UnresolvedOp) -> Vec<String> {
    use UnresolvedOp as U;
    fn named(expr: &UnresolvedScalar, out: &mut Vec<String>) {
        for c in expr.columns_referenced() {
            push_ref_name(c, out);
        }
        for op in expr.operator_refs() {
            walk(op, out);
        }
    }
    fn group_keys(g: &GroupKeys<ColumnRef>, out: &mut Vec<String>) {
        g.keys().iter().for_each(|k| push_ref_name(k, out));
    }
    fn measure_cols(measures: &[AggIntent<ColumnRef>], out: &mut Vec<String>) {
        for m in measures {
            for c in m.input_cols() {
                push_ref_name(&c, out);
            }
        }
    }
    fn walk(node: &UnresolvedOp, out: &mut Vec<String>) {
        match node {
            U::Scan { predicates, .. } => {
                for p in predicates {
                    named(&p.0, out);
                }
            }
            U::Values { rows, .. } => {
                for e in rows.iter().flatten() {
                    named(e, out);
                }
            }
            U::Aggregate {
                reduction,
                measures,
                having,
                child,
                ..
            } => {
                if let Reduction::Reduce(by) = reduction {
                    group_keys(by, out);
                }
                measure_cols(measures, out);
                if let Some(h) = having {
                    named(&h.0, out);
                }
                walk(child, out);
            }
            U::Dedup { cols, child } => {
                cols.iter().for_each(|c| push_ref_name(c, out));
                walk(child, out);
            }
            U::PromqlSeriesSample { by, child, .. } => {
                group_keys(by, out);
                walk(child, out);
            }
            U::Filter { pred, child } => {
                named(&pred.0, out);
                walk(child, out);
            }
            U::Project { cols, child, .. } => {
                for item in cols {
                    named(&item.expr, out);
                }
                walk(child, out);
            }
            U::Sort {
                keys,
                partition_by,
                child,
            } => {
                for k in keys {
                    named(&k.expr, out);
                }
                group_keys(partition_by, out);
                walk(child, out);
            }
            U::Limit {
                partition_by,
                child,
                ..
            } => {
                group_keys(partition_by, out);
                walk(child, out);
            }
            U::SQLWindowFunc {
                args,
                partition_by,
                order_by,
                child,
                ..
            } => {
                for a in args {
                    named(a, out);
                }
                group_keys(partition_by, out);
                for k in order_by {
                    named(&k.expr, out);
                }
                walk(child, out);
            }
            U::PromqlRelabel { value, child, .. } => {
                named(value, out);
                walk(child, out);
            }
            U::Join {
                pred, left, right, ..
            } => {
                named(&pred.0, out);
                walk(left, out);
                walk(right, out);
            }
            U::PromqlVectorFromScalar(inner) => named(inner, out),
            U::PromqlMap { child, .. }
            | U::PromqlScalarOp { child, .. }
            | U::PromqlInfoEnrich { child, .. }
            | U::PromqlSubquery { child, .. }
            | U::TimeRange { child, .. }
            | U::TimeShift { child, .. } => walk(child, out),
            U::Concat {
                children,
                discriminator_unique_key,
            } => {
                // An own-field `ColumnRef` must be seeded like `Dedup.cols`, or
                // a discriminator column referenced nowhere else in the tree is
                // absent from the usage-derived schema and fails `NotFound` later.
                if let Some(key) = discriminator_unique_key {
                    push_ref_name(key.discriminator(), out);
                    key.inner_key().iter().for_each(|c| push_ref_name(c, out));
                }
                children.iter().for_each(|c| walk(c, out));
            }
            U::SetOp { left, right, .. } => {
                walk(left, out);
                walk(right, out);
            }
            U::BinaryOp { lhs, rhs, .. } => {
                walk(lhs, out);
                walk(rhs, out);
            }
        }
    }
    let mut out: Vec<String> = Vec::new();
    walk(tree, &mut out);
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use asap_types::ir::operator::{AggIntent, Reduction, Source};

    use super::*;
    use crate::unresolved::UnresolvedSortKey;

    fn resolve_schema(tree: &UnresolvedOp) -> Schema {
        resolve_schema_with_inherited(tree, &[])
    }

    fn src(name: &str) -> UnresolvedOp {
        UnresolvedOp::Scan {
            source: Source::TimeSeries {
                metric: name.into(),
            },
            predicates: vec![],
            schema: None,
        }
    }

    // Both correlation inputs seed the usage-derived schema.
    #[test]
    fn pearson_corr_inputs_seed_usage_derived_schema() {
        let tree = UnresolvedOp::Aggregate {
            reduction: Reduction::by(vec![]),
            measures: vec![AggIntent::PearsonCorr {
                left: ColumnRef::Named("x".into()),
                right: ColumnRef::Named("y".into()),
            }],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: Rc::new(src("m")),
        };
        assert_eq!(collect_referenced_columns(&tree), vec!["x", "y"]);
        let schema = resolve_schema(&tree);
        assert!(schema.column_id("x").is_some());
        assert!(schema.column_id("y").is_some());
    }

    // A bare source gets exactly the (ts, value) floor.
    #[test]
    fn bare_source_yields_ts_value_floor() {
        let schema = resolve_schema(&src("m"));
        assert_eq!(schema.fields.len(), 2);
        assert_eq!(schema.fields[0].name, "ts");
        assert_eq!(schema.fields[1].name, "value");
        assert_eq!(schema.time_index, Some(0));
    }

    // Per-group ranking keys (`topk by (host)` → `Sort.partition_by`) are
    // seeded into the usage-derived leaf so they resolve positionally.
    #[test]
    fn sort_partition_keys_land_in_schema() {
        let tree = UnresolvedOp::Sort {
            keys: vec![UnresolvedSortKey {
                expr: UnresolvedScalar::Column(ColumnRef::SampleValue),
                ascending: false,
                nulls_first: false,
            }],
            partition_by: GroupKeys::by(vec![ColumnRef::Named("host".into())]),
            child: Rc::new(src("hits")),
        };
        let schema = resolve_schema(&tree);
        assert!(schema.column_id("host").is_some());
    }

    // `Limit.partition_by` (PromQL `topk by (..)`) is seeded like `Sort`'s.
    #[test]
    fn limit_partition_keys_land_in_schema() {
        let tree = UnresolvedOp::Limit {
            n: Some(3),
            offset: 0,
            partition_by: GroupKeys::by(vec![ColumnRef::Named("host".into())]),
            child: Rc::new(src("hits")),
        };
        let schema = resolve_schema(&tree);
        assert!(schema.column_id("host").is_some());
    }

    // A `Concat`'s discriminator key columns, even ones referenced nowhere
    // else, are seeded like `Dedup.cols` (issue #228 review).
    #[test]
    fn concat_discriminator_key_is_seeded_into_the_resolver_schema() {
        let tree = UnresolvedOp::concat_with_discriminator(
            vec![src("m")],
            ColumnRef::Named("phi".into()),
            vec![ColumnRef::Named("host".into())],
        );
        let schema = resolve_schema(&tree);
        assert!(schema.column_id("phi").is_some(), "discriminator seeded");
        assert!(schema.column_id("host").is_some(), "inner_key seeded");
    }

    // Inherited names are seeded alongside the tree's own references; plain
    // `resolve_schema` does not conjure them (issue #52).
    #[test]
    fn inherited_names_are_seeded_alongside_referenced() {
        let schema = resolve_schema_with_inherited(&src("m"), &["__name__".into()]);
        assert!(schema.column_id("__name__").is_some());
        let plain = resolve_schema(&src("m"));
        assert!(plain.column_id("__name__").is_none());
    }
}
