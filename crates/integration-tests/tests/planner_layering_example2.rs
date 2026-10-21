//! Acceptance tests for #509 "Example 2: One summary for several
//! computations — the summary-capability rule in Pass 2".
//!
//! Spec: `docs/design_docs/proposals/planner-layering-example2-acceptance.md`.
//! Written by a test designer who does not implement the stages. Tests that
//! need an unimplemented feature are `#[ignore]`d, naming it and the PR that
//! delivers it.
//!
//! The doc's workload is SQL; the SQL frontend lowers Q1 to a distinct
//! count and recognizes Q2 and Q3 as `Entropy` and `L2` (Q3 with a Float64
//! product). Its time filter stays a scan predicate, so the planning
//! stages also run on a PromQL stand-in with the same structure: three
//! statistics of one input over the same 1-min window, through the
//! frontend's `distinct_over_time`, `entropy_over_time` and `l2_over_time`.

mod physical_common;
mod planner_layering_common;

use std::collections::{BTreeMap, BTreeSet};

use asap_frontend_sql::{lower_sql_dialect, SqlCatalog};
use asap_types::ir::properties::ProbabilityExpr;
use asap_types::ir::properties::{AccuracyScope, AccuracyTarget};
use asap_types::ir::schema::DataType;
use asap_types::ir::schema::{Field, Schema, SketchAlgorithm, SketchParams, SketchStatistic};
use asap_types::ir::QueryRoot;
use asap_types::workload::{
    DataArrival, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query, QueryLanguage,
    QueryRequirements, QueryWorkload, RepeatedDemand, RepeatingEntry, RepetitionInterval,
    SourceFacts, SqlDialect,
};
use asap_logical_asap_optimization::accuracy::estimators::univmon;
use asap_types::ir::flat::flatten;
use planner_layering_common::*;

// ── Example 2 workload ───────────────────────────────────────────────────

const SQL_Q1: &str =
    "SELECT COUNT(DISTINCT src_ip) FROM flows WHERE ts >= now() - INTERVAL '1 minute'";
const SQL_Q2: &str = "SELECT -SUM(p * LN(p)) FROM (\
    SELECT COUNT(*) * 1.0 / SUM(COUNT(*)) OVER () AS p FROM flows \
    WHERE ts >= now() - INTERVAL '1 minute' GROUP BY src_ip)";
/// Q3 casts the counts to DOUBLE: the design's Int64 `SUM(c * c)` can
/// overflow, so the SQL frontend recognizes only the Float64 product.
const SQL_Q3: &str = "SELECT SQRT(SUM(CAST(c AS DOUBLE) * CAST(c AS DOUBLE))) FROM (\
    SELECT src_ip, COUNT(*) AS c FROM flows \
    WHERE ts >= now() - INTERVAL '1 minute' GROUP BY src_ip)";

/// The metric the PromQL stand-in reads.
const STANDIN_METRIC: &str = "flows_src_ip";
/// PromQL stand-in for Q1–Q3 (see the module docs).
const PROMQL_Q1: &str = "distinct_over_time(flows_src_ip[1m])";
const PROMQL_Q2: &str = "entropy_over_time(flows_src_ip[1m])";
const PROMQL_Q3: &str = "l2_over_time(flows_src_ip[1m])";

/// (ε, δ) of Q1, Q2, Q3. Q3 is the strictest.
const ACCURACY: [(f64, f64); 3] = [(0.02, 0.01), (0.05, 0.01), (0.01, 0.01)];

fn panel(query: &str, (epsilon, delta): (f64, f64), scope: AccuracyScope) -> RepeatingEntry {
    RepeatingEntry {
        query: Query(query.into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
        requirements: QueryRequirements {
            accuracy: AccuracyTarget {
                accuracy_scope: scope,
                ..AccuracyTarget::max_error(epsilon).failure_probability(delta)
            },
            max_latency_ms: None,
        },
        predictability: Predictability::Predictable { known_at: None },
        range_evaluation: None,
    }
}

/// The shared data workload with 10,000,000 distinct source IPs, in the
/// SQL `flows` table and the PromQL stand-in's `flows_src_ip` metric
/// (sampled every 15 s; SQL needs no sample interval).
fn data_workload() -> DataWorkload {
    let ten_million = SourceFacts {
        series_count: Some(10_000_000),
        ..SourceFacts::default()
    };
    let sampled = SourceFacts {
        sample_interval: Some(DurationMs(15_000)),
        ..ten_million.clone()
    };
    DataWorkload {
        arrival: DataArrival::Ingesting {
            rows_per_second: 1_000_000.0 / 15.0,
        },
        sources: [
            ("flows".to_string(), ten_million),
            (STANDIN_METRIC.to_string(), sampled),
        ]
        .into(),
    }
}

/// The SQL queries' targets cover their whole result, as the doc's do. The
/// PromQL stand-in states them per value: it reads one series, but the
/// series count of `flows_src_ip` (the 10M source IPs) is what plan
/// selection takes as the number of values a PromQL vector returns, so a
/// whole-result target would be checked over 10M values (#669b).
fn workload(language: QueryLanguage, queries: [&str; 3]) -> PlanningWorkload {
    let sql = matches!(language, QueryLanguage::SQL(_));
    let scope = if sql {
        AccuracyScope::WholeResult
    } else {
        AccuracyScope::PerValue
    };
    PlanningWorkload {
        query_workload: QueryWorkload {
            language,
            query_batch: None,
            repeating_queries: Some(
                queries
                    .iter()
                    .zip(ACCURACY)
                    .map(|(q, accuracy)| panel(q, accuracy, scope))
                    .collect(),
            ),
        },
        data_workload: Some(data_workload()),
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
            entry.requirements.accuracy.clone(),
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
            .sources
            .get_mut(STANDIN_METRIC)
            .expect("the stand-in's source")
            .max_distinct_per_evaluation = Some(bound);
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
async fn stage0_query_language_frontends_sql_q2_q3_lower_to_entropy_and_l2() {
    let roots = lower_sql(&sql_workload()).await;
    assert!(frontend_operations(&roots[1]).contains(&"aggregate:frequency_entropy".to_string()));
    assert!(frontend_operations(&roots[2]).contains(&"aggregate:frequency_l2".to_string()));
}

/// Once the SQL forms are recognized, the SQL workload has the stand-in's Pass 1 options.
#[tokio::test]
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

/// A UnivMon shared by several statistics is sized for every consumer:
/// each consumer is sized alone, and the shared build takes each
/// parameter's largest value among them.
#[test]
fn stage1_logical_asap_optimization_shared_univmon_takes_each_parameters_largest_own_size() {
    let run = pipeline();
    let own = |q| match own_univmon(&run, q) {
        SketchParams::UnivMon {
            heap_size,
            sketch_rows,
            sketch_cols,
            layers,
            ..
        } => [heap_size, sketch_rows, sketch_cols, u32::from(layers)],
        other => panic!("{other:?}"),
    };
    let mut seen = 0;
    for queries in [vec![0, 1, 2], vec![0, 1], vec![0, 2], vec![1, 2]] {
        let mut want = [0u32; 4];
        for &q in &queries {
            for (w, p) in want.iter_mut().zip(own(q)) {
                *w = (*w).max(p);
            }
        }
        let want = SketchParams::UnivMon {
            heap_size: want[0],
            sketch_rows: want[1],
            sketch_cols: want[2],
            layers: want[3] as u8,
            // Ignored by equality.
            relative_error: None,
        };
        for c in &run.logical {
            if let Some(params) = shared_univmon(c, &queries) {
                seen += 1;
                assert_eq!(params, want, "{} {queries:?}", c.id);
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

/// No candidate is discarded before plan selection: each logical candidate keeps one physical candidate with nothing materialized, plus one per other materialization assignment.
#[test]
fn stage2_physical_asap_optimization_keeps_every_logical_candidate() {
    assert_one_unmaterialized_physical_candidate_per_logical(&pipeline());
}

/// Physical ASAP-aware optimization keeps the shared UnivMon as one build node read by all three queries.
#[test]
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

/// The physical candidates that answer Q1 with `algorithm` and Q2 and Q3
/// exactly: their validity depends only on Q1's estimate.
fn q1_only_candidates<'a>(run: &'a Run, algorithm: &str) -> Vec<&'a str> {
    let ids: Vec<_> = run
        .logical
        .iter()
        .filter(|c| {
            !shares_a_summary(c)
                && (0..3)
                    .map(|q| query_option(&c.dag, &c.query_roots, q))
                    .eq([algorithm, "exact", "exact"])
        })
        .flat_map(|c| run.physical_of(c).map(|p| p.id.as_str()))
        .collect();
    assert!(
        !ids.is_empty(),
        "a candidate answers Q1 with {algorithm} alone"
    );
    ids
}

fn hll_only_candidates(run: &Run) -> Vec<&str> {
    q1_only_candidates(run, "Hll")
}

/// With the deployment's bound on distinct items per evaluation, plan
/// selection binds classic HLL's failure probability and accepts Q1's HLL
/// estimate; without a bound it stays unknown and the target unmet. Pass 1
/// sizes HLL from ε = 0.02 and δ = 0.01 (#694): `m ≥ 1/(2ε²δ)` gives
/// precision 17, which certifies δ = 0.01 at ε = 0.02 for bounds up to
/// 1,842 distinct items.
#[test]
fn stage3_plan_selection_accepts_hll_given_a_distinct_bound() {
    for bound in [9, 1_842] {
        let bounded = pipeline_with_distinct_bound(Some(bound));
        for id in hll_only_candidates(&bounded) {
            assert!(
                bounded.cost(id).is_some(),
                "bound {bound}: {id}: {:?}",
                bounded.invalid().get(id)
            );
        }
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

/// A bound too large for the precision Pass 1 chose (17 certifies at most
/// 1,842 distinct items) is met by a larger precision: plan selection
/// enlarges HLL until its failure probability over the bound at ε = 0.02
/// meets the target, up to precision 18 (#669b), which certifies up to
/// 3,791. Past that, up to Example 2's 10M source IPs, classic HLL's
/// linear-counting contract certifies nothing.
#[test]
fn stage3_plan_selection_enlarges_hll_for_a_larger_distinct_bound() {
    let run = pipeline_with_distinct_bound(Some(3_000));
    for id in hll_only_candidates(&run) {
        assert!(run.cost(id).is_some(), "{id}: {:?}", run.invalid().get(id));
    }
    for bound in [10_000, 10_000_000] {
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

/// KMV certifies Q1 over the 10M source IPs without a distinct bound
/// (#694): its bound holds for any number of distinct items, and Pass 1
/// sizes `k` for the requested δ (#700), so a stricter δ = 10⁻⁴ is met
/// too, by a larger `k`.
#[test]
fn stage3_plan_selection_accepts_kmv_for_any_failure_probability_without_a_bound() {
    let mut ks = Vec::new();
    for delta in [0.01, 1e-4] {
        let mut workload = standin_workload();
        let q1 = &mut workload
            .query_workload
            .repeating_queries
            .as_mut()
            .expect("repeating queries")[0];
        q1.requirements.accuracy = AccuracyTarget {
            accuracy_scope: AccuracyScope::PerValue,
            ..AccuracyTarget::max_error(0.02).failure_probability(delta)
        };
        let run = run_stages(&workload, lower_promql(&workload));
        for id in q1_only_candidates(&run, "Kmv") {
            assert!(
                run.cost(id).is_some(),
                "δ = {delta}: {id}: {:?}",
                run.invalid().get(id)
            );
            let k: BTreeSet<_> = sketch_builds(run.physical(id).dag())
                .into_iter()
                .filter_map(|(_, _, params)| match params {
                    SketchParams::Kmv { k, .. } => Some(k),
                    _ => None,
                })
                .collect();
            ks.push((delta, k));
        }
    }
    assert!(
        ks.iter().any(|(d, k)| *d == 0.01 && k.contains(&26_656)),
        "{ks:?}"
    );
    assert!(
        ks.iter()
            .any(|(d, k)| *d == 1e-4 && k.iter().all(|k| *k > 26_656)),
        "{ks:?}"
    );
}

/// The physical candidates among `ids` that build their sketch as a
/// tumbling-pane `PaneChain`.
fn tumbling<'a>(run: &Run, ids: Vec<&'a str>) -> Vec<&'a str> {
    ids.into_iter()
        .filter(|id| {
            let dag = run.physical(id).dag();
            sketch_builds(dag).iter().any(|(build, _, _)| {
                matches!(window_form(dag, *build), WindowForm::Tumbling { .. })
            })
        })
        .collect()
}

/// Plan selection's enlargement reaches a sketch carried by a tumbling-pane
/// `PaneChain` too (#713): Q1's HLL over a bound of 3,000 is valid only at
/// precision 18, above Pass 1's 17, so a valid tumbling-pane HLL candidate
/// had its chain's precision enlarged.
#[test]
fn stage3_plan_selection_enlarges_tumbling_pane_hll() {
    let run = pipeline_with_distinct_bound(Some(3_000));
    let hll = tumbling(&run, hll_only_candidates(&run));
    assert!(!hll.is_empty(), "a tumbling-pane HLL candidate answers Q1");
    for id in hll {
        assert!(run.cost(id).is_some(), "{id}: {:?}", run.invalid().get(id));
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
                statistic:
                    "classic_hll_failure_probability[p=17,eps=0.02](max_distinct[flows_src_ip])"
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

/// The stand-in with Q`query`'s target set to `(ε, δ)` per value and the
/// deployment's distinct bound for its source, when given. The other two
/// keep Example 2's targets.
fn standin_at(query: usize, (epsilon, delta): (f64, f64), bound: Option<u64>) -> PlanningWorkload {
    let mut workload = standin_with_distinct_bound(bound);
    workload
        .query_workload
        .repeating_queries
        .as_mut()
        .expect("repeating queries")[query]
        .requirements
        .accuracy = AccuracyTarget {
        accuracy_scope: AccuracyScope::PerValue,
        ..AccuracyTarget::max_error(epsilon).failure_probability(delta)
    };
    workload
}

fn run_of(workload: &PlanningWorkload) -> Run {
    run_stages(workload, lower_promql(workload))
}

/// The UnivMon of candidate `id` and its root guarantee for Q`query`, as
/// plan selection resizes them for `workload`'s distinct bound, with the
/// failure probability bound for `bound`.
fn sized_univmon(
    run: &Run,
    workload: &PlanningWorkload,
    id: &str,
    query: usize,
    bound: u64,
) -> (SketchParams, Option<(f64, f64)>) {
    let targets: Vec<_> = workload
        .query_workload
        .entries()
        .map(|entry| entry.requirements.accuracy.clone())
        .collect();
    let roots = asap_plan_selection::resize_for_failure_probability(
        &run.physical(id).candidate.roots,
        &targets,
        workload.data_workload.as_ref().expect("data workload"),
    )
    .expect("resized");
    let params = sketch_builds(&flatten(&roots).0)
        .into_iter()
        .find(|(_, algorithm, _)| *algorithm == SketchAlgorithm::UnivMon)
        .expect("a UnivMon")
        .2;
    let QueryRoot::Operator(root) = &roots[query] else {
        panic!("an operator root")
    };
    let guarantee = root.guarantee.as_ref().map(|guarantee| {
        let bound = guarantee.bind(&|statistic| {
            let (layers, _) = univmon::parse_layers_statistic(statistic)?;
            Some(univmon::failure_probability(layers, bound))
        });
        (
            bound.spec.error.evaluate().expect("an error bound"),
            bound
                .spec
                .failure_probability
                .evaluate()
                .expect("a failure probability"),
        )
    });
    (params, guarantee)
}

/// UnivMon's guarantee is the paper's: relative error ε at failure
/// probability 0.3, given layers that cover the source's distinct items
/// (Braverman–Ostrovsky, arXiv:1011.2571, Theorem 4.2). Q3's L2 and Q1's
/// distinct count at (0.1, 0.3) over a source of at most 16 distinct items
/// are accepted, sized by the paper's conditions: `⌈log2 16⌉ + 1 = 5`
/// layers, a heap of `5³/0.1² = 12,500` and a 1 × 2^25 Count Sketch.
#[test]
fn stage3_plan_selection_accepts_univmon_l2_and_distinct_at_the_paper_failure_probability() {
    for query in [2, 0] {
        let workload = standin_at(query, (0.1, 0.3), Some(16));
        let run = run_of(&workload);
        for id in univmon_only_candidates(&run, query) {
            assert!(run.cost(id).is_some(), "{id}: {:?}", run.invalid().get(id));
            assert_eq!(
                sized_univmon(&run, &workload, id, query, 16),
                (
                    SketchParams::UnivMon {
                        heap_size: 12_500,
                        sketch_rows: 1,
                        sketch_cols: 1 << 25,
                        layers: 5,
                        relative_error: Some(0.1),
                    },
                    Some((0.1, 0.3))
                ),
                "{id}"
            );
        }
    }
}

/// A target asking for a failure probability below the paper's 0.3 is
/// not met by UnivMon: Q3's L2 and Q1's distinct count at (0.1, 0.05) are
/// rejected, with the same bound and sizing as at 0.3.
#[test]
fn stage3_plan_selection_rejects_univmon_below_the_paper_failure_probability() {
    for query in [2, 0] {
        let run = run_of(&standin_at(query, (0.1, 0.05), Some(16)));
        let invalid = run.invalid();
        for id in univmon_only_candidates(&run, query) {
            let reason = invalid.get(id).unwrap_or_else(|| panic!("{id} is invalid"));
            assert!(
                reason.starts_with(&format!("q{}:", query + 1)),
                "{id}: {reason}"
            );
        }
    }
}

/// Entropy has no UnivMon guarantee even at (0.1, 0.3): the paper's
/// statement bounds `Σ f log f` relatively, which bounds the entropy
/// `log2 m − Σ f log2 f / m` only to `± ε log2 m`, and the Count Sketch
/// rule does not cover `x log x`'s heavy keys.
#[test]
fn stage3_plan_selection_rejects_univmon_entropy_at_the_paper_failure_probability() {
    let run = run_of(&standin_at(1, (0.1, 0.3), Some(16)));
    let invalid = run.invalid();
    for id in univmon_only_candidates(&run, 1) {
        let reason = invalid.get(id).unwrap_or_else(|| panic!("{id} is invalid"));
        assert!(
            reason.starts_with("q2: the output has no guarantee"),
            "{id}: {reason}"
        );
    }
}

/// Without the source's distinct bound, or with one past what the
/// library's 19 layers cover (`⌈log2 n⌉ + 1 ≤ 19`, so `n ≤ 2^18`), the
/// layers condition is not met and UnivMon is rejected; at `n = 2^18` and
/// (0.5, 0.3) it is accepted on 19 layers.
#[test]
fn stage3_plan_selection_rejects_univmon_past_its_layers() {
    let accepted = run_of(&standin_at(0, (0.5, 0.3), Some(1 << 18)));
    for id in univmon_only_candidates(&accepted, 0) {
        assert!(
            accepted.cost(id).is_some(),
            "{id}: {:?}",
            accepted.invalid().get(id)
        );
    }
    for bound in [None, Some((1 << 18) + 1), Some(10_000_000)] {
        let run = run_of(&standin_at(0, (0.5, 0.3), bound));
        let invalid = run.invalid();
        for id in univmon_only_candidates(&run, 0) {
            let reason = invalid
                .get(id)
                .unwrap_or_else(|| panic!("{bound:?}: {id} is invalid"));
            assert!(reason.starts_with("q1:"), "{bound:?} {id}: {reason}");
        }
    }
}

/// At Example 2's own targets (δ = 0.01) and 10M source IPs, no UnivMon
/// is accepted: δ is below the paper's 0.3.
#[test]
fn stage3_plan_selection_rejects_univmon_at_example2s_targets() {
    let run = pipeline_with_distinct_bound(Some(10_000_000));
    let invalid = run.invalid();
    for query in 0..3 {
        for id in univmon_only_candidates(&run, query) {
            assert!(invalid.contains_key(id), "Q{}: {id}", query + 1);
        }
    }
}

/// SQL Q3's L2 through UnivMon keeps its guarantee: the exact population
/// count joined beside it (a cross join, no key) and the `CASE` deciding the
/// empty-input NULL on that exact count keep the estimate's guarantee, so
/// plan selection accepts it, at the paper's failure probability (0.1, 0.3)
/// over at most 16 source IPs.
#[tokio::test]
async fn stage3_plan_selection_accepts_sql_univmon_l2_beside_its_exact_population_count() {
    let mut workload = sql_workload();
    let q3 = &mut workload
        .query_workload
        .repeating_queries
        .as_mut()
        .expect("repeating queries")[2];
    q3.requirements.accuracy = AccuracyTarget {
        accuracy_scope: AccuracyScope::WholeResult,
        ..AccuracyTarget::max_error(0.1).failure_probability(0.3)
    };
    workload
        .data_workload
        .as_mut()
        .expect("data workload")
        .sources
        .get_mut("flows")
        .expect("flows")
        .max_distinct_per_evaluation = Some(16);
    let run = run_stages(&workload, lower_sql(&workload).await);
    for id in univmon_only_candidates(&run, 2) {
        assert!(run.cost(id).is_some(), "{id}: {:?}", run.invalid().get(id));
    }
}

/// One materialized UnivMon for all three costs no more than three separate UnivMons.
#[test]
#[ignore = "needs a UnivMon entropy accuracy model and materializing the shared UnivMon (#604): not materialized, it is charged once per consuming query"]
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
    use asap_executor::physical_planner::{compile, promql_rows, InputContract};
    use asap_executor::runtime::Scope;
    use asap_executor::values::{Batch, Value};
    use asap_plan_selection::select_plan;
    use asap_plan_selection::PlanningModels;
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
    let selection = select_plan(
        &kmv,
        &entries,
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
        .flat_map(|batch| batch.to_rows())
        .flatten()
        .filter_map(|value| match value {
            Value::Float64(v) => Some(v),
            _ => None,
        })
        .collect();
    assert_eq!(values, [f64::from(distinct)], "{}", p.id);
}
