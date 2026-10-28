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
            assert!(
                (m.related_cost_per_second - 0.166).abs() < 0.005,
                "{}",
                m.summary
            );
            assert!(
                (witness.cost_per_second - 0.144).abs() < 0.005,
                "{}",
                m.summary
            );
        }
        other => panic!("expected #903's violation, got {other:?}"),
    }
}

/// #902: Q3 = p90 per city; the same query for `city = 'city1'` built its
/// own KLL instead of selecting the group from Q3's result. #943 fixed the
/// issue's ungrouped case and closed #902; this grouped variant
/// (`… AND city = 'city1' GROUP BY city`) holds only since #952's
/// finer-aggregate reuse (#950), and is now its regression test.
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
    let outcome = check(Relation::FilterOnGroupKey, workload, 1);
    assert!(
        matches!(outcome, Outcome::Holds(_)),
        "expected #902's case to hold, got {outcome:?}"
    );
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
        Outcome::Violated(m) => {
            let witness = m.witness.expect("a marginal relation has a witness");
            assert!(matches!(witness.derivation, Some(Witness::Filter { .. })));
            assert!(
                m.related_cost_per_second > witness.cost_per_second * 1.1,
                "{}",
                m.summary
            );
        }
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

/// A bounded worker campaign writes the summary with the design-doc coverage
/// table and one draft per finding, including cases stopped by its time limit.
#[test]
fn campaign_writes_summary_coverage_and_drafts() {
    use asap_devtools::planner_fuzz::report::{run_campaign, Campaign};
    let out = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("planner_fuzz_campaign_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let campaign = Campaign {
        seed: 7,
        cases: 2,
        time_budget_s: 600.0,
        relations: vec![Relation::LooserAccuracy, Relation::ExactDuplicate],
        seeds: vec![seed("issue903.workload.json")],
        out_dir: out.clone(),
        max_shrink_attempts: 5,
        // The report test must honor case_timeout_s even when an individual
        // case performs many planner calls; in-process campaigns cannot do so.
        worker: Some(env!("CARGO_BIN_EXE_planner_fuzz").into()),
        case_timeout_s: 60.0,
    };
    let summary = run_campaign(&campaign, FuzzSettings::default()).expect("the campaign runs");
    assert_eq!(summary.cases_run, 2);
    assert!(
        summary
            .failed_cases
            .iter()
            .all(|case| case.error.starts_with("timed out after ")),
        "worker failures other than the configured timeout: {:?}",
        summary.failed_cases
    );
    let markdown = std::fs::read_to_string(out.join("summary.md")).expect("summary.md");
    assert!(markdown.contains("## Design-doc reuse patterns"));
    assert!(markdown.contains("`a-looser-accuracy`"));
    for finding in &summary.findings {
        let draft = std::fs::read_to_string(&finding.draft).expect("the draft");
        assert!(draft.starts_with("# "));
        assert!(draft.contains("## Example"));
    }
}

/// One generated query exact or at ε = 0.01, δ = 0.001.
fn generated(spec: QuerySpec, approximate: bool) -> FuzzWorkload {
    use asap_devtools::planner_fuzz::generate::generated_workload;
    use asap_devtools::planner_fuzz::{FuzzQuery, Language};
    use asap_types::ir::properties::AccuracyTarget;
    let language = match spec {
        QuerySpec::Promql(_) => Language::Promql,
        QuerySpec::Sql(_) => Language::Sql,
    };
    let accuracy = if approximate {
        AccuracyTarget::max_error(0.01).failure_probability(0.001)
    } else {
        AccuracyTarget::exact()
    };
    generated_workload(
        language,
        vec![FuzzQuery::generated(spec, accuracy, 10_000, None)],
        1_000,
        15_000,
    )
}

fn promql(
    metric: &str,
    range: Option<RangeFunction>,
    outer: Option<(OuterOp, &[&str])>,
) -> QuerySpec {
    QuerySpec::Promql(PromqlSpec {
        metric: metric.into(),
        matchers: vec![],
        range,
        window_ms: 300_000,
        outer: outer.map(|(op, labels)| OuterAggregation {
            op,
            labels: labels.iter().map(|l| l.to_string()).collect(),
            without: false,
        }),
    })
}

/// The witness `relation` builds for `workload`'s query, which must build
/// and price (the check is conclusive).
fn witness_of(relation: Relation, workload: FuzzWorkload) -> Witness {
    match check(relation, workload, 0) {
        Outcome::Holds(m) | Outcome::Violated(m) => m
            .witness
            .and_then(|w| w.derivation)
            .expect("a marginal relation has a witness"),
        Outcome::Inconclusive { reason } => panic!("{relation:?} is inconclusive: {reason}"),
    }
}

/// The sketch rollup witness builds and prices for a grouped quantile:
/// a `SummaryMerge` with `group_by` over the finer per-group KLL.
#[test]
fn sketch_rollup_witness_prices_for_a_grouped_quantile() {
    let q = promql(
        "m_gauge",
        None,
        Some((OuterOp::Quantile(0.9), &["job", "instance"])),
    );
    assert_eq!(
        witness_of(Relation::CoarserGrouping, generated(q, true)),
        Witness::SketchRollup {
            labels: vec!["job".into()]
        }
    );
}

/// The exact rollup witness builds and prices for a grouped count (a sum
/// of the finer counts).
#[test]
fn exact_rollup_witness_prices_for_a_grouped_count() {
    let q = promql(
        "m_gauge",
        None,
        Some((OuterOp::Count, &["instance", "region"])),
    );
    assert!(matches!(
        witness_of(Relation::CoarserGrouping, generated(q, false)),
        Witness::ExactRollup { .. }
    ));
}

/// The quantile-rank witness builds and prices from the same KLL.
#[test]
fn quantile_rank_witness_prices_from_the_same_sketch() {
    let q = promql("m_gauge", Some(RangeFunction::QuantileOverTime(0.9)), None);
    assert_eq!(
        witness_of(Relation::OtherQuantileRank, generated(q, true)),
        Witness::QuantileRank { from: 0.9, to: 0.5 }
    );
}

/// The smaller top-k witness builds and prices over the same input.
#[test]
fn smaller_topk_witness_prices_over_the_same_input() {
    let q = promql(
        "m_counter",
        Some(RangeFunction::Rate),
        Some((OuterOp::Topk(10), &["job"])),
    );
    assert_eq!(
        witness_of(Relation::SmallerTopK, generated(q, false)),
        Witness::SmallerTopK { from: 10, to: 3 }
    );
}

/// (l): a filtered share of a grouped rate reads the rate from q's result
/// (ASAPWorkloads #12); the plain graft cannot price (its numerator is an
/// unplanned raw rate), so the witness planned with the operand must.
#[test]
fn ratio_witness_prices_over_the_shared_rate() {
    let q = promql(
        "m_counter",
        Some(RangeFunction::Rate),
        Some((OuterOp::Sum, &["job"])),
    );
    assert!(matches!(
        witness_of(Relation::BinaryOverResult, generated(q, false)),
        Witness::GraftWithOperand { .. }
    ));
}

/// (l), avg as sum / count: the sum is read from q's result.
#[test]
fn sum_over_count_witness_prices_over_the_shared_sum() {
    let q = promql(
        "m_gauge",
        Some(RangeFunction::SumOverTime),
        Some((OuterOp::Sum, &["job"])),
    );
    assert!(matches!(
        witness_of(Relation::BinaryOverResult, generated(q, false)),
        Witness::GraftWithOperand { .. } | Witness::Graft { .. }
    ));
}

/// (o): `increase` is q's `rate` times the window.
#[test]
fn increase_witness_prices_as_the_scaled_rate() {
    let q = promql(
        "m_counter",
        Some(RangeFunction::Rate),
        Some((OuterOp::Sum, &["job"])),
    );
    assert!(matches!(
        witness_of(Relation::EquivalentRangeFunction, generated(q, false)),
        Witness::Graft { .. }
    ));
}

/// (g): a threshold over a grouped minimum reads q's result.
#[test]
fn threshold_witness_prices_over_the_result() {
    let q = promql(
        "m_gauge",
        Some(RangeFunction::MaxOverTime),
        Some((OuterOp::Min, &["job"])),
    );
    assert!(matches!(
        witness_of(Relation::OuterOverResult, generated(q, false)),
        Witness::Graft { .. }
    ));
}

/// (m): a longer window lengthens q's pane chain of exact sums.
#[test]
fn longer_window_witness_prices_over_a_lengthened_chain() {
    let q = promql(
        "m_gauge",
        Some(RangeFunction::SumOverTime),
        Some((OuterOp::Sum, &["job"])),
    );
    assert!(matches!(
        witness_of(Relation::LongerWindow, generated(q, false)),
        Witness::LongerWindow { rollup: None, .. }
    ));
}

/// (n): the lengthened chain rolled up to one key fewer.
#[test]
fn longer_window_rollup_witness_prices() {
    let q = promql(
        "m_gauge",
        Some(RangeFunction::SumOverTime),
        Some((OuterOp::Sum, &["job", "region"])),
    );
    assert!(matches!(
        witness_of(Relation::LongerWindowRollup, generated(q, false)),
        Witness::LongerWindow {
            rollup: Some(_),
            ..
        }
    ));
}

/// (h): a distinct count of the high-cardinality column next to its
/// frequency L2 reads one UnivMon, from q's own state or from `W ∪ {q'}`
/// planned with UnivMon only (planner-layering Example 2).
#[test]
fn other_frequency_statistic_witness_prices_from_one_univmon() {
    use asap_devtools::planner_fuzz::generate::generated_workload;
    use asap_devtools::planner_fuzz::{FuzzQuery, Language};
    use asap_types::ir::properties::AccuracyTarget;
    let l2 = QuerySpec::Sql(SqlSpec {
        table: "metrics".into(),
        aggregate: SqlAggregate::FrequencyL2("src_ip".into()),
        group_by: vec![],
        filters: vec![],
        window_ms: 60_000,
    });
    let w = generated_workload(
        Language::Sql,
        vec![FuzzQuery::generated(
            l2,
            AccuracyTarget::max_error(0.05).failure_probability(0.01),
            10_000,
            None,
        )],
        10_000,
        15_000,
    );
    assert!(matches!(
        witness_of(Relation::OtherStatistic, w),
        Witness::OtherStatistic { .. } | Witness::UnivMonOnly
    ));
}
