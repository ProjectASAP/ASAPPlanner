//! Rule-fixture and production acceptance for Example 2: shared summaries across statistics.
//!
//! Numerical, accuracy, and operator assertions use explicitly supplied
//! configurations of the existing rules and physical evaluator. They do not
//! assume a bounded MILP run archives every supported workload plan. Separate
//! production checks exercise memo search and validated output selection.
//! Unsupported algorithms retain their existing ignored tests.

use crate::physical_common;
use crate::planner_layering_common;

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

// Targeted rule fixtures: exact, each local summary in isolation, and the
// all/pairwise UnivMon consumers needed by compatibility and cost assertions.
// Production optimization is tested separately; this is not a Cartesian search.
fn rule_fixture(workload: &PlanningWorkload, roots: Vec<QueryRoot>) -> Run {
    use asap_logical_asap_optimization::pass1::per_computation_candidates::{
        compose_logical_candidate, generate_per_computation_candidates,
        HowToComputeAggregateMeasure,
    };
    use asap_logical_asap_optimization::pass2::asap_aware_cse::{
        capability_sharing_candidates, pane_candidates, summary_capability_pass2_candidates,
        window_composition_pass2_candidates,
    };
    let local =
        generate_per_computation_candidates(roots.iter().cloned().enumerate().collect()).unwrap();
    let exact = vec![0; local.targets.len()];
    let mut choices = BTreeSet::from([exact.clone()]);
    let mut univmon = exact.clone();
    let mut shared_targets = Vec::new();
    for (target, options) in local.targets.iter().enumerate() {
        for option in 1..options.choice_count() {
            let mut choice = exact.clone();
            choice[target] = option;
            choices.insert(choice);
        }
        if let Some(option) = (0..options.choice_count()).find(|&choice| {
            matches!(
                options.computation_for(choice),
                Some((
                    HowToComputeAggregateMeasure::Summary(SketchAlgorithm::UnivMon),
                    _
                ))
            )
        }) {
            univmon[target] = option;
            shared_targets.push(target);
        }
    }
    choices.insert(univmon.clone());
    for &third in &shared_targets {
        for option in 0..local.targets[third].choice_count() {
            let mut choice = univmon.clone();
            choice[third] = option;
            choices.insert(choice);
        }
    }
    let grid: Vec<_> = workload
        .query_workload
        .entries()
        .map(|entry| entry.evaluation_grid_ms())
        .collect();
    let mut computations = Vec::new();
    for choice in choices {
        if local.absorbed_choice_varies(&choice) {
            continue;
        }
        let whole: Vec<_> = compose_logical_candidate(&local, &choice)
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect();
        computations.push(whole.clone());
        let shared = share_identical(&whole);
        if distinct_nodes(&shared) < distinct_nodes(&whole) {
            computations.push(shared);
        }
        let capabilities = capability_sharing_candidates(&whole);
        computations.extend(
            summary_capability_pass2_candidates(&whole, &capabilities)
                .unwrap()
                .into_iter()
                .map(|candidate| candidate.roots.unwrap()),
        );
        let panes = pane_candidates(&whole, &grid);
        // Isolate one pane configuration at a time; no Cartesian products of
        // independent window implementations are needed for the error tests.
        for pane in panes {
            computations.extend(
                window_composition_pass2_candidates(&whole, &[pane])
                    .unwrap()
                    .into_iter()
                    .map(|candidate| candidate.roots.unwrap()),
            );
        }
    }
    run_explicit_candidates(
        workload,
        roots,
        computations,
        asap_plan_selection::PlanningModels::builtin(),
    )
}

#[test]
fn production_memo_selects_a_validated_example2_plan() {
    let workload = standin_workload();
    let run = run_stages(&workload, lower_promql(&workload));
    assert_selects_cheapest_valid(&run);
    assert_eq!(run.selection.accepted[&run.selection.selected].len(), 3);
}

fn pipeline() -> Run {
    let workload = standin_workload();
    rule_fixture(&workload, lower_promql(&workload))
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
    rule_fixture(&workload, lower_promql(&workload))
}

/// Plans composed of the memo's candidates, priced by Stage 2 and plan
/// selection: every query on its group's first candidate; each query alone
/// on its own UnivMon; and, per UnivMon state group the memo shares between
/// queries, those queries reading it.
fn memo_pipeline() -> Run {
    use asap_integration_tests::planning::{compose_memo_plan, expanded_memo, own_choices};
    use asap_logical_asap_optimization::pass2::memo::candidates::{
        CandidateForMemoGroup, OriginOfCandidateForMemoGroup,
    };
    use asap_types::ir::schema::FieldDataType;
    use asap_types::ir::{ASAPOp, OperatorNode};
    let workload = standin_workload();
    let roots = lower_promql(&workload);
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let memo = expanded_memo(roots.iter().cloned().enumerate().collect(), &entries);
    let queries = roots.len();
    let is_univmon = |node: &std::rc::Rc<OperatorNode>| {
        matches!(node.asap(), Some(ASAPOp::SummaryAgg {
            family: FieldDataType::Sketch(kind, _), ..
        }) if *kind.algorithm() == SketchAlgorithm::UnivMon)
    };
    // Query `q`'s topmost own group's first candidate `keep` accepts.
    let own = |q: usize, keep: &dyn Fn(&CandidateForMemoGroup) -> bool| {
        own_choices(&memo, q, keep).into_iter().next()
    };
    let mut choices = vec![BTreeMap::new()];
    for q in 0..queries {
        if let Some(choice) = own(q, &|c| {
            c.origin == OriginOfCandidateForMemoGroup::Pass1
                && OperatorNode::reachable(&c.root_node).iter().any(is_univmon)
        }) {
            choices.push(BTreeMap::from([choice]));
        }
    }
    for (state, group) in memo.groups.iter().enumerate() {
        if !is_univmon(&group.logical_node) || group.queries.len() < 2 {
            continue;
        }
        let reading: Option<BTreeMap<_, _>> = group
            .queries
            .iter()
            .map(|&q| {
                own(q, &|c| {
                    c.input_groups.iter().any(|child| child.0 as usize == state)
                })
            })
            .collect();
        choices.extend(reading);
    }
    let computations = choices
        .iter()
        .map(|choice| {
            compose_memo_plan(&memo, queries, choice)
                .into_iter()
                .map(QueryRoot::Operator)
                .collect()
        })
        .collect();
    run_explicit_candidates(
        &workload,
        roots,
        computations,
        asap_plan_selection::PlanningModels::builtin(),
    )
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
    let sql = rule_fixture(&workload, lower_sql(&workload).await);
    let standin = pipeline();
    for q in 0..3 {
        assert_eq!(options(&sql, q), options(&standin, q), "Q{}", q + 1);
    }
}

/// The doc's SQL workload itself gets the summary-capability rule's shared
/// candidate: one UnivMon read by Q1, Q2 and Q3.
#[tokio::test]
async fn stage1_logical_asap_optimization_sql_adds_one_univmon_for_all_three() {
    let workload = sql_workload();
    let run = rule_fixture(&workload, lower_sql(&workload).await);
    assert!(
        run.logical.iter().any(|c| {
            let ums = univmons(c);
            ums.len() == 1 && ums[0].0 == BTreeSet::from([0, 1, 2])
        }),
        "no SQL candidate reads one UnivMon for Q1, Q2 and Q3"
    );
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
#[ignore = "needs Pass 1 specialized entropy and L2 summary families (#444)"]
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

/// The summary-capability rule adds one UnivMon build feeding a distinct-count, an entropy and an L2 estimate.
#[test]
fn stage1_logical_asap_optimization_summary_capability_adds_one_univmon_for_all_three() {
    let run = memo_pipeline();
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
    let run = memo_pipeline();
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
            distinct_failure_probability: None,
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
    let run = memo_pipeline();
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
#[ignore = "needs a UnivMon entropy accuracy model (#788); L2 and the distinct count have one"]
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
        let run = rule_fixture(&workload, lower_promql(&workload));
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

/// SQL Q3's L2 through UnivMon keeps its guarantee: the exact population
/// count joined beside it (a cross join, no key) and the `CASE` deciding the
/// empty-input NULL on that exact count keep the estimate's guarantee, so
/// plan selection accepts it.
#[tokio::test]
async fn stage3_plan_selection_accepts_sql_univmon_l2_beside_its_exact_population_count() {
    let workload = sql_workload();
    let run = rule_fixture(&workload, lower_sql(&workload).await);
    for id in univmon_only_candidates(&run, 2) {
        assert!(run.cost(id).is_some(), "{id}: {:?}", run.invalid().get(id));
    }
}

/// One materialized UnivMon for all three costs no more than three separate UnivMons.
#[test]
#[ignore = "needs a UnivMon entropy accuracy model (#788): without one, the shared UnivMon candidate is left unpriced"]
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
    use asap_executor::physical_planner::{compile, promql_rows};
    use asap_executor::runtime::Scope;
    use asap_executor::values::Value;
    use asap_plan_selection::select_plan;
    use asap_plan_selection::PlanningModels;

    let workload = standin_workload();
    let roots = lower_promql(&workload);
    let run = rule_fixture(&workload, roots.clone());
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
    // The whole plan runs at query time over the samples bound at its `Scan`.
    let dag = p.dag();
    let store = physical_common::raw_data_store(dag, |_, schema| {
        samples
            .iter()
            .map(|(ts, v)| promql_rows::series_row(schema, &labels, *ts, *v).unwrap())
            .collect()
    });
    let q1 = p.query_roots[0] as u64;
    let plan = compile(dag, &[], &[q1]).unwrap_or_else(|e| panic!("{}: {e}", p.id));
    let result = physical_common::execute(
        &plan,
        store,
        Scope::Query {
            evaluation_time_ms: 55_000,
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
