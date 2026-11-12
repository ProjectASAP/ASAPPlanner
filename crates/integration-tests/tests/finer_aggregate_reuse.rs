//! A coarse aggregate read from a finer one through a compensating filter and,
//! when its groups are coarser, a rollup of values or states.
use crate::planning_common::{self, fuzz_workload, with_queries};
use asap_devtools::planner_fuzz::FuzzWorkload;

const P90: &str = "percentile_cont(0.9) WITHIN GROUP (ORDER BY value)";
const WINDOW: &str = "ts > now() - INTERVAL '5' MINUTE";

/// The percentile workload (5-minute window, every 10 s, ε = 0.01) with `queries`.
fn workload(queries: &[String]) -> FuzzWorkload {
    with_queries(
        fuzz_workload("percentile_filtered_and_by_city", true),
        queries,
    )
}

/// The selected plan's cost per second for `queries`.
fn cost(queries: &[String]) -> f64 {
    planning_common::cost(&workload(queries))
}

/// Example B end to end: adding the filtered sum to the finer sums costs
/// only its read of the finer result, not a second scan and aggregation.
#[test]
fn filtered_sum_costs_little_beyond_the_finer_sums() {
    let finer =
        format!("SELECT city, host, SUM(value) FROM metrics WHERE {WINDOW} GROUP BY city, host");
    let filtered = format!("SELECT SUM(value) FROM metrics WHERE {WINDOW} AND city = 'NYC'");
    let alone = cost(std::slice::from_ref(&finer));
    let separate = cost(std::slice::from_ref(&filtered));
    let together = cost(&[finer, filtered]);
    println!("finer alone {alone}, filtered alone {separate}, together {together}");
    // Measured: 0.0272326/s alone, 0.0054229/s for the filtered sum alone,
    // 0.0272527/s together.
    assert!(
        together <= alone * 1.01,
        "{together} vs {alone} + {separate}"
    );
}

/// Example C end to end: the joint plan costs no more than planning each
/// query alone, and no more than on main before the state path (0.2285/s).
/// The joint optimum is 0.1963/s: q1 keeps the NYC and SF states of a KLL
/// by city built over q0's query-time rows.
#[test]
#[ignore = "needs #1095's engine: since #1047 P7 Stage 3 has no per-query seed, and the joint search ends at 0.3321/s, above the separate plans' 0.2285/s"]
fn filtered_kll_states_cost_no_more_than_separate_plans() {
    let grouped = format!("SELECT city, {P90} FROM metrics WHERE {WINDOW} GROUP BY city");
    let filtered = format!("SELECT {P90} FROM metrics WHERE {WINDOW} AND city IN ('NYC', 'SF')");
    let alone = cost(std::slice::from_ref(&grouped));
    let separate = cost(std::slice::from_ref(&filtered));
    let together = cost(&[grouped, filtered]);
    println!("grouped alone {alone}, filtered alone {separate}, together {together}");
    assert!(
        together <= (alone + separate) * 1.001,
        "{together} vs {alone} + {separate}"
    );
    assert!(together <= 0.2285 * 1.01, "{together} vs main's 0.2285");
}
