//! Shared test fixtures for the ASAP e2e test suite.
//!
//! This crate owns the integration tests that verify correct pre-ASAP IR
//! output for each input **PromQL** query workload (it depends on the PromQL
//! front end only). The *cross-language* equivalence tests — semantically
//! equivalent SQL and PromQL mapping to the same canonical pre-ASAP IR —
//! live in `crates/devtools/tests/cross_language.rs`, where both front ends
//! are in scope.
//!
//! `fixtures` provides column/schema constructors used across test files.
//! Expected IR trees are always hand-constructed inside each test — nothing
//! here derives or computes expected outputs.

pub mod fixtures {
    use asap_frontend_promql::lower_promql_workload;
    use asap_types::pre_asap::schema::{Column, DataType, Schema};
    use asap_types::pre_asap::QueryExpr;
    use asap_types::types::AccuracyTarget;
    use asap_types::workload::{
        AccuracyRequirement, BatchEntry, DataWorkload, DurationMs, Evidence, PlanningWorkload,
        Predictability, Query, QueryLanguage, QueryRequirements, QueryWorkload, TimeSelection,
    };

    /// Lower one query through the plan-ready workload API using the test
    /// suite's declared one-second source cadence.
    pub fn lower_promql(
        query: &str,
        accuracy: AccuracyTarget,
    ) -> Result<QueryExpr, asap_frontend_promql::PromqlError> {
        let workload = PlanningWorkload {
            query_workload: QueryWorkload {
                language: QueryLanguage::PromQL,
                query_batch: Some(vec![BatchEntry {
                    query: Query(query.into()),
                    requirements: QueryRequirements {
                        accuracy: AccuracyRequirement::Explicit(accuracy),
                        ..Default::default()
                    },
                    predictability: Predictability::Unknown,
                    invocations: 1,
                    execute_at: None,
                    time_selection: TimeSelection::default(),
                }]),
                repeating_queries: None,
            },
            data_workload: Some(DataWorkload {
                data_ingestion_interval: Evidence {
                    value: Some(DurationMs(1_000)),
                    ..Default::default()
                },
                ..Default::default()
            }),
        };
        let mut lowered = lower_promql_workload(&workload, 0)?;
        Ok(lowered.remove(0))
    }

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
    /// any label columns referenced in the query, in the order the SchemaResolver
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
