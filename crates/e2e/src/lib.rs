//! Shared test fixtures for the ASAP e2e test suite.
//!
//! This crate owns the integration tests that verify (a) correct L3 IR output
//! for each input query workload, and (b) semantically equivalent queries in
//! different languages map to the same IR.
//!
//! `fixtures` provides column/schema constructors used across test files.
//! Expected IR trees are always hand-constructed inside each test — nothing
//! here derives or computes expected outputs.

pub mod fixtures {
    use asap_control_core::intent_algebra::schema::{Column, DataType, Schema};

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
        }
    }
}
