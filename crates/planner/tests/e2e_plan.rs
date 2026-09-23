//! End-to-end coverage for the library facade (#429) and for plugging a
//! different optimization pass into it (#430).

use std::rc::Rc;

use asap_aware_mapping::pass::{
    OptimizationInput, OptimizationPass, OptimizeError, PlanOutput, PlanningModels,
};
use asap_aware_mapping::{Horizon, LifecycleInput, SummaryMaintenanceLifecycleCapabilities};
use asap_frontend_sql::SqlCatalog;
use asap_planner::{e2e_plan, FrontendInput, PlanError, UserInput, UserInputError};
use asap_types::pre_asap::schema::{Column, DataType, Schema};
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

fn lineitem_catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "lineitem",
        Schema::new(vec![
            Column::new("l_orderkey", DataType::Int64, false),
            Column::new("l_extendedprice", DataType::Float64, false),
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
    );

    let output = e2e_plan(input).await.expect("workload plans");
    let PlanOutput::Dag { plans } = &output else {
        panic!("no lifecycle input was supplied, so the DAG-only variant is expected");
    };
    assert_eq!(plans.len(), 2);
    assert_eq!(output.entry_indices(), vec![0, 1]);
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
            if let PlanOutput::Dag { plans } = &mut output {
                for plan in plans.iter_mut() {
                    plan.entry_index += 1;
                }
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
    )
    .with_lifecycle(LifecycleInput::new(
        NOW_MS + 1,
        SummaryMaintenanceLifecycleCapabilities::default(),
    ));

    assert!(matches!(
        input.validate(),
        Err(UserInputError::PlanningTimeMismatch { .. })
    ));
}

/// Supplying lifecycle input switches the output variant, and the DAG is still
/// there — inside each plan's `root`, not alongside it.
#[tokio::test]
async fn lifecycle_input_selects_the_lifecycle_variant() {
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
    )
    .with_lifecycle(
        LifecycleInput::new(NOW_MS, SummaryMaintenanceLifecycleCapabilities::default())
            .with_horizon(Horizon(3_600.0)),
    );

    let output = e2e_plan(input).await.expect("workload plans");
    let PlanOutput::DagWithLifecycle { plans } = &output else {
        panic!("lifecycle input was supplied, so the lifecycle variant is expected");
    };
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].entry_index, 0);
    let _: &Rc<_> = &plans[0].plan.root;
}
