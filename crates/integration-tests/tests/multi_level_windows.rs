//! A long range read on a short grid plans wider tumbling-window levels
//! (design doc `multi-level-window-chains.md`): the measured workload,
//! `quantile_over_time(0.99, data[1h])` as a 241-step range request every
//! 15 s over 10,000 series at 10 Hz.
use asap_cost::{plan_cost, CostInputs};
use asap_planner::{
    plan_installable, FrontendInput, InstallablePlan, PlanningModels, PlanningWorkload, UserInput,
};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};

/// Main's plan for this workload: one 15 s chain, 240 windows merged per
/// step, at this cost per second (cost model 1, default inputs).
const SINGLE_LEVEL_COST_PER_SECOND: f64 = 9.896383e4;

fn workload() -> PlanningWorkload {
    serde_json::from_str(
        r#"{"data_workload": {"arrival": {"ingesting": {"rows_per_second": 100000.0}},
              "sources": {"data": {"column_signs": {"value": "non_negative"},
                "metric_type": "gauge", "sample_interval": 100, "series_count": 10000}}},
            "query_workload": {"language": "promql", "query_batch": null, "repeating_queries": [
              {"demand": {"fixed_interval_at": {"evaluation_phase": 0, "interval": 15000}},
               "predictability": {"predictable": {"known_at": null}},
               "query": "quantile_over_time(0.99, data[1h])",
               "range_evaluation": {"span_ms": 3600000, "step_ms": 15000},
               "requirements": {"accuracy": {"failure_probability": 0.3, "max_error": 0.05},
                 "max_latency_ms": null}}]}}"#,
    )
    .unwrap()
}

async fn planned() -> InstallablePlan {
    let workload = workload();
    let models = PlanningModels::builtin();
    plan_installable(UserInput::new(&workload, FrontendInput::Promql, models))
        .await
        .unwrap()
        .0
}

/// The plan's cost, node by node, under the default cost inputs.
fn cost_of(plan: &InstallablePlan) -> asap_cost::PlanCost {
    let workload = workload();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    plan_cost(
        &plan.dag,
        &entries,
        &workload.data_workload.clone().unwrap(),
        &CostInputs::default(),
    )
    .unwrap()
    .0
}

fn nodes_where(plan: &InstallablePlan, f: impl Fn(&Operator<usize>) -> bool) -> Vec<usize> {
    plan.dag
        .nodes
        .iter()
        .filter(|node| f(&node.payload))
        .map(|node| node.id)
        .collect()
}

/// T6. The cover's merge is charged per state it reads at each of the
/// request's 241 steps every 15 s: about 12 windows per series (15 min,
/// 5 min, 1 min and 15 s levels: 3.02 + 2 + 4 + 3 on average), 120,000
/// states, at 16.0667 per second; each wider level is charged the narrower
/// states it merges per second at ingestion time.
#[tokio::test]
async fn cover_merge_is_charged_its_mean_windows() {
    let plan = planned().await;
    let cost = cost_of(&plan);
    let merge_over_cover = nodes_where(&plan, |operator| {
        matches!(operator, Operator::ASAP(ASAPOp::SummaryMerge { .. }))
    });
    let [merge] = merge_over_cover.as_slice() else {
        panic!("one merge: {merge_over_cover:?}");
    };
    let detail = &cost.per_node[merge].detail;
    assert!(
        detail.starts_with("merge 120000 states into 10000") && detail.contains("x 16.0667/s"),
        "{detail}"
    );
    for level in nodes_where(&plan, |operator| {
        matches!(operator, Operator::ASAP(ASAPOp::SummaryMergeWindows { .. }))
    }) {
        let detail = &cost.per_node[&level].detail;
        assert!(
            detail.contains("states merged") && detail.contains("x 1.0000/s"),
            "{detail}"
        );
    }
}

/// T7. Stage 3 selects wider levels, each built from the next narrower
/// one, and a cover: cheaper than the single 15 s chain, its merge
/// reading at most 31 windows per series per step.
#[tokio::test]
async fn stage3_picks_levels_for_a_long_range() {
    let plan = planned().await;
    let covers = nodes_where(&plan, |operator| {
        matches!(
            operator,
            Operator::NonASAP(NonASAPOp::TimeRangeCover { .. })
        )
    });
    assert_eq!(covers.len(), 1);
    assert!(!nodes_where(&plan, |operator| {
        matches!(operator, Operator::ASAP(ASAPOp::SummaryMergeWindows { .. }))
    })
    .is_empty());
    let cost = cost_of(&plan);
    let total = cost.cost_per_second.unwrap();
    assert!(total < SINGLE_LEVEL_COST_PER_SECOND, "{total}");
    let merge = nodes_where(&plan, |operator| {
        matches!(operator, Operator::ASAP(ASAPOp::SummaryMerge { .. }))
    })[0];
    let states: u64 = cost.per_node[&merge]
        .detail
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert!(states <= 31 * 10_000, "{states}");
}

/// T8. Each level is a view kept for its readers' longest read: the 15 s
/// chain still 2 h (1 h range + 1 h span), as on main, and each wider
/// level its own retention.
#[tokio::test]
async fn levels_cost_memory() {
    let plan = planned().await;
    for node in &plan.dag.nodes {
        if let Some(width) = node.payload.chain_tumbling_window_width_ms() {
            assert_eq!(node.retention_ms, Some(7_200_000), "{width} ms");
        }
    }
    let cost = cost_of(&plan);
    let narrowest = nodes_where(&plan, |operator| {
        matches!(
            operator,
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
        )
    })[0];
    assert!(cost.per_node[&narrowest]
        .detail
        .contains("keeps 8773440000 bytes"));
}

/// T11. The plan file with both new operators reads back as written, and
/// the Planner's executor compiles it.
#[tokio::test]
async fn installable_plan_round_trips_levels() {
    let plan = planned().await;
    let text = serde_json::to_string(&plan).unwrap();
    let read = InstallablePlan::from_json(&text).unwrap();
    // The file carries no planning-only field (each node's selected
    // candidate), so the plan read back writes the same file.
    assert_eq!(serde_json::to_string(&read).unwrap(), text);
    assert!(text.contains("SummaryMergeWindows") && text.contains("TimeRangeCover"));
    read.check_compiles().unwrap();
}
