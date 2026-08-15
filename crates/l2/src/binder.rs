//! The L3 **Binder** — name resolution as an explicit pass.
//!
//! [`Binder::bind`] produces the complete, self-contained [`Schema`] every
//! `ColumnId` in the converted canonical tree indexes into. The converter
//! ([`crate::lower::convert`]) then becomes purely structural: it threads the
//! Binder's schema and positional resolution downstream is **total**.
//!
//! The default [`UsageDerivedCatalog`] knows nothing — every schema is derived
//! purely from the query's own usage. That is the honest state for the
//! observability domain (metric label sets are open-ended). A registry-backed
//! `SchemaCatalog` is future work; the `Binder` pass does not change when it
//! lands, only the catalog impl swaps.

use crate::relational::QueryExpr as LQueryExpr;
use asap_types::pre_asap::expr_ir::ColumnRef;
use asap_types::pre_asap::expr_ir::L2Expr;
use asap_types::pre_asap::schema::{Column, DataType, Schema};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relational::{L2SortKey, QueryExpr as LQueryExpr, SourceSpec};
    use asap_types::pre_asap::expr_ir::ColumnRef;
    use asap_types::pre_asap::L2Expr;

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
