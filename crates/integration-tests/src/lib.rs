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
//! Expected IR DAGs are always hand-constructed inside each test — nothing
//! here derives or computes expected outputs.

pub mod fixtures {

    use asap_types::ir::schema::{DataType, Field, Schema};
    use asap_types::ir::OperatorNode;
    use asap_types::types::AccuracyTarget;
    use asap_types::workload::{
        AccuracyRequirement, BatchEntry, DataWorkload, DurationMs, Evidence, PlanningWorkload,
        Predictability, Query, QueryLanguage, QueryRequirements, QueryWorkload, TimeSelection,
    };
    use std::rc::Rc;

    /// Lower one query through the plan-ready workload API using the test
    /// suite's declared one-second source cadence.
    pub fn lower_promql(
        query: &str,
        accuracy: AccuracyTarget,
    ) -> Result<Rc<OperatorNode>, asap_frontend_promql::PromqlError> {
        match lower_promql_root(query, accuracy)? {
            asap_types::ir::QueryRoot::Operator(node) => Ok(node),
            _ => Err(asap_frontend_promql::PromqlError::UnsupportedFeature(
                "expected vector root".into(),
            )),
        }
    }

    pub fn lower_promql_root(
        query: &str,
        accuracy: AccuracyTarget,
    ) -> Result<asap_types::ir::QueryRoot, asap_frontend_promql::PromqlError> {
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
        let mut lowered = asap_frontend_promql::lower_promql_query_workload(&workload, 0)?;
        Ok(lowered.remove(0))
    }

    pub fn ts_col() -> Field {
        Field::plain("ts", DataType::Timestamp, false)
    }

    pub fn value_col() -> Field {
        Field::plain("value", DataType::Float64, false)
    }

    pub fn label_col(name: &str) -> Field {
        Field::plain(name, DataType::Utf8, true)
    }

    /// Canonical PromQL leaf schema: `(ts: Timestamp, value: Float64)` plus
    /// any label columns referenced in the query, in the order the SchemaResolver
    /// appends them (alphabetical after dedup).
    pub fn metric_schema(labels: &[&str]) -> Schema {
        let mut cols = vec![ts_col(), value_col()];
        cols.extend(labels.iter().map(|n| label_col(n)));
        Schema {
            fields: cols,
            time_index: Some(0),
            unique_keys: vec![],
            // Schemaless PromQL leaf: open (the metric's full label set is
            // runtime-only; this lists just the referenced labels).
            closed: false,
        }
    }
}

/// Timing and export helpers for post-ASAP plans.
pub mod post_asap {
    use asap_types::ir::physical_export::{compile_physical_asap_dag, PhysicalASAPDAG};
    use asap_types::ir::{
        apply_materialization_timings, MaterializationAssignment, OperatorNode, TimingMemo,
    };
    use std::rc::Rc;

    /// Time `root` under the default assignment (every summary computed at
    /// query time). Returns the timed copy; read `node.timing` on it.
    pub fn timed(root: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        timed_with(root, &MaterializationAssignment::all_query_time())
    }

    /// Time `root` with every summary maintained at ingestion time.
    pub fn maintained(root: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        timed_with(root, &MaterializationAssignment::all_ingestion_time())
    }

    fn timed_with(
        root: &Rc<OperatorNode>,
        assignment: &MaterializationAssignment,
    ) -> Rc<OperatorNode> {
        apply_materialization_timings(root, assignment, &mut TimingMemo::new())
            .expect("materialization timing failed")
    }

    /// Time `root` (default assignment), then export the physical DAG.
    pub fn post_asap_dag(root: &Rc<OperatorNode>) -> PhysicalASAPDAG {
        compile_physical_asap_dag(&timed(root)).expect("post-ASAP DAG export failed")
    }

    /// Time `root` with every summary maintained, then export the physical DAG.
    pub fn maintained_post_asap_dag(root: &Rc<OperatorNode>) -> PhysicalASAPDAG {
        compile_physical_asap_dag(&maintained(root)).expect("post-ASAP DAG export failed")
    }
}
