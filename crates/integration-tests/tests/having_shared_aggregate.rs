//! A key-only `HAVING` of one query is not pushed below an aggregate it
//! shares with another query (#1089): every candidate of the shared
//! aggregate's memo group returns the group's own groups.
use asap_devtools::planner_fuzz::{read_seed_file, FuzzSettings, FuzzWorkload, Planner};
use asap_executor::values::Value;
use asap_logical_asap_optimization::{
    pass1::per_computation_candidates::generate_per_computation_candidates,
    pass2::memo::{
        candidates::try_populate_pass1, shared_configurations::PaneWidthDomain,
        DagOfLogicalMemoGroups,
    },
};
use asap_types::ir::NonASAPOp;
use asap_types::stages::MemoCandidates;
use serde_json::json;
use std::path::Path;

use crate::physical_common;

const P90: &str = "percentile_cont(0.9) WITHIN GROUP (ORDER BY value)";
const WINDOW: &str = "ts > now() - INTERVAL '1' HOUR";

/// `queries` over `metrics(ts, city, host, value)`, each repeated with
/// `max_error` 0.01 and `failure_probability` 0.001.
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

/// The Pass 1 memo of `queries` with every Pass 2 sharing rule applied.
fn expanded(queries: &[String]) -> MemoCandidates {
    let roots: Vec<_> = Planner::new(FuzzSettings::default())
        .lower(&workload(queries))
        .unwrap()
        .into_iter()
        .collect();
    let dag = DagOfLogicalMemoGroups::from_roots(&roots);
    let inventory = generate_per_computation_candidates(&dag).unwrap();
    let memo = try_populate_pass1(dag, &inventory).unwrap();
    asap_logical_asap_optimization::test_support::sharing_rules_and_shared_state_groups(
        memo,
        PaneWidthDomain::SeedGrids,
    )
    .unwrap()
}

fn row(city: &str, host: &str, value: f64) -> Vec<Value> {
    vec![
        Value::Timestamp(0),
        Value::Utf8(city.into()),
        Value::Utf8(host.into()),
        Value::Float64(value),
    ]
}

/// The cities `node` returns over the #1089 rows, sorted.
fn cities(node: &std::rc::Rc<asap_types::ir::OperatorNode>) -> Vec<String> {
    let rows = vec![
        row("NYC", "h1", 10.0),
        row("NYC", "h2", 20.0),
        row("LA", "h1", 5.0),
        row("LA", "h2", 7.0),
        row("SF", "h1", 100.0),
    ];
    let mut cities: Vec<String> = physical_common::execute_raw_rows_at(node, rows, 1000)
        .into_iter()
        .map(|row| format!("{:?}", row[0]))
        .collect();
    cities.sort();
    cities
}

/// #1089: A (`HAVING city = 'NYC'`) listed before B (the same query without
/// it) share one aggregate group. Every candidate of that group returns all
/// three cities, as B needs; A filters above it.
#[test]
fn key_only_having_of_one_reader_stays_above_a_shared_aggregate() {
    let grouped = format!("SELECT city, {P90} FROM metrics WHERE {WINDOW} GROUP BY city");
    let memo = expanded(&[format!("{grouped} HAVING city = 'NYC'"), grouped]);
    let shared = memo
        .groups
        .iter()
        .position(|group| {
            group.reached_by_queries() == [0, 1]
                && matches!(
                    group.logical_node.non_asap(),
                    Some(NonASAPOp::Aggregate { .. })
                )
        })
        .expect("the aggregate both queries read");
    let expected = cities(&memo.groups[shared].logical_node);
    assert_eq!(expected.len(), 3, "{expected:?}");
    assert!(memo.candidates[shared].len() > 1, "summary candidates too");
    for candidate in &memo.candidates[shared] {
        assert_eq!(
            cities(&candidate.root_node),
            expected,
            "{:?} candidate {:?}",
            candidate.origin,
            candidate.root_node
        );
    }
}
