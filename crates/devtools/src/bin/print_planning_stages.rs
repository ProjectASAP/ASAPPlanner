// cargo run -p asap-devtools --bin print_planning_stages
// cargo run -p asap-devtools --bin print_planning_stages -- \
//     'sum by (job) (rate(http_requests_total[1m]))' \
//     'sum by (job) (sum_over_time(http_requests_total[5m]))'
//
// Prints every planner stage's output as text
// (`asap_devtools::planning_stages_text`): the frontends' roots, the memo
// groups, Pass 1's `PerComputationLogicalMemoCandidates`, the memo after
// Pass 2's local window composition, Pass 2's `LogicalASAPMemoCandidates`,
// Stage 2's `PhysicalASAPMemoCandidates` and Stage 3's `Selection`.
//
// Without arguments it plans #509 Example 1. Each argument is a PromQL
// query that replaces Example 1's queries, keeping its data facts
// (`http_requests_total`, a counter) and its panels' requirements: exact,
// every 10 s.

use asap_devtools::planner_layering_examples::example1;
use asap_devtools::planning_stages_text::{plan_promql_with_stage_outputs, planning_stages_text};
use asap_types::workload::{Query, RepeatingEntry};

fn main() {
    let queries: Vec<String> = std::env::args().skip(1).collect();
    // Example 1's workload; its queries replaced when given.
    let mut workload = example1();
    if !queries.is_empty() {
        let repeating = workload
            .query_workload
            .repeating_queries
            .as_mut()
            .expect("Example 1 has repeating queries");
        // Example 1's first panel: exact, every 10 s.
        let panel = repeating[0].clone();
        *repeating = queries
            .iter()
            .map(|query| RepeatingEntry {
                query: Query(query.clone()),
                ..panel.clone()
            })
            .collect();
    }
    println!("# Workload");
    for (index, entry) in workload.query_workload.entries().enumerate() {
        println!(
            "  q{index}: {}   accuracy={:?}",
            entry.query.0, entry.requirements.accuracy
        );
    }
    match plan_promql_with_stage_outputs(&workload) {
        Ok(stages) => print!("{}", planning_stages_text(&stages)),
        Err(error) => {
            eprintln!("print_planning_stages: {error}");
            std::process::exit(1);
        }
    }
}
