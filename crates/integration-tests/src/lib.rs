//! Shared test fixtures for the ASAP e2e test suite.
//!
//! This crate owns the integration tests that verify correct L3 IR output for
//! each input **PromQL** query workload (it depends on the PromQL front end
//! only). The *cross-language* equivalence tests — semantically equivalent SQL
//! and PromQL mapping to the same canonical L3 — live in
//! `crates/lower/tests/cross_language.rs`, where both front ends are in scope.
//!
//! `fixtures` provides column/schema constructors used across test files.
//! Expected IR trees are always hand-constructed inside each test — nothing
//! here derives or computes expected outputs.

pub mod fixtures {
    use asap_types::pre_asap::schema::{Column, DataType, Schema};

    pub fn ts_col() -> Column {
        Column::new("ts", DataType::Timestamp, false)
    }

    pub fn value_col() -> Column {
        Column::new("value", DataType::Float64, false)
    }

    pub fn label_col(name: &str) -> Column {
        Column::new(name, DataType::Utf8, true)
    }

    /// Canonical PromQL leaf schema: `(ts: Timestamp, value: Float64)` plus
    /// any label columns referenced in the query, in the order the Binder
    /// appends them (alphabetical after dedup).
    pub fn metric_schema(labels: &[&str]) -> Schema {
        let mut cols = vec![ts_col(), value_col()];
        cols.extend(labels.iter().map(|n| label_col(n)));
        Schema {
            columns: cols,
            time_index: Some(0),
            unique_keys: vec![],
            // Schemaless PromQL leaf: open (the metric's full label set is
            // runtime-only; this lists just the referenced labels).
            closed: false,
        }
    }
}
