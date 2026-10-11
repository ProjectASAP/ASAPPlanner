//! End-to-end coverage for the library facade (#429) and for plugging a
//! different optimization pass into it (#430).

use std::rc::Rc;

use asap_frontend_sql::{lower_sql_dialect, SqlCatalog};
use asap_logical_optimizer::pass1::logical_candidates::enumerate_local_logical_candidates;
use asap_plan_selection::{select_exhaustive, PlanningModels, MAX_ENUMERATED_CANDIDATES};
use asap_planner::pass::{OptimizationInput, OptimizationPass, OptimizeError, PlanOutput};
use asap_planner::{e2e_plan, FrontendInput, PlanError, UserInput, UserInputError};
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::QueryRoot;
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
/// `QueryWorkload::entries()` order, without the caller touching `CandidateLogicalASAPDAGs`.
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
    assert_eq!(output.plans.len(), 2);
    assert_eq!(output.entry_indices(), vec![0, 1]);
}

/// The facade selects what exhaustive Stage 1 → 3 selection selects over the
/// same inventory.
#[tokio::test]
async fn facade_plans_match_exhaustive_stage_pipeline_selection() {
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
    ))
    .await
    .expect("workload plans");

    // Every combination built and priced, the way a caller reaches it
    // without the facade.
    let mut roots = Vec::new();
    let mut targets = Vec::new();
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
        roots.push((index, QueryRoot::Operator(expr)));
        targets.push(Some(accuracy));
    }
    let inventory = enumerate_local_logical_candidates(roots).expect("Stage 1");
    let data = workload.data_workload.clone().unwrap_or_default();
    let enumeration = select_exhaustive(
        &inventory,
        &targets,
        &data,
        models,
        MAX_ENUMERATED_CANDIDATES,
    )
    .expect("selects");
    assert!(enumeration.combinations <= MAX_ENUMERATED_CANDIDATES);
    let exhaustive = enumeration
        .candidates
        .iter()
        .filter_map(|c| c.physical.as_ref())
        .find(|p| p.id == enumeration.selection.selected)
        .expect("selected candidate");

    let selection = output.selection.as_ref().expect("stage pipeline selection");
    assert_eq!(selection.selected, enumeration.selection.selected);
    assert!(selection.guaranteed_optimal());
    assert_eq!(output.plans.len(), exhaustive.roots.len());
    for (plan, root) in output.plans.iter().zip(&exhaustive.roots) {
        assert_eq!(
            &plan.root, root,
            "entry {}: the facade selected a different DAG",
            plan.entry_index
        );
    }
}

/// A repeating SQL query reaches the optimizer. `lower_sql_batch` walks
/// `query_batch` alone, so driving the frontend through it would drop the
/// repeating entries.
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
            let mut output = asap_planner::StagePipeline.optimize(input)?;
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

/// A repeating PromQL query yields one plan carrying its selected DAG root.
#[tokio::test]
async fn each_plan_carries_its_selected_root() {
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
    );

    let output = e2e_plan(input).await.expect("workload plans");
    assert_eq!(output.plans.len(), 1);
    assert_eq!(output.plans[0].entry_index, 0);
    let _: &Rc<_> = &output.plans[0].root;
    assert_eq!(output.operator_roots().len(), 1);
}

/// Scalar-only and mixed workloads preserve entry bindings without wrapper nodes.
#[tokio::test]
async fn scalar_roots_survive_planning_in_workload_order() {
    for queries in [
        vec!["2", "time()"],
        vec!["2", "up * 2", "scalar(sum(up)) + 1"],
    ] {
        let workload = PlanningWorkload {
            query_workload: QueryWorkload {
                language: QueryLanguage::PromQL,
                query_batch: Some(queries.iter().map(|q| batch(q)).collect()),
                repeating_queries: None,
            },
            data_workload: Some(DataWorkload {
                data_ingestion_interval: Evidence {
                    value: Some(DurationMs(1000)),
                    ..Default::default()
                },
                ..Default::default()
            }),
        };
        let output = e2e_plan(UserInput::new(
            &workload,
            FrontendInput::Promql {
                now_ms: NOW_MS,
                histograms: None,
            },
            PlanningModels::builtin(),
        ))
        .await
        .unwrap();
        assert_eq!(
            output.entry_indices(),
            (0..queries.len()).collect::<Vec<_>>()
        );
        assert!(matches!(
            output.roots()[0],
            asap_types::ir::QueryRoot::Scalar(_)
        ));
        assert_eq!(output.roots().len(), queries.len());
        if queries.len() == 3 {
            assert_eq!(output.plans[0].entry_index, 1);
            let asap_types::ir::QueryRoot::Scalar(expr) = &output.roots()[2] else {
                panic!()
            };
            assert_eq!(expr.operator_refs().len(), 1);
        }
    }
}
