//! Structurally identical summary producers chosen by different queries are
//! shared after Pass 1: one `Rc<OperatorNode>` across their plans.

use asap_types::ir::cse::share_common_sub_dags;
use asap_types::ir::{ASAPOp, OperatorNode};
use std::rc::Rc;

use asap_frontend_promql::lower_promql_workload;
use asap_frontend_sql::SqlCatalog;
use asap_logical_optimizer::accuracy::{
    AccuracyModel, DefaultAccuracyModel, EqualSplitAllocator, PropagationStats,
};
use asap_logical_optimizer::pass1::replacement::{default_size_params, DEFAULT_DELTA};
use asap_logical_optimizer::{
    search_workload_with_targets, ASAPStrategies, Replacement, ReplacementStrategy,
    ReplacementSubDAG, TargetSubDAG,
};
use asap_plan_selection::candidate_selection::global_selection;
use asap_plan_selection::PlanningModels;
use asap_plan_selection::{CostModel, DefaultCostModel};
use asap_planner::pass::{PlanOutput, QueryPlan};
use asap_planner::{e2e_plan, FrontendInput, UserInput};
use asap_types::ir::operator::agg_intent::default_quantile;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::{
    AccuracyError, BoundExpr, CompositionOperator, ErrorMetric, ProbabilityExpr, ResultGuarantee,
};
use asap_types::ir::schema::SketchStatistic;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, SketchKind, SketchParams};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, DataArrival, DataWorkload, DurationMs, Evidence, LatencyRequirement,
    PlanningWorkload, Predictability, Query, QueryLanguage, QueryRequirements, QueryWorkload, Rate,
    RepeatedDemand, RepeatingEntry, RepetitionInterval, SqlDialect, TimeSelection,
};

const NOW_MS: u64 = 1_700_000_000_000;

/// Stand-in for the workload-level amortization Stage 2 materialization will
/// price: a sketch candidate costs `preference(kind)` per sketch state, any
/// other candidate more than every sketch. Ranking is otherwise built-in.
struct PreferSketch(fn(&SketchKind) -> f64);

impl CostModel for PreferSketch {
    // Selection takes the cheapest candidate by `estimate_cost`.
    fn candidate_cost_covers_complete_plan(&self) -> bool {
        true
    }

    fn rank_candidates(
        &self,
        intent: &AggIntent,
        candidates: &[SketchAlgorithm],
    ) -> Vec<SketchAlgorithm> {
        DefaultCostModel.rank_candidates(intent, candidates)
    }

    fn estimate_cost(&self, candidate: &ReplacementSubDAG, _: &TargetSubDAG<'_>) -> f64 {
        let Replacement::SubDAG(root) = &candidate.replacement else {
            return 1e9;
        };
        let kinds: Vec<_> = OperatorNode::reachable(root)
            .into_iter()
            .filter_map(|node| match &node.operator {
                asap_types::ir::Operator::ASAP(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) => Some(kind.clone()),
                _ => None,
            })
            .collect();
        if kinds.is_empty() {
            1e9
        } else {
            kinds.iter().map(self.0).sum()
        }
    }
}

/// Prefers the largest KLL, i.e. one sized for the strictest consumer.
const PREFER_LARGE_KLL: PreferSketch = PreferSketch(|kind| match kind.params() {
    SketchParams::Kll { k } => 1.0 / f64::from(*k),
    _ => 1.0,
});

/// Prefers UnivMon, which can serve every frequency moment from one state.
const PREFER_UNIVMON: PreferSketch = PreferSketch(|kind| {
    if kind.algorithm() == &SketchAlgorithm::UnivMon {
        0.0
    } else {
        1.0
    }
});

fn requirements(epsilon: f64) -> QueryRequirements {
    QueryRequirements {
        accuracy: AccuracyRequirement::Explicit(AccuracyTarget::Epsilon(epsilon)),
        response_latency: LatencyRequirement::Unspecified,
    }
}

/// Every query repeats every ten minutes: six reads each over the horizon.
fn repeating(query: &str, epsilon: f64) -> RepeatingEntry {
    RepeatingEntry {
        query: Query(query.into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(600_000)),
        requirements: requirements(epsilon),
        predictability: Predictability::Unknown,
        time_selection: TimeSelection::default(),
    }
}

fn promql_workload(queries: &[(&str, f64)]) -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(
                queries
                    .iter()
                    .map(|(query, epsilon)| repeating(query, *epsilon))
                    .collect(),
            ),
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(15_000)),
                ..Default::default()
            },
            ingestion_rate: Evidence {
                value: Some(Rate(1.0)),
                ..Default::default()
            },
            ..Default::default()
        }),
    }
}

async fn plan_promql(queries: &[(&str, f64)]) -> PlanOutput {
    plan_promql_with(queries, &DefaultCostModel).await
}

async fn plan_promql_with(queries: &[(&str, f64)], cost: &dyn CostModel) -> PlanOutput {
    let workload = promql_workload(queries);
    let input = UserInput::new(
        &workload,
        FrontendInput::Promql {
            now_ms: NOW_MS,
            histograms: None,
        },
        PlanningModels::builtin().with_cost(cost),
    );
    e2e_plan(input).await.expect("workload plans")
}

async fn plan_sql(queries: &[&str]) -> PlanOutput {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
            query_batch: None,
            repeating_queries: Some(queries.iter().map(|query| repeating(query, 0.01)).collect()),
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
    let input = UserInput::new(
        &workload,
        FrontendInput::Sql { catalog: &catalog },
        PlanningModels::builtin(),
    );
    e2e_plan(input).await.expect("workload plans")
}

/// Every summary state (`SummaryAgg`) each plan reaches, in traversal order.
fn plan_states(plan: &QueryPlan) -> Vec<Rc<OperatorNode>> {
    OperatorNode::reachable(&plan.root)
        .into_iter()
        .filter(|node| {
            matches!(
                node.operator,
                asap_types::ir::Operator::ASAP(ASAPOp::SummaryAgg { .. })
            )
        })
        .collect()
}

/// Every summary state each plan reaches; each plan selects at least one.
fn states(output: &PlanOutput) -> Vec<Vec<Rc<OperatorNode>>> {
    output
        .plans
        .iter()
        .map(|plan| {
            let states = plan_states(plan);
            assert!(!states.is_empty(), "{:?}", plan.root);
            states
        })
        .collect()
}

/// Whether the two plans deploy exactly the same states, by pointer.
fn same_states(states: &[Vec<Rc<OperatorNode>>]) -> bool {
    states[0].len() == states[1].len()
        && states[0]
            .iter()
            .zip(&states[1])
            .all(|(left, right)| Rc::ptr_eq(left, right))
}

/// The summary states a consumer would run, deduplicated by pointer.
fn unique_deployments(output: &PlanOutput) -> usize {
    let mut seen: Vec<*const OperatorNode> = Vec::new();
    for plan in &output.plans {
        for state in plan_states(plan) {
            let ptr = Rc::as_ptr(&state);
            if !seen.contains(&ptr) {
                seen.push(ptr);
            }
        }
    }
    seen.len()
}

/// p50 and p99 over the same window and accuracy read one KLL: the
/// equal-params subset of summary capability. Both plans hold the same `Rc`,
/// so a consumer maintains it once.
#[tokio::test]
#[ignore = "Stage 3 selects the raw plan; query-time summaries never cost less until Stage 2 plans materialization: #580"]
async fn quantiles_with_equal_params_share_one_producer() {
    let output = plan_promql(&[
        ("quantile_over_time(0.5, lat[5m])", 0.01),
        ("quantile_over_time(0.99, lat[5m])", 0.01),
    ])
    .await;
    assert!(same_states(&states(&output)));
    assert!(!Rc::ptr_eq(&output.plans[0].root, &output.plans[1].root));
    assert_eq!(unique_deployments(&output), 1);
}

/// A different window or label selector is a different producer, even when
/// one query asks for a stricter accuracy than the other.
#[tokio::test]
#[ignore = "Stage 3 selects the raw plan; query-time summaries never cost less until Stage 2 plans materialization: #580"]
async fn different_producers_are_not_shared() {
    for queries in [
        [
            ("quantile_over_time(0.5, lat[5m])", 0.01),
            ("quantile_over_time(0.99, lat[10m])", 0.01),
        ],
        [
            ("quantile_over_time(0.5, lat[5m])", 0.01),
            ("quantile_over_time(0.99, lat[10m])", 0.001),
        ],
        [
            ("quantile_over_time(0.5, lat{job=\"a\"}[5m])", 0.01),
            ("quantile_over_time(0.99, lat{job=\"b\"}[5m])", 0.001),
        ],
        [
            ("quantile_over_time(0.5, lat{job=\"a\"}[5m])", 0.01),
            ("quantile_over_time(0.99, lat{job=\"b\"}[5m])", 0.01),
        ],
    ] {
        let output = plan_promql(&queries).await;
        assert!(!same_states(&states(&output)), "{queries:?}");
        assert_eq!(unique_deployments(&output), 2, "{queries:?}");
        for (plan, (_, epsilon)) in output.plans.iter().zip(queries) {
            assert_eq!(kll_k(plan), kll_k_for(epsilon), "{queries:?}");
        }
    }
}

/// The KLL `k` of the one state a plan deploys.
fn kll_k(plan: &QueryPlan) -> u32 {
    let states = plan_states(plan);
    let [deployment] = states.as_slice() else {
        panic!("one state: {:?}", states.len());
    };
    let asap_types::ir::Operator::ASAP(ASAPOp::SummaryAgg {
        family: FieldDataType::Sketch(kind, _),
        ..
    }) = &deployment.operator
    else {
        panic!("sketch state: {:?}", deployment.operator);
    };
    let SketchParams::Kll { k } = kind.params() else {
        panic!("KLL state: {kind:?}");
    };
    *k
}

/// The KLL `k` sized for `epsilon`.
fn kll_k_for(epsilon: f64) -> u32 {
    let SketchParams::Kll { k } = default_size_params(
        SketchAlgorithm::Kll,
        &default_quantile(0.5),
        epsilon,
        DEFAULT_DELTA,
    ) else {
        unreachable!()
    };
    k
}

/// p50 at ε=0.01 and p99 at ε=0.001 over the same input share one KLL sized
/// for the strictest consumer when the cost model prefers that candidate; each
/// reader's guarantee meets its own target.
#[tokio::test]
#[ignore = "Pass 2 cross-query sharing is not planned by the stage pipeline: #580"]
async fn quantiles_share_one_producer_sized_for_the_strictest_consumer() {
    let p50 = ("quantile_over_time(0.5, lat[5m])", 0.01);
    let p99 = ("quantile_over_time(0.99, lat[5m])", 0.001);
    assert!(kll_k_for(0.001) > kll_k_for(0.01));

    let output = plan_promql_with(&[p50, p99], &PREFER_LARGE_KLL).await;
    assert!(same_states(&states(&output)));
    assert_eq!(unique_deployments(&output), 1);
    for (plan, (_, epsilon)) in output.plans.iter().zip([p50, p99]) {
        assert_eq!(kll_k(plan), kll_k_for(0.001));
        let guarantee = plan.root.guarantee.as_ref().expect("certified");
        assert!(
            guarantee.bound.evaluate().unwrap() <= epsilon,
            "{guarantee:?}"
        );
    }

    // Alone, the looser query keeps its own, smaller KLL.
    let alone = plan_promql_with(&[p50], &PREFER_LARGE_KLL).await;
    assert_eq!(kll_k(&alone.plans[0]), kll_k_for(0.01));
}

/// Cross-series quantiles name their KLL state after the input column, not the
/// quantile, so p50 and p99 over one selector share it.
#[tokio::test]
#[ignore = "Pass 2 cross-query sharing is not planned by the stage pipeline: #580"]
async fn cross_series_p50_and_p99_share_one_producer() {
    let output = plan_promql(&[("quantile(0.5, lat)", 0.01), ("quantile(0.99, lat)", 0.01)]).await;
    assert!(same_states(&states(&output)));
    assert_eq!(unique_deployments(&output), 1);
}

/// An ungrouped aggregate has no unique key, so pre-ASAP CSE keeps the two
/// copies apart; their identical producers (rate, then sum) are shared here.
#[tokio::test]
#[ignore = "Stage 3 selects the raw plan; query-time summaries never cost less until Stage 2 plans materialization: #580"]
async fn identical_ungrouped_queries_share_their_producers() {
    let query = ("sum(rate(x[5m]))", 0.01);
    let output = plan_promql(&[query, query]).await;
    assert!(same_states(&states(&output)));
    assert_eq!(unique_deployments(&output), 2);
}

/// The SQL frontend reaches the same sharing for two copies of one filtered
/// percentile.
#[tokio::test]
#[ignore = "Stage 3 selects the raw plan; query-time summaries never cost less until Stage 2 plans materialization: #580"]
async fn identical_sql_percentiles_share_one_producer() {
    let query =
        "SELECT approx_percentile_cont(l_extendedprice, 0.5) FROM lineitem WHERE l_orderkey > 10";
    let output = plan_sql(&[query, query]).await;
    assert!(same_states(&states(&output)));
    assert_eq!(unique_deployments(&output), 1);
}

/// The quantile is a evaluation parameter: SQL p50 and p99 over one filtered
/// column build one KLL, named after its input, while each query keeps its
/// own output column.
#[tokio::test]
#[ignore = "Pass 2 cross-query sharing is not planned by the stage pipeline: #580"]
async fn sql_p50_and_p99_share_one_producer() {
    let p50 =
        "SELECT approx_percentile_cont(l_extendedprice, 0.5) FROM lineitem WHERE l_orderkey > 10";
    let p99 =
        "SELECT approx_percentile_cont(l_extendedprice, 0.99) FROM lineitem WHERE l_orderkey > 10";
    let output = plan_sql(&[p50, p99]).await;
    assert!(same_states(&states(&output)));
    assert_eq!(unique_deployments(&output), 1);
    let names: Vec<_> = output
        .plans
        .iter()
        .map(|plan| {
            plan.root
                .schema
                .fields
                .iter()
                .map(|field| field.name.clone())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(
        names,
        [
            ["approx_percentile_cont(lineitem.l_extendedprice,Float64(0.5))"],
            ["approx_percentile_cont(lineitem.l_extendedprice,Float64(0.99))"],
        ]
    );

    for queries in [
        [
            p50,
            "SELECT approx_percentile_cont(l_extendedprice, 0.99) FROM lineitem WHERE l_orderkey > 20",
        ],
        [
            p50,
            "SELECT approx_percentile_cont(l_orderkey, 0.99) FROM lineitem WHERE l_orderkey > 10",
        ],
    ] {
        let output = plan_sql(&queries).await;
        assert!(!same_states(&states(&output)), "{queries:?}");
        assert_eq!(unique_deployments(&output), 2, "{queries:?}");
    }
}

/// Synthetic evidence certifying UnivMon evaluations; it exercises sharing, never
/// runtime accuracy.
struct UnivMonEvidence;

impl AccuracyModel for UnivMonEvidence {
    fn local_guarantee(
        &self,
        family: &FieldDataType,
        query: &SketchStatistic,
    ) -> Option<ResultGuarantee> {
        if matches!(family, FieldDataType::Sketch(kind, _) if kind.algorithm() == &SketchAlgorithm::UnivMon)
        {
            let mut guarantee = ResultGuarantee::exact("SYNTHETIC test evidence; not measured");
            guarantee.metric = ErrorMetric::RelativeValue;
            guarantee.bound = BoundExpr::Constant { value: 0.01 };
            guarantee.failure_probability = ProbabilityExpr::Constant { value: 0.01 };
            Some(guarantee)
        } else {
            DefaultAccuracyModel.local_guarantee(family, query)
        }
    }

    fn propagate(
        &self,
        op: &CompositionOperator,
        inputs: &[ResultGuarantee],
        local: Option<&ResultGuarantee>,
        stats: &PropagationStats,
    ) -> Result<ResultGuarantee, AccuracyError> {
        DefaultAccuracyModel.propagate(op, inputs, local, stats)
    }

    fn satisfies(&self, guarantee: &ResultGuarantee, target: &AccuracyTarget) -> bool {
        DefaultAccuracyModel.satisfies(guarantee, target)
    }
}

/// Distinct count, entropy and L2 over one input, certified by an accuracy
/// model and selected by a cost model preferring UnivMon, read one UnivMon state: #515 sharing is the summary-capability rule
/// when the states are identical. The facade's stage pipeline does not plan
/// UnivMon sharing, so this runs the legacy search with the test model.
#[test]
fn certified_frequency_evaluations_share_one_univmon_state() {
    let queries = [
        ("distinct_over_time(m[5m])", 0.02),
        ("entropy_over_time(m[5m])", 0.02),
        ("l2_over_time(m[5m])", 0.02),
    ];
    let workload = promql_workload(&queries);
    let roots = lower_promql_workload(&workload, NOW_MS)
        .expect("lowers")
        .into_iter()
        .zip(queries)
        .enumerate()
        .map(|(index, (expr, (_, epsilon)))| (index, expr, Some(AccuracyTarget::Epsilon(epsilon))))
        .collect();
    let strategies: Vec<Box<dyn ReplacementStrategy>> = vec![Box::new(
        ASAPStrategies::new_with_planning_inputs(&UnivMonEvidence, &EqualSplitAllocator),
    )];
    let space = search_workload_with_targets(roots, &strategies, &UnivMonEvidence);
    let selection = global_selection(&space, &PREFER_UNIVMON);
    let assembled = space
        .roots
        .iter()
        .map(|(index, root)| {
            let dag = selection
                .assemble_selected_dag(root)
                .expect("assembles")
                .expect("root has a group");
            (*index, dag)
        })
        .collect();
    let mut states: Vec<Rc<OperatorNode>> = Vec::new();
    for (_, root) in share_common_sub_dags(assembled) {
        assert!(root.guarantee.is_some(), "{:?}", root.operator);
        let asap_types::ir::Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) =
            &root.operator
        else {
            panic!("summary evaluation: {:?}", root.operator);
        };
        assert!(matches!(
            &summary_input.operator,
            asap_types::ir::Operator::ASAP(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. })
                if kind.algorithm() == &SketchAlgorithm::UnivMon
        ));
        states.push(Rc::clone(summary_input));
    }
    assert_eq!(states.len(), 3);
    assert!(states.iter().all(|state| Rc::ptr_eq(state, &states[0])));
}
