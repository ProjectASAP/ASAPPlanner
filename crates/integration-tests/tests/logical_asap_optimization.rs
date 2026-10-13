//! Frontends → Pass 1 → composed candidate logical ASAP DAGs: every candidate of
//! a query is a valid DAG with the query's output column types.

use asap_aware_mapping::per_computation_candidates::{
    compose_logical_candidate, generate_per_computation_candidates,
};
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::ir::QueryRoot;
use asap_types::pre_asap::schema::{DataType, Field, Schema};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataWorkload, DurationMs, Evidence, PlanningWorkload, Query,
    QueryLanguage, QueryRequirements, QueryWorkload,
};

fn accuracy() -> AccuracyTarget {
    AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.01,
    }
}

async fn sql(query: &str) -> QueryRoot {
    let catalog = SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![
            Field::plain("src", DataType::Utf8, false),
            Field::plain("dst", DataType::Utf8, false),
            Field::plain("bytes", DataType::Float64, false),
        ]),
    );
    let root = lower_sql(query, &catalog, accuracy())
        .await
        .unwrap_or_else(|e| panic!("lower failed for {query:?}: {e}"));
    QueryRoot::Operator(root)
}

fn promql(query: &str) -> QueryRoot {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(accuracy()),
                    ..Default::default()
                },
                predictability: Default::default(),
                invocations: 1,
                execute_at: None,
                time_selection: Default::default(),
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(1000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    asap_frontend_promql::lower_promql_query_workload(&workload, 0)
        .unwrap_or_else(|e| panic!("lower failed for {query:?}: {e}"))
        .remove(0)
}

fn output_types(root: &QueryRoot) -> Vec<(String, bool)> {
    match root {
        QueryRoot::Operator(node) => node
            .schema
            .fields
            .iter()
            .map(|field| (format!("{:?}", field.dtype), field.nullable))
            .collect(),
        QueryRoot::Scalar(_) => vec![],
    }
}

/// Compose every candidate of `root`; returns how many there were.
fn compose_all(label: &str, root: QueryRoot) -> usize {
    let expected = output_types(&root);
    let inventory = generate_per_computation_candidates(vec![(0, root)]).unwrap();
    let radix: Vec<_> = inventory
        .targets
        .iter()
        .map(|target| target.choices.candidates.len())
        .collect();
    let total: usize = radix.iter().product();
    let mut choice = vec![0; radix.len()];
    for _ in 0..total {
        let composed = compose_logical_candidate(&inventory, &choice)
            .unwrap_or_else(|e| panic!("{label} {choice:?}: {e}"));
        let root = &composed[0].1;
        root.validate_structure()
            .unwrap_or_else(|e| panic!("{label} {choice:?}: {e}"));
        assert_eq!(output_types(root), expected, "{label} {choice:?}");
        for (digit, radix) in choice.iter_mut().zip(&radix).rev() {
            *digit += 1;
            if *digit < *radix {
                break;
            }
            *digit = 0;
        }
    }
    total
}

#[tokio::test]
async fn sql_candidates_compose() {
    for query in [
        "SELECT src, count(*) FROM flows GROUP BY src HAVING src = 'a'",
        "SELECT src, count(*) FROM flows GROUP BY src HAVING count(*) > 10",
        "SELECT src, avg(bytes), count(*), approx_distinct(dst) FROM flows GROUP BY src",
        "SELECT count(*) FILTER (WHERE bytes > 10), approx_percentile_cont(bytes, 0.99) FROM flows",
        "SELECT avg(bytes) FROM flows",
    ] {
        assert!(compose_all(query, sql(query).await) >= 1, "{query}");
    }
}

#[test]
fn promql_candidates_compose() {
    for query in [
        "avg by (job) (m)",
        "avg_over_time(m[5m])",
        "quantile(0.99, m) - quantile(0.5, m)",
        "scalar(quantile(0.99, m)) - scalar(quantile(0.5, m))",
        "topk by (job) (10, sum_over_time(m[1m]))",
        "count by (job) (m)",
    ] {
        assert!(compose_all(query, promql(query)) >= 1, "{query}");
    }
}
