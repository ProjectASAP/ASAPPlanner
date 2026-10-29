//! A coarse aggregate read from a finer one through a compensating filter and,
//! when its groups are coarser, a rollup of values or states (#950).
use asap_devtools::planner_fuzz::{read_seed_file, FuzzSettings, FuzzWorkload, Planner};
use asap_devtools::planning_stages::run_search_over;
use asap_executor::values::Value;
use asap_logical_asap_optimization::{
    pass1::per_computation_candidates::generate_per_computation_candidates,
    pass2::memo::{
        candidates::{
            try_populate_pass1, CandidateForMemoGroup, CandidateMemo, OriginOfCandidateForMemoGroup,
        },
        shared_configurations::PaneWidthDomain,
        state_groups::expand_concrete_summary_state_groups_in_domain,
        DagOfLogicalMemoGroups,
    },
};
use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode};
use serde_json::json;
use std::path::Path;

mod physical_common;

const P90: &str = "percentile_cont(0.9) WITHIN GROUP (ORDER BY value)";
const WINDOW: &str = "ts > now() - INTERVAL '5' MINUTE";

/// The #902 workload (5-minute window, every 10 s, ε = 0.01) with `queries`.
fn workload(queries: &[String]) -> FuzzWorkload {
    let mut w = read_seed_file(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/sharing_issue_fixtures/902.json"),
    )
    .unwrap();
    w.tables = vec![json!({"name":"metrics", "time_index":0, "columns":[
        {"name":"ts","type":"timestamp","nullable":false},
        {"name":"city","type":"utf8","nullable":true},
        {"name":"host","type":"utf8","nullable":true},
        {"name":"value","type":"float64","nullable":true}
    ]})];
    let template = w.queries[0].clone();
    w.queries = queries
        .iter()
        .map(|query| {
            let mut entry = template.clone();
            entry.entry.query.0 = query.clone();
            entry
        })
        .collect();
    w
}

/// The expanded candidate memo of `queries`, as plan selection receives it.
fn expanded(queries: &[String]) -> CandidateMemo {
    let roots: Vec<_> = Planner::new(FuzzSettings::default())
        .lower(&workload(queries))
        .unwrap()
        .into_iter()
        .enumerate()
        .collect();
    let initial = DagOfLogicalMemoGroups::from_roots(&roots);
    let inventory = generate_per_computation_candidates(roots).unwrap();
    let memo = try_populate_pass1(initial, &inventory).unwrap();
    expand_concrete_summary_state_groups_in_domain(memo, PaneWidthDomain::SeedGrids)
        .unwrap()
        .memo
}

/// The aggregate group of query `query` alone and its rollup candidates.
fn rollups(memo: &CandidateMemo, query: usize) -> (usize, Vec<&CandidateForMemoGroup>) {
    let owner = memo
        .groups
        .iter()
        .position(|group| {
            group.queries == [query]
                && matches!(group.node.non_asap(), Some(NonASAPOp::Aggregate { .. }))
        })
        .expect("the query's own aggregate group");
    let candidates = memo.candidates[owner]
        .iter()
        .filter(|c| c.origin == OriginOfCandidateForMemoGroup::Rollup)
        .collect();
    (owner, candidates)
}

/// Whether `candidate` reads another group's result through a `Filter`
/// (the value path), rather than merging summary states.
fn reads_finer_result(candidate: &CandidateForMemoGroup) -> bool {
    !OperatorNode::reachable(&candidate.node)
        .iter()
        .any(|node| matches!(node.asap(), Some(ASAPOp::SummaryMerge { .. })))
        && OperatorNode::reachable(&candidate.node)
            .iter()
            .any(|node| matches!(node.non_asap(), Some(NonASAPOp::Filter { .. })))
}

fn row(city: Option<&str>, host: &str, value: Option<f64>) -> Vec<Value> {
    vec![
        Value::Timestamp(0),
        city.map_or(Value::Null, |city| Value::Utf8(city.into())),
        Value::Utf8(host.into()),
        value.map_or(Value::Null, Value::Float64),
    ]
}

/// Row sets covering an empty input, an absent selected group, NULL samples,
/// several hosts per city and other cities.
fn row_sets() -> Vec<Vec<Vec<Value>>> {
    vec![
        vec![],
        vec![row(Some("LA"), "h1", Some(5.0))],
        vec![row(Some("NYC"), "h1", None)],
        vec![
            row(Some("NYC"), "h1", Some(10.0)),
            row(Some("NYC"), "h1", Some(30.0)),
            row(Some("NYC"), "h2", Some(20.0)),
            row(Some("SF"), "h1", Some(7.0)),
            row(Some("LA"), "h1", Some(90.0)),
            row(None, "h1", Some(1.0)),
        ],
    ]
}

/// The candidate gives the original query's rows on every row set.
fn assert_same_rows(memo: &CandidateMemo, owner: usize, candidate: &CandidateForMemoGroup) {
    let sorted = |rows: Vec<Vec<Value>>| {
        let mut rows: Vec<_> = rows.iter().map(|row| format!("{row:?}")).collect();
        rows.sort();
        rows
    };
    for rows in row_sets() {
        let before =
            physical_common::execute_raw_rows_at(&memo.groups[owner].node, rows.clone(), 1000);
        let after = physical_common::execute_raw_rows_at(&candidate.node, rows.clone(), 1000);
        assert_eq!(sorted(after), sorted(before), "rows {rows:?}");
    }
}

/// Example A: both dropped keys are pinned, so the quantile query reads one
/// row of the finer quantile result; no quantile rollup is needed.
#[test]
fn pinned_keys_read_one_finer_quantile_row() {
    let memo = expanded(&[
        format!("SELECT city, host, {P90} FROM metrics WHERE {WINDOW} GROUP BY city, host"),
        format!("SELECT {P90} FROM metrics WHERE {WINDOW} AND city = 'NYC' AND host = 'h1'"),
    ]);
    let (owner, candidates) = rollups(&memo, 1);
    let read = candidates
        .into_iter()
        .find(|c| reads_finer_result(c))
        .expect("a read of the finer quantile result");
    assert_same_rows(&memo, owner, read);
}

/// Example B: a filtered global sum keeps the `city = 'NYC'` rows of the
/// finer sums and sums them.
#[test]
fn filtered_sum_rolls_up_finer_sums() {
    let memo = expanded(&[
        format!("SELECT city, host, SUM(value) FROM metrics WHERE {WINDOW} GROUP BY city, host"),
        format!("SELECT SUM(value) FROM metrics WHERE {WINDOW} AND city = 'NYC'"),
    ]);
    let (owner, candidates) = rollups(&memo, 1);
    let read = candidates
        .into_iter()
        .find(|c| reads_finer_result(c))
        .expect("a rollup of the finer sums");
    assert_same_rows(&memo, owner, read);
}

/// A grouped query whose filter keeps several groups: filter, then roll the
/// finer `(city, host)` sums and maxima up to `city`.
#[test]
fn filtered_grouped_aggregates_roll_up_finer_values() {
    let memo = expanded(&[
        format!(
            "SELECT city, host, SUM(value), MAX(value) FROM metrics WHERE {WINDOW} GROUP BY city, host"
        ),
        format!(
            "SELECT city, SUM(value), MAX(value) FROM metrics WHERE {WINDOW} AND city IN ('NYC', 'SF') GROUP BY city"
        ),
    ]);
    let (owner, candidates) = rollups(&memo, 1);
    let read = candidates
        .into_iter()
        .find(|c| reads_finer_result(c))
        .expect("a filtered rollup of the finer values");
    assert_same_rows(&memo, owner, read);
}

/// Example C: two cities' p90 values cannot be combined, but their KLL
/// states can: keep the NYC and SF states, merge them, estimate. The merged
/// read gives the coarse KLL read's rows, SQL's one NULL row over an empty
/// selection included; the query's own read keeps the rows the `OR` form
/// keeps.
#[test]
fn filtered_kll_states_merge() {
    let memo = expanded(&[
        format!("SELECT city, {P90} FROM metrics WHERE {WINDOW} GROUP BY city"),
        format!("SELECT {P90} FROM metrics WHERE {WINDOW} AND city IN ('NYC', 'SF')"),
    ]);
    let (owner, candidates) = rollups(&memo, 1);
    let merged = candidates
        .into_iter()
        .find(|candidate| {
            OperatorNode::reachable(&candidate.node).iter().any(|node| {
                matches!(node.asap(), Some(ASAPOp::SummaryMerge { children, group_by: Some(_), .. })
                    if matches!(children[0].non_asap(), Some(NonASAPOp::Filter { .. })))
            })
        })
        .expect("a candidate merging the filtered finer states");
    let own_kll = |memo: &CandidateMemo, owner: usize| {
        memo.candidates[owner]
            .iter()
            .find(|candidate| {
                candidate.origin == OriginOfCandidateForMemoGroup::Pass1
                    && OperatorNode::reachable(&candidate.node).iter().any(|node| {
                        matches!(node.asap(), Some(ASAPOp::SummaryAgg { family, .. })
                            if format!("{family:?}").contains("Kll"))
                    })
            })
            .expect("the query's own KLL candidate")
            .clone()
    };
    let direct = own_kll(&memo, owner);
    let or_memo = expanded(&[format!(
        "SELECT {P90} FROM metrics WHERE {WINDOW} AND (city = 'NYC' OR city = 'SF')"
    )]);
    let or_direct = own_kll(&or_memo, rollups(&or_memo, 0).0);
    for rows in row_sets() {
        let before = physical_common::execute_raw_rows_at(&direct.node, rows.clone(), 1000);
        let or_form = physical_common::execute_raw_rows_at(&or_direct.node, rows.clone(), 1000);
        assert_eq!(
            format!("{before:?}"),
            format!("{or_form:?}"),
            "rows {rows:?}"
        );
        let after = physical_common::execute_raw_rows_at(&merged.node, rows.clone(), 1000);
        assert_eq!(format!("{after:?}"), format!("{before:?}"), "rows {rows:?}");
        assert_eq!(after.len(), 1);
    }
}

/// The same groups in another key order are one computation: neither
/// aggregate reads the other (two such reads would be a cycle).
#[test]
fn permuted_group_by_is_not_a_finer_read() {
    let memo = expanded(&[
        format!("SELECT city, host, SUM(value) FROM metrics WHERE {WINDOW} GROUP BY city, host"),
        format!("SELECT host, city, SUM(value) FROM metrics WHERE {WINDOW} GROUP BY host, city"),
    ]);
    for query in 0..2 {
        let (_, candidates) = rollups(&memo, query);
        assert!(!candidates.into_iter().any(reads_finer_result));
    }
}

/// A conjunct on a non-key column cannot be applied to finer sums.
#[test]
fn value_conjunct_has_no_finer_read() {
    let memo = expanded(&[
        format!("SELECT city, host, SUM(value) FROM metrics WHERE {WINDOW} GROUP BY city, host"),
        format!("SELECT SUM(value) FROM metrics WHERE {WINDOW} AND city = 'NYC' AND value > 3"),
    ]);
    let (_, candidates) = rollups(&memo, 1);
    assert!(!candidates.into_iter().any(reads_finer_result));
}

/// A dropped key that is not pinned leaves several quantile values per
/// group, and quantile values do not roll up.
#[test]
fn unpinned_key_has_no_quantile_value_read() {
    let memo = expanded(&[
        format!("SELECT city, host, {P90} FROM metrics WHERE {WINDOW} GROUP BY city, host"),
        format!("SELECT {P90} FROM metrics WHERE {WINDOW} AND city = 'NYC'"),
    ]);
    let (_, candidates) = rollups(&memo, 1);
    assert!(!candidates.into_iter().any(reads_finer_result));
}

/// The finer result lacks rows the query needs: compensation only removes rows.
#[test]
fn narrower_finer_aggregate_has_no_read() {
    let memo = expanded(&[
        format!(
            "SELECT city, host, SUM(value) FROM metrics WHERE {WINDOW} AND city = 'SF' GROUP BY city, host"
        ),
        format!("SELECT SUM(value) FROM metrics WHERE {WINDOW} AND city = 'NYC'"),
    ]);
    let (_, candidates) = rollups(&memo, 1);
    assert!(!candidates.into_iter().any(reads_finer_result));
}

/// The selected plan's cost per second for `queries`.
fn cost(queries: &[String]) -> f64 {
    use asap_plan_selection::{PlanSelectionSettings, PlanningModels, Stage1ToStage3Search};
    let w = workload(queries);
    let roots = Planner::new(FuzzSettings::default()).lower(&w).unwrap();
    let planning = w.planning_workload();
    let entries: Vec<_> = planning.query_workload.entries().collect();
    let settings = PlanSelectionSettings::default();
    let deployment = w.deployment();
    let models = PlanningModels::builtin()
        .with_plan_selection(&settings)
        .with_deployment_capabilities(&deployment);
    let run = run_search_over(
        roots,
        &entries,
        &w.data,
        models,
        Stage1ToStage3Search {
            milp_time_limit_ms: Some(30_000),
            ..Default::default()
        },
    )
    .unwrap();
    let selection = run.stage3_plan_selection.as_ref().unwrap();
    let value = selection.costs[&selection.selected]
        .cost_per_second
        .unwrap();
    println!("cost={value}; selected={}", selection.selected);
    value
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
/// by city built over q0's query-time rows. The search reaches the separate
/// plans' cost because the first round evaluates them combined when the joint
/// search ends above them; reaching 0.1963/s within the budget needs
/// candidate-level cuts for rejected proposals.
#[test]
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
