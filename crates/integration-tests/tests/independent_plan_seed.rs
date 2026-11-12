//! The joint search never returns a plan costlier than planning each query
//! on its own: each query's own plan, combined, is evaluated first.
use crate::planning_common::{self, fuzz_workload, with_queries};
use asap_devtools::planner_fuzz::FuzzWorkload;

const P90: &str = "percentile_cont(0.9) WITHIN GROUP (ORDER BY value)";
const W5: &str = "ts > now() - INTERVAL '5' MINUTE";

/// `queries` in the percentile workload over `metrics`, every 10 s.
fn sql(queries: &[String]) -> FuzzWorkload {
    with_queries(
        fuzz_workload("percentile_filtered_and_by_city", true),
        queries,
    )
}

/// The selected plan's cost per second for `queries`.
fn cost(queries: &[String]) -> f64 {
    planning_common::cost(&sql(queries))
}

/// Asserts the joint plan of `queries` costs no more than their separate
/// plans, and returns (joint, separate sum).
fn assert_no_costlier_than_separate(queries: &[String]) -> (f64, f64) {
    let separate: f64 = queries
        .iter()
        .map(|query| cost(std::slice::from_ref(query)))
        .sum();
    let together = cost(queries);
    println!("together {together}, separate sum {separate}");
    assert!(
        together <= separate * 1.001,
        "joint {together} above separate plans {separate}"
    );
    (together, separate)
}

/// Two per-city p90 queries: each alone maintains an ingestion-time KLL tumbling window
/// chain; together the search used to stop at a query-time plan about twice
/// as costly.
#[test]
fn per_city_percentiles_cost_no_more_than_separate_plans() {
    assert_no_costlier_than_separate(&[
        format!("SELECT {P90} FROM metrics WHERE city = 'NYC' AND {W5}"),
        format!("SELECT {P90} FROM metrics WHERE city = 'SF' AND {W5}"),
    ]);
}

/// A percentile and a maximum over one window share its scan, so neither
/// query has a plan of candidates the other cannot reach. Each alone keeps
/// ingestion-time state; together the search used to stop at a query-time
/// plan almost three times as costly.
#[test]
fn percentile_and_max_over_one_scan_cost_no_more_than_separate_plans() {
    assert_no_costlier_than_separate(&[
        format!("SELECT {P90} FROM metrics WHERE {W5}"),
        format!("SELECT MAX(value) FROM metrics WHERE {W5}"),
    ]);
}

/// The same with a grouped count over the shared scan.
#[test]
fn percentile_and_grouped_count_over_one_scan_cost_no_more_than_separate_plans() {
    assert_no_costlier_than_separate(&[
        format!("SELECT {P90} FROM metrics WHERE {W5}"),
        format!("SELECT city, COUNT(*) FROM metrics WHERE {W5} GROUP BY city"),
    ]);
}
