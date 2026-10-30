# Public library functions

Audience: developers embedding ASAPPlanner or supplying deployment models. This is
a compact reference for the public workflow APIs, not an
exhaustive symbol reference. The [CLI guide](../user_guide_docs/run-a-query.md) covers command-line inspection; the [design overview](../design_docs/architecture/README.md) defines ownership.

ASAPPlanner's output is the plan selected by its planning stages
(`PlanOutput`, see [Plan a workload through the stages](#plan-a-workload-through-the-stages)).
Downstream owns physical binding and commitment. The planner does not deploy a
plan, and a serializable DAG is not evidence of runtime readiness.

## Choose a library workflow

| Desired result | Calls | Example |
| --- | --- | --- |
| Pre-ASAP IR | Frontend `lower_*` | [Lower a query](#lower-a-query-into-pre-asap-ir) |
| Selected plan for a workload (Stages 0–3) | `asap_planner::e2e_plan` | [Plan a workload](#plan-a-workload-through-the-stages) |
| Every stage's candidates for already lowered roots | `asap_planner::run_planning` | [Plan a workload](#plan-a-workload-through-the-stages) |
| Physical DAG for an executor | `PlanOutput::physical_asap_dag`, or timing and export functions | [Export](#export) |

Each recipe ends at a different artifact. Use only the stages needed for that
artifact, while preserving the checks required by its intended consumer.

## Dependencies

Inside this workspace, depend on the frontend you need, the stage crates
(`asap-logical-asap-optimization` for Stage 1, `asap-physical-asap-optimization`
for Stage 2, `asap-plan-selection` for Stage 3), `asap-planner` for the facade that
runs the stages, `asap-cost` for the cost model and its inputs (`CostInputs`),
`asap-accuracy` for the accuracy model (`AccuracyModel`,
`AccuracyEvidenceProvider`), and `asap-types`, or on `asap-planner`, which wraps every frontend and
the planning stages. External users can use Git dependencies pinned to a
compatible revision; use the same revision across these crates, and pick a
revision at least as new as the APIs you call. Replace `REV` below with a
`main` revision that has the stage crates (`crates/plan-selection`).
For the examples below:

```toml
[dependencies]
asap-planner = { git = "https://github.com/ProjectASAP/ASAPPlanner", rev = "REV" }
asap-frontend-promql = { git = "https://github.com/ProjectASAP/ASAPPlanner", rev = "REV" }
asap-plan-selection = { git = "https://github.com/ProjectASAP/ASAPPlanner", rev = "REV" }
asap-cost = { git = "https://github.com/ProjectASAP/ASAPPlanner", rev = "REV" }
asap-accuracy = { git = "https://github.com/ProjectASAP/ASAPPlanner", rev = "REV" }
asap-types = { git = "https://github.com/ProjectASAP/ASAPPlanner", rev = "REV" }
# For the async e2e_plan example:
tokio = { version = "1", features = ["rt", "macros", "rt-multi-thread"] }
```

## Lower a query into Pre-ASAP IR

| Public function | Required input | Output |
| --- | --- | --- |
| `asap_frontend_promql::lower_promql_workload` | PromQL `PlanningWorkload`; a bare instant selector needs its metric's nonzero `sample_interval` in `DataWorkload.sources` | All-or-nothing `Result<Vec<Rc<OperatorNode>>, PromqlError>` for normalized batch and repeating entries |
| `asap_frontend_metricsql::lower_metricsql` | Query string, `AccuracyTarget` | `Result<Rc<OperatorNode>, MetricsqlError>` |
| `asap_frontend_sql::lower_sql` | Query string, `SqlCatalog`, accuracy | Async `Result<Rc<OperatorNode>, SqlError>`; default SQL dialect is DataFusionSQL |
| `asap_frontend_sql::lower_sql_dialect` | Same inputs plus `SqlDialect` | Async resolved Pre-ASAP query or error |
| `asap_frontend_sql::lower_sql_batch` | `QueryWorkload` and catalog | Per-query results for `query_batch`; does not iterate `repeating_queries` |

Lowering resolves the supported source language into the canonical query
representation: an `asap_types::ir::OperatorNode` DAG containing only
`NonASAPOp` operators, with no timing (see the
[Pre-ASAP IR reference](pre-asap-ir.md)). It does not enumerate Post-ASAP alternatives. A frontend may
reject unsupported syntax or semantics; a declared language/dialect enum does
not imply complete support. PromQL workload lowering uses normalized
`PlanningWorkload::query_workload.entries()` order, preserving entry-to-root associations for later
workload-aware operations. For SQL mixed one-time/repeating workloads, iterate
those entries with the single-query frontend.

### Definition and example

PromQL's public signature (types are imported from their respective crates):

```text
lower_promql_workload(workload: &PlanningWorkload)
    -> Result<Vec<Rc<OperatorNode>>, PromqlError>
```

A bare instant selector reads the latest sample within the sample interval
its metric declares (`DataWorkload.sources[metric].sample_interval`,
nonzero), whatever the arrival. A query with one fails with
`WorkloadError::MissingSampleInterval` when that metric declares no
interval. Explicit range selectors retain their query-specified range and
need no interval. Use `lower_promql_workload_with_histograms`
to supply a histogram catalog for the whole workload. Each entry carries its
own accuracy requirement, an `AccuracyTarget` with the fields `error` (ε),
`failure_probability` (δ, optional), `accuracy_metric` (optional) and
`accuracy_scope` (default the whole result). Build it with:

| Builder | Meaning | Example |
| --- | --- | --- |
| `AccuracyTarget::exact()` | No approximation permitted (the `QueryRequirements` default) | Exact aggregation/unchanged-query alternatives only |
| `AccuracyTarget::max_error(e)` | Error at most ε, with no δ requirement and no metric set | `max_error(0.01)` |
| `.failure_probability(d)` | Also require the bound to hold with probability at least 1 − δ | `max_error(0.01).failure_probability(0.05)` |
| `.accuracy_metric(m)` | State ε in `AccuracyMetric` `m` (`Absolute { scale }`, `Relative` or `Rank`); unset accepts a guarantee in any metric | `.accuracy_metric(AccuracyMetric::Rank)` |
| `.accuracy_scope(s)` | Which output values one statement covers (`AccuracyScope`): `PerValue`, `WholeResult` (default) or `GroupsWithRecordShareAtLeast(share)`, every group holding at least `share` (a fraction) of the records | `.accuracy_scope(AccuracyScope::GroupsWithRecordShareAtLeast(0.05))` |

In JSON the target is a plain struct, for example
`{"max_error": 0.01, "failure_probability": 0.001, "accuracy_metric": "rank"}`;
a record-share scope is `"accuracy_scope": {"groups_with_record_share_at_least": 0.05}`.

Epsilon does not mean the same error quantity for every statistic. Inspect the
candidate's guarantee and its error metric; a target is a requirement, not proof
that a supported candidate exists.

```rust
use asap_frontend_promql::lower_promql_workload;
use asap_types::workload::{
    BatchEntry, DataWorkload, DurationMs, Query,
    PlanningWorkload, QueryLanguage, QueryRequirements, QueryWorkload,
};
use asap_types::ir::properties::AccuracyTarget;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
            query: Query("sum(latency)".into()),
            requirements: QueryRequirements {
                accuracy: AccuracyTarget::exact(),
                ..Default::default()
            },
            predictability: Default::default(),
            invocations: 1,
            execute_at: None,
            }]),
            repeating_queries: None,
        },
        // `latency` is sampled every second.
        data_workload: Some(DataWorkload::sampled_every(
            ["latency".to_string()],
            DurationMs(1_000),
        )),
    };
    let pre_asap = lower_promql_workload(&workload)?;
    println!("{pre_asap:#?}");
    Ok(())
}
```

For SQL, the corresponding signatures are:

```text
async lower_sql(query: &str, catalog: &SqlCatalog, accuracy: AccuracyTarget)
    -> Result<Rc<OperatorNode>, SqlError>
async lower_sql_dialect(query: &str, catalog: &SqlCatalog,
    dialect: SqlDialect, accuracy: AccuracyTarget) -> Result<Rc<OperatorNode>, SqlError>
```

| `SqlDialect` value | Current behavior |
| --- | --- |
| `DataFusionSQL` | Default of `lower_sql`; uses DataFusion's supported SQL |
| `ClickhouseSQL` | Supported ClickHouse subset; not all ClickHouse functions |
| `ElasticSQL` | Returns `UnsupportedDialect` |

The catalog is required and describes your tables. For a complete schema-building
example, see `lineitem_catalog` in [the planner's end-to-end tests](../../crates/planner/tests/e2e_plan.rs).

## Plan a workload through the stages

`asap_planner::e2e_plan` runs the whole planner: the query-language frontends
(Stage 0), then the default optimization pass `Stage1ToStage3Pass`, which runs
logical ASAP-aware optimization (Stage 1), physical ASAP-aware optimization
(Stage 2) and plan selection (Stage 3): Pass 1 once, then Pass 2, Stage 2
and Stage 3 per pane-width round, keeping the best round's plan.

```text
asap_planner::e2e_plan(input: UserInput<'_>) -> Result<PlanOutput, PlanError>   // async
UserInput::new(workload: &PlanningWorkload, frontend_specific: FrontendInput<'_>,
    models: PlanningModels<'_>) -> UserInput<'_>
FrontendInput::{Sql { catalog: &SqlCatalog }, Promql { histograms: Option<HistogramCatalog> }, Metricsql}

// A caller that already holds lowered roots runs the stages through the facade:
asap_planner::run_planning(
    roots: Vec<QueryRoot>,
    entries: &[QueryWorkloadEntry],   // one per root: accuracy, latency bound, recurrence
    data: &DataWorkload,
    models: PlanningModels<'_>,
    settings: PlanSearchSettings,     // time_limit_ms, default 30 s
) -> Result<PlanningRun, PerComputationCandidateError>

// Stage 3 alone, over one round's Stage 2 output:
asap_plan_selection::select_plan(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    accuracy_checks: &AccuracyCheckInputs<'_>,   // accuracy model and evidence
    cost_inputs: &CostInputs,
    realizer: &dyn AssignmentRealizer,           // Stage 2's, e.g. Stage2AssignmentRealizer
    deadline: Option<Instant>,
) -> Result<Selection, ErrorSelectingPlan>
```

| Input or output | Meaning |
| --- | --- |
| `PlanningModels::new(accuracy, evidence)` | Your `&dyn AccuracyModel` and `&dyn AccuracyEvidenceProvider`, with default plan selection settings and deployment capabilities; `with_accuracy` and `with_evidence` replace one of them |
| `PlanningModels::builtin()` | Built-in accuracy model (`BuiltinAccuracyModel`) and no extra evidence (`NoAccuracyEvidence`). `with_plan_selection(&settings)` sets the deployment's `CostInputs`; `with_deployment_capabilities(&capabilities)` sets its `DeploymentCapabilities` (field `deployment_capabilities`) |
| `DeploymentCapabilities` (`asap_logical_asap_optimization::deployment_capabilities`) | What the executor supports: `supported_summaries` (`None`, the default, supports every summary the planner builds; otherwise `SupportedSummary { family: SketchAlgorithm, readouts: Vec<StatisticKind> }` entries; Pass 1 prunes others with `PruneReason::UnsupportedByDeployment`), `raw_data_retained` (default `true`; when `false`, a candidate that scans raw data at query time is pruned by Stage 2, and stored data is refused with `asap_planner::SelectionError::InvalidDeploymentCapabilities`) and `query_time_results_retained` (default `false`; when `true`, Stage 2 offers `MatAtQueryKeptAcrossEvaluations` for tumbling panes) |
| `CostInputs` (`asap_cost`) | `cost_model`, `resource_speed` (`ms_per_cpu_op`, `ms_per_scanned_byte`), `non_repeating_query_horizon_seconds` (default 3,600) and `measured_operation_costs` (`MeasuredOperationCosts`, default empty: `operators`, one `OperatorCost { operator, phase, sketch / exact / measure, ns_per_unit, ns_per_invocation, memory_bytes_per_state }` per measured operator kind, phase and family; `ns_per_evaluation` per query request; `provenance` (executor version, machine). A node with an entry is priced from it, a sketch between the two nearest measured sizes; every other node from the formulas times one calibration factor fitted from the measured nodes; each node's `NodeCost::price_source` says which. The earlier JSON array of `MeasuredOperationCost { sketch, operation, cpu_ns_per_operation }` still loads) |
| `CostModel::UsageOverTime(ResourceUnitCost)` | Cost model 1 (default, `ResourceUnitCost::illustrative()`): unit cost × usage per second; the cheapest valid candidate wins |
| `CostModel::ProvisionedCapacity(ResourceUnitCost)` | Cost model 2: unit cost × peak memory, peak CPU cores and disk; the cheapest valid candidate wins |
| `CostModel::ClusterCapacity(ClusterLimits)` | Cost model 3: memory, disk and optional CPU cores are hard limits; `ClusterObjective::{LowestLatency, LeastMemory}` chooses among the candidates that fit |
| `PlanOutput.plans` | One `QueryPlan { entry_index, root }` per query, in `QueryWorkload::entries()` order |
| `PlanOutput.selection` | `Some` with Stage 3's `Selection` under the default pass: `selected_candidates`, `candidates_with_proven_non_negative_weights`, `cost` (the selected plan's `PlanCost` with its `ResourceUsage` and `cost_per_second`, `None` under cost model 3), `accepted_accuracy_per_query`, `pruned_candidates` and `search_report` |
| `PlanOutput.pane_retention_ms` | For each pane chain the selected plan keeps beyond one evaluation, by node id, how long its panes are kept (`RealizedPlan::pane_retention_ms`) |
| `PlanningRun` | `per_computation_candidates`, the selected round's `logical_asap_candidates` and `physical_asap_candidates`, `selection`, `selected_physical_plan` (`RealizedPlan`), `selected_plan` (the roots to run), `memo_steps` and `report` (per round: counts and Pass 2, Stage 2 and Stage 3 times) |
| `PlanOutput::physical_asap_dag()` | The planned queries as one `PhysicalASAPDAG`, timed with the default `MaterializationAssignment` (see [Export](#export)) |

Each candidate's `ResourceUsage` holds its usage without prices:
`cpu_ops_per_second`, `scanned_bytes_per_second`, `average_memory_bytes`,
`peak_memory_bytes`, `disk_bytes`, `peak_cpu_cores` and `latency_per_query`.
The deployment's choice of cost model reads it.

Example (add `asap-planner` and an async runtime such as `tokio` to the
dependencies):

```rust
use asap_planner::{e2e_plan, FrontendInput, PlanningModels, UserInput};
use asap_types::ir::properties::AccuracyTarget;
use asap_types::workload::{
    BatchEntry, DataArrival, DataWorkload, DurationMs, MetricType, PlanningWorkload, Query,
    QueryLanguage, QueryRequirements, QueryWorkload, SourceFacts,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query("topk by (job) (10, sum_over_time(http_requests_total[1m]))".into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyTarget::max_error(0.01).failure_probability(0.001),
                    max_latency_ms: Some(100.0),
                },
                predictability: Default::default(),
                invocations: 1,
                execute_at: None,
            }]),
            repeating_queries: None,
        },
        // What is known about each source the queries read.
        data_workload: Some(DataWorkload {
            arrival: DataArrival::AtRest,
            sources: [(
                "http_requests_total".to_string(),
                SourceFacts {
                    series_count: Some(1_000_000),
                    metric_type: Some(MetricType::Counter),
                    sample_interval: Some(DurationMs(15_000)),
                    ..Default::default()
                },
            )]
            .into(),
        }),
    };
    let input = UserInput::new(
        &workload,
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin(),
    );
    let output = e2e_plan(input).await?;
    let selection = output.selection.as_ref().expect("Stage 3 selected a plan");
    println!(
        "selected {} candidates at {:?} per second ({:?})",
        selection.selected_candidates.candidates.len(),
        selection.cost.cost_per_second,
        selection.search_report.status,
    );
    for accepted in &selection.accepted_accuracy_per_query {
        println!("{accepted}");
    }
    let dag = output.physical_asap_dag()?;
    println!("{} physical nodes", dag.nodes.len());
    Ok(())
}
```

It prints the number of selected physical candidates, the selected plan's
cost per second and how the search ended, then the accuracy Stage 3
accepted for each query. To plan under cost model 3 instead:

```rust
use asap_cost::{ClusterLimits, ClusterObjective, CostInputs, CostModel};

let settings = CostInputs {
    cost_model: CostModel::ClusterCapacity(ClusterLimits {
        memory_bytes: 64 << 30,
        disk_bytes: 1 << 40,
        cpu_cores: None,
        objective: ClusterObjective::LowestLatency,
    }),
    ..Default::default()
};
let models = PlanningModels::builtin().with_plan_selection(&settings);
```

The `stage_pipeline` devtool runs the same stages and writes every stage's
candidates as one JSON document; see the
[Stage Viewer guide](../user_guide_docs/stage-viewer.md).

## Workload inputs and defaults

`PlanningWorkload` holds `QueryWorkload` and optional `DataWorkload` as peer
inputs. `QueryWorkload` contains the language and optional batch/repeating
entries. Entries carry requirements, predictability and recurrence; a
query's window comes from its text (range and offset). These facts are separate: repeated queries can read data at rest.
`DataWorkload` holds `arrival` (`DataArrival`: `AtRest`, `Ingesting` or
`Mixed` with `rows_per_second`, or `Unknown`) and `sources`, a map from
PromQL metric or SQL table name to `SourceFacts`: `series_count`,
`metric_type`, `values_non_negative`, `sample_interval` (PromQL and
MetricsQL), `stream_norms`, `max_distinct_per_evaluation` and
`distinct_values_per_label` (distinct values per label or column, which
plan selection prices a `by (...)` grouping's groups at: the product of its
keys' counts, at most the declared `series_count` (series or groups of
rows) and the rows it reads; with a key undeclared, no count is assumed and
the grouping is priced at the smaller of those two; over several sources a
key's counts add), `distinct_values_per_label_set` (distinct combinations
a set of dependent labels takes together, e.g.
`[{"labels": ["service", "endpoint"], "distinct_values": 625}]` when each
endpoint is in one service: a grouping over one source is priced at the
smallest product of declared set and label counts covering its keys, so
`by (service, endpoint)` is 625 groups rather than 25 x 625), and `rows_held` (the rows the source holds at rest,
which bound a scan with no window and data at rest). Every fact is declared: the planner has no
clock, so a caller drops facts that no longer hold before planning, and
pricing assumes no default: a scan whose source declares no arrival rate
(`rows_per_second`, or `series_count` with `sample_interval`) and no
`rows_held` fails with `AnalyticalCostError::MissingSourceFact`, naming the
source and the missing fact. Plan
selection bounds a query's number of values `G`, and its input row count,
by the `series_count` of the sources it reads, counting each branch of an
`or` or union, and a grouped result's `G` (PromQL or SQL) also by the
product of its keys' declared distinct values when every key declares one. `DataWorkload::validate()` checks the data facts: the
arrival rate must be finite and nonnegative, a sample interval nonzero,
stream norms finite and nonnegative, and distinct bounds and per-label
distinct-value counts positive.
`PlanningWorkload::validate()` shares these checks.

```json
{"arrival": {"ingesting": {"rows_per_second": 66666.7}},
 "sources": {"http_requests_total":
   {"series_count": 1000000, "metric_type": "counter", "sample_interval": 15000}}}
```

| Type/input | Current behavior | Caller responsibility |
| --- | --- | --- |
| `QueryRequirements::default()` | `AccuracyTarget::exact()`, no latency bound (`max_latency_ms: None`) | Pass approximation explicitly on each workload entry |
| `DataWorkload::default()` | Unknown arrival, no source facts | Supply facts needed for the requested comparisons |
| `SourceFacts::default()` | Every fact unknown | Unknown facts are not zero; drop facts that no longer hold before planning |
| `CostInputs::default()` | Cost model 1 with the illustrative `ResourceUnitCost`, no measured operation costs | Supply the deployment's unit costs, speed and measurements for calibrated comparisons |
| `DeploymentCapabilities::default()` | Every summary supported, raw data retained, query-time results not kept | Declare what the executor supports |

`Default` is a Rust constructor contract, not a general serde omission rule.
Several workload fields require explicit serialized values. A struct field being
optional also does not guarantee every planning operation can succeed without it.

## Export

| Function/type | Purpose |
| --- | --- |
| `asap_types::ir::apply_materialization_timings(&root, &assignment, &mut NodeToTimedNodeCache::new())` | Write execution timing into every node from a `MaterializationAssignment` (default: all query time) and validate the data-state edges; `PlanOutput::physical_asap_dag()` applies the default to a planned workload |
| `asap_types::ir::physical_export::compile_physical_asap_dag(&timed_root)` | Export a timed DAG as a `PhysicalASAPDAG`; rejects an untimed node; `compile_physical_asap_workload(&roots)` exports a batch with one root per query; `.validate()` checks it explicitly |
| `PhysicalASAPDAG::materialized_views()` and `.raw_data_retention()` (`asap_types::ir::materialized_view`) | What a runtime keeps for an installed DAG, derived from the DAG and the `retention_ms` installation sets: each materialized view (refresh mode, coverage, inputs, the node's `retention_ms`) and how far back each raw-data source is kept for query-time scans (each query-time `Scan`'s `retention_ms`) |

Choose the export matching your intended handoff: an untimed semantic DAG is not
interchangeable with a versioned execution contract. Preserve
cost/guarantee evidence needed downstream instead of exporting only a bare DAG.
For public symbol details, build local API documentation with:

```sh
cargo doc -p asap-logical-asap-optimization -p asap-physical-asap-optimization \
  -p asap-plan-selection -p asap-types --no-deps
```

## Source references

- [Frontend PromQL](../../crates/frontend-promql/src/lib.rs), [SQL](../../crates/frontend-sql/src/lib.rs), [MetricsQL](../../crates/frontend-metricsql/src/lib.rs)
- [Planner facade](../../crates/planner/src/lib.rs), [planning stages](../../crates/planner/src/run.rs), [plan selection and cost models 1–3](../../crates/plan-selection/src/lib.rs)
- [Deployment capabilities](../../crates/logical-asap-optimization/src/deployment_capabilities.rs)
- [Workload types](../../crates/types/src/workload/mod.rs)
- [Planner-runtime contract](../design_docs/architecture/planner-runtime-contract.md)
