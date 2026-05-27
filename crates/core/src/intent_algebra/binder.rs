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

use crate::intent_algebra::expr_ir::ColumnRef;
use crate::intent_algebra::expr_ir::L2Expr;
use crate::intent_algebra::relational::QueryExpr as LQueryExpr;
use crate::intent_algebra::schema::{Column, DataType, Schema};

/// The DB / source-schema metadata source — resolves a source (metric /
/// table) name to its known columns.
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

        // Append one column per referenced-but-unknown name (group keys etc.).
        for name in collect_referenced_columns(tree) {
            if !columns.iter().any(|c| c.name == name) {
                columns.push(Column::new(name, DataType::Utf8, true));
            }
        }

        let time_index = columns.iter().position(|c| c.name == "ts");
        Schema {
            columns,
            time_index,
            unique_keys: Vec::new(),
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
fn collect_referenced_columns(tree: &LQueryExpr) -> Vec<String> {
    fn named(expr: &L2Expr, out: &mut Vec<String>) {
        for c in expr.columns_referenced() {
            if let ColumnRef::Named(n) = c {
                out.push(n.clone());
            }
        }
    }
    let mut out: Vec<String> = Vec::new();
    tree.walk(&mut |node| match node {
        LQueryExpr::Aggregate { keys, having, .. } => {
            out.extend(keys.iter().cloned());
            if let Some(h) = having {
                named(h, &mut out);
            }
        }
        LQueryExpr::TopK { by, .. } => out.extend(by.iter().cloned()),
        LQueryExpr::Partition { keys, .. } => out.extend(keys.keys().iter().cloned()),
        LQueryExpr::Filter { pred, .. } => named(pred, &mut out),
        LQueryExpr::Project { cols, .. } => {
            for item in cols {
                named(&item.expr, &mut out);
            }
        }
        LQueryExpr::Sort { keys, .. } => {
            for k in keys {
                named(&k.expr, &mut out);
            }
        }
        LQueryExpr::Join { pred: Some(p), .. } => named(p, &mut out),
        _ => {}
    });
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent_algebra::query_expr::PartitionKeys;
    use crate::intent_algebra::relational::{QueryExpr as LQueryExpr, SourceSpec};

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
    fn partition_keys_land_in_schema() {
        let tree = LQueryExpr::Partition {
            keys: PartitionKeys::By(vec!["host".into()]),
            input: Box::new(src("hits")),
        };
        let schema = Binder::new().bind(&tree);
        assert!(schema.column_id("host").is_some());
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
