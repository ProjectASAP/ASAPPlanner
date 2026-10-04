//! The stage pipeline's dynamic program selects the exhaustive minimum
//! (#572): on #509 Example 1 and on small nested, top-k, shared-input and SQL
//! workloads, the sharing variant and combination it picks are the ones that
//! building and pricing every combination of every variant picks.

use asap_frontend_sql::{lower_sql_dialect, SqlCatalog};
use asap_logical_optimizer::pass2::identical_expressions::{
    stage1_logical_candidates, Sharing, SharingVariant,
};
use asap_plan_selection::PlanningModels;
use asap_plan_selection::{
    select_exhaustive, select_plan, SelectionMethod, MAX_ENUMERATED_CANDIDATES,
};
use asap_planner::{e2e_plan, FrontendInput, UserInput};
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::schema_support::with_promql_series_identity;
use asap_types::ir::QueryRoot;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataArrival, DataDistribution, DataWorkload, DurationMs,
    Evidence, EvidenceSource, LatencyRequirement, PlanningWorkload, Predictability, Query,
    QueryLanguage, QueryRequirements, QueryTimeScope, QueryWorkload, Rate, RepeatedDemand,
    RepeatingEntry, RepetitionInterval, RootDemand, SqlDialect, TimeSelection,
};

type Inventory = Vec<SharingVariant<usize>>;

fn declared<T>(value: T) -> Evidence<T> {
    Evidence {
        value: Some(value),
        source: EvidenceSource::Declared,
        ..Default::default()
    }
}

/// #509 Example 1 over its shared data workload, as `stage_pipeline` builds it.
fn example1() -> PlanningWorkload {
    let panel = |query: &str, accuracy, response_latency| RepeatingEntry {
        query: Query(query.into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
        requirements: QueryRequirements {
            accuracy: AccuracyRequirement::Explicit(accuracy),
            response_latency,
        },
        predictability: Predictability::Predictable { known_at: None },
        time_selection: TimeSelection {
            scope: QueryTimeScope::RealTime,
            lookback: Some(DurationMs(60_000)),
            as_of: None,
        },
    };
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![
                panel(
                    "sum by (job) (rate(http_requests_total[1m]))",
                    AccuracyTarget::Exact,
                    LatencyRequirement::Unspecified,
                ),
                panel(
                    "topk by (job) (10, sum_over_time(http_requests_total[1m]))",
                    AccuracyTarget::EpsilonDelta {
                        epsilon: 0.01,
                        delta: 0.001,
                    },
                    LatencyRequirement::ExplicitMaxMs(100.0),
                ),
            ]),
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            data_ingestion_interval: declared(DurationMs(15_000)),
            ingestion_volume: Evidence::default(),
            ingestion_rate: declared(Rate(1_000_000.0 / 15.0)),
            input_cardinality: declared(1_000_000),
            distribution: declared(DataDistribution::Zipf),
            metric_types: Default::default(),
        }),
    }
}

fn batch(query: &str, accuracy: AccuracyTarget) -> BatchEntry {
    BatchEntry {
        query: Query(query.into()),
        requirements: QueryRequirements {
            accuracy: AccuracyRequirement::Explicit(accuracy),
            ..Default::default()
        },
        predictability: Predictability::Unknown,
        invocations: 1,
        execute_at: None,
        time_selection: TimeSelection::default(),
    }
}

fn promql(queries: &[&str], series: u64) -> PlanningWorkload {
    let accuracy = AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.001,
    };
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(queries.iter().map(|q| batch(q, accuracy.clone())).collect()),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            data_ingestion_interval: declared(DurationMs(15_000)),
            ingestion_rate: declared(Rate(10.0)),
            input_cardinality: declared(series),
            ..Default::default()
        }),
    }
}

/// Stage 1 as the stage pipeline builds it: series identity, then Pass 1
/// with and without identical sub-DAGs merged, with tumbling forms for
/// repeating entries.
fn promql_inventory(workload: &PlanningWorkload) -> Inventory {
    let roots = asap_frontend_promql::lower_promql_query_workload(workload, 0)
        .expect("lowers")
        .into_iter()
        .enumerate()
        .map(|(index, root)| match root {
            QueryRoot::Operator(node) => (
                index,
                QueryRoot::Operator(with_promql_series_identity(&node).unwrap()),
            ),
            QueryRoot::Scalar(_) => panic!("operator roots"),
        })
        .collect();
    stage1_logical_candidates(roots, &Default::default(), &targets(workload)).expect("Stage 1")
}

fn targets(workload: &PlanningWorkload) -> Vec<RootDemand> {
    workload
        .query_workload
        .entries()
        .map(|entry| RootDemand::from(&entry))
        .collect()
}

/// The dynamic program's choice is the exhaustive winner's; returns that
/// winner's id.
fn assert_dp_matches_exhaustive(
    inventory: &Inventory,
    workload: &PlanningWorkload,
    combinations: usize,
) -> String {
    let (selected, method, _) = selects_exhaustive_minimum(inventory, workload, combinations);
    assert_eq!(method, SelectionMethod::TreeDp);
    selected
}

/// [`select_plan`] chooses what building and pricing every combination of
/// every variant chooses, however it gets there; returns the winner's id,
/// how `select_plan` found it, and its variant.
fn selects_exhaustive_minimum(
    inventory: &Inventory,
    workload: &PlanningWorkload,
    combinations: usize,
) -> (String, SelectionMethod, Sharing) {
    let targets = targets(workload);
    let data = workload.data_workload.clone().unwrap_or_default();
    let models = PlanningModels::builtin();
    let exhaustive = select_exhaustive(
        inventory,
        &targets,
        &data,
        models,
        combinations.max(MAX_ENUMERATED_CANDIDATES),
    )
    .expect("exhaustive selection");
    assert_eq!(exhaustive.combinations, combinations);
    assert_eq!(exhaustive.candidates.len(), combinations);
    let winner = exhaustive
        .candidates
        .iter()
        .find(|c| {
            c.physical
                .iter()
                .any(|p| p.id == exhaustive.selection.selected)
        })
        .expect("winner was built");

    let plan = select_plan(inventory, &targets, &data, models).expect("selects");
    assert!(plan.selection.guaranteed_optimal());
    assert_eq!(
        (plan.sharing, &plan.choice),
        (winner.sharing, &winner.choice)
    );
    assert_eq!(plan.selection.selected, exhaustive.selection.selected);
    (
        exhaustive.selection.selected,
        plan.selection.method,
        plan.sharing,
    )
}

/// #509 Example 1: the dynamic program picks the cheapest of its 88
/// combinations (44 per sharing variant, including Q2's whole-expression
/// top-k sketches, which absorb its `sum_over_time`, and Q2's exact sum in
/// 10-s tumbling panes): P79, all exact with the range selector shared and
/// no panes (rebuilding every pane at each evaluation costs more until
/// Stage 2 keeps them).
#[test]
fn example1_dp_equals_exhaustive() {
    let workload = example1();
    let stage1 = promql_inventory(&workload);
    let selected = assert_dp_matches_exhaustive(&stage1, &workload, 88);
    assert_eq!(selected, "P79");
}

/// PromQL queries repeated every `interval_ms`, each over the last 5 min.
fn repeating(queries: &[&str], interval_ms: u32) -> PlanningWorkload {
    let mut workload = promql(&[], 1_000);
    workload.query_workload.query_batch = None;
    workload.query_workload.repeating_queries = Some(
        queries
            .iter()
            .map(|query| RepeatingEntry {
                query: Query(query.to_string()),
                demand: RepeatedDemand::FixedInterval(RepetitionInterval(interval_ms)),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(AccuracyTarget::EpsilonDelta {
                        epsilon: 0.01,
                        delta: 0.01,
                    }),
                    ..Default::default()
                },
                predictability: Predictability::Predictable { known_at: None },
                time_selection: TimeSelection::default(),
            })
            .collect(),
    );
    workload
}

/// #509 Example 3, Pattern B: the 5-min p99 every minute has exact, KLL
/// and DDSketch, each sketch also in 1-min tumbling panes; the dynamic
/// program picks the exhaustive minimum of the 5.
#[test]
fn tumbling_window_forms_dp_equals_exhaustive() {
    let workload = repeating(&["quantile_over_time(0.99, x[5m])"], 60_000);
    assert_dp_matches_exhaustive(&promql_inventory(&workload), &workload, 5);
}

/// Pass 2 shares panes: a 5-min and a 3-min p99 every minute both build
/// 1-min KLL panes over one scan, and the shared variant merges the 3 panes
/// they have in common, so 5 pane builds serve both instead of 8. Selection
/// over both variants still picks the exhaustive minimum.
#[test]
fn identical_panes_are_shared_across_queries() {
    use asap_plan_selection::select_exhaustive;
    use asap_types::ir::{ASAPOp, Operator, OperatorNode};
    let workload = repeating(
        &[
            "quantile_over_time(0.99, x[5m])",
            "quantile_over_time(0.99, x[3m])",
        ],
        60_000,
    );
    let stage1 = promql_inventory(&workload);
    let data = workload.data_workload.clone().unwrap();
    let enumeration = select_exhaustive(
        &stage1,
        &targets(&workload),
        &data,
        PlanningModels::builtin(),
        usize::MAX,
    )
    .expect("enumerates");
    // Both queries' targets choose KLL in tumbling panes (alternative 3).
    let pane_builds = |sharing: Sharing| {
        let candidate = enumeration
            .candidates
            .iter()
            .find(|c| c.sharing == sharing && c.choice == [3, 3])
            .expect("both in KLL panes");
        let roots: Vec<_> = candidate
            .logical
            .as_ref()
            .unwrap()
            .iter()
            .map(|(_, root)| match root {
                QueryRoot::Operator(node) => node.clone(),
                QueryRoot::Scalar(_) => unreachable!(),
            })
            .collect();
        let mut builds: Vec<_> = roots
            .iter()
            .flat_map(OperatorNode::reachable)
            .filter(|n| matches!(n.operator, Operator::ASAP(ASAPOp::SummaryAgg { .. })))
            .map(|n| std::rc::Rc::as_ptr(&n))
            .collect();
        builds.sort();
        builds.dedup();
        builds.len()
    };
    assert_eq!(pane_builds(Sharing::Independent), 8);
    assert_eq!(pane_builds(Sharing::IdenticalExpressions), 5);
    selects_exhaustive_minimum(&stage1, &workload, enumeration.combinations);
}

/// When sharing merges a whole target (`rate(x[1m])` read by both queries),
/// the shared variant has one target for it, and the dynamic program still
/// selects the exhaustive minimum over both variants (16 + 8 combinations).
#[test]
fn shared_target_dp_equals_exhaustive() {
    let workload = promql(
        &["sum by (job) (rate(x[1m]))", "max by (job) (rate(x[1m]))"],
        1_000,
    );
    let stage1 = promql_inventory(&workload);
    let targets: Vec<_> = stage1.iter().map(|v| v.inventory.targets.len()).collect();
    assert_eq!(targets, [4, 3]);
    let selected = assert_dp_matches_exhaustive(&stage1, &workload, 24);
    assert!(
        selected.trim_start_matches('P').parse::<usize>().unwrap() > 16,
        "a shared candidate wins: {selected}"
    );
}

/// Nested targets (`sum` over `rate`) select the exhaustive minimum.
#[test]
fn nested_sum_over_rate_dp_equals_exhaustive() {
    let workload = promql(&["sum by (job) (rate(x[1m]))"], 1_000);
    assert_dp_matches_exhaustive(&promql_inventory(&workload), &workload, 4);
}

/// An aggregate over a top-k, with inputs both below and above k × groups
/// rows, selects the exhaustive minimum over all 40 combinations.
#[test]
fn count_over_topk_dp_equals_exhaustive() {
    for series in [3, 1_000_000] {
        let workload = promql(&["count(topk by (job) (10, sum_over_time(m[1m])))"], series);
        assert_dp_matches_exhaustive(&promql_inventory(&workload), &workload, 40);
    }
}

/// Two PromQL queries sharing a source select the exhaustive minimum.
#[test]
fn two_query_promql_dp_equals_exhaustive() {
    let workload = promql(
        &[
            "sum by (job) (rate(x[1m]))",
            "topk by (job) (10, sum_over_time(x[1m]))",
        ],
        1_000,
    );
    assert_dp_matches_exhaustive(&promql_inventory(&workload), &workload, 64);
}

/// A SQL workload (distinct count and percentile) selects the exhaustive minimum.
#[tokio::test]
async fn sql_dp_equals_exhaustive() {
    let accuracy = AccuracyTarget::Epsilon(0.01);
    let queries = [
        "SELECT COUNT(DISTINCT l_orderkey) FROM lineitem",
        "SELECT approx_percentile_cont(l_extendedprice, 0.99) FROM lineitem",
    ];
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
            query_batch: Some(queries.iter().map(|q| batch(q, accuracy.clone())).collect()),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::AtRest,
            ..Default::default()
        }),
    };
    let catalog = SqlCatalog::new().with_table(
        "lineitem",
        Schema::new(vec![
            Field::plain("l_orderkey", DataType::Int64, false),
            Field::plain("l_extendedprice", DataType::Float64, false),
        ]),
    );
    let mut roots = Vec::new();
    for (index, query) in queries.iter().enumerate() {
        let root = lower_sql_dialect(query, &catalog, SqlDialect::DataFusionSQL, accuracy.clone())
            .await
            .expect("lowers");
        roots.push((index, QueryRoot::Operator(root)));
    }
    let inventory = stage1_logical_candidates(roots, &Default::default(), &[]).expect("Stage 1");
    assert_dp_matches_exhaustive(&inventory, &workload, 15);
}

/// A SQL grouped count, which also has a HydraCms alternative (#580 W7),
/// and a percentile select the exhaustive minimum.
#[tokio::test]
async fn sql_hydra_count_dp_equals_exhaustive() {
    let accuracy = AccuracyTarget::Epsilon(0.1);
    let queries = [
        "SELECT l_orderkey, COUNT(*) FROM lineitem GROUP BY l_orderkey",
        "SELECT approx_percentile_cont(l_extendedprice, 0.99) FROM lineitem",
    ];
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
            query_batch: Some(queries.iter().map(|q| batch(q, accuracy.clone())).collect()),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::AtRest,
            ..Default::default()
        }),
    };
    let catalog = SqlCatalog::new().with_table(
        "lineitem",
        Schema::new(vec![
            Field::plain("l_orderkey", DataType::Int64, false),
            Field::plain("l_extendedprice", DataType::Float64, false),
        ]),
    );
    let mut roots = Vec::new();
    for (index, query) in queries.iter().enumerate() {
        let root = lower_sql_dialect(query, &catalog, SqlDialect::DataFusionSQL, accuracy.clone())
            .await
            .expect("lowers");
        roots.push((index, QueryRoot::Operator(root)));
    }
    let inventory = stage1_logical_candidates(roots, &Default::default(), &[]).expect("Stage 1");
    assert!(inventory[0]
        .inventory
        .targets
        .iter()
        .any(|t| t.groupings.iter().any(|g| *g != Default::default())));
    // (pass-through, Count acc, CMS, CountSketch, UnivMon, HydraCms) × (pass-through, KLL, DDSketch).
    assert_dp_matches_exhaustive(&inventory, &workload, 18);
}

/// PromQL queries, each with its own ε (δ = 0.001).
fn promql_with(queries: &[(&str, f64)]) -> PlanningWorkload {
    let mut workload = promql(&[], 1_000);
    workload.query_workload.query_batch = Some(
        queries
            .iter()
            .map(|(q, epsilon)| {
                batch(
                    q,
                    AccuracyTarget::EpsilonDelta {
                        epsilon: *epsilon,
                        delta: 0.001,
                    },
                )
            })
            .collect(),
    );
    workload
}

/// p50 at ε=0.01 and p99 at ε=0.001 over one input: Stage 1 has an
/// independent, an identical-expression and a summary-capability variant
/// (pass-through, KLL, DDSketch per target: 9 combinations each). Sharing
/// one KLL couples the two targets, so selection builds every combination
/// of that variant, and picks the exhaustive minimum: the shared KLL.
#[test]
fn summary_capability_dp_equals_exhaustive() {
    let workload = promql_with(&[
        ("quantile_over_time(0.5, lat[5m])", 0.01),
        ("quantile_over_time(0.99, lat[5m])", 0.001),
    ]);
    let stage1 = promql_inventory(&workload);
    assert_eq!(
        stage1.iter().map(|v| v.sharing).collect::<Vec<_>>(),
        [
            Sharing::Independent,
            Sharing::IdenticalExpressions,
            Sharing::SummaryCapability
        ]
    );
    let (_, method, sharing) = selects_exhaustive_minimum(&stage1, &workload, 27);
    assert_eq!(method, SelectionMethod::Exhaustive);
    assert_eq!(sharing, Sharing::SummaryCapability);
}

/// The same with an unrelated third query (pass-through or exact max): the
/// shared KLL is still the minimum over all 3 × 18 combinations.
#[test]
fn summary_capability_with_an_unrelated_query_dp_equals_exhaustive() {
    let workload = promql_with(&[
        ("quantile_over_time(0.5, lat[5m])", 0.01),
        ("quantile_over_time(0.99, lat[5m])", 0.001),
        ("max_over_time(other[5m])", 0.01),
    ]);
    let stage1 = promql_inventory(&workload);
    assert_eq!(stage1.len(), 3);
    let (_, _, sharing) = selects_exhaustive_minimum(&stage1, &workload, 54);
    assert_eq!(sharing, Sharing::SummaryCapability);
}

/// SQL p50 and p99 over one filtered column: pre-ASAP CSE merges nothing
/// (the scan has no unique key), so the summary-capability variant is the
/// only sharing one, and the shared KLL is the exhaustive minimum.
#[tokio::test]
async fn sql_summary_capability_dp_equals_exhaustive() {
    let accuracy = AccuracyTarget::Epsilon(0.01);
    let queries = [
        "SELECT approx_percentile_cont(l_extendedprice, 0.5) FROM lineitem WHERE l_orderkey > 10",
        "SELECT approx_percentile_cont(l_extendedprice, 0.99) FROM lineitem WHERE l_orderkey > 10",
    ];
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
            query_batch: Some(queries.iter().map(|q| batch(q, accuracy.clone())).collect()),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::AtRest,
            ..Default::default()
        }),
    };
    let catalog = SqlCatalog::new().with_table(
        "lineitem",
        Schema::new(vec![
            Field::plain("l_orderkey", DataType::Int64, false),
            Field::plain("l_extendedprice", DataType::Float64, false),
        ]),
    );
    let mut roots = Vec::new();
    for (index, query) in queries.iter().enumerate() {
        let root = lower_sql_dialect(query, &catalog, SqlDialect::DataFusionSQL, accuracy.clone())
            .await
            .expect("lowers");
        roots.push((index, QueryRoot::Operator(root)));
    }
    let stage1 = stage1_logical_candidates(roots, &Default::default(), &[]).expect("Stage 1");
    assert_eq!(
        stage1.iter().map(|v| v.sharing).collect::<Vec<_>>(),
        [Sharing::Independent, Sharing::SummaryCapability]
    );
    let (_, _, sharing) = selects_exhaustive_minimum(&stage1, &workload, 18);
    assert_eq!(sharing, Sharing::SummaryCapability);
}

/// Through the facade, Example 1 selects the exhaustive winner, P79: both
/// queries exact, Q1's rate and sum and Q2's sum as exact accumulators, over
/// one shared range selector, and no tumbling panes.
#[tokio::test]
async fn facade_selects_the_example1_exhaustive_winner() {
    let workload = example1();
    let output = e2e_plan(UserInput::new(
        &workload,
        FrontendInput::Promql {
            now_ms: 0,
            histograms: None,
        },
        PlanningModels::builtin(),
    ))
    .await
    .expect("plans");
    let selection = output.selection.as_ref().expect("selection");
    assert_eq!(selection.selected, "P79");
    assert_eq!(output.plans.len(), 2);
    assert!(output.plans.iter().all(|plan| {
        asap_types::ir::OperatorNode::reachable(&plan.root)
            .iter()
            .all(|n| n.asap().is_none_or(|op| op.kind_name() != "SummaryMerge"))
    }));
    let scans = |root: &std::rc::Rc<asap_types::ir::OperatorNode>| {
        asap_types::ir::OperatorNode::reachable(root)
            .into_iter()
            .filter(|n| matches!(n.non_asap(), Some(asap_types::ir::NonASAPOp::Scan { .. })))
            .map(|n| std::rc::Rc::as_ptr(&n))
            .collect::<Vec<_>>()
    };
    assert_eq!(scans(&output.plans[0].root), scans(&output.plans[1].root));
    assert_eq!(selection.method, SelectionMethod::TreeDp);
    assert_eq!(output.entry_indices(), vec![0, 1]);
    // The plans are already timed at query time; exporting them again
    // re-times nothing.
    let dag = output.execution_timed_dag().expect("exports");
    assert_eq!(dag.roots.len(), 2);
}
