//! Frontends → Pass 1 → composed candidate logical ASAP DAGs: every candidate of
//! a query is a valid DAG with the query's output columns (names, types and
//! nullability).

use std::rc::Rc;

use asap_aware_mapping::per_computation_candidates::{
    compose_logical_candidate, generate_per_computation_candidates,
};
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};
use asap_types::post_asap::{FieldDataType, SketchAlgorithm, SummaryInputExpr, SummaryUpdate};
use asap_types::pre_asap::schema::{DataType, Field, Schema};
use asap_types::pre_asap::ColumnRef;
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

fn output_columns(root: &QueryRoot) -> Vec<(String, String, bool)> {
    match root {
        QueryRoot::Operator(node) => node
            .schema
            .fields
            .iter()
            .map(|field| {
                (
                    field.name.clone(),
                    format!("{:?}", field.dtype),
                    field.nullable,
                )
            })
            .collect(),
        QueryRoot::Scalar(_) => vec![],
    }
}

/// Compose every candidate of `root`; returns how many there were.
fn compose_all(label: &str, root: QueryRoot) -> usize {
    let expected = output_columns(&root);
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
        assert_eq!(output_columns(root), expected, "{label} {choice:?}");
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
        "SELECT sum(flows.bytes) FROM flows",
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
        "topk(3, sum by (job) (rate(m[5m])))",
        "count by (job) (m)",
        "count_over_time(m[5m])",
        "quantile_over_time(0.99, m[5m])",
    ] {
        assert!(compose_all(query, promql(query)) >= 1, "{query}");
    }
}

/// The summary estimates of `root`'s candidates that vary one target's
/// choice, every other target taking its first.
fn composed_estimates(root: QueryRoot) -> Vec<Rc<OperatorNode>> {
    let inventory = generate_per_computation_candidates(vec![(0, root)]).unwrap();
    let targets = inventory.targets.len();
    let choices: Vec<_> = inventory
        .targets
        .iter()
        .enumerate()
        .flat_map(|(target, computation)| {
            (1..computation.choices.candidates.len()).map(move |index| {
                let mut choice = vec![0; targets];
                choice[target] = index;
                choice
            })
        })
        .collect();
    choices
        .into_iter()
        .flat_map(|choice| {
            let composed = compose_logical_candidate(&inventory, &choice).unwrap();
            let QueryRoot::Operator(node) = &composed[0].1 else {
                panic!("operator root expected")
            };
            OperatorNode::reachable(node)
        })
        .filter(|node| matches!(node.asap(), Some(ASAPOp::SummaryEstimate { .. })))
        .collect()
}

fn update_of(estimate: &OperatorNode) -> (SketchAlgorithm, SummaryUpdate) {
    let Some(ASAPOp::SummaryEstimate { summary_input, .. }) = estimate.asap() else {
        unreachable!()
    };
    let Some(ASAPOp::SummaryAgg {
        family: FieldDataType::Sketch(kind, _),
        input,
        ..
    }) = summary_input.asap()
    else {
        panic!("sketch state expected")
    };
    (kind.algorithm().clone(), input.clone())
}

/// A heap sketch's bound is per item frequency, not Top-K membership, and
/// its weights are not proven non-negative: a Top-K estimate gets no
/// guarantee. A unit-count CMS estimate keeps its bound.
#[test]
fn topk_estimates_carry_no_frequency_bound() {
    let topk = composed_estimates(promql("topk by (job) (10, sum_over_time(m[1m]))"));
    let algorithms: Vec<_> = topk.iter().map(|node| update_of(node).0).collect();
    assert_eq!(
        algorithms,
        [
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap
        ]
    );
    assert!(topk.iter().all(|node| node.guarantee.is_none()));

    let count = composed_estimates(promql("count by (job) (m)"));
    let cms = count
        .iter()
        .find(|node| update_of(node).0 == SketchAlgorithm::Cms)
        .expect("CMS candidate");
    assert!(cms.guarantee.as_ref().is_some_and(|g| !g.is_exact()));
}

/// Top-K over a closed PromQL child without series identity ranks the
/// child's labels outside the partition.
#[test]
fn topk_over_an_aggregate_ranks_its_labels() {
    let estimates = composed_estimates(promql("topk(3, sum by (job) (rate(m[5m])))"));
    assert_eq!(estimates.len(), 2);
    for estimate in &estimates {
        assert_eq!(
            update_of(estimate).1.item,
            Some(SummaryInputExpr::Tuple(vec![SummaryInputExpr::Column(
                ColumnRef::Named("job".into())
            )]))
        );
    }
}

/// A subquery inside a measure filter is substituted too: each choice for
/// the inner count gives its own DAG, and an approximate inner count clears
/// the outer measure's guarantee.
#[tokio::test]
async fn subquery_in_a_measure_filter_is_substituted() {
    let query = "SELECT src, count(*) FILTER (WHERE bytes > (SELECT count(*) FROM flows)) \
                 FROM flows GROUP BY src";
    let inventory = generate_per_computation_candidates(vec![(0, sql(query).await)]).unwrap();
    assert_eq!(inventory.targets.len(), 2);
    let inner = inventory
        .targets
        .iter()
        .position(|target| {
            target
                .target
                .non_asap()
                .and_then(|op| match op {
                    asap_types::ir::NonASAPOp::Aggregate { reduction, .. } => {
                        reduction.group_keys()
                    }
                    _ => None,
                })
                .is_some_and(|by| by.keys().is_empty())
        })
        .expect("inner count");
    let candidates = &inventory.targets[inner].choices.candidates;
    assert!(candidates.len() > 1);
    let (mut seen, mut approximate_inner) = (Vec::new(), false);
    for (index, candidate) in candidates.iter().enumerate() {
        let mut choice = vec![0; 2];
        choice[inner] = index;
        let composed = compose_logical_candidate(&inventory, &choice).unwrap();
        let QueryRoot::Operator(root) = &composed[0].1 else {
            panic!("operator root expected")
        };
        let shown = format!("{root:?}");
        assert!(!seen.contains(&shown), "{candidate:?} composes as another");
        seen.push(shown);
        let finalize = OperatorNode::reachable(root)
            .into_iter()
            .find(|node| {
                matches!(
                    node.asap(),
                    Some(ASAPOp::FinalizeExactAccumulator { child })
                        if matches!(child.asap(), Some(ASAPOp::SummaryAgg { filter: Some(_), .. }))
                )
            })
            .expect("filtered outer count");
        let Some(ASAPOp::FinalizeExactAccumulator { child }) = finalize.asap() else {
            unreachable!()
        };
        let exact = |node: &OperatorNode| node.guarantee.as_ref().is_some_and(|g| g.is_exact());
        // The state's children: its input rows, then the subquery.
        let inputs_exact = child.children().into_iter().all(|node| exact(node));
        assert_eq!(exact(&finalize), inputs_exact, "{candidate:?}");
        approximate_inner |= !inputs_exact;
    }
    assert!(approximate_inner);
}
