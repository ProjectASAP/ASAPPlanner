//! Workloads whose queries share computation, planned through the production
//! stages: adding a query that can read an existing result must not cost
//! more than planning the first query alone.
use crate::planning_common::{cost, cost_with, fuzz_workload, selected_plan};
use asap_cost::CostInputs;
use asap_types::ir::NonASAPOp;
use asap_types::plan::physical_asap_dag::PhysicalASAPOperatorPayload as Payload;
use serde_json::json;

/// Deriving the coarse sum costs only a sum over the 150 fine groups.
/// Stage 2 offers that plan; Stage 3 selects a `OneEvaluation` build whose
/// not-materialized sibling would avoid a second scan.
#[test]
#[ignore = "Stage 3 misses the cheaper plan Stage 2 offers (#1142)"]
fn exact_coarse_sum_reuses_fine_sum() {
    let related = fuzz_workload("avg_sum_by_instance_region_and_instance", false);
    let mut base = related.clone();
    base.queries.truncate(1);
    let base_cost = cost(&base);
    let related_cost = cost(&related);
    assert!(
        related_cost <= base_cost + 0.00004,
        "{related_cost} vs {base_cost}"
    );
}

/// The 1m sum reads the newest tumbling window of the existing 2m chain.
#[test]
fn nested_exact_sum_reuses_newest_tumbling_windows() {
    let related = fuzz_workload("nested_exact_sum", false);
    let mut base = related.clone();
    base.queries.truncate(1);
    let base_cost = cost(&base);
    let related_cost = cost(&related);
    assert!(
        related_cost <= base_cost * 1.001,
        "{related_cost} vs {base_cost}"
    );
}

/// The slower copy reads the same maintained sum.
#[test]
fn slower_repeat_reuses_faster_sum() {
    let related = fuzz_workload("slower_repeat_exact_sum", false);
    let mut base = related.clone();
    base.queries.truncate(1);
    let base_cost = cost(&base);
    let related_cost = cost(&related);
    assert!(
        related_cost <= base_cost * 1.001,
        "{related_cost} vs {base_cost}"
    );
}

/// The filtered query reads the by-city quantile's summary states instead
/// of its own scan and aggregation: with summaries' CPU costed free (so
/// the sketch plans are selected, as under the earlier unit costs), it
/// adds at most 0.1% to the by-city quantile's cost.
///
/// Under the default unit costs both quantiles are exact plans, and no
/// rule reads the filtered quantile from the by-city exact values
/// (#1358): the filtered query pays its own scan and sort. The last
/// assertion pins that; when the rule exists it fails, and should then
/// expect the reuse.
#[test]
fn grouped_filtered_quantile_plans() {
    let settings = asap_integration_tests::planning::summary_cpu_free();
    let w = fuzz_workload("percentile_filtered_and_by_city", true);
    let mut grouped = w.clone();
    grouped.queries = vec![w.queries[2].clone()];
    let grouped_cost = cost_with(&grouped, &settings);
    let mut filtered_and_grouped = w.clone();
    filtered_and_grouped.queries.remove(0);
    let together = cost_with(&filtered_and_grouped, &settings);
    assert!(
        together <= grouped_cost * 1.001,
        "filtered query must reuse the grouped result: {together} vs {grouped_cost}"
    );
    let defaults = CostInputs::default();
    let grouped_default = cost_with(&grouped, &defaults);
    let together_default = cost_with(&filtered_and_grouped, &defaults);
    assert!(
        together_default > grouped_default * 1.001,
        "under the default costs the filtered query now reuses the grouped exact result \
         ({together_default} vs {grouped_default}): update this test"
    );
}

/// Duplicate exact TopK queries share their result and inner window work.
#[test]
fn duplicate_exact_topk_shares_work() {
    let mut w = fuzz_workload("nested_exact_sum", false);
    for query in &mut w.queries {
        query.entry.query.0 = "topk(3, sum_over_time(m_gauge[2m]))".into();
    }
    let mut base = w.clone();
    base.queries.truncate(1);
    let single = cost(&base);
    let duplicate = cost(&w);
    assert!(duplicate <= single * 1.001, "{duplicate} vs {single}");
}

/// SQL variants of the nested and slower-repeat cases: nested windows and slower queries reuse one sum.
#[test]
fn sql_sum_shares_nested_windows_and_slower_readers() {
    let mut w = fuzz_workload("identical_percentiles", false);
    w.queries.truncate(1);
    w.queries[0].entry.query.0 =
        "SELECT SUM(value) FROM metrics WHERE ts > now() - INTERVAL '5' MINUTE".into();
    w.queries[0].entry.demand = serde_json::from_value(
        json!({"fixed_interval_at": {"interval": 60000, "evaluation_phase": 0}}),
    )
    .unwrap();
    let single = cost(&w);
    let mut nested = w.clone();
    let mut shorter = w.queries[0].clone();
    shorter.entry.query.0 = shorter.entry.query.0.replace("'5'", "'1'");
    nested.queries.push(shorter);
    let nested_cost = cost(&nested);
    assert!(nested_cost <= single * 1.001, "{nested_cost} vs {single}");
    let mut slower = w.queries[0].clone();
    slower.entry.demand = serde_json::from_value(
        json!({"fixed_interval_at": {"interval": 120000, "evaluation_phase": 0}}),
    )
    .unwrap();
    w.queries.push(slower);
    let repeated_cost = cost(&w);
    assert!(
        repeated_cost <= single * 1.001,
        "{repeated_cost} vs {single}"
    );
}

/// Two identical exact percentile queries pay for their result once.
#[test]
fn identical_exact_percentiles_share_work() {
    let related = fuzz_workload("identical_percentiles", false);
    let mut base = related.clone();
    base.queries.truncate(1);
    let base_cost = cost(&base);
    let related_cost = cost(&related);
    assert!(
        related_cost <= base_cost * 1.001,
        "{related_cost} vs {base_cost}"
    );
}

/// Different ranks share the source window but still compute their own
/// values: the plan scans the table and selects the window once, and both
/// percentiles read that one node.
#[test]
fn different_exact_percentiles_share_the_input_window() {
    let mut related = fuzz_workload("identical_percentiles", false);
    related.queries[1].entry.query.0 = related.queries[1].entry.query.0.replace("0.99", "0.5");
    let plan = selected_plan(&related, &CostInputs::default());
    let scans: Vec<_> = plan
        .nodes
        .iter()
        .filter(|node| matches!(node.payload, Payload::NonASAP(NonASAPOp::Scan { .. })))
        .collect();
    assert_eq!(scans.len(), 1, "one scan of the table: {plan:#?}");
    let windows: Vec<_> = plan
        .nodes
        .iter()
        .filter(|node| matches!(node.payload, Payload::NonASAP(NonASAPOp::TimeRange { .. })))
        .collect();
    assert!(windows.len() <= 1, "one input window: {plan:#?}");
    // The node both percentiles read: the window, or the scan if the
    // window is a filter over it.
    let shared = windows.first().unwrap_or(&scans[0]).id;
    let readers = plan
        .edges
        .iter()
        .filter(|edge| edge.producer == shared)
        .count();
    assert_eq!(readers, 2, "both percentiles read node {shared}: {plan:#?}");
}
