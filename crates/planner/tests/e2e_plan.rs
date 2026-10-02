//! End-to-end coverage for the library facade (#429) and for plugging a
//! different optimization pass into it (#430).

use std::rc::Rc;

use asap_aware_mapping::pass::{
    OptimizationInput, OptimizationPass, OptimizeError, PlanOutput, PlanningModels,
};
use asap_aware_mapping::replacement::default_strategies_with_evidence;
use asap_aware_mapping::{
    search_workload_with_targets, Horizon, LifecycleInput, SummaryMaintenanceLifecycleCapabilities,
};
use asap_frontend_sql::{lower_sql_dialect, SqlCatalog};
use asap_planner::{e2e_plan, FrontendInput, PlanError, UserInput, UserInputError};
use asap_types::post_asap::SummaryExpr;
use asap_types::pre_asap::schema::{Field, DataType, Schema};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataArrival, DataWorkload, DurationMs, Evidence,
    LatencyRequirement, PlanningWorkload, Predictability, Query, QueryLanguage, QueryRequirements,
    QueryWorkload, RepeatedDemand, RepeatingEntry, RepetitionInterval, SqlDialect, TimeSelection,
};

const NOW_MS: u64 = 1_700_000_000_000;

fn approximate() -> QueryRequirements {
    QueryRequirements {
        accuracy: AccuracyRequirement::Explicit(AccuracyTarget::Epsilon(0.01)),
        response_latency: LatencyRequirement::Unspecified,
    }
}

fn batch(sql: &str) -> BatchEntry {
    BatchEntry {
        query: Query(sql.into()),
        requirements: approximate(),
        predictability: Predictability::AdHoc,
        invocations: 1,
        execute_at: None,
        time_selection: TimeSelection::default(),
    }
}

/// The planning clock and default capabilities, no horizon: the least a
/// caller can supply.
fn lifecycle() -> LifecycleInput {
    LifecycleInput::new(NOW_MS, SummaryMaintenanceLifecycleCapabilities::default())
}

fn lineitem_catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "lineitem",
        Schema::new(vec![
            Field::plain("l_orderkey", DataType::Int64, false),
            Field::plain("l_extendedprice", DataType::Float64, false),
        ]),
    )
}

fn sql_workload(
    batch_entries: Vec<BatchEntry>,
    repeating: Option<Vec<RepeatingEntry>>,
) -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
            query_batch: Some(batch_entries),
            repeating_queries: repeating,
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::AtRest,
            ..Default::default()
        }),
    }
}

/// The facade turns a prepared workload into one selected DAG per query, in
/// `QueryWorkload::entries()` order, without the caller touching `PlanSpace`.
#[tokio::test]
async fn plans_every_query_in_entry_order() {
    let workload = sql_workload(
        vec![
            batch("SELECT COUNT(DISTINCT l_orderkey) FROM lineitem"),
            batch("SELECT approx_percentile_cont(l_extendedprice, 0.99) FROM lineitem"),
        ],
        None,
    );
    let catalog = lineitem_catalog();
    let input = UserInput::new(
        &workload,
        FrontendInput::Sql { catalog: &catalog },
        PlanningModels::builtin(),
        lifecycle(),
    );

    let output = e2e_plan(input).await.expect("workload plans");
    assert_eq!(output.plans.len(), 2);
    assert_eq!(output.entry_indices(), vec![0, 1]);
}

/// With the built-in cost model no lifecycle cost is ever known, and
/// lifecycle-aware selection then finalizes every summary target as raw
/// recompute: the cost-only selection picks a sketch for the same workload.
/// This pins that behavior so the facade's output is not mistaken for a
/// decision; it is a defect of `DefaultCostModel`, not addressed here.
#[tokio::test]
async fn builtin_cost_model_cannot_price_lifecycles_and_falls_back_to_raw_recompute() {
    let workload = sql_workload(
        vec![
            batch("SELECT COUNT(DISTINCT l_orderkey) FROM lineitem"),
            batch("SELECT approx_percentile_cont(l_extendedprice, 0.99) FROM lineitem"),
        ],
        None,
    );
    let catalog = lineitem_catalog();
    let models = PlanningModels::builtin();

    let output = e2e_plan(UserInput::new(
        &workload,
        FrontendInput::Sql { catalog: &catalog },
        models,
        lifecycle(),
    ))
    .await
    .expect("workload plans");

    // The cost-only selection over the same search space, the way a caller
    // reaches it without the facade.
    let mut roots = Vec::new();
    for (index, entry) in workload.query_workload.entries().enumerate() {
        let accuracy = entry.requirements.accuracy.target();
        let expr = lower_sql_dialect(
            &entry.query.0,
            &catalog,
            SqlDialect::DataFusionSQL,
            accuracy.clone(),
        )
        .await
        .expect("lowers");
        roots.push((index, Rc::new(expr), Some(accuracy)));
    }
    let strategies = default_strategies_with_evidence(models.cost, models.evidence);
    let space = search_workload_with_targets(roots, &strategies, models.accuracy);
    let selection = space.global_selection(models.cost);

    assert_eq!(output.plans.len(), space.roots.len());
    for (plan, (_, root)) in output.plans.iter().zip(&space.roots) {
        let cost_only = selection
            .assemble_selected_dag(root)
            .expect("assembles")
            .expect("root has a group");
        assert!(
            !matches!(cost_only.expr, SummaryExpr::KeepPreAsap(_)),
            "entry {}: cost-only selection was expected to pick a summary",
            plan.entry_index
        );
        assert!(
            matches!(plan.plan.root.expr, SummaryExpr::KeepPreAsap(_))
                && plan.plan.selected_raw_recompute
                && plan.plan.deployments.is_empty()
                && plan.plan.summary_total_cost.is_none()
                && plan.plan.raw_recompute_total_cost.is_none(),
            "entry {}: the built-in model priced a lifecycle",
            plan.entry_index
        );
    }
}

/// A repeating SQL query reaches the optimizer. `lower_sql_batch` walks
/// `query_batch` alone, so driving the frontend through it would drop exactly
/// the entries whose recurrence the lifecycle stage reads.
#[tokio::test]
async fn lowers_repeating_sql_entries_too() {
    let workload = sql_workload(
        vec![batch("SELECT COUNT(DISTINCT l_orderkey) FROM lineitem")],
        Some(vec![RepeatingEntry {
            query: Query(
                "SELECT approx_percentile_cont(l_extendedprice, 0.99) FROM lineitem".into(),
            ),
            demand: RepeatedDemand::FixedInterval(RepetitionInterval(60_000)),
            requirements: approximate(),
            predictability: Predictability::Unknown,
            time_selection: TimeSelection::default(),
        }]),
    );
    let catalog = lineitem_catalog();
    let input = UserInput::new(
        &workload,
        FrontendInput::Sql { catalog: &catalog },
        PlanningModels::builtin(),
        lifecycle(),
    );

    let output = e2e_plan(input).await.expect("workload plans");
    assert_eq!(
        output.len(),
        2,
        "batch entry and repeating entry both planned"
    );
    assert_eq!(output.entry_indices(), vec![0, 1]);
}

/// A caller-supplied pass replaces the shipped algorithm entirely: the trait is
/// the whole optimization stage, not a rule inside it.
#[tokio::test]
async fn runs_a_caller_supplied_pass_instead_of_the_shipped_one() {
    struct CountingPass;

    impl OptimizationPass for CountingPass {
        fn name(&self) -> &'static str {
            "counting"
        }
        fn optimize(&self, input: OptimizationInput<'_>) -> Result<PlanOutput, OptimizeError> {
            // Keeping every query on its raw path is a legal plan; this pass
            // exists to prove it is the one that ran.
            Err(OptimizeError::ContractViolation {
                pass: self.name(),
                detail: format!("saw {} entry/entries", input.workload.len()),
            })
        }
    }

    let workload = sql_workload(
        vec![batch("SELECT COUNT(DISTINCT l_orderkey) FROM lineitem")],
        None,
    );
    let catalog = lineitem_catalog();
    let pass = CountingPass;
    let input = UserInput::new(
        &workload,
        FrontendInput::Sql { catalog: &catalog },
        PlanningModels::builtin(),
        lifecycle(),
    )
    .with_pass(&pass);

    let err = e2e_plan(input).await.unwrap_err();
    let PlanError::Optimize(OptimizeError::ContractViolation { pass, detail }) = err else {
        panic!("expected the caller's pass to have run");
    };
    assert_eq!(pass, "counting");
    assert_eq!(detail, "saw 1 entry/entries");
}

/// The harness catches a pass that mislabels which query a plan belongs to,
/// rather than letting the mislabelled plan reach a deployment.
#[tokio::test]
async fn harness_rejects_a_pass_that_mislabels_entry_indices() {
    struct Mangling;

    impl OptimizationPass for Mangling {
        fn name(&self) -> &'static str {
            "mangling"
        }
        fn optimize(&self, input: OptimizationInput<'_>) -> Result<PlanOutput, OptimizeError> {
            let mut output = asap_aware_mapping::MajorPass.optimize(input)?;
            for plan in output.plans.iter_mut() {
                plan.entry_index += 1;
            }
            Ok(output)
        }
    }

    let workload = sql_workload(
        vec![batch("SELECT COUNT(DISTINCT l_orderkey) FROM lineitem")],
        None,
    );
    let catalog = lineitem_catalog();
    let pass = Mangling;
    let input = UserInput::new(
        &workload,
        FrontendInput::Sql { catalog: &catalog },
        PlanningModels::builtin(),
        lifecycle(),
    )
    .with_pass(&pass);

    let err = e2e_plan(input).await.unwrap_err();
    assert!(
        matches!(
            err,
            PlanError::Optimize(OptimizeError::ContractViolation {
                pass: "mangling",
                ..
            })
        ),
        "got {err}"
    );
}

/// A frontend input that cannot lower the workload's language fails before any
/// query is parsed.
#[tokio::test]
async fn rejects_a_frontend_that_does_not_match_the_workload_language() {
    let workload = sql_workload(
        vec![batch("SELECT COUNT(DISTINCT l_orderkey) FROM lineitem")],
        None,
    );
    let input = UserInput::new(
        &workload,
        FrontendInput::Promql {
            now_ms: NOW_MS,
            histograms: None,
        },
        PlanningModels::builtin(),
        lifecycle(),
    );

    let err = e2e_plan(input).await.unwrap_err();
    assert!(matches!(
        err,
        PlanError::Input(UserInputError::FrontendMismatch { .. })
    ));
}

/// Two planning clocks would let the DAG be built for one instant and priced
/// for another; the input check refuses that before lowering.
#[test]
fn rejects_disagreeing_planning_clocks() {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![batch("up")]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(15_000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    let input = UserInput::new(
        &workload,
        FrontendInput::Promql {
            now_ms: NOW_MS,
            histograms: None,
        },
        PlanningModels::builtin(),
        LifecycleInput::new(
            NOW_MS + 1,
            SummaryMaintenanceLifecycleCapabilities::default(),
        ),
    );

    assert!(matches!(
        input.validate(),
        Err(UserInputError::PlanningTimeMismatch { .. })
    ));
}

/// The maintenance decisions ride inside each plan, and the DAG is still
/// there — inside the plan's `root`, not alongside it.
#[tokio::test]
async fn lifecycle_decisions_ride_inside_each_plan() {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![RepeatingEntry {
                query: Query("count_over_time(up[5m])".into()),
                demand: RepeatedDemand::FixedInterval(RepetitionInterval(60_000)),
                requirements: approximate(),
                predictability: Predictability::Unknown,
                time_selection: TimeSelection::default(),
            }]),
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(15_000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    let input = UserInput::new(
        &workload,
        FrontendInput::Promql {
            now_ms: NOW_MS,
            histograms: None,
        },
        PlanningModels::builtin(),
        lifecycle().with_horizon(Horizon(3_600.0)),
    );

    let output = e2e_plan(input).await.expect("workload plans");
    assert_eq!(output.plans.len(), 1);
    assert_eq!(output.plans[0].entry_index, 0);
    let _: &Rc<_> = &output.plans[0].plan.root;
    assert_eq!(output.dags().len(), 1);
}
