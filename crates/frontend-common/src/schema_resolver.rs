//! The **SchemaResolver** — name resolution as an explicit pass.
//!
//! [`SchemaResolver::resolve_schema`] produces the complete, self-contained
//! [`Schema`] every `ColumnId` in a schemaless leaf's scope indexes into, so
//! positional resolution in [`resolve`](crate::resolve) is total.
//!
//! The default [`UsageDerivedCatalog`] knows nothing — every schema is derived
//! purely from the query's own usage. That is the honest state for the
//! observability domain (metric label sets are open-ended). A registry-backed
//! `SchemaCatalog` is future work; only the catalog impl swaps when it lands.

use asap_types::pre_asap::{AggIntent, ColumnRef, DataType, Field, GroupKeys, Reduction, Schema};

use crate::unresolved::{UnresolvedOp, UnresolvedScalar};

/// The DB / source-schema metadata source — resolves a source (metric /
/// table) name to its known columns. Distinct from `Scan.schema`, which is
/// the *resolved* binding schema this feeds. Even a registry-backed PromQL
/// catalog yields an **open** schema: a metric's labels are per-series and
/// time-varying, so the registry is a superset hint, not a per-row contract.
pub trait SchemaCatalog {
    /// Columns known for `source`. `None` when unknown — the resolver then
    /// falls back to a usage-derived column set.
    fn columns_for(&self, source: &str) -> Option<Vec<Field>>;
}

/// The default catalog: knows nothing.
pub struct UsageDerivedCatalog;

impl SchemaCatalog for UsageDerivedCatalog {
    fn columns_for(&self, _source: &str) -> Option<Vec<Field>> {
        None
    }
}

/// The explicit name-resolution pass.
pub struct SchemaResolver<C: SchemaCatalog = UsageDerivedCatalog> {
    catalog: C,
}

impl Default for SchemaResolver<UsageDerivedCatalog> {
    fn default() -> Self {
        Self::new()
    }
}

impl SchemaResolver<UsageDerivedCatalog> {
    pub fn new() -> Self {
        Self {
            catalog: UsageDerivedCatalog,
        }
    }
}

impl<C: SchemaCatalog> SchemaResolver<C> {
    pub fn with_catalog(catalog: C) -> Self {
        Self { catalog }
    }

    /// The complete [`Schema`] in scope for a query rooted at `tree`: the
    /// time axis, the synthetic `value` column, and one column per distinct
    /// name referenced anywhere in the tree.
    pub fn resolve_schema(&self, tree: &UnresolvedOp) -> Schema {
        self.resolve_schema_with_inherited(tree, &[])
    }

    /// Like [`resolve_schema`](Self::resolve_schema), but also seeds
    /// `inherited` label names referenced by an **enclosing** scope rather
    /// than by `tree` itself. This is how an independently-bound `BinaryOp`
    /// side still sees an outer aggregate's group keys — the `__name__` /
    /// `job` in `sum by (__name__)(a or b)`, which appear in neither side's
    /// own matchers (issue #52).
    pub fn resolve_schema_with_inherited(
        &self,
        tree: &UnresolvedOp,
        inherited: &[String],
    ) -> Schema {
        let mut columns: Vec<Field> = leftmost_scan_name(tree)
            .and_then(|name| self.catalog.columns_for(name))
            .unwrap_or_else(default_leaf_columns);

        // Ensure the (ts, value) floor is present.
        for floor in default_leaf_columns() {
            if !columns.iter().any(|c| c.name == floor.name) {
                columns.push(floor);
            }
        }

        // One column per referenced-but-unknown name, plus the inherited ones.
        let referenced = collect_referenced_columns(tree);
        for name in referenced.iter().chain(inherited) {
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

/// The leftmost `Scan`'s source name, following the relational skeleton only
/// (never the operators referenced from scalar positions: those are bound in
/// their own scope).
fn leftmost_scan_name(tree: &UnresolvedOp) -> Option<&str> {
    use asap_types::pre_asap::Source;
    use UnresolvedOp as U;
    match tree {
        U::Scan { source, .. } => Some(match source {
            Source::TimeSeries { metric } => metric.as_str(),
            Source::Table { table_ref } => table_ref.as_str(),
        }),
        U::Values { .. } | U::ScalarBridge(_) | U::PromqlVectorFromScalar(_) => None,
        U::PromqlRelabel { child, .. }
        | U::PromqlInfoEnrich { child, .. }
        | U::PromqlSeriesSample { child, .. }
        | U::Filter { child, .. }
        | U::Project { child, .. }
        | U::Aggregate { child, .. }
        | U::Dedup { child, .. }
        | U::Sort { child, .. }
        | U::Limit { child, .. }
        | U::PromqlSubquery { child, .. }
        | U::TimeRange { child, .. }
        | U::TimeShift { child, .. }
        | U::SQLWindowFunc { child, .. } => leftmost_scan_name(child),
        U::Concat { children, .. } => children.first().and_then(|c| leftmost_scan_name(c)),
        U::Join { left, .. } | U::SetOp { left, .. } | U::BinaryOp { lhs: left, .. } => {
            leftmost_scan_name(left)
        }
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
pub fn collect_referenced_columns(tree: &UnresolvedOp) -> Vec<String> {
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
            U::ScalarBridge(inner) | U::PromqlVectorFromScalar(inner) => named(inner, out),
            U::PromqlInfoEnrich { child, .. }
            | U::PromqlSubquery { child, .. }
            | U::TimeRange { child, .. }
            | U::TimeShift { child, .. } => walk(child, out),
            U::Concat {
                children,
                discriminator_unique_key,
            } => {
                // An own-field `ColumnRef` must be seeded like `Dedup.cols`, or
                // a discriminator column referenced nowhere else in the tree is
                // absent from the fallback schema and fails `NotFound` later.
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

    use asap_types::pre_asap::{AggIntent, Reduction, Source};

    use super::*;
    use crate::unresolved::UnresolvedSortKey;

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
            having: None,
            child: Rc::new(src("m")),
        };
        assert_eq!(collect_referenced_columns(&tree), vec!["x", "y"]);
        let schema = SchemaResolver::new().resolve_schema(&tree);
        assert!(schema.column_id("x").is_some());
        assert!(schema.column_id("y").is_some());
    }

    // A bare source gets exactly the (ts, value) floor.
    #[test]
    fn bare_source_yields_ts_value_floor() {
        let schema = SchemaResolver::new().resolve_schema(&src("m"));
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
        let schema = SchemaResolver::new().resolve_schema(&tree);
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
        let schema = SchemaResolver::new().resolve_schema(&tree);
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
        let schema = SchemaResolver::new().resolve_schema(&tree);
        assert!(schema.column_id("phi").is_some(), "discriminator seeded");
        assert!(schema.column_id("host").is_some(), "inner_key seeded");
    }

    // Inherited names are seeded alongside the tree's own references; plain
    // `resolve_schema` does not conjure them (issue #52).
    #[test]
    fn inherited_names_are_seeded_alongside_referenced() {
        let schema =
            SchemaResolver::new().resolve_schema_with_inherited(&src("m"), &["__name__".into()]);
        assert!(schema.column_id("__name__").is_some());
        let plain = SchemaResolver::new().resolve_schema(&src("m"));
        assert!(plain.column_id("__name__").is_none());
    }

    // A catalog-known source supplies its base columns, typed as the catalog says.
    #[test]
    fn custom_catalog_supplies_base_columns() {
        struct FixedCatalog;
        impl SchemaCatalog for FixedCatalog {
            fn columns_for(&self, source: &str) -> Option<Vec<Field>> {
                (source == "known").then(|| {
                    vec![
                        Field::plain("ts", DataType::Timestamp, false),
                        Field::plain("value", DataType::Float64, false),
                        Field::plain("datacenter", DataType::Utf8, false),
                    ]
                })
            }
        }
        let schema = SchemaResolver::with_catalog(FixedCatalog).resolve_schema(&src("known"));
        let dc = schema
            .column_id("datacenter")
            .and_then(|id| schema.fields.get(id));
        assert!(matches!(dc, Some(c) if !c.nullable));
    }
}
