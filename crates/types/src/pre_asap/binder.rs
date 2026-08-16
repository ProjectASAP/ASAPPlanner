//! The L3 **Binder** — name resolution as an explicit pass.
//!
//! [`Binder::bind`] produces the complete, self-contained [`Schema`] every
//! `ColumnId` in the converted canonical tree indexes into. The converter
//! ([`super::lower::convert`]) then becomes purely structural: it threads the
//! Binder's schema and positional resolution downstream is **total**.
//!
//! The default [`UsageDerivedCatalog`] knows nothing — every schema is derived
//! purely from the query's own usage. That is the honest state for the
//! observability domain (metric label sets are open-ended). A registry-backed
//! `SchemaCatalog` is future work; the `Binder` pass does not change when it
//! lands, only the catalog impl swaps.

use super::expr_ir::ColumnRef;
use super::expr_ir::L2Expr;
use super::query_expr::L2QueryExpr;
use super::relational::QueryExpr as LQueryExpr;
use super::schema::{Column, DataType, Schema};

/// The DB / source-schema metadata source — resolves a source (metric /
/// table) name to its known columns.
/// Source of truth for a source's columns — the "catalog". `SqlCatalog` backs
/// it for SQL; PromQL uses [`UsageDerivedCatalog`] (returns `None`) until a
/// registry-backed impl (returning a metric's known label set) drops in here.
/// Distinct from `Scan.schema`, which is the *resolved* binding schema this
/// feeds — the catalog is the input, the schema is the result. Even a
/// registry-backed PromQL catalog yields an **open** schema
/// ([`Schema::closed`](asap_types::pre_asap::schema::Schema::closed) `= false`): a metric's
/// labels are per-series and time-varying, so the registry is a superset hint,
/// not a per-row contract.
pub trait SchemaCatalog {
    /// Columns known for `source`. `None` when unknown — the [`Binder`] then
    /// falls back to a usage-derived column set.
    fn columns_for(&self, source: &str) -> Option<Vec<Column>>;
}

/// The default catalog: knows nothing. Every schema the [`Binder`] produces
/// is derived purely from the query's own usage.
pub struct UsageDerivedCatalog;

impl SchemaCatalog for UsageDerivedCatalog {
    fn columns_for(&self, _source: &str) -> Option<Vec<Column>> {
        None
    }
}

/// The L3 Binder — the explicit name-resolution pass.
pub struct Binder<C: SchemaCatalog = UsageDerivedCatalog> {
    catalog: C,
}

impl Default for Binder<UsageDerivedCatalog> {
    fn default() -> Self {
        Self::new()
    }
}

impl Binder<UsageDerivedCatalog> {
    pub fn new() -> Self {
        Self {
            catalog: UsageDerivedCatalog,
        }
    }
}

impl<C: SchemaCatalog> Binder<C> {
    pub fn with_catalog(catalog: C) -> Self {
        Self { catalog }
    }

    /// Resolve the complete [`Schema`] in scope for a query rooted at `tree`.
    ///
    /// Contains the time axis, the synthetic `value` column, and one column
    /// per distinct name referenced anywhere in the tree — so positional
    /// `ColumnId` resolution downstream is total.
    pub fn bind(&self, tree: &LQueryExpr) -> Schema {
        self.bind_with_inherited(tree, &[])
    }

    /// Like [`bind`](Self::bind), but also seeds `inherited` label names that are
    /// referenced by an **enclosing** scope rather than by `tree` itself. This is
    /// how an independently-bound `BinaryOp` side (each side re-binds against its
    /// own sub-tree) still sees an outer aggregate's group keys — e.g. the
    /// `__name__` / `job` in `sum by (__name__)(a or b)`, which appear in neither
    /// side's own matchers (issue #52).
    pub fn bind_with_inherited(&self, tree: &LQueryExpr, inherited: &[String]) -> Schema {
        let mut columns: Vec<Column> = tree
            .source_name()
            .and_then(|name| self.catalog.columns_for(name))
            .unwrap_or_else(default_leaf_columns);

        // Ensure the (ts, value) floor is present.
        for floor in default_leaf_columns() {
            if !columns.iter().any(|c| c.name == floor.name) {
                columns.push(floor);
            }
        }

        // Append one column per referenced-but-unknown name (group keys etc.),
        // plus any inherited-from-enclosing-scope names.
        let referenced = collect_referenced_columns(tree);
        for name in referenced.iter().chain(inherited) {
            if !columns.iter().any(|c| c.name == *name) {
                columns.push(Column::new(name.clone(), DataType::Utf8, true));
            }
        }

        let time_index = columns.iter().position(|c| c.name == "ts");
        Schema {
            columns,
            time_index,
            unique_keys: Vec::new(),
            // Usage-derived (schemaless PromQL): the metric's full label set is
            // open and runtime-only, so this lists only what the query references.
            closed: false,
        }
    }

    /// Like [`bind`](Self::bind)/[`bind_with_inherited`](Self::bind_with_inherited),
    /// but for a front end that emits the canonical
    /// [`QueryExpr`](super::query_expr::QueryExpr) shape directly
    /// (`L2QueryExpr` = `QueryExpr<ColumnRef>`, unresolved) instead of the
    /// legacy [`relational`](super::relational) tree — see
    /// [`resolve`](super::resolve). Front ends migrating off
    /// `relational`/`convert_root` (issue #179) use this one; every rule above
    /// (the `(ts, value)` floor, usage-derived label columns, inherited names
    /// for an independently-bound `BinaryOp` side) applies identically.
    pub fn bind_query_expr(&self, tree: &L2QueryExpr) -> Schema {
        self.bind_query_expr_with_inherited(tree, &[])
    }

    /// [`bind_query_expr`](Self::bind_query_expr) with inherited names — the
    /// canonical-tree counterpart to
    /// [`bind_with_inherited`](Self::bind_with_inherited).
    pub fn bind_query_expr_with_inherited(&self, tree: &L2QueryExpr, inherited: &[String]) -> Schema {
        let mut columns: Vec<Column> = leftmost_scan_name(tree)
            .and_then(|name| self.catalog.columns_for(name))
            .unwrap_or_else(default_leaf_columns);

        for floor in default_leaf_columns() {
            if !columns.iter().any(|c| c.name == floor.name) {
                columns.push(floor);
            }
        }

        let referenced = collect_referenced_columns_qe(tree);
        for name in referenced.iter().chain(inherited) {
            if !columns.iter().any(|c| c.name == *name) {
                columns.push(Column::new(name.clone(), DataType::Utf8, true));
            }
        }

        let time_index = columns.iter().position(|c| c.name == "ts");
        Schema {
            columns,
            time_index,
            unique_keys: Vec::new(),
            closed: false,
        }
    }
}

/// The conventional PromQL leaf shape: `(ts: Timestamp, value: Float64)`.
fn default_leaf_columns() -> Vec<Column> {
    vec![
        Column::new("ts", DataType::Timestamp, false),
        Column::new("value", DataType::Float64, false),
    ]
}

/// Collect every distinct column name the converter resolves positionally:
/// group keys (`Aggregate.keys`, `TopK.by`, `Partition.keys`) **and** the
/// columns referenced by name in filter / having / project / sort / join
/// expressions (e.g. a PromQL label matcher `m{env="prod"}` references `env`).
/// The Binder seeds these into the usage-derived leaf so positional resolution
/// downstream is total.
/// Push a `ColumnRef`'s bare name (the schema-seedable identifier). `Qualified`
/// collapses to its `name`; `SampleValue`/`Wildcard` carry no name.
fn push_ref_name(c: &ColumnRef, out: &mut Vec<String>) {
    match c {
        ColumnRef::Named(n) => out.push(n.clone()),
        ColumnRef::Qualified { name, .. } => out.push(name.clone()),
        ColumnRef::SampleValue | ColumnRef::Wildcard => {}
    }
}

pub(crate) fn collect_referenced_columns(tree: &LQueryExpr) -> Vec<String> {
    fn named(expr: &L2Expr, out: &mut Vec<String>) {
        for c in expr.columns_referenced() {
            push_ref_name(c, out);
        }
    }
    let mut out: Vec<String> = Vec::new();
    tree.walk(&mut |node| match node {
        LQueryExpr::Aggregate { keys, having, .. } => {
            keys.iter().for_each(|k| push_ref_name(k, &mut out));
            if let Some(h) = having {
                named(h, &mut out);
            }
        }
        LQueryExpr::TopK { by, .. } => by.iter().for_each(|k| push_ref_name(k, &mut out)),
        // `limitk`/`limit_ratio` grouping keys resolve positionally (issue #86).
        LQueryExpr::Sample { keys, .. } => keys.iter().for_each(|k| push_ref_name(k, &mut out)),
        LQueryExpr::Filter { pred, .. } => named(pred, &mut out),
        LQueryExpr::Project { cols, .. } => {
            for item in cols {
                named(&item.expr, &mut out);
            }
        }
        LQueryExpr::Sort {
            keys, partition_by, ..
        } => {
            for k in keys {
                named(&k.expr, &mut out);
            }
            // Per-group ranking keys (`topk by (…)`) must be seeded into the
            // leaf schema so they resolve positionally to `Sort.partition_by`.
            partition_by.iter().for_each(|k| push_ref_name(k, &mut out));
        }
        LQueryExpr::Join { pred: Some(p), .. } => named(p, &mut out),
        // A relabel's source labels are referenced by name inside `value`
        // (`label_replace(instance, …)`); seed them so they resolve
        // positionally. `dst` is an output — only seeded if `value` also reads
        // it (an in-place `label_replace` on the same label).
        LQueryExpr::Relabel { value, .. } => named(value, &mut out),
        _ => {}
    });
    out.sort();
    out.dedup();
    out
}

/// The leftmost `Scan`'s source name in a canonical (`L2QueryExpr`) tree —
/// the [`collect_referenced_columns_qe`] counterpart to
/// [`relational::QueryExpr::source_name`](super::relational::QueryExpr::source_name),
/// which the legacy tree carries as a method because it owns a dedicated
/// `Source(SourceSpec)` leaf; the canonical tree's `Scan` leaf needs this
/// walk written out instead.
fn leftmost_scan_name(tree: &L2QueryExpr) -> Option<&str> {
    use L2QueryExpr as QE;
    match tree {
        QE::Scan { source, .. } => Some(match source {
            super::query_expr::Source::TimeSeries { metric } => metric.as_str(),
            super::query_expr::Source::Table { table_ref } => table_ref.as_str(),
        }),
        QE::Scalar(_) | QE::EvalTime => None,
        QE::VectorFromScalar(child) | QE::ScalarFromVector(child) => leftmost_scan_name(child),
        QE::Relabel { child, .. }
        | QE::InfoJoin { child, .. }
        | QE::Sample { child, .. }
        | QE::Filter { child, .. }
        | QE::Project { child, .. }
        | QE::Aggregate { child, .. }
        | QE::Distinct { child, .. }
        | QE::Sort { child, .. }
        | QE::Limit { child, .. }
        | QE::Subquery { child, .. }
        | QE::TimeRange { child, .. }
        | QE::TimeShift { child, .. }
        | QE::WindowFunc { child, .. } => leftmost_scan_name(child),
        QE::Merge { children } => children.first().and_then(leftmost_scan_name),
        QE::Join { left, .. } | QE::SetOp { left, .. } | QE::BinaryOp { lhs: left, .. } => {
            leftmost_scan_name(left)
        }
    }
}

/// [`collect_referenced_columns`]'s counterpart for a canonical
/// (`L2QueryExpr`) tree — every distinct name a front end constructing
/// [`QueryExpr<ColumnRef>`](super::query_expr::QueryExpr) directly needs
/// seeded into the usage-derived leaf: `Scan.predicates`, `Aggregate`'s
/// `reduction`/`having`/per-measure `col`, `Distinct.cols`, `Sample.by`,
/// `Filter.pred`, `Project.cols`, `Sort.keys`/`partition_by`,
/// `WindowFunc.args`/`partition_by`/`order_by`, `Join.pred`, `Relabel.value`.
pub(crate) fn collect_referenced_columns_qe(tree: &L2QueryExpr) -> Vec<String> {
    use L2QueryExpr as QE;
    fn named(expr: &L2Expr, out: &mut Vec<String>) {
        for c in expr.columns_referenced() {
            push_ref_name(c, out);
        }
    }
    fn opt_ref(c: &Option<ColumnRef>, out: &mut Vec<String>) {
        if let Some(c) = c {
            push_ref_name(c, out);
        }
    }
    fn group_keys(g: &super::query_expr::GroupKeys<ColumnRef>, out: &mut Vec<String>) {
        g.keys().iter().for_each(|k| push_ref_name(k, out));
    }
    fn measure_cols(measures: &[super::agg_intent::AggIntent<ColumnRef>], out: &mut Vec<String>) {
        for m in measures {
            opt_ref(&m.input_col(), out);
        }
    }
    fn walk(node: &L2QueryExpr, out: &mut Vec<String>) {
        match node {
            QE::Scan { predicates, .. } => {
                for super::query_expr::Predicate(p) in predicates {
                    named(p, out);
                }
            }
            QE::Aggregate {
                reduction,
                measures,
                having,
                child,
                ..
            } => {
                if let super::query_expr::Reduction::Reduce(by) = reduction {
                    group_keys(by, out);
                }
                measure_cols(measures, out);
                if let Some(super::query_expr::Predicate(h)) = having {
                    named(h, out);
                }
                walk(child, out);
            }
            QE::Distinct { cols, child } => {
                cols.iter().for_each(|c| push_ref_name(c, out));
                walk(child, out);
            }
            QE::Sample { by, child, .. } => {
                group_keys(by, out);
                walk(child, out);
            }
            QE::Filter { pred, child } => {
                named(&pred.0, out);
                walk(child, out);
            }
            QE::Project { cols, child, .. } => {
                for item in cols {
                    named(&item.expr, out);
                }
                walk(child, out);
            }
            QE::Sort {
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
            QE::WindowFunc {
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
            QE::Relabel { value, child, .. } => {
                named(value, out);
                walk(child, out);
            }
            QE::Join {
                pred, left, right, ..
            } => {
                named(&pred.0, out);
                walk(left, out);
                walk(right, out);
            }
            QE::Scalar(_) | QE::EvalTime => {}
            QE::VectorFromScalar(child) | QE::ScalarFromVector(child) => walk(child, out),
            QE::InfoJoin { child, .. }
            | QE::Limit { child, .. }
            | QE::Subquery { child, .. }
            | QE::TimeRange { child, .. }
            | QE::TimeShift { child, .. } => walk(child, out),
            QE::Merge { children } => children.iter().for_each(|c| walk(c, out)),
            QE::SetOp { left, right, .. } => {
                walk(left, out);
                walk(right, out);
            }
            QE::BinaryOp { lhs, rhs, .. } => {
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
    use super::super::relational::{L2SortKey, QueryExpr as LQueryExpr, SourceSpec};
    use super::super::L2Expr;
    use super::*;

    fn src(name: &str) -> LQueryExpr {
        LQueryExpr::Source(SourceSpec::new(name))
    }

    #[test]
    fn bare_source_yields_ts_value_floor() {
        let schema = Binder::new().bind(&src("m"));
        assert_eq!(schema.columns.len(), 2);
        assert_eq!(schema.columns[0].name, "ts");
        assert_eq!(schema.columns[1].name, "value");
        assert_eq!(schema.time_index, Some(0));
    }

    #[test]
    fn sort_partition_keys_land_in_schema() {
        // Per-group ranking keys (`topk by (host)` → `Sort.partition_by`) must be
        // seeded into the usage-derived leaf so they resolve positionally.
        let tree = LQueryExpr::Sort {
            keys: vec![L2SortKey {
                expr: L2Expr::Column(ColumnRef::SampleValue),
                ascending: false,
                nulls_first: false,
            }],
            partition_by: vec![ColumnRef::Named("host".into())],
            input: Box::new(src("hits")),
        };
        let schema = Binder::new().bind(&tree);
        assert!(schema.column_id("host").is_some());
    }

    #[test]
    fn inherited_names_are_seeded_alongside_referenced() {
        // A `BinaryOp` side re-binds against its own sub-tree, but must still see
        // an enclosing aggregate's group key (`__name__` / `job`) that appears in
        // neither side's own matchers (issue #52). `bind_with_inherited` seeds it.
        let schema = Binder::new().bind_with_inherited(&src("m"), &["__name__".into()]);
        assert!(schema.column_id("__name__").is_some());
        // `bind` (no inheritance) does not conjure it.
        let plain = Binder::new().bind(&src("m"));
        assert!(plain.column_id("__name__").is_none());
    }

    #[test]
    fn custom_catalog_supplies_base_columns() {
        struct FixedCatalog;
        impl SchemaCatalog for FixedCatalog {
            fn columns_for(&self, source: &str) -> Option<Vec<Column>> {
                (source == "known").then(|| {
                    vec![
                        Column::new("ts", DataType::Timestamp, false),
                        Column::new("value", DataType::Float64, false),
                        Column::new("datacenter", DataType::Utf8, false),
                    ]
                })
            }
        }
        let schema = Binder::with_catalog(FixedCatalog).bind(&src("known"));
        let dc = schema
            .column_id("datacenter")
            .and_then(|id| schema.columns.get(id));
        assert!(matches!(dc, Some(c) if !c.nullable));
    }
}
