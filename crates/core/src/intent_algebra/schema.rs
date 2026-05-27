//! Layer 3 schema flow — every L3 edge carries a typed `Schema`.
//!
//! Per `control_plane/docs/design.md` §6 "Schema flow — every L3 edge carries
//! a typed schema". The DAG is type-checked: a node's output schema is a
//! function of its inputs and parameters and is verifiable independently
//! of the surrounding context.
//!
//! `Schema::unique_keys` is the load-bearing field for the workload-level
//! CSE pass (`design.md` §6 "DAG, not tree" + the batched-queries example
//! around line ~1284). Two `QueryExpr::Ref` consumers can share a producer
//! only when its output schema is provably stable across reads — the
//! unique-key metadata is what lets the deduper assert that.
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
/// **Consumed by**: workload-level CSE (`CostModel::workload_cost` in the
/// design, not yet shipped). The single-query path, the `Bind*` rules,
/// push-down, and L5 emitters do not read this field.
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

    /// Whether this schema has *any* provable unique key. The CSE pass
    /// reads this to decide whether two `Ref` consumers can safely share
    /// a producer (see `design.md` §6 line ~1284 + the unit test in
    /// `tests::cse_substitution_legal_only_with_unique_keys`).
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

// ── CSE legality (the load-bearing consumer of `unique_keys`) ────────────────
//
// Phase F per `control_plane/docs/design.md` §6 Schema flow + the batched-
// queries example (§6 line ~1320):
//
//   "CSE legality leans on `Schema::unique_keys` (§6 Schema flow): two
//    `QueryExpr::Ref` consumers can share a producer only when its
//    output schema is provably stable across reads — the unique-key
//    metadata is what lets the deduper assert that without re-running
//    the producer's logic."
//
// `cse_reuse_is_legal` is the gatekeeper. The workload-level CSE pass
// (`intent_algebra::cse::dedupe_subtrees`) consults it before emitting
// a `LetBinding` to share a producer between ≥2 `Ref` consumers.

use thiserror::Error;

/// Errors returned by [`cse_reuse_is_legal`] when shared-producer reuse
/// would violate the design's stability invariant.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CseError {
    /// Producer schema lacks any `unique_keys` set — row identity is
    /// not provably stable across reads, so two `Ref` consumers cannot
    /// safely share it. The deduper falls back to per-consumer
    /// recomputation. Per design.md §6 line ~1356.
    #[error(
        "shared-producer CSE refused: producer schema has no unique_keys \
         (design.md §6 schema-flow — without a provable unique key the \
         deduper cannot assert row identity across reads)"
    )]
    NoUniqueKeys,
    /// Trivially-callable case: only one consumer means no reuse to
    /// gate. Returned so the caller can short-circuit instead of
    /// emitting a degenerate `LetBinding`.
    #[error("CSE not applicable: {0} consumer(s) — need ≥ 2 for shared-producer reuse")]
    InsufficientConsumers(usize),
}

/// Two `QueryExpr::Ref` nodes can share a producer (same `LetBinding`)
/// only when the producer's output schema has stable per-row identity —
/// i.e. `Schema::unique_keys` is non-empty. This is the gatekeeper:
/// returns `Ok(())` if shared-producer reuse is legal, otherwise `Err`.
///
/// Per design.md §6 line ~1356 — `unique_keys` is what makes CSE
/// provably correct. The deduper consults this before emitting a
/// `LetBinding`, and `CostModel::workload_cost` only credits a shared
/// binding when this gate has fired green.
///
/// `consumer_count` is the number of `QueryExpr::Ref { name }` sites the
/// deduper has identified for the candidate binding. Single-consumer
/// cases short-circuit with `InsufficientConsumers` — a `LetBinding`
/// with one `Ref` is just a no-op alias and shouldn't be hoisted.
pub fn cse_reuse_is_legal(producer_schema: &Schema, consumer_count: usize) -> Result<(), CseError> {
    if consumer_count < 2 {
        return Err(CseError::InsufficientConsumers(consumer_count));
    }
    if !producer_schema.has_unique_key() {
        return Err(CseError::NoUniqueKeys);
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, dtype: DataType) -> Column {
        Column::new(name, dtype, false)
    }

    /// `cse_reuse_is_legal` accepts a producer schema with at least one
    /// `unique_keys` set + ≥2 consumers. This is the design.md §6
    /// "load-bearing" green path.
    #[test]
    fn cse_reuse_legal_when_unique_keys_set() {
        let producer = Schema::with_time_index(
            vec![
                col("ts", DataType::Timestamp),
                col("service", DataType::Utf8),
                col("value", DataType::Float64),
            ],
            0,
            vec![vec![0, 1]],
        );
        assert_eq!(cse_reuse_is_legal(&producer, 2), Ok(()));
        assert_eq!(cse_reuse_is_legal(&producer, 5), Ok(()));
    }

    /// Schema without `unique_keys` is the conservative-default case —
    /// the deduper must refuse to share it. Pins design.md §6 line
    /// ~1356 ("Without it, the deduper has to be conservative and reuse
    /// drops on the floor").
    #[test]
    fn cse_reuse_illegal_when_unique_keys_empty() {
        let producer = Schema::new(vec![col("a", DataType::Int64), col("b", DataType::Float64)]);
        assert_eq!(
            cse_reuse_is_legal(&producer, 2),
            Err(CseError::NoUniqueKeys)
        );
    }

    /// Single-consumer case is short-circuited — no `LetBinding` should
    /// be emitted for one `Ref` because there's no reuse to credit.
    #[test]
    fn cse_reuse_rejects_single_consumer() {
        let producer = Schema::with_time_index(
            vec![col("ts", DataType::Timestamp), col("v", DataType::Float64)],
            0,
            vec![vec![0]],
        );
        assert_eq!(
            cse_reuse_is_legal(&producer, 1),
            Err(CseError::InsufficientConsumers(1))
        );
        assert_eq!(
            cse_reuse_is_legal(&producer, 0),
            Err(CseError::InsufficientConsumers(0))
        );
    }

    /// Empty `unique_keys` rejection takes precedence over the consumer
    /// count check only when both pass — but here we verify the
    /// insufficient-consumers branch fires first (a defensive ordering
    /// so callers see the clearer error when they get the call wrong).
    #[test]
    fn cse_reuse_consumer_check_precedes_unique_key_check() {
        let producer = Schema::new(vec![col("a", DataType::Int64)]);
        // Both conditions fail; consumer check is reported.
        assert_eq!(
            cse_reuse_is_legal(&producer, 1),
            Err(CseError::InsufficientConsumers(1))
        );
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
}
