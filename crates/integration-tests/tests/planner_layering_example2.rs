//! Acceptance tests for #509 "Example 2: One summary for several
//! computations — the summary-capability rule in Pass 2".
//!
//! Spec: `docs/design_docs/proposals/planner-layering-example2-acceptance.md`.
//! Written by a test designer who does not implement the stages. Tests that
//! need an unimplemented feature are `#[ignore]`d, naming it and the PR that
//! delivers it.
//!
//! The doc's workload is SQL. The SQL frontend lowers Q1 to a distinct count
//! but not Q2 and Q3 to `Entropy` and `L2` (the doc's own TODO, #598), so
//! the planning stages run on a PromQL stand-in with the same structure:
//! three statistics of one input over the same 1-min window, through the
//! frontend's `distinct_over_time`, `entropy_over_time` and `l2_over_time`.

mod physical_common;
mod planner_layering_common;

use std::collections::{BTreeMap, BTreeSet};

use asap_frontend_sql::{lower_sql_dialect, SqlCatalog};
use asap_types::ir::QueryRoot;
use asap_types::post_asap::{
    Field, ProbabilityExpr, Schema, SketchAlgorithm, SketchParams, SketchStatistic,
};
use asap_types::pre_asap::DataType;
use asap_types::types::{ErrorMetric, RequestedAccuracy};
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

/// The metric each query's target names: Q1's distinct count defaults to a
/// relative error, Q2's entropy to an absolute one, and Q3's L2 norm names
/// a relative error (an L2 root's default is an absolute error).
const METRIC: [Option<ErrorMetric>; 3] = [None, None, Some(ErrorMetric::Relative)];

fn panel(query: &str, (epsilon, delta): (f64, f64), metric: Option<ErrorMetric>) -> RepeatingEntry {
    let requested = RequestedAccuracy::epsilon_delta(epsilon, delta);
    RepeatingEntry {
        query: Query(query.into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
        requirements: QueryRequirements {
            accuracy: AccuracyRequirement::Explicit(match metric {
                Some(metric) => requested.with_metric(metric),
                None => requested,
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
        stream_norms: Default::default(),
        max_distinct_per_evaluation: Default::default(),
        metric_types: Default::default(),
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
                    .zip(METRIC)
                    .map(|((q, accuracy), metric)| panel(q, accuracy, metric))
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
            entry.requirements.accuracy.requested(),
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

/// The stand-in with the deployment's bound on distinct items per
/// evaluation of its one source, when given.
fn standin_with_distinct_bound(bound: Option<u64>) -> PlanningWorkload {
    let mut workload = standin_workload();
    if let Some(bound) = bound {
        workload
            .data_workload
            .as_mut()
            .expect("data workload")
            .max_distinct_per_evaluation =
            declared(BTreeMap::from([("flows_src_ip".into(), bound)]));
    }
    workload
}

fn pipeline_with_distinct_bound(bound: Option<u64>) -> Run {
    let workload = standin_with_distinct_bound(bound);
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
        ..
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

// ── Workload and query-language frontends ─────────────────────────────────────────────────

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
async fn stage0_query_language_frontends_sql_q1_lowers_to_a_distinct_count() {
    let roots = lower_sql(&sql_workload()).await;
    let ops = frontend_operations(&roots[0]);
    assert!(ops.contains(&"scan".to_string()), "{ops:?}");
    assert!(
        ops.contains(&"aggregate:cardinality".to_string()),
        "{ops:?}"
    );
}

/// The SQL frontend recognizes Q2 as `Entropy(src_ip)` and Q3 as `L2(src_ip)`.
#[tokio::test]
#[ignore = "needs SQL frontend recognition of the Entropy and L2 idioms (#598)"]
async fn stage0_query_language_frontends_sql_q2_q3_lower_to_entropy_and_l2() {
    let roots = lower_sql(&sql_workload()).await;
    assert!(frontend_operations(&roots[1]).contains(&"aggregate:frequency_entropy".to_string()));
    assert!(frontend_operations(&roots[2]).contains(&"aggregate:frequency_l2".to_string()));
}

/// Once the SQL forms are recognized, the SQL workload has the stand-in's Pass 1 options.
#[tokio::test]
#[ignore = "needs SQL frontend recognition of the Entropy and L2 idioms (#598)"]
async fn stage1_logical_asap_optimization_sql_has_the_standin_options() {
    let workload = sql_workload();
    let sql = run_stages(&workload, lower_sql(&workload).await);
    let standin = pipeline();
    for q in 0..3 {
        assert_eq!(options(&sql, q), options(&standin, q), "Q{}", q + 1);
    }
}

/// The stand-in lowers to the three statistics, each over a 1-min range of one input.
#[test]
fn stage0_query_language_frontends_standin_lowers_to_three_statistics_of_one_input() {
    let roots = lower_promql(&standin_workload());
    let ops: Vec<_> = roots.iter().map(frontend_operations).collect();
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

/// The frontends give one summary-free workload DAG whose queries share no node.
#[test]
fn stage0_query_language_frontends_one_summary_free_dag_without_sharing() {
    let run = pipeline();
    let stage0_query_language_frontends = &run.stage0_query_language_frontends;
    assert_eq!(stage0_query_language_frontends.query_roots.len(), 3);
    assert!(stage0_query_language_frontends
        .dag
        .nodes
        .iter()
        .all(|n| !is_summary(&n.operator)));
    assert!(cross_query_nodes(
        &stage0_query_language_frontends.dag,
        &stage0_query_language_frontends.query_roots
    )
    .is_empty());
}

// ── Logical ASAP-aware optimization ──────────────────────────────────────

/// Pass 1 offers an exact and a UnivMon option for each of the three statistics.
#[test]
fn stage1_logical_asap_optimization_pass1_offers_exact_and_univmon_for_each_statistic() {
    let run = pipeline();
    for q in 0..3 {
        let found = options(&run, q);
        assert!(found.contains("exact"), "Q{}: {found:?}", q + 1);
        assert!(found.contains("UnivMon"), "Q{}: {found:?}", q + 1);
    }
}

/// Pass 1 offers a specialized distinct-count summary for Q1.
#[test]
fn stage1_logical_asap_optimization_pass1_offers_a_distinct_count_summary() {
    let found = options(&pipeline(), 0);
    assert!(
        found.iter().any(|o| ["Hll", "Kmv"].contains(&o.as_str())),
        "{found:?}"
    );
}

/// Pass 1 offers a specialized entropy summary for Q2 and a norm summary for Q3.
#[test]
#[ignore = "needs Pass 1 specialized entropy and L2 summary families (no PR yet)"]
fn stage1_logical_asap_optimization_pass1_offers_specialized_entropy_and_l2_summaries() {
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
fn stage1_logical_asap_optimization_keeps_every_independent_combination() {
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
#[test]
#[ignore = "needs the Pass 2 summary-capability rule for frequency moments (#595, #596)"]
fn stage1_logical_asap_optimization_summary_capability_adds_one_univmon_for_all_three() {
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
#[ignore = "needs the Pass 2 summary-capability rule for frequency moments (#595, #596)"]
fn stage1_logical_asap_optimization_summary_capability_adds_pairwise_shared_univmons() {
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
#[test]
#[ignore = "needs the Pass 2 summary-capability rule for frequency moments (#595, #596)"]
fn stage1_logical_asap_optimization_shared_univmon_is_sized_for_the_strictest_consumer() {
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
fn stage1_logical_asap_optimization_univmon_is_sized_per_accuracy_target() {
    let run = pipeline();
    assert!(footprint(&own_univmon(&run, 2)) > footprint(&own_univmon(&run, 1)));
}

/// Only a UnivMon, never a specialized summary, is shared across the statistics.
#[test]
fn stage1_logical_asap_optimization_only_univmon_is_shared_across_statistics() {
    let run = pipeline();
    for c in &run.logical {
        for (id, algorithm, _) in sketch_builds(&c.dag) {
            if readers(&c.dag, &c.query_roots, id).len() > 1 {
                assert_eq!(algorithm, SketchAlgorithm::UnivMon, "{}", c.id);
            }
        }
    }
}

/// Logical candidates are valid DAGs with unique ids.
#[test]
fn stage1_logical_asap_optimization_candidates_are_valid_and_uniquely_named() {
    assert_valid_and_uniquely_named(&pipeline());
}

// ── Physical ASAP-aware optimization ─────────────────────────────────────

/// No candidate is discarded before plan selection: logical candidates map one-to-one onto physical ones.
#[test]
fn stage2_physical_asap_optimization_keeps_every_logical_candidate() {
    assert_one_physical_candidate_per_logical(&pipeline());
}

/// Physical ASAP-aware optimization keeps the shared UnivMon as one build node read by all three queries.
#[test]
#[ignore = "needs the Pass 2 summary-capability rule for frequency moments (#595, #596)"]
fn stage2_physical_asap_optimization_keeps_the_shared_univmon_as_one_build() {
    let run = pipeline();
    let mut seen = 0;
    for c in run
        .logical
        .iter()
        .filter(|c| shared_univmon(c, &[0, 1, 2]).is_some())
    {
        for p in run.physical_of(c) {
            seen += 1;
            let builds: Vec<_> = sketch_builds(p.dag())
                .into_iter()
                .filter(|(_, a, _)| *a == SketchAlgorithm::UnivMon)
                .collect();
            assert_eq!(builds.len(), 1, "{}", p.id);
            assert_eq!(readers(p.dag(), &p.query_roots, builds[0].0).len(), 3);
        }
    }
    assert!(seen > 0);
}

// ── Plan selection ───────────────────────────────────────────────────────

/// Plan selection selects one candidate and gives every other one a reason.
#[test]
fn stage3_plan_selection_selects_one_and_explains_the_rest() {
    assert_selects_one_and_explains_the_rest(&pipeline());
}

/// The selected plan is the cheapest valid candidate for the whole workload.
#[test]
fn stage3_plan_selection_selects_cheapest_valid() {
    assert_selects_cheapest_valid(&pipeline());
}

/// Every node of a valid candidate is charged once per query that reads it: nothing is materialized, so a shared node is recomputed for each.
#[test]
fn stage3_plan_selection_charges_each_node_per_consuming_query() {
    assert_each_node_charged_per_consuming_query(&pipeline());
}

/// `asap_sketchlib` has no Theta sketch, so no candidate builds one.
#[test]
fn stage3_plan_selection_has_no_theta_candidate() {
    let run = pipeline();
    for (id, cost) in &run.selection.costs {
        assert!(
            !cost
                .per_node
                .values()
                .any(|n| n.detail.starts_with("build Theta")),
            "{id} builds Theta"
        );
    }
}

/// A UnivMon estimate has a guarantee, so plan selection judges it against its target instead of rejecting it for having none.
#[test]
#[ignore = "needs a UnivMon entropy accuracy model; L2 and the distinct count have one"]
fn stage3_plan_selection_judges_univmon_with_an_accuracy_model() {
    let run = pipeline();
    let invalid = run.invalid();
    for c in &run.logical {
        for p in run.physical_of(c) {
            let Some(reason) = invalid.get(p.id.as_str()) else {
                continue;
            };
            for q in 0..3 {
                if query_option(&c.dag, &c.query_roots, q) == "UnivMon" {
                    let unguaranteed = format!("q{}: the output has no guarantee", q + 1);
                    assert!(!reason.starts_with(&unguaranteed), "{}: {reason}", p.id);
                }
            }
        }
    }
}

/// The physical candidates that answer Q1 with HLL and Q2 and Q3 exactly:
/// their validity depends only on Q1's HLL estimate.
fn hll_only_candidates(run: &Run) -> Vec<&str> {
    let ids: Vec<_> = run
        .logical
        .iter()
        .filter(|c| {
            !shares_a_summary(c)
                && (0..3)
                    .map(|q| query_option(&c.dag, &c.query_roots, q))
                    .eq(["Hll", "exact", "exact"])
        })
        .flat_map(|c| run.physical_of(c).map(|p| p.id.as_str()))
        .collect();
    assert!(!ids.is_empty(), "a candidate answers Q1 with HLL alone");
    ids
}

/// With the deployment's bound on distinct items per evaluation, plan
/// selection binds classic HLL's failure probability and accepts Q1's HLL
/// estimate; without a bound it stays unknown and the target unmet. Pass 1
/// sizes HLL from ε = 0.02 alone (precision 12), which certifies δ = 0.01
/// for at most 9 distinct items.
#[test]
fn stage3_plan_selection_accepts_hll_given_a_distinct_bound() {
    let bounded = pipeline_with_distinct_bound(Some(9));
    for id in hll_only_candidates(&bounded) {
        assert!(
            bounded.cost(id).is_some(),
            "{id}: {:?}",
            bounded.invalid().get(id)
        );
    }
    let unbounded = pipeline_with_distinct_bound(None);
    let invalid = unbounded.invalid();
    for id in hll_only_candidates(&unbounded) {
        let reason = invalid.get(id).unwrap_or_else(|| panic!("{id} is invalid"));
        assert!(reason.starts_with("q1:"), "{id}: {reason}");
        assert!(
            reason.contains("failure probability None"),
            "{id}: {reason}"
        );
    }
}

/// A bound too large for the precision Pass 1 chose leaves δ over its
/// target: one inside classic HLL's certified range gives a failure
/// probability above 0.01, and one outside it certifies nothing (δ = 1).
#[test]
fn stage3_plan_selection_rejects_hll_whose_distinct_bound_is_too_large() {
    for bound in [10, 100, 10_000_000] {
        let run = pipeline_with_distinct_bound(Some(bound));
        let invalid = run.invalid();
        for id in hll_only_candidates(&run) {
            let reason = invalid
                .get(id)
                .unwrap_or_else(|| panic!("bound {bound}: {id} is invalid"));
            assert!(reason.starts_with("q1:"), "{id}: {reason}");
            assert!(
                !reason.contains("failure probability None"),
                "{id}: {reason}"
            );
        }
    }
}

/// Stage 1 does not read the distinct bound: its candidates are the same
/// with and without one, and Q1's HLL estimate leaves its failure
/// probability symbolic in the source's bound.
#[test]
fn stage1_logical_asap_optimization_leaves_the_hll_failure_probability_symbolic() {
    let bounded = pipeline_with_distinct_bound(Some(9));
    let unbounded = pipeline_with_distinct_bound(None);
    let stage1 = |run: &Run| {
        run.logical
            .iter()
            .map(|c| format!("{} {:?}", c.id, c.roots))
            .collect::<Vec<_>>()
    };
    assert_eq!(stage1(&bounded), stage1(&unbounded));
    let mut seen = 0;
    for c in unbounded
        .logical
        .iter()
        .filter(|c| !shares_a_summary(c) && query_option(&c.dag, &c.query_roots, 0) == "Hll")
    {
        let QueryRoot::Operator(q1) = &c.roots[0] else {
            panic!("operator root")
        };
        let guarantee = q1.guarantee.as_ref().expect("Q1 has a guarantee");
        assert_eq!(
            guarantee.spec.failure_probability,
            ProbabilityExpr::Unknown {
                statistic: "classic_hll_failure_probability[p=12](max_distinct[flows_src_ip])"
                    .into()
            },
            "{}",
            c.id
        );
        seen += 1;
    }
    assert!(seen > 0, "a candidate answers Q1 with HLL");
}

/// The physical candidates that answer Q`query` with UnivMon and the other
/// two exactly: their validity depends only on that UnivMon estimate.
fn univmon_only_candidates(run: &Run, query: usize) -> Vec<&str> {
    let ids: Vec<_> = run
        .logical
        .iter()
        .filter(|c| {
            !shares_a_summary(c)
                && (0..3).all(|q| {
                    query_option(&c.dag, &c.query_roots, q)
                        == if q == query { "UnivMon" } else { "exact" }
                })
        })
        .flat_map(|c| run.physical_of(c).map(|p| p.id.as_str()))
        .collect();
    assert!(
        !ids.is_empty(),
        "a candidate answers Q{} with UnivMon alone",
        query + 1
    );
    ids
}

/// Q1's UnivMon is sized for (0.02, 0.01): a 337,500-key heap on 19
/// layers. Its distinct count is certified once the deployment bounds the
/// source's distinct items, both at 9 (the top layer is always complete)
/// and at 10M (7 layers suffice, where classic HLL certifies nothing);
/// without a bound its failure probability stays unknown.
#[test]
fn stage3_plan_selection_accepts_univmon_distinct_given_a_distinct_bound() {
    for bound in [9, 10_000_000] {
        let run = pipeline_with_distinct_bound(Some(bound));
        for id in univmon_only_candidates(&run, 0) {
            assert!(
                run.cost(id).is_some(),
                "{bound} {id}: {:?}",
                run.invalid().get(id)
            );
        }
    }
    let unbounded = pipeline_with_distinct_bound(None);
    let invalid = unbounded.invalid();
    for id in univmon_only_candidates(&unbounded, 0) {
        let reason = invalid.get(id).unwrap_or_else(|| panic!("{id} is invalid"));
        assert!(reason.starts_with("q1:"), "{id}: {reason}");
        assert!(
            reason.contains("failure probability None"),
            "{id}: {reason}"
        );
    }
}

/// A distinct bound past what 19 layers cover for Q1's heap
/// (337,500 · 2^17) certifies nothing.
#[test]
fn stage3_plan_selection_rejects_univmon_distinct_past_its_layers() {
    let run = pipeline_with_distinct_bound(Some(337_500 << 17 | 1));
    let invalid = run.invalid();
    for id in univmon_only_candidates(&run, 0) {
        let reason = invalid.get(id).unwrap_or_else(|| panic!("{id} is invalid"));
        assert!(reason.starts_with("q1:"), "{id}: {reason}");
    }
}

/// Q3's L2 at (0.01, 0.01) is certified from layer 0 alone: a 5 × 56,817
/// Count Sketch on one layer, with no data-workload statistic.
#[test]
fn stage3_plan_selection_accepts_univmon_l2() {
    let run = pipeline();
    assert_eq!(
        own_univmon(&run, 2),
        SketchParams::UnivMon {
            heap_size: 1,
            sketch_rows: 5,
            sketch_cols: 56_817,
            layers: 1,
            distinct_failure_probability: None,
        }
    );
    for id in univmon_only_candidates(&run, 2) {
        assert!(run.cost(id).is_some(), "{id}: {:?}", run.invalid().get(id));
    }
}

/// One materialized UnivMon for all three costs no more than three separate UnivMons.
#[test]
#[ignore = "needs one UnivMon for all three (#595, #596), a UnivMon accuracy model (#621) and materializing it (#604): not materialized, it is charged once per consuming query"]
fn stage3_plan_selection_shared_univmon_costs_no_more_than_three() {
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
fn stage3_plan_selection_selected_plan_has_at_most_one_univmon() {
    let run = pipeline();
    let selected = run.physical(&run.selection.selected);
    let count = sketch_builds(selected.dag())
        .iter()
        .filter(|(_, a, _)| *a == SketchAlgorithm::UnivMon)
        .count();
    assert!(count <= 1, "{}: {count} UnivMons", selected.id);
}

/// Stage 3 over the candidates that answer Q1 with KMV and Q2 and Q3
/// exactly selects one, and the executor runs its Q1: the distinct count of
/// the window's sample values. Below KMV's `k` distinct values its estimate
/// is exact. Only Q1 is compiled: the exact entropy and L2 of Q2 and Q3 do
/// not compile yet ("per-entity aggregate requires an explicit entity
/// binding").
#[test]
fn stage3_plan_selection_selected_kmv_plan_executes_end_to_end() {
    use asap_aware_mapping::stage3_plan_selection::select_plan;
    use asap_aware_mapping::PlanningModels;
    use asap_physical_operators::physical_planner::{compile, promql_rows, InputContract};
    use asap_physical_operators::runtime::Scope;
    use asap_physical_operators::values::{Batch, Value};
    use std::sync::Arc;

    let workload = standin_workload();
    let roots = lower_promql(&workload);
    let run = run_stages(&workload, roots.clone());
    let kmv: Vec<_> = run
        .logical
        .iter()
        .filter(|c| {
            !shares_a_summary(c)
                && (0..3)
                    .map(|q| query_option(&c.dag, &c.query_roots, q))
                    .eq(["Kmv", "exact", "exact"])
        })
        .flat_map(|c| run.physical_of(c).map(|p| p.candidate.clone()))
        .collect();
    assert!(!kmv.is_empty(), "a candidate answers Q1 with KMV");
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let targets: Vec<_> = entries
        .iter()
        .zip(&roots)
        .map(|(e, root)| Some(e.requirements.accuracy.requested().for_root(root).unwrap()))
        .collect();
    let recurrence: Vec<_> = entries.iter().map(|e| e.recurrence.clone()).collect();
    let selection = select_plan(
        &kmv,
        &targets,
        &recurrence,
        workload.data_workload.as_ref().unwrap(),
        PlanningModels::builtin(),
    )
    .expect("Stage 3 selects a KMV plan");
    let p = run.physical(&selection.selected);
    assert_eq!(query_option(p.dag(), &p.query_roots, 0), "Kmv");

    // 1,000 flow records over 50 s with 300 distinct source IPs.
    let distinct = 300u32;
    let samples: Vec<(i64, f64)> = (0..1_000u32)
        .map(|i| (i64::from(i) * 50, f64::from(i * 7 % distinct)))
        .collect();
    let labels = BTreeMap::from([("__name__".to_string(), "flows_src_ip".to_string())]);
    let dag = p.dag();
    let mut inputs = BTreeMap::new();
    let mut batches = BTreeMap::new();
    for node in &dag.nodes {
        if relational(&node.payload).as_deref() == Some("time_range") {
            let schema = Arc::new(node.output_schema.clone());
            let rows = samples
                .iter()
                .map(|(ts, v)| promql_rows::series_row(&schema, &labels, *ts, *v).unwrap())
                .collect();
            batches.insert(
                node.id as u64,
                Batch::try_new(schema.clone(), rows).unwrap(),
            );
            inputs.insert(node.id as u64, InputContract::bounded(schema));
        }
    }
    let q1 = p.query_roots[0] as u64;
    let plan = compile(dag, inputs, &[q1]).unwrap_or_else(|e| panic!("{}: {e}", p.id));
    let batches = plan
        .input_contracts()
        .map(|(id, _)| (id, batches[&id].clone()))
        .collect();
    let result = physical_common::execute(
        &plan,
        batches,
        Scope::Query {
            evaluation_time_ms: 55_000,
            revision: 1,
        },
    );
    let values: Vec<f64> = result[0]
        .iter()
        .flat_map(|batch| batch.rows())
        .flat_map(|row| row.iter())
        .filter_map(|value| match value {
            Value::Float64(v) => Some(*v),
            _ => None,
        })
        .collect();
    assert_eq!(values, [f64::from(distinct)], "{}", p.id);
}
