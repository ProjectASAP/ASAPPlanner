//! `planner_fuzz` finds the open missed-reuse issues on today's main.
//!
//! Each test plans an issue's workload and checks the relation that issue
//! breaks. They are open bugs, so each expects a violation. When the bug
//! is fixed the relation holds: flip the expectation to
//! `Outcome::Holds` (and keep the test as the fix's regression test).

use std::path::Path;

use asap_devtools::planner_fuzz::generate::{
    OuterAggregation, OuterOp, PromqlSpec, QuerySpec, RangeFunction, SqlAggregate, SqlSpec,
};
use asap_devtools::planner_fuzz::relations::{evaluate, Check, Outcome, Relation, Witness};
use asap_devtools::planner_fuzz::{read_seed_file, FuzzSettings, FuzzWorkload, Planner};

fn seed(name: &str) -> FuzzWorkload {
    read_seed_file(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/planner_fuzz_seeds")
            .join(name),
    )
    .expect("the seed file reads")
}

fn check(relation: Relation, workload: FuzzWorkload, target: usize) -> Outcome {
    evaluate(
        &mut Planner::new(FuzzSettings::default()),
        &Check {
            relation,
            workload,
            target,
            added: None,
        },
    )
}

/// #903: Q1 = `quantile_over_time(0.9, data[5m])` exact; Q2 = Q1 at
/// ε = 0.01, δ = 0.001 builds a KLL instead of reading Q1's exact result
/// (0.166 per second against the witness's 0.144). Open bug: flip to
/// `Holds` when #903 is fixed.
#[test]
fn finds_issue_903_looser_accuracy_not_reading_the_exact_result() {
    let mut workload = seed("issue903.workload.json");
    workload.queries.truncate(1);
    match check(Relation::LooserAccuracy, workload, 0) {
        Outcome::Violated(m) => {
            let witness = m.witness.expect("a marginal relation has a witness");
            assert_eq!(witness.derivation, Some(Witness::Same));
            assert!((m.related_cost - 0.166).abs() < 0.005, "{}", m.summary);
            assert!((witness.cost - 0.144).abs() < 0.005, "{}", m.summary);
        }
        other => panic!("expected #903's violation, got {other:?}"),
    }
}

/// #902: Q3 = p90 per city; the same query for `city = 'city1'` builds its
/// own KLL instead of selecting the group from Q3's result. Open bug: flip
/// to `Holds` when #902 is fixed.
#[test]
fn finds_issue_902_filter_on_group_key_not_reading_the_grouped_result() {
    let mut workload = seed("issue902.json");
    let per_city = SqlSpec {
        table: "metrics".into(),
        aggregate: SqlAggregate::Quantile(0.9),
        group_by: vec!["city".into()],
        filters: vec![],
        window_ms: 300_000,
    };
    workload.queries = vec![
        workload.queries[0].clone(),
        workload.queries[2].with_spec(QuerySpec::Sql(per_city)),
    ];
    match check(Relation::FilterOnGroupKey, workload, 1) {
        Outcome::Violated(m) => {
            let witness = m.witness.expect("a marginal relation has a witness");
            assert!(matches!(witness.derivation, Some(Witness::Filter { .. })));
            assert!(m.related_cost > witness.cost, "{}", m.summary);
        }
        other => panic!("expected #902's violation, got {other:?}"),
    }
}

/// #891: the per-pod CPU rate query filtered on a group key does not read
/// the grouped rate result. Open bug: flip to `Holds` when #891 is fixed.
#[test]
fn finds_issue_891_filtered_rate_not_reading_the_grouped_rate() {
    let mut workload = seed("issue891.workload.json");
    let grouped = PromqlSpec {
        metric: "container_cpu_usage_seconds_total".into(),
        matchers: vec![],
        range: Some(RangeFunction::Rate),
        window_ms: 300_000,
        outer: Some(OuterAggregation {
            op: OuterOp::Sum,
            labels: vec![
                "cluster".into(),
                "namespace".into(),
                "pod".into(),
                "container".into(),
            ],
            without: false,
        }),
    };
    workload.queries = vec![workload.queries[0].with_spec(QuerySpec::Promql(grouped))];
    match check(Relation::FilterOnGroupKey, workload, 0) {
        Outcome::Violated(m) => assert!(m.witness.is_some(), "{}", m.summary),
        other => panic!("expected #891's violation, got {other:?}"),
    }
}

/// A relation that holds: an exact duplicate of a query the planner shares.
#[test]
fn exact_duplicate_of_a_shared_query_holds() {
    let mut workload = seed("issue903.workload.json");
    workload.queries.truncate(1);
    assert!(
        matches!(
            check(Relation::ExactDuplicate, workload, 0),
            Outcome::Holds(_)
        ),
        "a duplicate is answered from the same node"
    );
}

/// A small in-process campaign writes the summary with the design-doc
/// coverage table, and one draft per finding.
#[test]
fn campaign_writes_summary_coverage_and_drafts() {
    use asap_devtools::planner_fuzz::report::{run_campaign, Campaign};
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("planner_fuzz_campaign");
    let _ = std::fs::remove_dir_all(&out);
    let campaign = Campaign {
        seed: 7,
        cases: 2,
        time_budget_s: 600.0,
        relations: vec![Relation::LooserAccuracy, Relation::ExactDuplicate],
        seeds: vec![seed("issue903.workload.json")],
        out_dir: out.clone(),
        max_shrink_attempts: 5,
        worker: None,
        case_timeout_s: 60.0,
    };
    let summary = run_campaign(&campaign, FuzzSettings::default()).expect("the campaign runs");
    assert_eq!(summary.cases_run, 2);
    let markdown = std::fs::read_to_string(out.join("summary.md")).expect("summary.md");
    assert!(markdown.contains("## Design-doc reuse patterns"));
    assert!(markdown.contains("`a-looser-accuracy`"));
    for finding in &summary.findings {
        let draft = std::fs::read_to_string(&finding.draft).expect("the draft");
        assert!(draft.starts_with("# "));
        assert!(draft.contains("## Example"));
    }
}
