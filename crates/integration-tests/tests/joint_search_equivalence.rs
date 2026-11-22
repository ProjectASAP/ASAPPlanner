//! End-to-end behavior of the joint candidate planner after solver cutover.
//!
//! Tiny tests compare bounded search with complete traversal of the *same*
//! generated catalog. This is not an independent solver oracle: exhaustive
//! assignment correctness is tested separately against the MILP model oracle.
//! Larger fixtures check validated behavior without asserting global optimality.

use asap_cost::{ClusterLimits, ClusterObjective, CostInputs, CostModel};
use asap_devtools::planning_stages::{lower_promql_roots, lower_sql_roots};
use asap_planner::{
    run_planning, CandidateExclusion, ErrorSelectingPlan, PlanSearchSettings, PlanningModels,
    PlanningRun, RejectionCause, SelectionError,
};
use asap_types::accuracy::AccuracyTarget;
use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::{NonASAPOp, QueryRoot};
use asap_types::plan::physical_asap_dag::PhysicalASAPOperatorPayload;
use asap_types::workload::{
    DataArrival, DataWorkload, DurationMs, PlanningWorkload, Predictability, PrometheusMetricType,
    Query, QueryLanguage, QueryRequirements, QueryWorkload, QueryWorkloadEntry, RepeatedDemand,
    RepeatingEntry, RepetitionInterval, SourceFacts, SqlDialect, TimestampMs,
};

fn workload_of(queries: &[(&str, u32)], accuracy: AccuracyTarget) -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(
                queries
                    .iter()
                    .map(|&(query, interval)| RepeatingEntry {
                        query: Query(query.into()),
                        // Evaluated at multiples of the interval: a known grid,
                        // which a chain delivers on.
                        demand: RepeatedDemand::FixedIntervalAt {
                            interval: RepetitionInterval(interval),
                            evaluation_phase: TimestampMs(0),
                        },
                        requirements: QueryRequirements {
                            accuracy: accuracy.clone(),
                            max_latency_ms: None,
                        },
                        predictability: Predictability::Predictable { known_at: None },
                        range_evaluation: None,
                    })
                    .collect(),
            ),
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 24.0,
                max_out_of_order_ms: None,
            },
            sources: ["m", "n", "c_total"]
                .into_iter()
                .map(|metric| {
                    (
                        metric.into(),
                        SourceFacts {
                            series_count: Some(8),
                            // Five minutes of each series, the longest
                            // range these queries read: a query-time plan
                            // reads every row it asks for.
                            rows_held: Some(2_400),
                            sample_interval: Some(DurationMs(1000)),
                            metric_type: Some(if metric == "c_total" {
                                PrometheusMetricType::Counter
                            } else {
                                PrometheusMetricType::Gauge
                            }),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
        }),
    }
}

fn run(
    roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    _exhaustive: bool,
) -> PlanningRun {
    run_planning(
        roots.to_vec(),
        entries,
        data,
        models,
        PlanSearchSettings::default(),
    )
    .unwrap()
}

fn assert_valid(run: &PlanningRun, queries: usize) {
    let (plan, selection) = run
        .selection
        .as_ref()
        .unwrap_or_else(|error| panic!("joint planning failed: {error}"));
    plan.validate().unwrap();
    assert_eq!(plan.roots.len() + plan.scalar_roots.len(), queries);
    assert_eq!(selection.accepted_accuracy_per_query.len(), queries);
}

fn run_workload(
    workload: &PlanningWorkload,
    settings: &CostInputs,
    deployment: &DeploymentCapabilities,
) -> PlanningRun {
    let roots = lower_promql_roots(workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let data = workload.data_workload.as_ref().unwrap();
    let run = run(
        &roots,
        &entries,
        data,
        PlanningModels::builtin()
            .with_cost_inputs(settings)
            .with_deployment_capabilities(deployment),
        false,
    );
    assert_valid(&run, entries.len());
    run
}

// Fixed-catalog terminal/producer scaling is tested in candidates::scopes.
#[test]
fn repeated_queries_preserve_all_outputs_and_exact_guarantees() {
    let settings = CostInputs::default();
    let deployment = DeploymentCapabilities::default();
    for count in [2, 8] {
        let workload = workload_of(
            &vec![("sum_over_time(m[1m])", 60_000); count],
            AccuracyTarget::exact(),
        );
        let run = run_workload(&workload, &settings, &deployment);
        let (_, selection) = run.selection.as_ref().unwrap();
        let accepted = &selection.accepted_accuracy_per_query;
        assert_eq!(accepted.len(), count);
        for accuracy in accepted {
            assert_eq!(accuracy.accepted_metric, None, "every output remains exact");
        }
    }
}

// Different windows over the same source remain legal with retained raw data
// disabled; their selected ingestion state must cover both output contracts.
#[test]
fn different_windows_and_raw_retention_policies_produce_valid_plans() {
    let workload = workload_of(
        &[
            ("sum_over_time(m[1m])", 60_000),
            ("sum_over_time(m[5m])", 60_000),
        ],
        AccuracyTarget::exact(),
    );
    for raw_data_retained in [true, false] {
        run_workload(
            &workload,
            &CostInputs::default(),
            &DeploymentCapabilities {
                raw_data_retained,
                ..Default::default()
            },
        );
    }
}

// Factoring a fused TopK candidate must not absorb the separately requested sum.
#[test]
fn fused_top_k_preserves_its_independently_requested_inner_sum() {
    let workload = workload_of(
        &[
            ("topk(5, sum by (job) (sum_over_time(m[5m])))", 60_000),
            ("sum by (job) (sum_over_time(m[5m]))", 60_000),
        ],
        AccuracyTarget::max_error(0.01),
    );
    run_workload(
        &workload,
        &CostInputs::default(),
        &DeploymentCapabilities::default(),
    );
}

// Grouped rollups coexist with temporal quantiles without merging distinct outputs.
#[test]
fn sketch_rollup_coexists_with_other_quantile_outputs() {
    let workload = workload_of(
        &[
            ("quantile_over_time(0.5, m[1m])", 10_000),
            ("quantile by (host, job) (0.5, m)", 10_000),
            ("quantile by (job) (0.5, m)", 10_000),
        ],
        AccuracyTarget::max_error(0.01).failure_probability(0.001),
    );
    run_workload(
        &workload,
        &CostInputs::default(),
        &DeploymentCapabilities::default(),
    );
}

// Local sketch contracts do not replace composed output accuracy validation.
#[test]
fn composed_quantile_and_sum_outputs_are_validated_together() {
    let workload = workload_of(
        &[
            ("sum by (job) (sum_over_time(m[5m]))", 60_000),
            ("avg by (job) (quantile_over_time(0.99, m[5m]))", 60_000),
        ],
        AccuracyTarget::max_error(0.05),
    );
    run_workload(
        &workload,
        &CostInputs::default(),
        &DeploymentCapabilities::default(),
    );
}

/// Example 2: three dashboard panels over one flow stream, every 10 s
/// over the last minute, with 10,000,000 distinct sources, and the
/// `flows(ts, src_ip)` table they read (the design names only these two
/// columns). The queries are the distinct sources (ε = 0.02), the entropy
/// of the per-source counts (ε = 0.05) and their L2 norm (ε = 0.01, the
/// strictest), each with δ = 0.01. Q3 casts the counts to DOUBLE: the
/// design's Int64 `c * c` can overflow, so the SQL frontend recognizes
/// only the Float64 product.
fn example2() -> (PlanningWorkload, asap_frontend_sql::SqlCatalog) {
    use asap_types::ir::schema::{DataType, Field, Schema};
    // (query, ε, δ) per panel.
    let panels = [
        (
            "SELECT COUNT(DISTINCT src_ip) FROM flows WHERE ts >= now() - INTERVAL '1 minute'",
            0.02,
            0.01,
        ),
        (
            "SELECT -SUM(p * LN(p)) FROM (\
             SELECT COUNT(*) * 1.0 / SUM(COUNT(*)) OVER () AS p FROM flows \
             WHERE ts >= now() - INTERVAL '1 minute' GROUP BY src_ip)",
            0.05,
            0.01,
        ),
        (
            "SELECT SQRT(SUM(CAST(c AS DOUBLE) * CAST(c AS DOUBLE))) FROM (\
             SELECT src_ip, COUNT(*) AS c FROM flows \
             WHERE ts >= now() - INTERVAL '1 minute' GROUP BY src_ip)",
            0.01,
            0.01,
        ),
    ];
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
            query_batch: None,
            repeating_queries: Some(
                panels
                    .iter()
                    .map(|&(query, epsilon, delta)| RepeatingEntry {
                        query: Query(query.into()),
                        demand: RepeatedDemand::FixedIntervalAt {
                            interval: RepetitionInterval(10_000),
                            evaluation_phase: TimestampMs(0),
                        },
                        requirements: QueryRequirements {
                            accuracy: AccuracyTarget::max_error(epsilon).failure_probability(delta),
                            max_latency_ms: None,
                        },
                        predictability: Predictability::Predictable { known_at: None },
                        range_evaluation: None,
                    })
                    .collect(),
            ),
        },
        // SQL needs no sample interval; `flows` holds 10,000,000 groups
        // (one per source IP).
        data_workload: Some(DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 1_000_000.0 / 15.0,
                max_out_of_order_ms: None,
            },
            sources: [(
                "flows".to_string(),
                SourceFacts {
                    series_count: Some(10_000_000),
                    ..SourceFacts::default()
                },
            )]
            .into(),
        }),
    };
    let catalog = asap_frontend_sql::SqlCatalog::new().with_table(
        "flows",
        Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("src_ip", DataType::Utf8, false),
            ],
            0,
            vec![],
        ),
    );
    (workload, catalog)
}

// The SQL frontend keeps repeated and different-window COUNT DISTINCT outputs.
#[test]
fn repeated_sql_queries_preserve_separate_output_references() {
    let (mut workload, catalog) = example2();
    let entries = workload.query_workload.repeating_queries.as_mut().unwrap();
    let first = entries[0].clone();
    *entries = [1, 5, 1].into_iter().map(|minutes| RepeatingEntry {
        query: Query(format!("SELECT COUNT(DISTINCT src_ip) FROM flows WHERE ts >= now() - INTERVAL '{minutes} minute'")), ..first.clone()
    }).collect();
    let roots = lower_sql_roots(&workload, &catalog).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let run = run(
        &roots,
        &entries,
        workload.data_workload.as_ref().unwrap(),
        PlanningModels::builtin(),
        false,
    );
    assert_valid(&run, 3);
}

// Request work on a literal output is charged though no operator node
// owns it. `1` every 1 ms is 1,000 requests per second; at 2 ms
// (2,000,000 ns) per request and the default 1 ns per CPU operation that
// is 2,000,000 operations each, 2e9 per second. The default unit cost
// prices one operation at 1e-6 (one cost unit is one core-millisecond),
// so the plan costs 2,000 per second, all of it request overhead.
#[test]
fn rootless_request_overhead_is_charged() {
    let workload = workload_of(&[("1", 1)], AccuracyTarget::exact());
    let roots = lower_promql_roots(&workload).unwrap();
    let mut settings = CostInputs::default();
    settings.measured_operation_costs.ns_per_evaluation = 2_000_000.0;
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let result = run(
        &roots,
        &entries,
        workload.data_workload.as_ref().unwrap(),
        PlanningModels::builtin().with_cost_inputs(&settings),
        false,
    );
    let (plan, selection) = result.selection.as_ref().unwrap();
    assert!(plan.nodes.is_empty(), "a literal needs no operator node");
    let cost = selection
        .selected_plan_cost_per_second
        .expect("cost model 1");
    assert!((cost - 2_000.0).abs() < 1e-6, "{cost}");
}

// The same request overhead counts against a latency bound and a cluster's
// CPU: 2 ms per request is over `max_latency_ms` 1, and 2 cores of work
// are over 1 core. Stage 3 checks neither yet: latency (L) and resource
// budgets are outside the selection MILP, and it refuses cost model 3.
#[test]
#[ignore = "Stage 3 checks no latency bound and no cluster capacity (#1316)"]
fn rootless_request_overhead_respects_capacity_and_latency() {
    let mut workload = workload_of(&[("1", 1)], AccuracyTarget::exact());
    let roots = lower_promql_roots(&workload).unwrap();
    for capacity in [true, false] {
        let mut settings = CostInputs::default();
        settings.measured_operation_costs.ns_per_evaluation = 2_000_000.0;
        if capacity {
            settings.cost_model = CostModel::ClusterCapacity(ClusterLimits {
                memory_bytes: 1_000_000_000,
                disk_bytes: 1_000_000_000,
                cpu_cores: Some(1),
                objective: ClusterObjective::LowestLatency,
            });
        } else {
            workload.query_workload.repeating_queries.as_mut().unwrap()[0]
                .requirements
                .max_latency_ms = Some(1.0);
        }
        let entries: Vec<_> = workload.query_workload.entries().collect();
        let result = run(
            &roots,
            &entries,
            workload.data_workload.as_ref().unwrap(),
            PlanningModels::builtin().with_cost_inputs(&settings),
            false,
        );
        // Refused because the request overhead does not fit, not because
        // the cost model is unsupported.
        assert!(matches!(
            &result.selection,
            Err(SelectionError::Selection(error))
                if !matches!(error, ErrorSelectingPlan::UnsupportedCostModel(_))
        ));
    }
}

// An exact operator the executor does not run is planned like any other,
// from raw rows, until the deployment declares it unsupported; then no
// plan is returned. `absent_over_time(m[5m])` exactly has one plan: a
// per-series `Aggregate[AbsentOverTime]` over the 5 m of raw rows (no
// summary answers it). Excluding that measure leaves no candidate.
#[test]
fn unsupported_exact_operation_declared_by_the_deployment_returns_no_plan() {
    let workload = workload_of(
        &[("absent_over_time(m[5m])", 60_000)],
        AccuracyTarget::exact(),
    );
    let settings = CostInputs::default();
    let planned = run_workload(&workload, &settings, &DeploymentCapabilities::default());
    let (plan, _) = planned.selection.as_ref().unwrap();
    assert!(plan.nodes.iter().any(|node| matches!(
        &node.payload,
        PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Aggregate { measures, .. })
            if measures.as_slice() == [AggIntent::AbsentOverTime]
    )));

    let mut deployment = DeploymentCapabilities::default();
    deployment.excluded_candidates.push(CandidateExclusion {
        aggregate_measure: Some("absent_over_time".to_string()),
        ..Default::default()
    });
    let roots = lower_promql_roots(&workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let refused = run(
        &roots,
        &entries,
        workload.data_workload.as_ref().unwrap(),
        PlanningModels::builtin()
            .with_cost_inputs(&settings)
            .with_deployment_capabilities(&deployment),
        false,
    );
    let Err(SelectionError::Selection(ErrorSelectingPlan::NoValidCandidate(rejections))) =
        &refused.selection
    else {
        panic!(
            "expected no valid candidate, got {:?}",
            refused.selection.as_ref().map(|(plan, _)| plan)
        );
    };
    assert!(
        !rejections.is_empty()
            && rejections
                .iter()
                .all(|rejection| rejection.cause == RejectionCause::ExcludedByDeployment),
        "{rejections:?}"
    );
}

// Deterministic small mixed workloads exercise exact temporal operators, counter
// semantics, different sources and windows without claiming timeout optimality.
#[test]
fn seeded_small_mixed_workloads_return_validated_plans() {
    let queries = [
        "sum_over_time(m[1m])",
        "sum_over_time(n[5m])",
        "max_over_time(m[1m])",
        "rate(c_total[5m])",
    ];
    for seed in [3u64, 17, 29] {
        let first = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let second = first.wrapping_mul(6364136223846793005).wrapping_add(1);
        let workload = workload_of(
            &[
                (queries[(first >> 32) as usize % queries.len()], 60_000),
                (queries[(second >> 32) as usize % queries.len()], 60_000),
            ],
            AccuracyTarget::exact(),
        );
        run_workload(
            &workload,
            &CostInputs::default(),
            &DeploymentCapabilities::default(),
        );
    }
}

/// A query-time computation over raw rows its source holds fewer of than
/// arrive over the range it needs is not offered: it would answer from fewer
/// samples than asked. `sum_over_time(m[1m])` every 60 s over 8 series at one
/// sample a second needs 60 rows a series per state, 480 over the minute,
/// but `m` holds 80 rows: every query-time chain (and the query-time exact
/// sum) is pruned with `ReadsFewerSourceRowsThanItsRange { rows: 80,
/// rows_in_range: 480 }`, and the selected plan builds the chain at
/// ingestion time, where it sees every row.
#[test]
fn a_query_time_computation_short_of_its_source_rows_is_not_offered() {
    use asap_types::ir::properties::ExecutionTiming;
    use asap_types::ir::ASAPOp;
    use asap_types::ir::Operator;
    use asap_types::stages::PruneReason;
    let mut workload = workload_of(&[("sum_over_time(m[1m])", 60_000)], AccuracyTarget::exact());
    // `m` holds 80 rows, ten seconds of its 8 series.
    workload
        .data_workload
        .as_mut()
        .unwrap()
        .sources
        .get_mut("m")
        .unwrap()
        .rows_held = Some(80);
    let run = run_workload(
        &workload,
        &CostInputs::default(),
        &DeploymentCapabilities {
            raw_data_retained: true,
            ..Default::default()
        },
    );
    let physical = run.physical_asap_candidates.as_ref().unwrap();
    let is_chain = |candidate: &asap_types::stages::PhysicalCandidate| {
        matches!(
            physical
                .logical_candidates
                .memo
                .candidate(candidate.logical_candidate_id)
                .expression
                .operator,
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
        )
    };
    let (plan, selection) = run.selection.as_ref().unwrap();
    // Every query-time chain the selection could have read is pruned for
    // reading 80 of the 480 rows of its minute.
    let query_time_chains: Vec<_> = physical
        .physical_candidates
        .iter()
        .filter(|candidate| {
            is_chain(candidate)
                && candidate.physical_properties.timing() == ExecutionTiming::QueryTime
        })
        .collect();
    assert!(
        !query_time_chains.is_empty(),
        "Stage 2 offers a query-time chain"
    );
    for chain in &query_time_chains {
        assert!(
            selection.pruned_candidates.iter().any(|pruned| {
                pruned.candidate == **chain
                    && pruned.reason
                        == PruneReason::ReadsFewerSourceRowsThanItsRange {
                            rows: 80,
                            rows_in_range: 480,
                        }
            }),
            "{chain:?} is not pruned for its source rows: {:?}",
            selection.pruned_candidates
        );
    }
    // The selected plan builds the chain at ingestion time.
    let chains: Vec<_> = plan
        .nodes
        .iter()
        .filter(|node| {
            matches!(
                node.payload,
                PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
            )
        })
        .collect();
    assert!(
        !chains.is_empty(),
        "the plan builds a chain: {:#?}",
        plan.nodes
            .iter()
            .map(|n| (
                format!("{:?}", n.payload)
                    .chars()
                    .take(90)
                    .collect::<String>(),
                n.output_state.timing
            ))
            .collect::<Vec<_>>()
    );
    assert!(chains
        .iter()
        .all(|node| node.output_state.timing == ExecutionTiming::IngestionTime));
}

/// One query every `every_ms` over `m` alone: `series` series sampled every
/// `sample_interval_ms`, holding `rows_held` rows, arriving as `arrival`.
fn one_source_workload(
    query: &str,
    every_ms: u32,
    accuracy: AccuracyTarget,
    series: u64,
    sample_interval_ms: u64,
    rows_held: u64,
    arrival: DataArrival,
) -> PlanningWorkload {
    let mut workload = workload_of(&[(query, every_ms)], accuracy);
    let data = workload.data_workload.as_mut().unwrap();
    data.arrival = arrival;
    data.sources.retain(|source, _| source == "m");
    let m = data.sources.get_mut("m").unwrap();
    m.series_count = Some(series);
    m.sample_interval = Some(DurationMs(sample_interval_ms));
    m.rows_held = Some(rows_held);
    workload
}

/// Whether any candidate was pruned for reading fewer raw rows than its
/// range.
fn pruned_for_too_few_rows(run: &PlanningRun) -> bool {
    use asap_types::stages::PruneReason;
    run.selection.as_ref().is_ok_and(|(_, selection)| {
        selection.pruned_candidates.iter().any(|pruned| {
            matches!(
                pruned.reason,
                PruneReason::ReadsFewerSourceRowsThanItsRange { .. }
            )
        })
    })
}

/// Data at rest is all held: 1,000 series every 15 s holding 60,000 rows
/// (fifteen minutes) answer `sum_over_time(m[1h])` from every row there is,
/// so its query-time plan is offered (nothing arrives to build at
/// ingestion time).
#[test]
fn data_at_rest_is_never_short_of_its_range() {
    let workload = one_source_workload(
        "sum_over_time(m[1h])",
        60_000,
        AccuracyTarget::exact(),
        1_000,
        15_000,
        60_000,
        DataArrival::AtRest,
    );
    let run = run_workload(
        &workload,
        &CostInputs::default(),
        &DeploymentCapabilities::default(),
    );
    assert!(!pruned_for_too_few_rows(&run));
}

/// A query-time build counts the rows it reads at the exact rate, as a
/// chain does: one series every 25 s over 1 min holds 2.4 rows (a time
/// range rounds it to 2, a chain's window to 3 before); every 3 s over
/// 10 s, 3.33 (3 and 4). Planning runs `logical_output`'s debug assertion
/// that candidates of the same states fold the same rows.
#[test]
fn query_time_builds_fold_the_exact_rows_of_their_range() {
    for (query, every_ms, sample_interval_ms) in [
        ("quantile_over_time(0.9, m[1m])", 60_000, 25_000),
        ("quantile_over_time(0.9, m[10s])", 10_000, 3_000),
    ] {
        let workload = one_source_workload(
            query,
            every_ms,
            AccuracyTarget::max_error(0.05),
            1,
            sample_interval_ms,
            1_000_000,
            DataArrival::Ingesting {
                rows_per_second: 1_000.0 / sample_interval_ms as f64,
                max_out_of_order_ms: None,
            },
        );
        run_workload(
            &workload,
            &CostInputs::default(),
            &DeploymentCapabilities::default(),
        );
    }
}

/// A label filter passes a share of the rows on; the states over it still
/// fold alike whichever candidate builds them (the debug assertion).
#[test]
fn a_filtered_quantile_folds_the_same_rows_at_every_timing() {
    let workload = one_source_workload(
        r#"quantile_over_time(0.9, m{job="a"}[5m])"#,
        60_000,
        AccuracyTarget::max_error(0.05),
        1_000,
        15_000,
        100_000_000,
        DataArrival::Ingesting {
            rows_per_second: 1_000.0 / 15.0,
            max_out_of_order_ms: None,
        },
    );
    run_workload(
        &workload,
        &CostInputs::default(),
        &DeploymentCapabilities::default(),
    );
}

/// An offset reaches further back than its range: with five minutes held,
/// `sum_over_time(m[5m] offset 5m)` needs the five before them, none of
/// which are held, so its query-time plans are not offered (the plan builds
/// a chain at ingestion time, which keeps ten minutes).
#[test]
fn an_offset_counts_in_the_range_a_read_needs() {
    let query = "sum_over_time(m[5m] offset 5m)";
    let workload = one_source_workload(
        query,
        60_000,
        AccuracyTarget::exact(),
        8,
        1_000,
        8 * 300,
        DataArrival::Ingesting {
            rows_per_second: 8.0,
            max_out_of_order_ms: None,
        },
    );
    let roots = lower_promql_roots(&workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let settings = CostInputs::default();
    let deployment = DeploymentCapabilities {
        raw_data_retained: true,
        ..Default::default()
    };
    let run = run(
        &roots,
        &entries,
        workload.data_workload.as_ref().unwrap(),
        PlanningModels::builtin()
            .with_cost_inputs(&settings)
            .with_deployment_capabilities(&deployment),
        false,
    );
    let rejected_for_rows = match &run.selection {
        Ok(_) => pruned_for_too_few_rows(&run),
        Err(error) => error.to_string().contains("fewer samples than asked"),
    };
    assert!(rejected_for_rows, "{query}");
}
