//! Workloads whose queries share computation, planned through the production
//! stages: adding a query that can read an existing result must not cost
//! more than planning the first query alone.
use crate::planning_common::{cost, fuzz_workload};
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

/// The filtered query avoids its independent scan and aggregation.
#[test]
fn grouped_filtered_quantile_plans() {
    assert!(
        cost(&fuzz_workload("percentile_filtered_and_by_city", true)) < 0.25,
        "filtered query must reuse the grouped result"
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

/// Different ranks share the source window but still compute their own values.
#[test]
fn different_exact_percentiles_share_the_input_window() {
    let mut related = fuzz_workload("identical_percentiles", false);
    related.queries[1].entry.query.0 = related.queries[1].entry.query.0.replace("0.99", "0.5");
    let related_cost = cost(&related);
    assert!(related_cost <= 0.0883, "{related_cost}");
}
