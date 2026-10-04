//! Acceptance tests for #509 "Example 2: One summary for several
//! computations — the summary-capability rule in Pass 2".
//!
//! Spec: `docs/design_docs/proposals/planner-layering-example2-acceptance.md`.
//! Written by a test designer who does not implement the stages. Tests that
//! need an unimplemented feature are `#[ignore]`d, naming it.
//!
//! The doc's workload is SQL. The SQL frontend lowers Q1 to a distinct count
//! but not Q2 and Q3 to `Entropy` and `L2` (the doc's own TODO), so Stages
//! 1–3 run on a PromQL stand-in with the same structure: three statistics
//! of one input over the same 1-min window, through the frontend's
//! `distinct_over_time`, `entropy_over_time` and `l2_over_time`.

mod planner_layering_common;

use std::collections::BTreeSet;

use asap_frontend_sql::{lower_sql_dialect, SqlCatalog};
use asap_types::ir::schema::{
    DataType, Field, Schema, SketchAlgorithm, SketchParams, SketchStatistic,
};
use asap_types::ir::QueryRoot;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, DataArrival, DataDistribution, DataWorkload, DurationMs,
    LatencyRequirement, PlanningWorkload, Predictability, Query, QueryLanguage, QueryRequirements,
    QueryTimeScope, QueryWorkload, Rate, RepeatedDemand, RepeatingEntry, RepetitionInterval,
    SqlDialect, TimeSelection,
};
use planner_layering_common::*;

// ── Example 2 workload ───────────────────────────────────────────────────

const SQL_Q1: &str =
    "SELECT COUNT(DISTINCT src_ip) FROM flows WHERE ts >= now() - INTERVAL '1 minute'";
const SQL_Q2: &str = "SELECT -SUM(p * LN(p)) FROM (\
    SELECT COUNT(*) * 1.0 / SUM(COUNT(*)) OVER () AS p FROM flows \
    WHERE ts >= now() - INTERVAL '1 minute' GROUP BY src_ip)";
const SQL_Q3: &str = "SELECT SQRT(SUM(c * c)) FROM (\
    SELECT src_ip, COUNT(*) AS c FROM flows \
    WHERE ts >= now() - INTERVAL '1 minute' GROUP BY src_ip)";

/// PromQL stand-in for Q1–Q3 (see the module docs).
const PROMQL_Q1: &str = "distinct_over_time(flows_src_ip[1m])";
const PROMQL_Q2: &str = "entropy_over_time(flows_src_ip[1m])";
const PROMQL_Q3: &str = "l2_over_time(flows_src_ip[1m])";

/// (ε, δ) of Q1, Q2, Q3. Q3 is the strictest.
const ACCURACY: [(f64, f64); 3] = [(0.02, 0.01), (0.05, 0.01), (0.01, 0.01)];

fn panel(query: &str, (epsilon, delta): (f64, f64)) -> RepeatingEntry {
    RepeatingEntry {
        query: Query(query.into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
        requirements: QueryRequirements {
            accuracy: AccuracyRequirement::Explicit(AccuracyTarget::EpsilonDelta {
                epsilon,
                delta,
            }),
            response_latency: LatencyRequirement::Unspecified,
        },
        predictability: Predictability::Predictable { known_at: None },
        time_selection: TimeSelection {
            scope: QueryTimeScope::RealTime,
            lookback: Some(DurationMs(60_000)),
            as_of: None,
        },
    }
}

/// The shared data workload with 10,000,000 distinct source IPs.
fn data_workload(ingestion_interval: bool) -> DataWorkload {
    DataWorkload {
        arrival: DataArrival::ContinuouslyIngesting,
        data_ingestion_interval: if ingestion_interval {
            declared(DurationMs(15_000))
        } else {
            Default::default()
        },
        ingestion_volume: Default::default(),
        ingestion_rate: declared(Rate(1_000_000.0 / 15.0)),
        input_cardinality: declared(10_000_000),
        distribution: declared(DataDistribution::Zipf),
    }
}

fn workload(language: QueryLanguage, queries: [&str; 3]) -> PlanningWorkload {
    let sql = matches!(language, QueryLanguage::SQL(_));
    PlanningWorkload {
        query_workload: QueryWorkload {
            language,
            query_batch: None,
            repeating_queries: Some(
                queries
                    .iter()
                    .zip(ACCURACY)
                    .map(|(q, accuracy)| panel(q, accuracy))
                    .collect(),
            ),
        },
        // `data_ingestion_interval` is not needed for SQL; PromQL needs it.
        data_workload: Some(data_workload(!sql)),
    }
}

/// Example 2 as the doc writes it.
fn sql_workload() -> PlanningWorkload {
    workload(
        QueryLanguage::SQL(SqlDialect::DataFusionSQL),
        [SQL_Q1, SQL_Q2, SQL_Q3],
    )
}

fn standin_workload() -> PlanningWorkload {
    workload(QueryLanguage::PromQL, [PROMQL_Q1, PROMQL_Q2, PROMQL_Q3])
}

/// `flows(ts, src_ip)`: the doc names only these two columns.
fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "flows",
        Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("src_ip", DataType::Utf8, false),
            ],
            0,
            vec![],
        ),
    )
}

async fn lower_sql(workload: &PlanningWorkload) -> Vec<QueryRoot> {
    let mut roots = Vec::new();
    for entry in workload.query_workload.entries() {
        let root = lower_sql_dialect(
            &entry.query.0,
            &catalog(),
            SqlDialect::DataFusionSQL,
            entry.requirements.accuracy.target(),
        )
        .await
        .unwrap_or_else(|e| panic!("{}: {e}", entry.query.0));
        roots.push(QueryRoot::Operator(root));
    }
    roots
}

fn pipeline() -> Run {
    let workload = standin_workload();
    run_stages(&workload, lower_promql(&workload))
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// UnivMon build nodes of `c` with the queries that read each.
fn univmons(c: &Logical) -> Vec<(BTreeSet<usize>, SketchParams, BTreeSet<String>)> {
    sketch_builds(&c.dag)
        .into_iter()
        .filter(|(_, algorithm, _)| *algorithm == SketchAlgorithm::UnivMon)
        .map(|(id, _, params)| {
            let statistics = estimates_of(&c.dag, id)
                .into_iter()
                .map(|(_, s)| format!("{s:?}"))
                .collect();
            (readers(&c.dag, &c.query_roots, id), params, statistics)
        })
        .collect()
}

/// The UnivMon read by every query in `queries` and by no other.
fn shared_univmon(c: &Logical, queries: &[usize]) -> Option<SketchParams> {
    let want: BTreeSet<_> = queries.iter().copied().collect();
    univmons(c)
        .into_iter()
        .find(|(readers, ..)| *readers == want)
        .map(|(_, params, _)| params)
}

/// Whether some summary build is read by more than one query.
fn shares_a_summary(c: &Logical) -> bool {
    sketch_builds(&c.dag)
        .iter()
        .any(|(id, ..)| readers(&c.dag, &c.query_roots, *id).len() > 1)
}

fn options(run: &Run, query: usize) -> BTreeSet<String> {
    run.logical
        .iter()
        .filter(|c| !shares_a_summary(c))
        .map(|c| query_option(&c.dag, &c.query_roots, query))
        .collect()
}

/// UnivMon state size: heap entries plus counters over every layer.
fn footprint(params: &SketchParams) -> u64 {
    let SketchParams::UnivMon {
        heap_size,
        sketch_rows,
        sketch_cols,
        layers,
    } = params
    else {
        panic!("UnivMon params: {params:?}");
    };
    u64::from(*heap_size) + u64::from(*sketch_rows) * u64::from(*sketch_cols) * u64::from(*layers)
}

/// Q`query`'s own UnivMon parameters, from an independent candidate.
fn own_univmon(run: &Run, query: usize) -> SketchParams {
    run.logical
        .iter()
        .filter(|c| !shares_a_summary(c))
        .find_map(|c| shared_univmon(c, &[query]))
        .expect("Pass 1 offers UnivMon")
}

// ── Workload and Stage 0 ─────────────────────────────────────────────────

/// The encoded SQL workload is valid and normalizes to Q1, Q2, Q3.
#[test]
fn workload_encodes_example2() {
    let workload = sql_workload();
    workload.validate().expect("valid workload");
    let queries: Vec<_> = workload
        .query_workload
        .entries()
        .map(|e| e.query.0)
        .collect();
    assert_eq!(queries, [SQL_Q1, SQL_Q2, SQL_Q3]);
    standin_workload().validate().expect("valid stand-in");
}

/// The SQL frontend lowers Q1 to a distinct count over `flows`.
#[tokio::test]
async fn stage0_sql_q1_lowers_to_a_distinct_count() {
    let roots = lower_sql(&sql_workload()).await;
    let ops = stage0_operations(&roots[0]);
    assert!(ops.contains(&"scan".to_string()), "{ops:?}");
    assert!(
        ops.contains(&"aggregate:cardinality".to_string()),
        "{ops:?}"
    );
}

/// The SQL frontend recognizes Q2 as `Entropy(src_ip)` and Q3 as `L2(src_ip)`.
#[tokio::test]
#[ignore = "needs SQL frontend recognition of the Entropy and L2 forms (doc TODO)"]
async fn stage0_sql_q2_q3_lower_to_entropy_and_l2() {
    let roots = lower_sql(&sql_workload()).await;
    assert!(stage0_operations(&roots[1]).contains(&"aggregate:frequency_entropy".to_string()));
    assert!(stage0_operations(&roots[2]).contains(&"aggregate:frequency_l2".to_string()));
}

/// Once the SQL forms are recognized, the SQL workload has the stand-in's Pass 1 options.
#[tokio::test]
#[ignore = "needs SQL frontend recognition of the Entropy and L2 forms (doc TODO)"]
async fn stage1_sql_has_the_standin_options() {
    let workload = sql_workload();
    let sql = run_stages(&workload, lower_sql(&workload).await);
    let standin = pipeline();
    for q in 0..3 {
        assert_eq!(options(&sql, q), options(&standin, q), "Q{}", q + 1);
    }
}

/// The stand-in lowers to the three statistics, each over a 1-min range of one input.
#[test]
fn stage0_standin_lowers_to_three_statistics_of_one_input() {
    let roots = lower_promql(&standin_workload());
    let ops: Vec<_> = roots.iter().map(stage0_operations).collect();
    for (ops, statistic) in ops.iter().zip([
        "aggregate:cardinality",
        "aggregate:frequency_entropy",
        "aggregate:frequency_l2",
    ]) {
        let mut want = vec!["scan".to_string(), statistic.into(), "time_range".into()];
        want.sort();
        assert_eq!(ops, &want);
    }
}

/// Stage 0 is one summary-free workload DAG whose queries share no node.
#[test]
fn stage0_one_summary_free_dag_without_sharing() {
    let run = pipeline();
    let s0 = &run.stage0;
    s0.dag.validate().expect("valid DAG");
    assert_eq!(s0.query_roots.len(), 3);
    assert!(s0.dag.nodes.iter().all(|n| !is_summary(&n.payload)));
    assert!(cross_query_nodes(&s0.dag, &s0.query_roots).is_empty());
}

// ── Stage 1 ──────────────────────────────────────────────────────────────

/// Pass 1 offers an exact and a UnivMon option for each of the three statistics.
#[test]
fn stage1_pass1_offers_exact_and_univmon_for_each_statistic() {
    let run = pipeline();
    for q in 0..3 {
        let found = options(&run, q);
        assert!(found.contains("exact"), "Q{}: {found:?}", q + 1);
        assert!(found.contains("UnivMon"), "Q{}: {found:?}", q + 1);
    }
}

/// Pass 1 offers a specialized distinct-count summary for Q1.
#[test]
fn stage1_pass1_offers_a_distinct_count_summary() {
    let found = options(&pipeline(), 0);
    assert!(
        found
            .iter()
            .any(|o| ["Hll", "Theta", "Kmv"].contains(&o.as_str())),
        "{found:?}"
    );
}

/// Pass 1 offers a specialized entropy summary for Q2 and a norm summary for Q3.
#[test]
#[ignore = "needs Pass 1 specialized entropy and L2 summary families"]
fn stage1_pass1_offers_specialized_entropy_and_l2_summaries() {
    let run = pipeline();
    for q in [1, 2] {
        let found = options(&run, q);
        assert!(
            found.iter().any(|o| o != "exact" && o != "UnivMon"),
            "Q{}: {found:?}",
            q + 1
        );
    }
}

/// Every combination of the three queries' own options is kept as an independent candidate.
#[test]
fn stage1_keeps_every_independent_combination() {
    let run = pipeline();
    let found: BTreeSet<_> = run
        .logical
        .iter()
        .filter(|c| !shares_a_summary(c))
        .map(|c| {
            (0..3)
                .map(|q| query_option(&c.dag, &c.query_roots, q))
                .collect::<Vec<_>>()
        })
        .collect();
    let (o1, o2, o3) = (options(&run, 0), options(&run, 1), options(&run, 2));
    assert_eq!(found.len(), o1.len() * o2.len() * o3.len());
}

/// The summary-capability rule adds one UnivMon build feeding a distinct-count, an entropy and an L2 estimate.
/// (Passes today through the identical-expression rule: Pass 1 gives every
/// UnivMon the same parameters, so the shared-input variant merges them.)
#[test]
fn stage1_summary_capability_adds_one_univmon_for_all_three() {
    let run = pipeline();
    let shared = run.logical.iter().find(|c| {
        let ums = univmons(c);
        ums.len() == 1 && ums[0].0 == BTreeSet::from([0, 1, 2])
    });
    let c = shared.expect("a candidate with one UnivMon read by Q1, Q2 and Q3");
    let statistics = &univmons(c)[0].2;
    for s in [
        SketchStatistic::Cardinality,
        SketchStatistic::FrequencyEntropy,
        SketchStatistic::FrequencyL2,
    ] {
        assert!(statistics.contains(&format!("{s:?}")), "{statistics:?}");
    }
}

/// For each pair, a candidate shares one UnivMon between the two while the third keeps each of its own options.
#[test]
#[ignore = "needs Pass 2 summary-capability rule"]
fn stage1_summary_capability_adds_pairwise_shared_univmons() {
    let run = pipeline();
    for (pair, third) in [([0, 1], 2), ([0, 2], 1), ([1, 2], 0)] {
        let thirds: BTreeSet<_> = run
            .logical
            .iter()
            .filter(|c| shared_univmon(c, &pair).is_some())
            .map(|c| query_option(&c.dag, &c.query_roots, third))
            .collect();
        assert_eq!(thirds, options(&run, third), "pair {pair:?}");
    }
}

/// A UnivMon shared by several statistics is sized for the strictest consumer.
/// (Holds today only because UnivMon parameters do not depend on ε.)
#[test]
fn stage1_shared_univmon_is_sized_for_the_strictest_consumer() {
    let run = pipeline();
    let strictest = |queries: &[usize]| {
        *queries
            .iter()
            .min_by(|a, b| ACCURACY[**a].0.total_cmp(&ACCURACY[**b].0))
            .unwrap()
    };
    let mut seen = 0;
    for queries in [vec![0, 1, 2], vec![0, 1], vec![0, 2], vec![1, 2]] {
        for c in &run.logical {
            if let Some(params) = shared_univmon(c, &queries) {
                seen += 1;
                assert_eq!(params, own_univmon(&run, strictest(&queries)), "{}", c.id);
            }
        }
    }
    assert!(seen > 0, "no shared UnivMon");
}

/// Pass 1 sizes UnivMon for its query's target: ε = 0.01 (Q3) needs a larger state than ε = 0.05 (Q2).
#[test]
#[ignore = "needs UnivMon sizing for an accuracy target (Pass 1 uses fixed parameters)"]
fn stage1_univmon_is_sized_per_accuracy_target() {
    let run = pipeline();
    assert!(footprint(&own_univmon(&run, 2)) > footprint(&own_univmon(&run, 1)));
}

/// Only a UnivMon, never a specialized summary, is shared across the statistics.
#[test]
fn stage1_only_univmon_is_shared_across_statistics() {
    let run = pipeline();
    for c in &run.logical {
        for (id, algorithm, _) in sketch_builds(&c.dag) {
            if readers(&c.dag, &c.query_roots, id).len() > 1 {
                assert_eq!(algorithm, SketchAlgorithm::UnivMon, "{}", c.id);
            }
        }
    }
}

/// Stage 1 candidates are valid DAGs with unique ids.
#[test]
fn stage1_candidates_are_valid_and_uniquely_named() {
    assert_valid_and_uniquely_named(&pipeline());
}

// ── Stage 2 ──────────────────────────────────────────────────────────────

/// No candidate is discarded before Stage 3: Stage 2 maps logical candidates one-to-one.
#[test]
fn stage2_keeps_every_logical_candidate() {
    assert_stage2_bijection(&pipeline());
}

/// Stage 2 keeps the shared UnivMon as one build node read by all three queries.
#[test]
fn stage2_keeps_the_shared_univmon_as_one_build() {
    let run = pipeline();
    let mut seen = 0;
    for c in run
        .logical
        .iter()
        .filter(|c| shared_univmon(c, &[0, 1, 2]).is_some())
    {
        for p in run.physical_of(c) {
            seen += 1;
            let builds: Vec<_> = sketch_builds(&p.dag)
                .into_iter()
                .filter(|(_, a, _)| *a == SketchAlgorithm::UnivMon)
                .collect();
            assert_eq!(builds.len(), 1, "{}", p.id);
            assert_eq!(readers(&p.dag, &p.query_roots, builds[0].0).len(), 3);
        }
    }
    assert!(seen > 0);
}

// ── Stage 3 ──────────────────────────────────────────────────────────────

/// Stage 3 selects one candidate and gives every other one a reason.
#[test]
fn stage3_selects_one_and_explains_the_rest() {
    assert_selects_one_and_explains_the_rest(&pipeline());
}

/// The selected plan is the cheapest valid candidate for the whole workload.
#[test]
fn stage3_selects_cheapest_valid() {
    assert_selects_cheapest_valid(&pipeline());
}

/// Every node is charged once, so a summary read by several queries is costed once.
#[test]
fn stage3_charges_each_node_once() {
    assert_each_node_charged_once(&pipeline());
}

/// UnivMon candidates are judged by an accuracy model instead of being rejected for lacking one.
#[test]
#[ignore = "needs a UnivMon accuracy model in Stage 3"]
fn stage3_judges_univmon_with_an_accuracy_model() {
    let run = pipeline();
    for (id, reason) in run.invalid() {
        assert!(
            !reason.contains("no accuracy model for UnivMon"),
            "{id}: {reason}"
        );
    }
}

/// One UnivMon for all three costs no more than three separate UnivMons.
#[test]
#[ignore = "needs a UnivMon accuracy model in Stage 3"]
fn stage3_shared_univmon_costs_no_more_than_three() {
    let run = pipeline();
    let cost = |c: &Logical| run.cost(&run.physical_of(c).next().unwrap().id);
    let separate: Vec<_> = run
        .logical
        .iter()
        .filter(|c| {
            !shares_a_summary(c)
                && (0..3).all(|q| query_option(&c.dag, &c.query_roots, q) == "UnivMon")
        })
        .map(|c| cost(c).unwrap_or_else(|| panic!("{} is not priced", c.id)))
        .collect();
    assert!(!separate.is_empty(), "independent all-UnivMon candidate");
    let mut compared = 0;
    for shared in run
        .logical
        .iter()
        .filter(|c| shared_univmon(c, &[0, 1, 2]).is_some())
    {
        let shared_cost = cost(shared).unwrap_or_else(|| panic!("{} is not priced", shared.id));
        for separate_cost in &separate {
            assert!(
                shared_cost <= *separate_cost,
                "{shared_cost} vs {separate_cost}"
            );
        }
        compared += 1;
    }
    assert!(compared > 0);
}

/// The selected plan updates at most one UnivMon per flow record: separate UnivMons are dominated by the shared one.
#[test]
fn stage3_selected_plan_has_at_most_one_univmon() {
    let run = pipeline();
    let selected = run.physical(&run.selection.selected);
    let count = sketch_builds(&selected.dag)
        .iter()
        .filter(|(_, a, _)| *a == SketchAlgorithm::UnivMon)
        .count();
    assert!(count <= 1, "{}: {count} UnivMons", selected.id);
}
