//! End-to-end coverage for the library facade (#429) and for plugging a
//! different optimization pass into it (#430).

use asap_frontend_sql::{lower_sql_dialect, SqlCatalog};
use asap_planner::PlanningModels;
use asap_planner::{e2e_plan, FrontendInput, PlanError, UserInput, UserInputError};
use asap_planner::{
    run_planning, OptimizationInput, OptimizationPass, OptimizeError, PlanOutput,
    Stage1ToStage3Pass,
};
use asap_types::accuracy::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::workload::{
    BatchEntry, DataArrival, DataWorkload, DurationMs, PlanningWorkload, Predictability, Query,
    QueryLanguage, QueryRequirements, QueryWorkload, RepeatedDemand, RepeatingEntry,
    RepetitionInterval, SourceFacts, SqlDialect, TimestampMs,
};

fn approximate() -> QueryRequirements {
    QueryRequirements {
        accuracy: AccuracyTarget::max_error(0.01),
        max_latency_ms: None,
    }
}

/// [`approximate`], stated as a rank error for a quantile query, whose
/// target names its metric.
fn approximate_for(sql: &str) -> QueryRequirements {
    let mut requirements = approximate();
    if sql.contains("percentile") {
        requirements.accuracy =
            AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Rank);
    }
    requirements
}

fn batch(sql: &str) -> BatchEntry {
    BatchEntry {
        query: Query(sql.into()),
        requirements: approximate_for(sql),
        predictability: Predictability::AdHoc,
        invocations: 1,
        execute_at: None,
        range_evaluation: None,
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
        // `lineitem` at rest, holding a million rows.
        data_workload: Some(DataWorkload {
            arrival: DataArrival::AtRest,
            sources: [(
                "lineitem".to_string(),
                SourceFacts {
                    rows_held: Some(1_000_000),
                    ..SourceFacts::default()
                },
            )]
            .into(),
        }),
    }
}

/// The facade turns a prepared workload into one selected DAG per query, in
/// `QueryWorkload::entries()` order, without the caller touching any stage's
/// candidates.
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
    assert_eq!(output.plan.roots.len(), 2);
    assert!(output.plan.scalar_roots.is_empty());
}

/// The facade returns the plan the planning stages build over the frontends'
/// roots ([`run_planning`]).
#[tokio::test]
async fn facade_plans_match_run_planning() {
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

    // The same stages over the same roots, the way a caller reaches them
    // without the facade.
    let mut roots = Vec::new();
    let mut targets = Vec::new();
    for (index, entry) in workload.query_workload.entries().enumerate() {
        let accuracy = entry.requirements.accuracy.clone();
        let expr = lower_sql_dialect(&entry.query.0, &catalog, SqlDialect::DataFusionSQL)
            .await
            .expect("lowers");
        roots.push((index, expr.into()));
        targets.push(Some(accuracy));
    }
    let data = workload.data_workload.clone().unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    assert_eq!(
        entries
            .iter()
            .map(|e| Some(e.requirements.accuracy.clone()))
            .collect::<Vec<_>>(),
        targets
    );
    let roots = roots.into_iter().map(|(_, root)| root).collect();
    let run = run_planning(
        roots,
        &entries,
        &data,
        models,
        asap_planner::PlanSearchSettings::default(),
    )
    .unwrap();
    let (plan, _selection) = run.selection.as_ref().expect("a candidate is selected");
    assert_eq!(&output.plan, plan);
}

/// A repeating SQL query reaches the optimizer, not only `query_batch` entries.
#[tokio::test]
async fn lowers_repeating_sql_entries_too() {
    let workload = sql_workload(
        vec![batch("SELECT COUNT(DISTINCT l_orderkey) FROM lineitem")],
        Some(vec![RepeatingEntry {
            query: Query(
                "SELECT approx_percentile_cont(l_extendedprice, 0.99) FROM lineitem".into(),
            ),
            demand: RepeatedDemand::FixedIntervalAt {
                interval: RepetitionInterval(60_000),
                evaluation_phase: TimestampMs(0),
            },
            requirements: approximate_for("approx_percentile_cont"),
            predictability: Predictability::Unknown,
            range_evaluation: None,
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
        output.plan.roots.len(),
        2,
        "batch entry and repeating entry both planned"
    );
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

/// The harness catches a pass whose plan answers fewer queries than the
/// workload has, rather than letting the plan reach a deployment.
#[tokio::test]
async fn harness_rejects_a_pass_that_drops_a_query() {
    struct Mangling;

    impl OptimizationPass for Mangling {
        fn name(&self) -> &'static str {
            "mangling"
        }
        fn optimize(&self, input: OptimizationInput<'_>) -> Result<PlanOutput, OptimizeError> {
            let mut output = Stage1ToStage3Pass.optimize(input)?;
            output.plan.roots.pop();
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
    let input = UserInput::new(&workload, FrontendInput::Promql, PlanningModels::builtin());

    let err = e2e_plan(input).await.unwrap_err();
    assert!(matches!(
        err,
        PlanError::Input(UserInputError::FrontendMismatch { .. })
    ));
}

/// A repeating PromQL query yields a plan with one root.
#[tokio::test]
async fn each_plan_carries_its_selected_root() {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![RepeatingEntry {
                query: Query("count_over_time(up[5m])".into()),
                demand: RepeatedDemand::FixedIntervalAt {
                    interval: RepetitionInterval(60_000),
                    evaluation_phase: TimestampMs(0),
                },
                requirements: approximate(),
                predictability: Predictability::Unknown,
                range_evaluation: None,
            }]),
        },
        // A range selector only: no sample interval is needed; 1,000 rows
        // arrive per second.
        data_workload: Some(DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 1_000.0,
                max_out_of_order_ms: None,
            },
            ..Default::default()
        }),
    };
    let input = UserInput::new(&workload, FrontendInput::Promql, PlanningModels::builtin());

    let output = e2e_plan(input).await.expect("workload plans");
    assert_eq!(output.plan.roots.len(), 1);
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
            data_workload: Some(
                DataWorkload::sampled_every(
                    queries
                        .iter()
                        .flat_map(|q| asap_frontend_promql::promql_metric_names(q).unwrap()),
                    DurationMs(1000),
                )
                .with_series_count(1_000),
            ),
        };
        let output = e2e_plan(UserInput::new(
            &workload,
            FrontendInput::Promql,
            PlanningModels::builtin(),
        ))
        .await
        .unwrap();
        let roots = output.plan.query_roots().unwrap();
        assert!(matches!(roots[0], asap_types::ir::QueryRoot::Scalar(_)));
        assert_eq!(roots.len(), queries.len());
        if queries.len() == 3 {
            assert!(matches!(roots[1], asap_types::ir::QueryRoot::Operator(_)));
            let asap_types::ir::QueryRoot::Scalar(expr) = &roots[2] else {
                panic!()
            };
            assert_eq!(expr.operator_refs().len(), 1);
        }
    }
}
