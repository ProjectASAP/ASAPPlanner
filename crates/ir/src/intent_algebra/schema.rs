//! Layer 3 schema flow — every L3 edge carries a typed `Schema`.
//!
//! Per `control_plane/docs/design.md` §6 "Schema flow — every L3 edge carries
//! a typed schema". The DAG is type-checked: a node's output schema is a
//! function of its inputs and parameters and is verifiable independently
//! of the surrounding context.
//!
//! `Schema::unique_keys` is metadata for reuse-aware planning: a producer's
//! output can only be safely shared across consumers when its row identity
//! is provably stable across reads, which is what this field records.
//!
//! Single-query plans don't read this field; it lives here so the metadata
//! is available the moment workload-aware planning lands without requiring
//! an L3-wide schema change.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// Index into [`Schema::columns`] used everywhere a column position is
/// referenced (group-by keys, unique-key sets, the time axis index).
///
/// Aliased to `usize` to match `design.md`'s `Vec<Vec<usize>>` for
/// `unique_keys`. Kept as a named type so downstream code can pattern on
/// the intent ("this is a column position, not just any number").
pub type ColumnId = usize;

/// One column in a [`Schema`]. Mirrors `design.md` §6 `Field` —
/// `name + dtype + nullable`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Column {
    /// Column name as it appears in the producer's output. PromQL leaves
    /// produce label-name + the synthetic `value` / `timestamp` columns;
    /// SQL leaves carry their `information_schema` names.
    pub name: String,
    /// Column data type. Kept narrow at L3 (`Int64` / `Float64` / `Utf8`
    /// / `Bool` / `Timestamp`); `Sketch(...)` is an L4-only addition per
    /// design.md §6.4 and is intentionally absent here.
    pub dtype: DataType,
    /// Whether NULL values are allowed in this column. PromQL value
    /// columns are non-nullable; SQL columns inherit their DDL nullability.
    pub nullable: bool,
    /// Optional table/alias qualifier (SQL `t.col` / `t AS a` → `a`). Travels
    /// with the column through joins so a `ColumnRef::Qualified` can pick the
    /// right side when both carry the same `name`. `None` for PromQL labels and
    /// unqualified columns.
    #[serde(default)]
    pub table: Option<String>,
}

impl Column {
    /// An unqualified column (`table = None`).
    pub fn new(name: impl Into<String>, dtype: DataType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            dtype,
            nullable,
            table: None,
        }
    }

    /// This column re-qualified under `table` (e.g. by a `SubqueryAlias`).
    pub fn with_table(mut self, table: impl Into<String>) -> Self {
        self.table = Some(table.into());
        self
    }
}

/// L3 column data types. Deliberately narrow: no sketch state at this
/// layer (see `design.md` §6.4 for the L4 `DataType::Sketch(...)`
/// extension).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataType {
    /// 64-bit signed integer. Counter columns, group-cardinality outputs.
    Int64,
    /// 64-bit IEEE-754 float. Quantile / Avg / Sum-over-floats output.
    Float64,
    /// UTF-8 string. PromQL label values, SQL `VARCHAR` / `TEXT`.
    Utf8,
    /// Boolean — predicate output, `unless` / `and` / `or` PromQL ops.
    Bool,
    /// Wall-clock timestamp. PromQL leaves carry exactly one of these
    /// (the `time_index` column); SQL leaves may or may not.
    Timestamp,
}

/// Per-edge L3 schema. Flowing between any two L3 operators, on every
/// node's input and output.
///
/// `unique_keys` is metadata for reuse-aware planning: each inner `Vec<ColumnId>`
/// is a set of column indices that together uniquely identify rows. The
/// outer `Vec` allows multiple unique-key sets (primary key + another
/// unique constraint). Populated by per-node input/output spec —
/// `Aggregate { by, .. }` emits `unique_keys = [by]`; `Distinct { cols }`
/// adds `cols`; most other nodes pass through.
///
/// **Consumed by**: a future workload-level reuse pass (not yet shipped).
/// The single-query path, the `Bind*` rules, push-down, and L5 emitters do
/// not read this field.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Schema {
    /// Columns flowing on this edge, in positional order.
    pub columns: Vec<Column>,
    /// Index into `columns` for the time axis, if any. PromQL leaves
    /// always carry one; SQL leaves may or may not.
    #[serde(default)]
    pub time_index: Option<ColumnId>,
    /// Unique-key sets — each inner vec is a tuple of column indices
    /// that together uniquely identifies a row. Empty `Vec` means
    /// "no provable unique constraint" (the conservative default).
    #[serde(default)]
    pub unique_keys: Vec<Vec<ColumnId>>,
    /// Whether this schema **completely enumerates** the columns at this point.
    ///
    /// - `true` (**closed**): there are no columns beyond these — a catalog-backed
    ///   SQL source, or an output fully determined by an `Aggregate`/`Project`.
    /// - `false` (**open**): a dynamic / superset schema — the runtime row may
    ///   carry more columns than are listed (a schemaless PromQL leaf lists only
    ///   the `(ts, value)` floor + the labels the query references).
    ///
    /// This mirrors Apache Calcite's `DynamicRecordType` (schema-on-read): the
    /// schema starts open at a schemaless leaf and is **frozen to closed** by the
    /// first operator that fully determines its output columns (`Aggregate` /
    /// `Project`). Consumers needing completeness (label validation, full-output
    /// enumeration, cardinality for the cost model) must check this; positional
    /// resolution does not care. **Invariant: open ⇒ do not apply closed-world
    /// validation** (PromQL tolerates unknown labels). Defaults to `false` (open)
    /// — the conservative choice when completeness is unknown.
    #[serde(default)]
    pub closed: bool,
}

impl Schema {
    /// Construct a `Schema` from columns alone — no time index, no
    /// unique-key constraint. Used by `Scan` over a tabular source
    /// when the catalog supplies no primary-key metadata.
    pub fn new(columns: Vec<Column>) -> Self {
        Self {
            columns,
            time_index: None,
            unique_keys: Vec::new(),
            closed: false,
        }
    }

    /// Construct a `Scan`-style schema with explicit `time_index` +
    /// inferred unique keys (e.g. PromQL leaves: `[time_index, label_set]`).
    pub fn with_time_index(
        columns: Vec<Column>,
        time_index: ColumnId,
        unique_keys: Vec<Vec<ColumnId>>,
    ) -> Self {
        Self {
            columns,
            time_index: Some(time_index),
            unique_keys,
            closed: false,
        }
    }

    /// Look up a column by name (first match). `None` if not present.
    pub fn column_id(&self, name: &str) -> Option<ColumnId> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// Look up a column by `(table, name)` qualifier — disambiguates columns
    /// that share a `name` across a join (`a.k` vs `b.k`). `None` if no column
    /// has both that qualifier and name.
    pub fn column_id_qualified(&self, table: &str, name: &str) -> Option<ColumnId> {
        self.columns
            .iter()
            .position(|c| c.name == name && c.table.as_deref() == Some(table))
    }

    /// Whether this schema has *any* provable unique key — the signal a
    /// future reuse-aware planning pass would need to decide whether a
    /// producer's output can be safely shared across consumers.
    pub fn has_unique_key(&self) -> bool {
        !self.unique_keys.is_empty()
    }

    /// Append `cols` as an additional unique-key set if not already present.
    /// Used by `Distinct { cols }` per design.md §6 schema-flow table:
    /// "the input schema with `unique_keys` tightened to include `cols`".
    pub fn add_unique_key(&mut self, cols: Vec<ColumnId>) {
        if !self.unique_keys.contains(&cols) {
            self.unique_keys.push(cols);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, dtype: DataType) -> Column {
        Column::new(name, dtype, false)
    }

    #[test]
    fn schema_new_has_no_time_or_unique_key() {
        let s = Schema::new(vec![col("k", DataType::Utf8), col("v", DataType::Float64)]);
        assert!(s.time_index.is_none());
        assert!(!s.has_unique_key());
        assert_eq!(s.column_id("k"), Some(0));
        assert_eq!(s.column_id("v"), Some(1));
        assert_eq!(s.column_id("missing"), None);
    }

    #[test]
    fn schema_with_time_index_populates_metadata() {
        let s = Schema::with_time_index(
            vec![
                col("ts", DataType::Timestamp),
                col("service", DataType::Utf8),
                col("value", DataType::Float64),
            ],
            0,
            vec![vec![0, 1]],
        );
        assert_eq!(s.time_index, Some(0));
        assert!(s.has_unique_key());
        assert_eq!(s.unique_keys, vec![vec![0, 1]]);
    }

    #[test]
    fn add_unique_key_dedupes() {
        let mut s = Schema::new(vec![col("a", DataType::Utf8), col("b", DataType::Utf8)]);
        s.add_unique_key(vec![0]);
        s.add_unique_key(vec![0]);
        s.add_unique_key(vec![0, 1]);
        assert_eq!(s.unique_keys, vec![vec![0], vec![0, 1]]);
    }

    #[test]
    fn schema_serde_roundtrip() {
        let s = Schema::with_time_index(
            vec![
                col("ts", DataType::Timestamp),
                col("value", DataType::Float64),
            ],
            0,
            vec![vec![0]],
        );
        let json = serde_json::to_string(&s).unwrap();
        let back: Schema = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn schema_closed_defaults_to_open_when_absent() {
        // `closed` is `#[serde(default)]` so schemas serialized before the field
        // existed deserialize to `closed: false` (open) — the conservative
        // default (don't claim completeness you can't prove).
        let mut v = serde_json::to_value(Schema::new(vec![col("a", DataType::Utf8)])).unwrap();
        assert!(v.as_object_mut().unwrap().remove("closed").is_some());
        let back: Schema = serde_json::from_value(v).unwrap();
        assert!(!back.closed, "absent `closed` ⇒ open");
    }

    #[test]
    fn column_table_defaults_to_none_when_absent() {
        // `Column.table` is `#[serde(default)]` so schemas serialized before the
        // qualifier field existed still deserialize (to `table: None`) instead
        // of erroring. Drop the key from a serialized column to simulate that.
        let mut v = serde_json::to_value(col("svc", DataType::Utf8)).unwrap();
        assert!(v.as_object_mut().unwrap().remove("table").is_some());
        let back: Column = serde_json::from_value(v).unwrap();
        assert_eq!(back, col("svc", DataType::Utf8));
        assert!(back.table.is_none());
    }

    #[test]
    fn qualified_column_serde_roundtrip() {
        let c = col("service", DataType::Utf8).with_table("hosts");
        let back: Column = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
        assert_eq!(back.table.as_deref(), Some("hosts"));
    }
}
