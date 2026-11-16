//! Two contracts of Stage 3's `select_plan`, checked against an oracle that
//! enumerates every plan of the physical memo without Stage 3's cost
//! coefficients or MILP and costs each with `asap_cost::plan_cost`
//! (`plan_enumeration`):
//!
//! - **Installable selection.** Stage 3 never returns a plan that the
//!   planner's own install rules refuse (`PhysicalASAPDAG::materialized_views`
//!   and `PhysicalASAPDAG::raw_data_retention`). A rule over one node's
//!   subtree excludes that node's candidate (`PruneReason::NotInstallable`)
//!   and the selection runs again; when no plan installs, Stage 3 returns an
//!   error.
//! - **Optimality.** The selected plan's `plan_cost` is the least
//!   `plan_cost` of any installable plan, within a relative 1e-6. A cost
//!   coefficient wrong only on candidates the MILP did not select is
//!   invisible to "objective = `plan_cost` of the selected plan"; it is not
//!   invisible here.
//!
//! Expected costs come from the oracle; no expected value is read from
//! Stage 3.

use asap_cost::{CostInputs, CostModel, ResourceUnitCost};
use asap_devtools::planning_stages::{
    lower_promql_roots, lower_sql_roots, run_stages_until, sql_catalog, RunUntil,
};
use asap_planner::{DeploymentCapabilities, PlanningModels, SelectionError};
use asap_stage3_plan_selection::Selection;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::stages::{Lifetime, PhysicalASAPMemoCandidates, PhysicalCandidateId, PruneReason};
use asap_types::workload::{DataWorkload, PlanningWorkload, QueryWorkloadEntry};
use serde_json::{json, Value};

use crate::plan_enumeration::{
    enumerate_plans, plan_signature, queries_reaching, set_least_retention, Enumeration,
};

/// The relative tolerance of a cost comparison: HiGHS solves at gap 0, so
/// only its feasibility tolerance and the objective's scaling separate the
/// optimum from the cheapest plan.
const RELATIVE_TOLERANCE: f64 = 1e-6;

/// At most this many complete choices per workload; above it the oracle
/// cannot run and the test fails saying so.
const MAX_CHOICES: usize = 400_000;

/// One workload of the contracts.
#[derive(Clone)]
struct Workload {
    name: &'static str,
    /// The planning workload (queries and data).
    planning: PlanningWorkload,
    /// SQL table declarations, `None` for PromQL.
    sql_tables: Option<Vec<String>>,
    raw_data_retained: bool,
    ignore_accuracy_guarantees: bool,
    cost_inputs: CostInputs,
}

/// A PromQL query `query` every `interval_ms` at phase `phase_ms`; a range
/// query with `(step_ms, span_ms)`; within `accuracy` (JSON, `null` for
/// none).
fn promql(
    query: &str,
    interval_ms: u64,
    phase_ms: u64,
    range: Option<(u64, u64)>,
    accuracy: Value,
) -> Value {
    let mut entry = json!({
        "query": query,
        "demand": {"fixed_interval_at": {"interval": interval_ms, "evaluation_phase": phase_ms}},
        "requirements": {},
        "predictability": {"predictable": {"known_at": null}},
    });
    if !accuracy.is_null() {
        entry["requirements"]["accuracy"] = accuracy;
    }
    if let Some((step_ms, span_ms)) = range {
        entry["range_evaluation"] = json!({"step_ms": step_ms, "span_ms": span_ms});
    }
    entry
}

/// `entries` as a PromQL workload over the gauge `m` and the counter `c`
/// (1,000 series each, one sample per second, 2,000 rows/s), raw data kept
/// or not, accuracy enforced, default unit costs.
fn over_m_and_c(name: &'static str, entries: Vec<Value>, raw_data_retained: bool) -> Workload {
    let planning = serde_json::from_value(json!({
        "query_workload": {"language": "promql", "repeating_queries": entries},
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 2000.0}},
            "sources": {
                "m": {"series_count": 1000, "metric_type": "gauge", "sample_interval": 1000},
                "c": {"series_count": 1000, "metric_type": "counter", "sample_interval": 1000},
            },
        },
    }))
    .unwrap();
    Workload {
        name,
        planning,
        sql_tables: None,
        raw_data_retained,
        ignore_accuracy_guarantees: false,
        cost_inputs: CostInputs::default(),
    }
}

/// SQL `queries` every `interval_ms` over `flows(ts, src, dst, bytes)`
/// (1,000 rows/s), each within `accuracy`, raw data kept or not.
fn over_flows(
    name: &'static str,
    queries: &[&str],
    interval_ms: u64,
    accuracy: Value,
    raw_data_retained: bool,
) -> Workload {
    over_flows_holding(
        name,
        queries,
        interval_ms,
        accuracy,
        raw_data_retained,
        None,
    )
}

/// [`over_flows`] with the table holding `rows_held` rows (`None`:
/// undeclared, so a query-time scan of the whole table cannot be costed).
fn over_flows_holding(
    name: &'static str,
    queries: &[&str],
    interval_ms: u64,
    accuracy: Value,
    raw_data_retained: bool,
    rows_held: Option<u64>,
) -> Workload {
    let flows = json!({"name": "flows", "time_index": 0, "columns": [
        {"name": "ts", "type": "timestamp"},
        {"name": "src", "type": "utf8"},
        {"name": "dst", "type": "utf8"},
        {"name": "bytes", "type": "float64"},
    ]});
    let planning = serde_json::from_value(json!({
        "query_workload": {
            "language": {"sql": "datafusion_sql"},
            "repeating_queries": queries.iter().map(|query| {
                let mut entry = json!({
                    "query": query,
                    "demand": {"fixed_interval_at": {"interval": interval_ms, "evaluation_phase": 0}},
                    "requirements": {},
                    "predictability": {"predictable": {"known_at": null}},
                });
                if !accuracy.is_null() {
                    entry["requirements"]["accuracy"] = accuracy.clone();
                }
                entry
            }).collect::<Vec<_>>(),
        },
        "data_workload": {
            "arrival": {"ingesting": {"rows_per_second": 1000.0}},
            "sources": {"flows": match rows_held {
                Some(rows_held) => json!({"series_count": 1000, "rows_held": rows_held}),
                None => json!({"series_count": 1000}),
            }},
        },
    }))
    .unwrap();
    Workload {
        name,
        planning,
        sql_tables: Some(vec![flows.to_string()]),
        raw_data_retained,
        ignore_accuracy_guarantees: false,
        cost_inputs: CostInputs::default(),
    }
}

/// Unit costs that charge what the default ones leave at 0: 50 µs of
/// runtime overhead per query request and 1e-9 per byte-second on disk.
fn overhead_and_disk_costs() -> CostInputs {
    let mut settings = CostInputs::default();
    settings.measured_operation_costs.ns_per_evaluation = 50_000.0;
    settings.cost_model = CostModel::UsageOverTime(ResourceUnitCost {
        cost_per_disk_byte_second: 1e-9,
        ..ResourceUnitCost::illustrative()
    });
    settings
}

/// What the stages produced for a workload.
struct Run {
    physical: PhysicalASAPMemoCandidates,
    entries: Vec<QueryWorkloadEntry>,
    data: DataWorkload,
    selection: Result<(PhysicalASAPDAG, Selection), SelectionError>,
}

/// Runs the stages over `w` through Stage 3 (`select_plan`).
fn run(w: &Workload) -> Run {
    let entries: Vec<_> = w.planning.query_workload.entries().collect();
    let data = w.planning.data_workload.clone().unwrap();
    let roots = match &w.sql_tables {
        Some(tables) => lower_sql_roots(&w.planning, &sql_catalog(tables).unwrap().0).unwrap(),
        None => lower_promql_roots(&w.planning).unwrap(),
    };
    let capabilities = DeploymentCapabilities {
        raw_data_retained: w.raw_data_retained,
        ..Default::default()
    };
    let models = PlanningModels::builtin()
        .with_cost_inputs(&w.cost_inputs)
        .with_deployment_capabilities(&capabilities)
        .with_ignore_accuracy_guarantees(w.ignore_accuracy_guarantees);
    let stages = run_stages_until(roots, &entries, &data, models, RunUntil::Stage3)
        .unwrap_or_else(|error| panic!("{}: planning failed before Stage 3: {error}", w.name));
    let selection = stages.run.selection;
    let Some(physical) = stages.run.physical_asap_candidates else {
        panic!("{}: no Stage 2 output: {:?}", w.name, selection.err());
    };
    Run {
        physical,
        entries,
        data,
        selection,
    }
}

/// The oracle over `run`; panics, so the test reports "cannot run", when
/// the memo is too large or has a shape the oracle does not model.
fn oracle(w: &Workload, run: &Run) -> Enumeration {
    let enumeration = enumerate_plans(
        &run.physical,
        &run.entries,
        &run.data,
        &w.cost_inputs,
        MAX_CHOICES,
    )
    .unwrap_or_else(|error| panic!("{}: the oracle cannot run: {error}", w.name));
    println!(
        "{}: {} choices, {} refused, {} not costable (first: {:?}), {} installable, {} not installable",
        w.name,
        enumeration.choices,
        enumeration.refused,
        enumeration.not_costable,
        enumeration.first_cost_error,
        enumeration.installable,
        enumeration.not_installable
    );
    enumeration
}

/// `plan_cost` of `plan` per second under `w`'s cost inputs.
fn plan_cost(w: &Workload, run: &Run, plan: &PhysicalASAPDAG) -> f64 {
    let (cost, _) = asap_cost::plan_cost(plan, &run.entries, &run.data, &w.cost_inputs)
        .unwrap_or_else(|error| panic!("{}: plan_cost: {error:?}", w.name));
    cost.cost_per_second.expect("cost model 1")
}

/// One line per node: id, candidate, operator, timing, lifetime,
/// retention, inputs.
fn describe(plan: &PhysicalASAPDAG) -> String {
    plan.nodes
        .iter()
        .map(|node| {
            let inputs: Vec<PhysicalASAPNodeId> = plan
                .edges
                .iter()
                .filter(|edge| edge.consumer == node.id)
                .map(|edge| edge.producer)
                .collect();
            format!(
                "  {} v{:?} {} {:?} {:?} retention_ms={:?} <- {inputs:?}",
                node.id,
                node.selected_physical_candidate.map(|v| v.0),
                node.payload.kind_name(),
                node.output_state.timing,
                node.lifetime,
                node.retention_ms,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The planner's own install rules over `plan`.
fn install_error(plan: &PhysicalASAPDAG) -> Option<String> {
    plan.materialized_views()
        .err()
        .or_else(|| plan.raw_data_retention().err())
        .map(|error| format!("{error:?}"))
}

/// The optimality contract over `w`: Stage 3 returns a plan exactly when
/// some plan installs, the plan installs, and its `plan_cost` is the least
/// of any installable plan's.
fn assert_selects_the_cheapest_installable_plan(w: &Workload) {
    let run = run(w);
    let found = oracle(w, &run);
    match (&found.cheapest_installable, &run.selection) {
        (None, Err(_)) => {}
        (None, Ok((plan, _))) => panic!(
            "{}: no plan installs, yet Stage 3 selected one ({:?}):\n{}",
            w.name,
            install_error(plan),
            describe(plan)
        ),
        (Some(best), Err(error)) => panic!(
            "{}: Stage 3 selected no plan ({error}); the cheapest installable plan costs {}:\n{}",
            w.name,
            best.cost_per_second,
            describe(&best.plan)
        ),
        (Some(best), Ok((plan, _))) => {
            if let Some(error) = install_error(plan) {
                panic!(
                    "{}: the selected plan does not install ({error}):\n{}",
                    w.name,
                    describe(plan)
                );
            }
            let selected = plan_cost(w, &run, plan);
            let mut least_retention = plan.clone();
            set_least_retention(&mut least_retention, &run.entries);
            let selected_at_least_retention = plan_cost(w, &run, &least_retention);
            println!(
                "{}: selected {selected}, cheapest installable {} ({} at the least retention)",
                w.name, best.cost_per_second, selected_at_least_retention
            );
            assert!(
                selected <= best.cost_per_second * (1.0 + RELATIVE_TOLERANCE),
                "{}: the selected plan costs {selected}/s, the cheapest installable plan {}/s \
                 (+{:.3}%; the selected plan at the least retention its readers need: {}/s)\n\
                 selected:\n{}\ncheapest:\n{}",
                w.name,
                best.cost_per_second,
                100.0 * (selected / best.cost_per_second - 1.0),
                selected_at_least_retention,
                describe(plan),
                describe(&best.plan)
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The corpus: small workloads whose plans the oracle enumerates.

fn corpus() -> Vec<Workload> {
    let none = Value::Null;
    let mut workloads = vec![
        over_m_and_c(
            "p01_sum_5m_raw",
            vec![promql(
                "sum_over_time(m[5m])",
                60_000,
                0,
                None,
                none.clone(),
            )],
            true,
        ),
        over_m_and_c(
            "p02_sum_5m_noraw",
            vec![promql(
                "sum_over_time(m[5m])",
                60_000,
                0,
                None,
                none.clone(),
            )],
            false,
        ),
        over_m_and_c(
            "p03_rate_5m_raw",
            vec![promql("sum(rate(c[5m]))", 60_000, 0, None, none.clone())],
            true,
        ),
        over_m_and_c(
            "p04_max_14m_every_7m_noraw",
            vec![promql(
                "max_over_time(m[14m])",
                420_000,
                0,
                None,
                none.clone(),
            )],
            false,
        ),
        over_m_and_c(
            "p05_count_1m_raw",
            vec![promql(
                "count_over_time(m[1m])",
                60_000,
                0,
                None,
                none.clone(),
            )],
            true,
        ),
        over_m_and_c(
            "p06_two_phases_one_grid_raw",
            vec![
                promql("sum_over_time(m[1m])", 20_000, 10_000, None, none.clone()),
                promql("max_over_time(m[1m])", 10_000, 0, None, none.clone()),
            ],
            true,
        ),
        over_m_and_c(
            "p07_range_query_1h_span_raw",
            vec![promql(
                "sum_over_time(m[5m])",
                60_000,
                0,
                Some((60_000, 3_600_000)),
                none.clone(),
            )],
            true,
        ),
        over_m_and_c(
            "p08_two_filters_one_source_raw",
            vec![
                promql(
                    r#"sum_over_time(m{job="a"}[5m])"#,
                    60_000,
                    0,
                    None,
                    none.clone(),
                ),
                promql(
                    r#"sum_over_time(m{job="b"}[5m])"#,
                    60_000,
                    0,
                    None,
                    none.clone(),
                ),
            ],
            true,
        ),
        over_m_and_c(
            "p09_quantile_rank_target_noraw",
            vec![promql(
                "quantile_over_time(0.9, m[5m])",
                60_000,
                0,
                None,
                json!({"accuracy_metric": "rank", "max_error": 0.01, "failure_probability": 0.01}),
            )],
            false,
        ),
        over_m_and_c(
            "p10_offset_1h_hourly_plus_plain_noraw",
            vec![
                promql(
                    "sum_over_time(m[1m] offset 1h)",
                    3_600_000,
                    0,
                    None,
                    none.clone(),
                ),
                promql("sum_over_time(m[1m])", 10_000, 0, None, none.clone()),
            ],
            false,
        ),
        over_flows(
            "s01_sql_windowed_sum",
            &["SELECT sum(bytes) FROM flows WHERE ts > now() - INTERVAL '5' MINUTE"],
            10_000,
            none.clone(),
            true,
        ),
    ];
    let mut with_disk = over_m_and_c(
        "p11_two_filters_overhead_and_disk_raw",
        vec![
            promql(
                r#"sum_over_time(m{job="a"}[5m])"#,
                60_000,
                0,
                None,
                none.clone(),
            ),
            promql(
                r#"sum_over_time(m{job="b"}[5m])"#,
                60_000,
                0,
                None,
                none.clone(),
            ),
        ],
        true,
    );
    with_disk.cost_inputs = overhead_and_disk_costs();
    workloads.push(with_disk);
    workloads.extend(regressions());
    workloads
}

/// The regression workloads of the installable-selection and cost
/// findings.
fn regressions() -> Vec<Workload> {
    let none = Value::Null;
    vec![
        // y13: an ingestion-time `TimeShift(90s)` view over the arriving
        // rows was selected, under-costed, and refused at install
        // (`ViewReadsUnwindowedSource`).
        y13(),
        // An ingestion-time `Scan` feeding a 60 s chain and a 420 s chain:
        // charged once by Stage 3, once per window by `plan_cost`.
        over_m_and_c(
            "r5_scan_feeding_two_chain_widths_noraw",
            vec![
                promql("sum_over_time(m[5m])", 60_000, 0, None, none.clone()),
                promql("max_over_time(m[14m])", 420_000, 0, None, none.clone()),
            ],
            false,
        ),
        sql_table_without_window(),
        sql_table_without_window_holding_rows(),
        subquery_over_raw_range(),
        negative_offset("b08_negative_offset_noraw", false),
        negative_offset("b09_negative_offset_raw", true),
    ]
}

/// y13 (also the installable-selection design's example):
/// `sum_over_time(m[5m] offset 90s)` every 60 s, raw data not kept.
fn y13() -> Workload {
    over_m_and_c(
        "y13_offset_90s_noraw",
        vec![promql(
            "sum_over_time(m[5m] offset 90s)",
            60_000,
            0,
            None,
            Value::Null,
        )],
        false,
    )
}

/// A SQL percentile over a table with no time window, every 10 s, rank
/// error 0.01: an ingestion-time stored `Project` over the whole table
/// was selected and refused at install (`ViewReadsUnwindowedSource`). The
/// table declares no `rows_held`, so no query-time plan of the whole table
/// can be costed either: no plan installs, and Stage 3 must return an
/// error.
fn sql_table_without_window() -> Workload {
    over_flows(
        "sql_percentile_whole_table",
        &["SELECT approx_percentile_cont(bytes, 0.9) FROM flows"],
        10_000,
        json!({"accuracy_metric": "rank", "max_error": 0.01, "failure_probability": 0.01}),
        true,
    )
}

/// [`sql_table_without_window`] over a table declared to hold 1,000,000
/// rows, so a query-time plan reading the whole table can be costed.
fn sql_table_without_window_holding_rows() -> Workload {
    over_flows_holding(
        "sql_percentile_whole_table_rows_held",
        &["SELECT approx_percentile_cont(bytes, 0.9) FROM flows"],
        10_000,
        json!({"accuracy_metric": "rank", "max_error": 0.01, "failure_probability": 0.01}),
        true,
        Some(1_000_000),
    )
}

/// The smallest shape of the o11y subquery workloads: a maximum over an
/// hour of 1 m steps of a rate over raw data, every 60 s, raw data kept.
/// A query-time `TimeRange` over the raw `Scan` under the subquery was
/// selected and refused at install (`UnsupportedRawDataWindow`); on this
/// head every plan of this memo installs.
fn subquery_over_raw_range() -> Workload {
    over_m_and_c(
        "subquery_max_of_rate_1h_raw",
        vec![promql(
            "max_over_time(sum(rate(c[5m]))[1h:1m])",
            60_000,
            0,
            None,
            Value::Null,
        )],
        true,
    )
}

/// o11y-bench q05's shape over `c`: the maximum over 6 h of 1 m steps of
/// a ratio of two rates over raw data, every 60 s, raw data kept.
fn ratio_subquery_over_raw_ranges() -> Workload {
    over_m_and_c(
        "subquery_max_of_rate_ratio_6h_raw",
        vec![promql(
            r#"max_over_time((sum(rate(c{job="a"}[5m])) / sum(rate(c[5m])))[6h:1m])"#,
            60_000,
            0,
            None,
            Value::Null,
        )],
        true,
    )
}

/// b08 (`raw` false) and b09 (`raw` true): `sum_over_time(m[5m] offset
/// -5m)` every 60 s reads a window ending after the evaluation.
fn negative_offset(name: &'static str, raw: bool) -> Workload {
    over_m_and_c(
        name,
        vec![promql(
            "sum_over_time(m[5m] offset -5m)",
            60_000,
            0,
            None,
            Value::Null,
        )],
        raw,
    )
}

fn workload(name: &str) -> Workload {
    corpus()
        .into_iter()
        .find(|w| w.name == name)
        .unwrap_or_else(|| panic!("no workload {name}"))
}

// ---------------------------------------------------------------------------
// Optimality: the selected plan is the cheapest installable plan.

macro_rules! cheapest_installable_tests {
    ($($test:ident => $name:literal,)*) => {$(
        #[test]
        fn $test() {
            assert_selects_the_cheapest_installable_plan(&workload($name));
        }
    )*};
}

cheapest_installable_tests! {
    cheapest_p01_sum_5m_raw => "p01_sum_5m_raw",
    cheapest_p02_sum_5m_noraw => "p02_sum_5m_noraw",
    cheapest_p03_rate_5m_raw => "p03_rate_5m_raw",
    cheapest_p04_max_14m_every_7m_noraw => "p04_max_14m_every_7m_noraw",
    cheapest_p05_count_1m_raw => "p05_count_1m_raw",
    cheapest_p06_two_phases_one_grid_raw => "p06_two_phases_one_grid_raw",
    cheapest_p07_range_query_1h_span_raw => "p07_range_query_1h_span_raw",
    cheapest_p08_two_filters_one_source_raw => "p08_two_filters_one_source_raw",
    cheapest_p09_quantile_rank_target_noraw => "p09_quantile_rank_target_noraw",
    cheapest_p10_offset_1h_hourly_plus_plain_noraw => "p10_offset_1h_hourly_plus_plain_noraw",
    cheapest_p11_two_filters_overhead_and_disk_raw => "p11_two_filters_overhead_and_disk_raw",
    cheapest_s01_sql_windowed_sum => "s01_sql_windowed_sum",
    cheapest_y13_offset_90s_noraw => "y13_offset_90s_noraw",
    cheapest_r5_scan_feeding_two_chain_widths_noraw => "r5_scan_feeding_two_chain_widths_noraw",
    cheapest_sql_percentile_whole_table => "sql_percentile_whole_table",
    cheapest_sql_percentile_whole_table_rows_held => "sql_percentile_whole_table_rows_held",
    cheapest_subquery_max_of_rate_1h_raw => "subquery_max_of_rate_1h_raw",
    cheapest_b08_negative_offset_noraw => "b08_negative_offset_noraw",
    cheapest_b09_negative_offset_raw => "b09_negative_offset_raw",
}

/// Reversing the order of a workload's queries changes no plan's cost, so
/// the cheapest installable plan's cost is the same: Stage 3's selected
/// cost is too.
#[test]
fn the_selected_cost_does_not_depend_on_query_order() {
    for name in [
        "p06_two_phases_one_grid_raw",
        "p08_two_filters_one_source_raw",
        "p10_offset_1h_hourly_plus_plain_noraw",
        "r5_scan_feeding_two_chain_widths_noraw",
    ] {
        let w = workload(name);
        let mut reversed = w.clone();
        if let Some(queries) = reversed.planning.query_workload.repeating_queries.as_mut() {
            queries.reverse();
        }
        let cost = |w: &Workload| {
            let run = run(w);
            let (plan, _) = run
                .selection
                .as_ref()
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            plan_cost(w, &run, plan)
        };
        let (forward, backward) = (cost(&w), cost(&reversed));
        assert!(
            (forward - backward).abs() <= RELATIVE_TOLERANCE * forward.abs().max(1e-12),
            "{name}: {forward}/s in workload order, {backward}/s reversed"
        );
    }
}

// ---------------------------------------------------------------------------
// The oracle's own checks: its space holds Stage 3's plan.

/// For every corpus workload Stage 3 plans and the oracle can enumerate,
/// the oracle built Stage 3's plan (the same executions over the same
/// edges, `plan_signature`): otherwise the oracle misses plans Stage 3 may
/// select, and the optimality contract proves nothing.
#[test]
fn the_oracle_space_holds_every_selected_plan() {
    let mut failures = Vec::new();
    for w in corpus() {
        let run = run(&w);
        let Ok((plan, _)) = &run.selection else {
            continue;
        };
        // A memo the oracle cannot enumerate is reported, not checked: its
        // own `cheapest_*` test fails as "cannot run".
        let found = match enumerate_plans(
            &run.physical,
            &run.entries,
            &run.data,
            &w.cost_inputs,
            MAX_CHOICES,
        ) {
            Ok(found) => found,
            Err(error) => {
                println!("{}: not checked, the oracle cannot run: {error}", w.name);
                continue;
            }
        };
        if !found.plan_signatures.contains(&plan_signature(plan)) {
            failures.push(format!(
                "{}: the oracle did not build the selected plan {}:\n{}",
                w.name,
                plan_signature(plan),
                describe(plan)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

// ---------------------------------------------------------------------------
// Feasibility and constraints of every selected plan.

/// Every corpus workload Stage 3 plans, with its plan.
fn selected_plans() -> Vec<(Workload, Run)> {
    corpus()
        .into_iter()
        .map(|w| {
            let run = run(&w);
            (w, run)
        })
        .filter(|(_, run)| run.selection.is_ok())
        .collect()
}

/// Every selected plan passes the planner's install rules.
#[test]
fn every_selected_plan_passes_the_install_rules() {
    let failures: Vec<String> = selected_plans()
        .iter()
        .filter_map(|(w, run)| {
            let (plan, _) = run.selection.as_ref().unwrap();
            install_error(plan).map(|error| format!("{}: {error}\n{}", w.name, describe(plan)))
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// A query-time result that is not materialized streams to one reader: no
/// selected node with ℓ = None at query time is read by two edges or by an
/// edge and a query.
#[test]
fn no_unmaterialized_query_time_node_has_two_readers() {
    let mut failures = Vec::new();
    for (w, run) in selected_plans() {
        let (plan, _) = run.selection.as_ref().unwrap();
        for node in &plan.nodes {
            if node.output_state.timing != ExecutionTiming::QueryTime
                || node.lifetime != Lifetime::None
            {
                continue;
            }
            let readers = plan
                .edges
                .iter()
                .filter(|edge| edge.producer == node.id)
                .count()
                + plan.roots.iter().filter(|&&root| root == node.id).count()
                + plan
                    .scalar_roots
                    .iter()
                    .flat_map(|(_, expression)| expression.operator_refs())
                    .filter(|&&id| id == node.id)
                    .count();
            if readers > 1 {
                failures.push(format!(
                    "{}: node {} has {readers} readers\n{}",
                    w.name,
                    node.id,
                    describe(plan)
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Each plan edge is an admissible physical edge into that input of its
/// consumer's candidate.
#[test]
fn every_selected_edge_is_admissible() {
    let mut failures = Vec::new();
    for (w, run) in selected_plans() {
        let (plan, _) = run.selection.as_ref().unwrap();
        let candidate = |id: PhysicalASAPNodeId| {
            plan.nodes
                .iter()
                .find(|node| node.id == id)
                .unwrap()
                .selected_physical_candidate
                .unwrap()
        };
        for node in &plan.nodes {
            let consumer = candidate(node.id);
            let producers: Vec<PhysicalCandidateId> = node
                .payload
                .children()
                .iter()
                .map(|&&producer| candidate(producer))
                .collect();
            for (input, producer) in producers.into_iter().enumerate() {
                let admissible = run
                    .physical
                    .admissible_edges_into(consumer, input)
                    .iter()
                    .any(|&edge| run.physical.edge(edge).producer_candidate_id == producer);
                if !admissible {
                    failures.push(format!(
                        "{}: node {} input {input} reads v{} over no admissible edge",
                        w.name, node.id, producer.0
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// (X): no query reads, anywhere below its root, a candidate it must read
/// only exactly.
#[test]
fn no_query_reads_a_sketch_of_a_group_it_must_read_exactly() {
    let mut failures = Vec::new();
    for (w, run) in selected_plans() {
        let (plan, _) = run.selection.as_ref().unwrap();
        for (id, queries) in queries_reaching(plan) {
            let node = plan.nodes.iter().find(|node| node.id == id).unwrap();
            let v = node.selected_physical_candidate.unwrap();
            let logical = run.physical.physical_candidates[v.0].logical_candidate_id;
            for query in queries {
                if run
                    .physical
                    .logical_candidates
                    .query_must_not_read(query, logical)
                {
                    failures.push(format!(
                        "{}: query {query} reads node {id} (v{})",
                        w.name, v.0
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Stage 3 retains at least what the plan's readers need (the least
/// retention the oracle sets): otherwise the installer refuses the plan or
/// a later evaluation misses rows.
#[test]
fn stage3_retains_at_least_what_its_plans_readers_need() {
    let mut failures = Vec::new();
    for (w, run) in selected_plans() {
        let (plan, _) = run.selection.as_ref().unwrap();
        let mut least = plan.clone();
        set_least_retention(&mut least, &run.entries);
        for (selected, needed) in plan.nodes.iter().zip(&least.nodes) {
            let short = match (selected.retention_ms, needed.retention_ms) {
                (Some(selected), Some(needed)) => selected < needed,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if short {
                failures.push(format!(
                    "{}: node {} retains {:?} ms, its readers need {:?}",
                    w.name, selected.id, selected.retention_ms, needed.retention_ms
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Stage 3's objective is the `plan_cost` of the plan it selects, over the
/// whole corpus (the example tests in `stage3_objective` check it on
/// chosen shapes).
#[test]
fn the_objective_is_the_plan_cost_over_the_corpus() {
    let mut failures = Vec::new();
    for (w, run) in selected_plans() {
        let (plan, selection) = run.selection.as_ref().unwrap();
        let cost = plan_cost(&w, &run, plan);
        let objective = selection.selected_plan_cost_per_second;
        if (objective - cost).abs() > 1e-9 * cost.abs().max(1e-12) {
            failures.push(format!(
                "{}: objective {objective} != plan_cost {cost}",
                w.name
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ---------------------------------------------------------------------------
// Installable selection on the regression workloads.

/// The candidates Stage 3 reports as `NotInstallable`.
fn not_installable(selection: &Selection) -> Vec<asap_types::stages::PhysicalCandidate> {
    selection
        .pruned_candidates
        .iter()
        .filter(|pruned| pruned.reason == PruneReason::NotInstallable)
        .map(|pruned| pruned.candidate)
        .collect()
}

/// When the cheapest plan of all does not install, the MILP's first
/// optimum is refused: Stage 3 reports the candidate of the node the
/// refused rule names as `NotInstallable`, and selects a plan that
/// installs at the cheapest installable cost.
fn assert_refused_candidate_is_reported(w: &Workload) {
    let run = run(w);
    let found = oracle(w, &run);
    let cheapest = found.cheapest.as_ref().expect("a costed plan");
    let error = cheapest
        .install_error
        .as_ref()
        .unwrap_or_else(|| panic!("{}: the cheapest plan installs; nothing to refuse", w.name));
    let node = error.node_id().unwrap_or_else(|| {
        panic!(
            "{}: the cheapest plan's error {error:?} is not subtree-local",
            w.name
        )
    });
    let refused = cheapest.plan.nodes.iter().find(|n| n.id == node).unwrap();
    let refused = run.physical.physical_candidates[refused.selected_physical_candidate.unwrap().0];
    let (plan, selection) = run
        .selection
        .as_ref()
        .unwrap_or_else(|e| panic!("{}: Stage 3 selected no plan: {e}", w.name));
    assert!(
        not_installable(selection).contains(&refused),
        "{}: {refused:?} (node {node} of the cheapest plan, {error:?}) is not reported NotInstallable: {:?}",
        w.name,
        selection.pruned_candidates
    );
    assert_eq!(install_error(plan), None, "{}:\n{}", w.name, describe(plan));
}

/// The SQL whole-table case: the cheapest plan stores a `Project` over
/// the whole table at ingestion time, which no ingestion run can refresh.
#[test]
fn a_view_over_a_whole_table_is_reported_not_installable() {
    assert_refused_candidate_is_reported(&sql_table_without_window_holding_rows());
}

/// o11y-bench q05's shape (too many plans for the oracle): the plan
/// Stage 3 returns passes the install rules (I1 without the oracle).
#[test]
fn a_ratio_subquery_over_raw_ranges_selects_an_installable_plan() {
    let w = ratio_subquery_over_raw_ranges();
    let run = run(&w);
    match &run.selection {
        Ok((plan, _)) => assert_eq!(install_error(plan), None, "{}:\n{}", w.name, describe(plan)),
        Err(error) => println!("{}: no plan ({error})", w.name),
    }
}

/// No candidate Stage 3 reports `NotInstallable` is in the plan it
/// returns, on any corpus workload.
#[test]
fn no_candidate_reported_not_installable_is_selected() {
    let mut failures = Vec::new();
    for (w, run) in selected_plans() {
        let (plan, selection) = run.selection.as_ref().unwrap();
        let refused = not_installable(selection);
        for node in &plan.nodes {
            let v = node.selected_physical_candidate.unwrap();
            if refused.contains(&run.physical.physical_candidates[v.0]) {
                failures.push(format!(
                    "{}: node {} (v{}) is reported NotInstallable",
                    w.name, node.id, v.0
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// y13 without raw data: Stage 3's plan installs and is the cheapest
/// installable plan. The ingestion-time `TimeShift(90s)` view over the
/// arriving rows is not installable whatever it costs.
#[test]
fn y13_selects_an_installable_plan_and_no_shift_view_over_arriving_rows() {
    let w = y13();
    let run = run(&w);
    let (plan, _) = run.selection.as_ref().expect("a plan");
    assert_eq!(install_error(plan), None, "{}", describe(plan));
    let shift_views: Vec<PhysicalASAPNodeId> = plan
        .nodes
        .iter()
        .filter(|node| {
            node.output_state.timing == ExecutionTiming::IngestionTime
                && matches!(node.lifetime, Lifetime::AcrossEvaluations(_))
                && node.payload.kind_name() == "TimeShift"
        })
        .map(|node| node.id)
        .collect();
    assert!(
        shift_views.is_empty(),
        "shift views {shift_views:?}:\n{}",
        describe(plan)
    );
}

/// b08 and b09: no plan reads a window ending after the evaluation, so
/// none installs (the oracle finds none) and Stage 3 returns an error.
#[test]
fn a_negative_offset_has_no_installable_plan_and_stage3_returns_an_error() {
    for w in [negative_offset("b08", false), negative_offset("b09", true)] {
        let run = run(&w);
        let found = oracle(&w, &run);
        assert!(
            found.cheapest_installable.is_none(),
            "{}: the oracle installs\n{}",
            w.name,
            describe(&found.cheapest_installable.as_ref().unwrap().plan)
        );
        assert!(
            run.selection.is_err(),
            "{}: Stage 3 selected a plan",
            w.name
        );
    }
}
