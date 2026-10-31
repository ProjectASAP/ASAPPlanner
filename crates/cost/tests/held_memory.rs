//! Query-time working memory in [`plan_cost`]: how long a `NotMaterialized`
//! node's working memory is held depends on whether a materialized node
//! reads it.

use asap_cost::execution_demand::evaluation_rate;
use asap_cost::ResourceUnitCost;
use asap_cost::{kept_pane_chains, node_map, plan_cost, CostInputs, CostModel, PlanCost};
use asap_logical_asap_optimization::test_support::lower_promql;
use asap_physical_asap_optimization::optimize_physical_asap;
use asap_types::ir::physical_export::PhysicalASAPDAG;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::QueryRoot;
use asap_types::physical::{Materialization, Medium};
use asap_types::workload::{
    DataArrival, DataWorkload, QueryRecurrence, RepeatedDemand, RepetitionInterval, SourceFacts,
};

/// Data arriving at 10 000 rows per second over 10 000 series of `m`.
fn data() -> DataWorkload {
    DataWorkload {
        arrival: DataArrival::Ingesting {
            rows_per_second: 10_000.0,
        },
        sources: [(
            "m".to_string(),
            SourceFacts {
                series_count: Some(10_000),
                ..SourceFacts::default()
            },
        )]
        .into_iter()
        .collect(),
    }
}

/// Two queries repeating every 10 s and every 20 s.
fn recurrence() -> [QueryRecurrence; 2] {
    [10_000, 20_000].map(|interval_ms| {
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
            interval_ms,
        )))
    })
}

/// `plan_cost` of `dag` for [`recurrence`], at `cost_per_memory_byte_second`.
fn cost(dag: &PhysicalASAPDAG, cost_per_memory_byte_second: f64) -> PlanCost {
    let mut unit_cost = ResourceUnitCost::illustrative();
    unit_cost.cost_per_memory_byte_second = cost_per_memory_byte_second;
    let settings = CostInputs {
        cost_model: CostModel::UsageOverTime(unit_cost),
        ..CostInputs::default()
    };
    let nodes = node_map(dag);
    plan_cost(
        dag,
        &nodes,
        &kept_pane_chains(dag),
        &recurrence(),
        &[],
        &data(),
        &settings,
        1.0,
    )
    .unwrap()
    .0
}

/// Per node holding query-time memory, (its id, its held bytes, its
/// held-memory cost per second at unit memory cost 1).
fn held_memory_costs(dag: &PhysicalASAPDAG) -> Vec<(usize, f64, f64)> {
    let (with, without) = (cost(dag, 1.0), cost(dag, 0.0));
    with.per_node
        .iter()
        .filter_map(|(id, node)| {
            let held = node.detail.split("; holds ").nth(1)?;
            let bytes: f64 = held.split(' ').next().unwrap().parse().unwrap();
            let term =
                node.cost_per_second.unwrap() - without.per_node[id].cost_per_second.unwrap();
            Some((*id, bytes, term))
        })
        .collect()
}

fn materialization_of(dag: &PhysicalASAPDAG, id: usize) -> Materialization {
    dag.nodes
        .iter()
        .find(|n| n.id == id)
        .unwrap()
        .materialization
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-12 * a.abs().max(b.abs()).max(1e-300)
}

/// Query 0 is `sum(...)` over query 1's output, so it runs longer. Without
/// a materialized node, each query computes the shared nodes and holds
/// their working memory for its own latency: `bytes · Σ rate_q · latency_q`.
/// When query 1's output is `MatAtQuery`, the nodes below it are computed
/// once per execution of it, at its rate, and held until the
/// longest reader, query 0, finishes, as the `MatAtQuery` state itself is:
/// `bytes · rate · latency_0`, not the rate-weighted mean of the two
/// latencies.
#[test]
fn working_memory_read_by_a_materialized_node_is_held_for_the_longest_reader() {
    let outer = lower_promql("sum(sum_over_time(m[1m]))", AccuracyTarget::exact());
    let inner = outer.children()[0].clone();
    let plans = optimize_physical_asap(
        "L",
        &[QueryRoot::Operator(outer), QueryRoot::Operator(inner)],
        &[],
        &data(),
        false,
    )
    .unwrap();
    let dag_with = |materialization: Materialization| {
        plans
            .iter()
            .map(|plan| &plan.dag)
            .find(|dag| materialization_of(dag, dag.roots[1]) == materialization)
            .unwrap()
    };
    let recomputed = dag_with(Materialization::NotMaterialized);
    let shared = dag_with(Materialization::MatAtQuery(Medium::Memory));
    let latency_ms = |dag| {
        let cost = cost(dag, 1.0);
        [0, 1].map(|q| cost.usage.latency_per_query[q].total_ms())
    };

    // Recomputed per query: the rate-weighted sum of both latencies.
    let [outer_ms, inner_ms] = latency_ms(recomputed);
    assert!(outer_ms > inner_ms, "{outer_ms} vs {inner_ms}");
    let below = |dag: &PhysicalASAPDAG| {
        let materialized = dag.roots[1];
        held_memory_costs(dag)
            .into_iter()
            .filter(move |(id, _, _)| *id != materialized && *id != dag.roots[0])
            .collect::<Vec<_>>()
    };
    let checked = below(recomputed);
    assert!(!checked.is_empty(), "the shared nodes hold memory");
    for (id, bytes, term) in checked {
        let expected = bytes * (0.1 * outer_ms + 0.05 * inner_ms) / 1_000.0;
        assert!(close(term, expected), "node {id}: {term} vs {expected}");
    }

    // Below a MatAtQuery node: the longest reader's latency at the
    // materialized node's rate (its readers' schedules together).
    let materialized_rate = evaluation_rate(
        recurrence().iter().map(|r| (r, None)),
        CostInputs::default().non_repeating_query_horizon_seconds,
    )
    .unwrap();
    let [outer_ms, inner_ms] = latency_ms(shared);
    assert!(outer_ms > inner_ms, "{outer_ms} vs {inner_ms}");
    let checked = below(shared);
    assert!(!checked.is_empty(), "the nodes below it hold memory");
    for (id, bytes, term) in checked {
        assert_eq!(
            materialization_of(shared, id),
            Materialization::NotMaterialized
        );
        let expected = bytes * materialized_rate * outer_ms / 1_000.0;
        assert!(close(term, expected), "node {id}: {term} vs {expected}");
    }
}
