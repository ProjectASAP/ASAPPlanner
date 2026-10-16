# Public library functions

Audience: developers embedding ASAPPlanner or adding strategies/models. This is
a compact reference for the public workflow APIs, not an
exhaustive symbol reference. The [CLI guide](../user_guide_docs/run-a-query.md) covers command-line inspection; the [design overview](../design_docs/architecture/README.md) defines ownership.

ASAPPlanner's output is the plan selected by its planning stages
(`PlanOutput`, see [Plan a workload through the stages](#plan-a-workload-through-the-stages)).
The legacy search below outputs `CandidateLogicalASAPDAGs`; ranking is a view over its candidates.
Downstream owns physical binding and commitment. Selection/DAG assembly helpers
do not deploy a plan, and a serializable DAG is not evidence of runtime readiness.

## Choose a library workflow

| Desired result | Calls | Example |
| --- | --- | --- |
| Pre-ASAP IR | Frontend `lower_*` | [Lower a query](#lower-a-query-into-pre-asap-ir) |
| Selected plan for a workload (Stages 0–3) | `asap_planner::e2e_plan` | [Plan a workload](#plan-a-workload-through-the-stages) |
| All ranked candidates | `search_workload_with_targets` -> `cost_sorted` | [Generate and rank](#generate-and-rank-candidates) |
| Custom optimization set | Construct `Vec<Box<dyn ReplacementStrategy>>`, then search | [Strategies and models](#choose-strategies-and-models) |
| Selected semantic DAG / export | `global_selection` -> `assemble_selected_dag` -> export | [Selection example](#optional-whole-plan-selection-and-dag-assembly) |

Each recipe ends at a different artifact. Use only the stages needed for that
artifact, while preserving the checks required by its intended consumer.

## Dependencies

Inside this workspace, depend on the frontend you need, `asap-aware-mapping`,
and `asap-types`, or on `asap-planner`, which wraps every frontend and the
planning stages. External users can use Git dependencies pinned to a compatible
revision; use the same revision across these crates. For the example below:

```toml
[dependencies]
asap-frontend-promql = { git = "https://github.com/ProjectASAP/ASAPPlanner", rev = "e7fdb2492c42c9f5b34760706a5162aa586d3025" }
asap-aware-mapping = { git = "https://github.com/ProjectASAP/ASAPPlanner", rev = "e7fdb2492c42c9f5b34760706a5162aa586d3025" }
asap-types = { git = "https://github.com/ProjectASAP/ASAPPlanner", rev = "e7fdb2492c42c9f5b34760706a5162aa586d3025" }
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
| `.accuracy_scope(s)` | Which output values one statement covers (`AccuracyScope`) | `.accuracy_scope(AccuracyScope::WholeResult)` |

In JSON the target is a plain struct, for example
`{"error": 0.01, "failure_probability": 0.001, "accuracy_metric": "rank"}`.

Epsilon does not mean the same error quantity for every statistic. Inspect the
candidate's guarantee and its error metric; a target is a requirement, not proof
that a supported candidate exists.

```rust
use asap_frontend_promql::lower_promql_workload;
use asap_types::workload::{
    BatchEntry, DataWorkload, DurationMs, Query,
    PlanningWorkload, QueryLanguage, QueryRequirements, QueryWorkload,
};
use asap_types::types::AccuracyTarget;

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
(Stage 2) and plan selection (Stage 3) in order.

```text
asap_planner::e2e_plan(input: UserInput<'_>) -> Result<PlanOutput, PlanError>   // async
UserInput::new(workload: &PlanningWorkload, frontend_specific: FrontendInput<'_>,
    models: PlanningModels<'_>) -> UserInput<'_>
FrontendInput::{Sql { catalog: &SqlCatalog }, Promql { histograms: Option<HistogramCatalog> }, Metricsql}

// A caller that already holds lowered roots calls the stages directly:
asap_aware_mapping::run_stage1_to_stage3<Id>(
    roots: Vec<(Id, QueryRoot)>,
    entries: &[QueryWorkloadEntry],   // one per root: accuracy, latency bound, recurrence
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<Stage1ToStage3Run<Id>, PerComputationCandidateError>
```

| Input or output | Meaning |
| --- | --- |
| `PlanningModels::builtin()` | Built-in accuracy model and no extra evidence. `with_plan_selection(&settings)` sets the deployment's `PlanSelectionSettings`; `with_deployment_capabilities(&capabilities)` sets its `DeploymentCapabilities` (field `deployment_capabilities`) |
| `DeploymentCapabilities` (`asap_aware_mapping::deployment_capabilities`) | What the executor supports: `supported_summaries` (`None`, the default, supports every summary the planner builds; otherwise `SupportedSummary { family: SketchAlgorithm, readouts: Vec<StatisticKind> }` entries; Pass 1 prunes others with `PruneReason::UnsupportedByDeployment`), `raw_data_retained` (default `true`; when `false`, a candidate that scans raw data at query time is invalid, and stored data is refused with `SelectionError::InvalidDeploymentCapabilities`) and `query_time_results_retained` (default `false`; when `true`, Stage 2 offers `MatAtQueryKeptAcrossEvaluations` for tumbling panes) |
| `PlanSelectionSettings` (`asap_aware_mapping::stage3_plan_selection`) | `cost_model`, `resource_speed` (`ms_per_cpu_op`, `ms_per_scanned_byte`) and `non_repeating_query_horizon_seconds` (default 3,600) |
| `CostModel::UsageOverTime(ResourceUnitCost)` | Cost model 1 (default, `ResourceUnitCost::illustrative()`): unit cost × usage per second; the cheapest valid candidate wins |
| `CostModel::ProvisionedCapacity(ResourceUnitCost)` | Cost model 2: unit cost × peak memory, peak CPU cores and disk; the cheapest valid candidate wins |
| `CostModel::ClusterCapacity(ClusterLimits)` | Cost model 3: memory, disk and optional CPU cores are hard limits; `ClusterObjective::{LowestLatency, LeastMemory, ParetoFront}` chooses among the candidates that fit |
| `PlanOutput.plans` | One `QueryPlan { entry_index, root }` per query, in `QueryWorkload::entries()` order |
| `PlanOutput.selection` | Stage 3's `Selection`: `selected`, `costs` (each candidate's `CandidateCost` with its `ResourceUsage` and `cost_per_second`, `None` under cost model 3), `accepted`, `rejected` (with reasons) and `front` |
| `PlanOutput::execution_timed_dag()` | The planned queries as one `PhysicalASAPDAG`, timed with the default `MaterializationAssignment` (see [Export and explain](#export-and-explain)) |

Each candidate's `ResourceUsage` holds its usage without prices:
`cpu_ops_per_second`, `scanned_bytes_per_second`, `average_memory_bytes`,
`peak_memory_bytes`, `disk_bytes`, `peak_cpu_cores` and `latency_per_query`.
The deployment's choice of cost model reads it.

Example (add `asap-planner` and an async runtime such as `tokio` to the
dependencies):

```rust
use asap_planner::{e2e_plan, FrontendInput, PlanningModels, UserInput};
use asap_types::types::AccuracyTarget;
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
        "selected {} at {:?} per second",
        selection.selected, selection.costs[&selection.selected].cost_per_second
    );
    for rejection in &selection.rejected {
        println!("{}: {}", rejection.id, rejection.reason);
    }
    let dag = output.execution_timed_dag()?;
    println!("{} physical nodes", dag.nodes.len());
    Ok(())
}
```

It prints `selected P1 at Some(0.000313...) per second`, then the two
rejected heap top-k candidates, `P2` and `P3`, each with the reason
`q1: the output has no guarantee, ...`. To plan under cost
model 3 instead:

```rust
use asap_aware_mapping::stage3_plan_selection::{
    ClusterLimits, ClusterObjective, CostModel, PlanSelectionSettings,
};

let settings = PlanSelectionSettings {
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

## Generate and rank candidates

### Target sub-DAG candidates

`TargetSubDAGCandidates` collects alternatives for one query subexpression
discovered by search. `CandidateLogicalASAPDAGs` contains these per-target candidate sets and
the workload's query roots. A root is a whole query; an inner expression can
also be a target.

For example, a supported `quantile(0.99, latency)` subexpression may have multiple
summary alternatives. Those alternatives belong to the same candidate set
because they are choices for the same computation. Another subexpression has its
own candidate set. If two queries reference a shared subexpression, they can
share its selected computation.

`cost_sorted()` returns a `RankedTargetSubDAGCandidates` for each target: the subexpression,
its candidates in ranked order, and a cost entry aligned with each candidate.
It keeps the alternatives available; it does not select an entire workload plan.

### API definition

```text
search_workload_with_targets<'s, Id>(
    roots: Vec<(Id, Rc<OperatorNode>, Option<AccuracyTarget>)>,
    strategies: &[Box<dyn ReplacementStrategy + 's>],
    accuracy_model: &dyn AccuracyModel,
) -> Result<CandidateLogicalASAPDAGs<Id>, SearchError>

CandidateLogicalASAPDAGs::cost_sorted(&self, cost_model: &dyn CostModel)
    -> Vec<RankedTargetSubDAGCandidates<'_>>
```

| Argument | Choices / meaning | Required? |
| --- | --- | --- |
| `roots` | One tuple per query: caller ID, canonical IR, and root target | Yes |
| Root target | `Some(AccuracyTarget::…)` applies an explicit end-to-end requirement; `None` adds no explicit root target | Tuple field required; value optional |
| `strategies` | Default factory output or an explicit strategy vector; see option tables below | Yes; even an empty vector does not disable automatic workload strategies |
| `accuracy_model` | `DefaultAccuracyModel` or a custom `AccuracyModel` implementation | Yes |
| Ranking `cost_model` | `DefaultCostModel` or an evidence-backed/custom `CostModel` | Yes |

`search_workload_with_targets` normally rejects candidates without a guarantee
that satisfies the root target. One exception is a direct DDSketch quantile
ratio: without input-domain evidence, it remains in `CandidateLogicalASAPDAGs` with
`guarantee: None` so the downstream backend can decide whether to select it.
Its presence does **not** mean it satisfies the target. `cost_sorted` still
shows it, but `global_selection` skips it and DAG assembly uses the exact fallback
unless a certified alternative is available. A backend that wants the
uncertified candidate must explicitly inspect it and check its own domain
evidence and execution requirements before selecting or deploying it.

### Example


The following complete Rust example lowers one query, supplies an explicit root
accuracy target, and prints every ranked candidate instead of selecting a winner.
The default cost model is suitable for inspection, not deployment calibration.

```rust
use asap_frontend_promql::lower_promql_workload;
use asap_types::workload::{
    BatchEntry, DataWorkload, DurationMs, Query,
    PlanningWorkload, QueryLanguage, QueryRequirements, QueryWorkload,
};
use asap_aware_mapping::{
    default_strategies_with, search_workload_with_targets,
    DefaultAccuracyModel, DefaultCostModel,
};
use asap_types::types::AccuracyTarget;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let accuracy = AccuracyTarget::max_error(0.01);
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
            query: Query("quantile(0.99, latency)".into()),
            requirements: QueryRequirements {
                accuracy: accuracy.clone(),
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
    let root = lower_promql_workload(&workload)?.remove(0);
    let cost_model = DefaultCostModel;
    let strategies = default_strategies_with(&cost_model);
    let space = search_workload_with_targets(
        vec![("q1", root, Some(accuracy))],
        &strategies,
        &DefaultAccuracyModel,
    )?;
    for group in space.cost_sorted(&cost_model) {
        for (candidate, cost) in group.candidates.iter().zip(&group.costs) {
            println!("candidate={candidate:?}, reported_cost={cost:?}");
        }
    }
    Ok(())
}
```

| API (`asap_aware_mapping`, unless qualified) | Inputs | Output and limits |
| --- | --- | --- |
| `search_workload` | `(query_id, Rc<OperatorNode>)` roots | `CandidateLogicalASAPDAGs` with built-in strategies/model; no explicit per-root target argument |
| `search_workload_with` | Roots, strategy slice | `CandidateLogicalASAPDAGs`; callers choose context-free replacement strategies |
| `search_workload_with_targets` | Roots with optional end-to-end targets, strategies, accuracy model | Candidate space with supplied root-target checks; `None` does not supply a root-level requirement; uncertified direct DDSketch ratios remain available for backend selection |
| `CandidateLogicalASAPDAGs::cost_sorted` | Cost model | `Vec<RankedTargetSubDAGCandidates>`; retains alternatives and pairs `candidates[i]` with `costs[i]` |
| `CandidateLogicalASAPDAGs::cost_sorted_with_recurrence` | Cost model, recurrence profiles, optional horizon | Ranked per-target candidate sets or `RecurrenceError`; uses recurrence for applicable share/recompute comparisons |
| `ASAPStrategies::replacements` through `ReplacementStrategy` | One `TargetSubDAG` | Alternatives at that target; not whole-workload search |

`cost_sorted` is a ranking view, not a request to discard all but the first
candidate. Display costs follow model hooks and may be unavailable/non-finite;
they are not necessarily a globally sortable physical-cost scalar. Unavailable
cost alternatives may remain for explanation. Inspect eligibility and evidence
before physical selection; do not treat their presence as deployment permission.

### Enumerate candidate DAGs per root

```text
CandidateLogicalASAPDAGs::enumerate_candidate_dags_for_root(&self, id: &Id, expansion_limit: usize)
    -> Result<CandidateDAGInventory<Id>, RealizationError>
```

Returns every distinct finalized DAG for one root, unranked; other roots'
choices are not multiplied in. Exceeding `expansion_limit` is an error, never a
partial inventory.

For PromQL roots that carry a target, `search_workload_with_targets` also asks
each strategy's `ReplacementStrategy::propose_for_root`. `ASAPStrategies`
answers an instant-vector TopK with current-series heap realizations over rows
carrying the complete series identity (`$promql_series_identity`). They are
finalized, deduplicated, and marked `ReplacementProvenance::RootPhysicalRealization`.
Callers do not apply `with_series_identity` themselves. Compile each with
`promql_rows::compile_current_series_evaluation`; other queries keep their previous
inventory. `global_selection` never commits these candidates; the backend
compiles and prices them. CandidateLogicalASAPDAGs lists no placement variants: node timing
comes from a `MaterializationAssignment` (all query time for the legacy search;
Stage 2 materialization chooses one per physical candidate).

## Choose strategies and models

### Strategy options

The `strategies` argument takes Rust objects implementing `ReplacementStrategy`,
not string names or a closed enum. These built-in context-free choices can be
combined in one vector; each proposes candidates where its applicability checks
pass. An omitted strategy contributes no proposals of its own.

| Value to put inside `Box::new(...)` | Meaning | In default factories? |
| --- | --- | --- |
| `ASAPStrategies::new(&model)` | Enumerates supported exact/sketch implementations and parameter choices for aggregate targets | Yes |
| `HydraGroupingStrategy::new(&model)` | Considers a shared multi-subpopulation structure for supported grouped sketch families, subject to accuracy evidence | Yes |
| `SharedSubDAGStrategy` | Proposes sharing versus independent recomputation at reused sub-DAGs | Yes |
| `SemanticEquivalentRewriteStrategy` | Proposes supported equivalent aggregate rewrites, including decomposing average into sum/count | Yes |
| `ExactCompositionStrategy::new(&model)` | Proposes supported exact operations around summary evaluations or in maintenance | Yes |
| Your `ReplacementStrategy` implementation | Adds domain-specific legal replacement proposals | No |

`AvgToSumOverCountStrategy` is an alias for `SemanticEquivalentRewriteStrategy`
at this revision; it is not a separate narrow rewrite to enable alongside it.

The following are derived automatically from the workload by `search_workload*`:

| Automatic behavior | Meaning | Can the strategy vector disable it? |
| --- | --- | --- |
| Canonical sharing/CSE | Interns structurally equal input subexpressions | No |
| `RollupStrategy` | Proposes compatible reuse across grouping granularities | No |
| `AccuracyReconciliationStrategy` | Proposes compatible sharing across different accuracy requirements | No |
| `TopKLimitReuseStrategy` | Proposes reuse among compatible top-k limits | No |

The current API does not expose a universal enable/disable flag for every pass.
For inspecting only one strategy at one target, use
`ReplacementStrategy::replacements(&TargetSubDAG)`; this does not perform the
whole-workload search. Selecting a strategy does not force its candidate to win.

### Factory choices

```text
default_strategies() -> Vec<Box<dyn ReplacementStrategy>>
default_strategies_with<'a>(cost_model: &'a dyn CostModel)
    -> Vec<Box<dyn ReplacementStrategy + 'a>>
replacement::default_strategies_with_evidence<'a>(
    cost_model: &'a dyn CostModel, evidence: &'a dyn AccuracyEvidenceProvider,
) -> Vec<Box<dyn ReplacementStrategy + 'a>>
```

| Factory | Use when | Models used |
| --- | --- | --- |
| `default_strategies()` | Exploring with built-in defaults | Built-in cost/accuracy defaults |
| `default_strategies_with(&model)` | Supplying deployment-specific costing/sizing | Supplied cost model; default accuracy |
| `default_strategies_with_evidence(&model, &evidence)` | Supplying planning-time accuracy evidence as well | Supplied cost and evidence; default accuracy |
| Explicit vector | Controlling which context-free strategies are supplied | Models passed into each constructor |

### Example: supply two strategies and run search

```rust
use asap_frontend_promql::lower_promql_workload;
use asap_types::workload::{
    BatchEntry, DataWorkload, DurationMs, Query,
    PlanningWorkload, QueryLanguage, QueryRequirements, QueryWorkload,
};
use asap_aware_mapping::{
    search_workload_with_targets, DefaultAccuracyModel, DefaultCostModel,
    ReplacementStrategy, ASAPStrategies, SharedSubDAGStrategy,
};
use asap_types::types::AccuracyTarget;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let accuracy = AccuracyTarget::max_error(0.01);
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
            query: Query("quantile(0.99, latency)".into()),
            requirements: QueryRequirements {
                accuracy: accuracy.clone(),
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
    let root = lower_promql_workload(&workload)?.remove(0);
    let model = DefaultCostModel;
    let strategies: Vec<Box<dyn ReplacementStrategy + '_>> = vec![
        Box::new(ASAPStrategies::new(&model)),
        Box::new(SharedSubDAGStrategy),
    ];
    let space = search_workload_with_targets(
        vec![("q1", root, Some(accuracy))], &strategies, &DefaultAccuracyModel,
    )?;
    println!("{:#?}", space.cost_sorted(&model));
    Ok(())
}
```

This omits Hydra and semantic/exact-composition strategies from the supplied
vector. Automatic workload strategies still run. Omitting an optimization does
not waive semantic or accuracy requirements.

### Model and evidence options

Traits permit custom implementations; the following are concrete built-in options.
Module-qualified paths below are relative to `asap_aware_mapping`.

| Parameter | Available value / constructor | Meaning |
| --- | --- | --- |
| `&dyn CostModel` | `DefaultCostModel` | Built-in ordering/sizing and structural estimates; no measured deployment guarantee |
| `&dyn CostModel` | `empirical_cost::EmpiricalCostModel::new(provider)` | Offline sketch-benchmark model: ranks algorithms using matching offline measurements |
| `&dyn CostModel` | `physical_plan_cost_model::PhysicalPlanCostModel::new(&provider, calibration)?` | Deployment-specific physical-plan model: compares complete physical alternatives using provider evidence and resource calibration; evidence may be offline or online |
| `&dyn AccuracyModel` | `DefaultAccuracyModel` | Built-in guarantee rules and satisfaction checks |
| `&dyn AccuracyEvidenceProvider` | `NoAccuracyEvidence` | No extra planning-time statistics; evidence-dependent claims remain unavailable |
| Any provider trait above | Your implementation | Supplies alternative models/evidence under the same contracts |

### Offline measurements versus physical-plan costing

These models differ in scope, not simply in whether they are offline or online.

| Model | Evidence and comparison | Missing evidence / limits |
| --- | --- | --- |
| `EmpiricalCostModel` | Offline sketch benchmarks matched to exact parameters, distribution, environment and validity interval; current algorithm ranking uses measured update CPU nanoseconds | If the measurements required for ranking are incomplete, preserves the incoming algorithm order. `estimate_cost()` still uses `DefaultCostModel` structural scores |
| `PhysicalPlanCostModel` | A downstream provider supplies a consistent evidence snapshot and complete physical alternatives; calibration converts modeled resource quantities into comparable costs | A candidate with incomplete evidence is unavailable, without structural-cost fallback. Current candidate admission also requires it to cost less than the raw alternative |

`PhysicalPlanCostModel` does not collect online telemetry itself. Its provider
may supply offline estimates/calibration or evidence derived from online
observations. Therefore, “offline sketch-benchmark model” and “physical-plan cost
model” describe their roles more accurately than “offline model” and “online model.”

For example, a sketch with the lowest measured update cost can rank first under
`EmpiricalCostModel`, while its complete execution plan can still cost more than
another sketch or raw execution under `PhysicalPlanCostModel`. Offline error
measurements alone do not authorize smaller sketch parameters or replace formal
accuracy guarantees.

### Example: configure all sketch-strategy providers

```rust
use asap_aware_mapping::{
    DefaultAccuracyModel, DefaultCostModel,
    NoAccuracyEvidence, ReplacementStrategy, ASAPStrategies,
};

fn main() {
    let cost = DefaultCostModel;
    let accuracy = DefaultAccuracyModel;
    let evidence = NoAccuracyEvidence;
    let strategies: Vec<Box<dyn ReplacementStrategy + '_>> = vec![Box::new(
        ASAPStrategies::new_with_planning_inputs_and_evidence(
            &cost, &accuracy, &evidence,
        ),
    )];
    // Use &strategies and &accuracy in search_workload_with_targets.
    println!("{} explicitly configured strategy", strategies.len());
}
```

Constructor definition:

```text
ASAPStrategies::new_with_planning_inputs_and_evidence(
    cost_model: &dyn CostModel,
    accuracy_model: &dyn AccuracyModel,
    evidence: &dyn AccuracyEvidenceProvider,
) -> ASAPStrategies
```

All provider arguments are required for this constructor. They must outlive the
strategy vector. `ASAPStrategies::new(&cost_model)` is the shorter
constructor using the default accuracy model and no extra evidence.

| Extension point | What it controls | What it cannot establish alone |
| --- | --- | --- |
| `ReplacementStrategy` | Proposed semantic alternatives | Permission to violate query semantics or downstream support |
| `CostModel` | Candidate ordering/sizing hooks and recurrence cost hooks | Correctness, measured costs without evidence, or installed runtime support |
| `AccuracyModel` | Derivation, propagation and satisfaction of guarantees | A meaningful guarantee without its required assumptions/evidence |
| `AccuracyEvidenceProvider` | Planning-time statistics used by supported strategies | Authority to change query requirements |

Models may be consumed during generation as well as ranking. Construct strategies
with the intended model/evidence; replacing only the final sorting model does not
regenerate parameter choices. For evidence-aware defaults, use
`asap_aware_mapping::replacement::default_strategies_with_evidence`.
For custom accuracy/evidence on sketches,
`ASAPStrategies::new_with_planning_inputs_and_evidence` exposes these providers.
Keep each provider's evidence scope and freshness valid for the query population.

## Workload inputs and defaults

`PlanningWorkload` holds `QueryWorkload` and optional `DataWorkload` as peer
inputs. `QueryWorkload` contains the language and optional batch/repeating
entries. Entries carry requirements, predictability and recurrence; a
query's window comes from its text (range and offset). These facts are separate: repeated queries can read data at rest.
`DataWorkload` holds `arrival` (`DataArrival`: `AtRest`, `Ingesting` or
`Mixed` with `rows_per_second`, or `Unknown`) and `sources`, a map from
PromQL metric or SQL table name to `SourceFacts`: `series_count`,
`metric_type`, `sample_interval` (PromQL and MetricsQL), `stream_norms` and
`max_distinct_per_evaluation`. Every fact is declared: the planner has no
clock, so a caller drops facts that no longer hold before planning. Plan
selection bounds a query's number of values `G`, and its input row count,
by the `series_count` of the sources it reads, counting each branch of an
`or` or union. `DataWorkload::validate()` checks the data facts: the
arrival rate must be finite and nonnegative, a sample interval nonzero,
stream norms finite and nonnegative, and distinct bounds positive.
`PlanningWorkload::validate()` shares these checks.

```json
{"arrival": {"ingesting": {"rows_per_second": 66666.7}},
 "sources": {"http_requests_total":
   {"series_count": 1000000, "metric_type": "counter", "sample_interval": 15000}}}
```

| Type/input | Current behavior | Caller responsibility |
| --- | --- | --- |
| `QueryRequirements::default()` | `AccuracyTarget::exact()`, no latency bound (`max_latency_ms: None`) | Pass approximation explicitly and thread per-root requirements into search |
| `DataWorkload::default()` | Unknown arrival, no source facts | Supply facts needed for the requested comparisons |
| `SourceFacts::default()` | Every fact unknown | Unknown facts are not zero; drop facts that no longer hold before planning |
| `DefaultCostModel` | Built-in ordering/sizing and structural cost hooks | Supply deployment evidence for calibrated comparisons |

`Default` is a Rust constructor contract, not a general serde omission rule.
Several workload fields require explicit serialized values. A struct field being
optional also does not guarantee every planning operation can succeed without it.

## Optional whole-plan selection and DAG assembly

### What does global selection mean?

`global_selection()` coordinates choices **across target sub-DAG candidate sets
in the workload**. Here, “global” describes that cross-target scope. It does not
mean a proven globally optimal solution over every possible physical plan, nor
selection across every machine in a deployment.

Consider this conceptual dependency DAG:

```text
Q1 --+
     +--> A --> B
Q2 --+

A's candidate set: alternatives for computing A
B's candidate set: alternatives for computing B
```

Both queries need A, and computing A needs B. Choosing to compute A once and
share it, versus recomputing it for each consumer, changes how many evaluations
of B are needed. That can change which choice for B is preferable.

`cost_sorted()` ranks each target's alternatives using that target's recorded
consumer count. `global_selection()` accounts for ancestor sharing decisions
when deriving effective usage counts, and keeps coupled parent/child composition
choices consistent. The result records coordinated choices; `assemble_selected_dag()` then
constructs the selected semantic DAG while preserving shared nodes.

| Operation | Question answered | Result |
| --- | --- | --- |
| `cost_sorted()` | How are the alternatives ranked for each subexpression? | Ranked alternatives per target |
| `global_selection()` | Which compatible choices should be used together, accounting for sharing and dependencies? | A coordinated selection across targets under the supplied model |

Plain `global_selection()` does not decide materialization or establish
physical deployment feasibility. Stage 2 materialization owns
materialization; downstream still owns physical commitment.

| Method on `CandidateLogicalASAPDAGs` / `GlobalSelection` | Behavior |
| --- | --- |
| `CandidateLogicalASAPDAGs::global_selection(&model)` | Compatible structural selection across targets; no recurrence or materialization planning implied |
| `CandidateLogicalASAPDAGs::global_selection_with_recurrence(...)` | Compatible selection using supplied recurrence profiles/horizon; no materialization commitments implied |
| `GlobalSelection::assemble_selected_dag(&target)` | `Result<Option<Rc<OperatorNode>>, RealizationError>`; constructs untimed semantic IR, not stored summary data |

Use a target associated with the searched space; DAG assembly can return `None`
when that target is absent. A downstream integration can use these convenience
APIs when its supplied model/evidence supports the intended comparison. Neither
plain structural selection nor taking each target's first candidate substitutes
for checking complete physical alternatives and deployment constraints.

### API definition and example

```text
CandidateLogicalASAPDAGs::global_selection(&self, cost_model: &dyn CostModel) -> GlobalSelection<'_>
GlobalSelection::assemble_selected_dag(&self, target: &Rc<OperatorNode>)
    -> Result<Option<Rc<OperatorNode>>, RealizationError>
```

For structural inspection only, this complete example selects a semantic root
and prints it. It performs no materialization or deployment
planning.

```rust
use asap_frontend_promql::lower_promql_workload;
use asap_types::workload::{
    BatchEntry, DataWorkload, DurationMs, Query,
    PlanningWorkload, QueryLanguage, QueryRequirements, QueryWorkload,
};
use asap_aware_mapping::{search_workload, DefaultCostModel};
use asap_types::types::AccuracyTarget;

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
    let root = lower_promql_workload(&workload)?.remove(0);
    let space = search_workload(vec![("q1", root)])?;
    let selection = space.global_selection(&DefaultCostModel);
    // Search may canonicalize roots; use the root returned by CandidateLogicalASAPDAGs.
    if let Some(selected) = selection.assemble_selected_dag(&space.roots[0].1)? {
        println!("{selected:#?}");
    }
    Ok(())
}
```

## Export and explain

| Function/type | Purpose |
| --- | --- |
| `asap_types::ir::apply_materialization_timings(&root, &assignment, &mut TimingMemo::new())` | Write execution timing into every node from a `MaterializationAssignment` (default: all query time) and validate the data-state edges; `PlanOutput::execution_timed_dag()` applies the default to a planned workload |
| `asap_types::ir::physical_export::compile_physical_asap_dag(&timed_root)` | Export a timed DAG as a `PhysicalASAPDAG` (`PHYSICAL_ASAP_DAG_WIRE_VERSION`); rejects an untimed node; `compile_physical_asap_workload(&roots)` exports a batch with one root per query |
| `PhysicalASAPDAGDocument::new(dag)` and `.validate()` | Versioned semantic envelope and explicit validation; constructing it alone does not validate |
| `explain_replacements` / `explain_replacements_with` | Findings from default/custom-strategy search; not a complete physical feasibility report |

Choose the export matching your intended handoff: an untimed semantic DAG is not
interchangeable with a versioned execution contract. Preserve
cost/guarantee evidence needed downstream instead of exporting only a bare DAG.
For public symbol details, build local API documentation with:

```sh
cargo doc -p asap-aware-mapping -p asap-types --no-deps
```

## Source references

- [Frontend PromQL](../../crates/frontend-promql/src/lib.rs), [SQL](../../crates/frontend-sql/src/lib.rs), [MetricsQL](../../crates/frontend-metricsql/src/lib.rs)
- [Planner facade](../../crates/planner/src/lib.rs), [planning stages](../../crates/asap-aware-mapping/src/pass/stage1_to_stage3.rs), [plan selection and cost models 1–3](../../crates/asap-aware-mapping/src/stage3_plan_selection.rs)
- [Search, ranking and selection](../../crates/asap-aware-mapping/src/replacement.rs)
- [Cost models](../../crates/asap-aware-mapping/src/cost_model.rs)
- [Workload types](../../crates/types/src/workload.rs)
- [Planner-runtime contract](../design_docs/architecture/planner-runtime-contract.md)
