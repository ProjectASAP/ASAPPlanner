# Public library functions

Audience: developers embedding ASAPPlanner or supplying deployment models. This
is a compact reference for the public workflow APIs, not an exhaustive symbol
reference. The [CLI guide](../user_guide_docs/run-a-query.md) covers
command-line inspection; the [design overview](../design_docs/architecture/README.md)
defines ownership.

ASAPPlanner runs the #509 stage pipeline: Stage 1 lists the logical
alternatives of each query (Pass 1) and the sharing variants across queries
(Pass 2), Stage 2 builds the physical candidates (materialization), and Stage 3
checks each candidate's accuracy and capabilities and selects the cheapest.
Downstream owns physical binding and commitment: a selected DAG is not evidence
of runtime readiness.

## Choose a library workflow

| Desired result | Calls | Example |
| --- | --- | --- |
| Pre-ASAP IR | Frontend `lower_*` | [Lower a query](#lower-a-query-into-pre-asap-ir) |
| Selected plan for a workload | `asap_planner::e2e_plan` | [Plan a workload](#plan-a-workload) |
| Selected plan from Pre-ASAP roots | `asap_plan_selection::plan_stages` | [Stage pipeline](#run-the-stage-pipeline-on-pre-asap-roots) |
| Stage 1 alternatives only | `stage1_logical_candidates` | [Stage 1 inventory](#inspect-stage-1-alternatives) |
| Exported DAG | `apply_materialization_timings` -> `compile_*_asap_dag` | [Export](#export) |

## Dependencies

Inside this workspace, depend on the frontend you need, `asap-planner` (the
facade), or `asap-logical-optimizer` (Stage 1) and `asap-plan-selection`
(Stages 2 and 3 entry point), and `asap-types`. External users can use Git
dependencies pinned to a compatible revision; use the same revision across
these crates.

## Lower a query into Pre-ASAP IR

| Public function | Required input | Output |
| --- | --- | --- |
| `asap_frontend_promql::lower_promql_workload` | PromQL `PlanningWorkload` with a nonzero `data_ingestion_interval` | All-or-nothing `Result<Vec<Rc<OperatorNode>>, PromqlError>` for normalized batch and repeating entries |
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
lower_promql_workload(workload: &PlanningWorkload, now_ms: u64)
    -> Result<Vec<Rc<OperatorNode>>, PromqlError>
```

`DataWorkload.data_ingestion_interval` must contain a nonzero `Evidence<DurationMs>`.
Pass the actual planning time as `now_ms` (Unix milliseconds), consistently with
downstream planning. Expired or future cadence evidence is rejected,
as is expiring evidence without an observation timestamp. The histogram variant
takes the same timestamp after its histogram catalog argument. The examples use
`0` only because their explicitly supplied cadence is timeless.
Bare instant selectors receive this selection horizon; explicit range selectors
retain their query-specified range. Use `lower_promql_workload_with_histograms`
to supply a histogram catalog for the whole workload. Each entry carries its
own accuracy requirement, with these explicit target choices:

| Value | Meaning | Example |
| --- | --- | --- |
| `AccuracyTarget::Exact` | No approximation permitted | Exact aggregation/unchanged-query alternatives only |
| `AccuracyTarget::Epsilon(e)` | An epsilon error requirement interpreted by the relevant accuracy rule | `Epsilon(0.01)` |
| `AccuracyTarget::EpsilonDelta { epsilon, delta }` | Error requirement with a failure-probability bound | `{ epsilon: 0.01, delta: 0.05 }` |

Epsilon does not mean the same error quantity for every statistic: the
accuracy model checks it against each estimate's own error metric. A target is
a requirement, not proof that a supported candidate exists.

```rust
use asap_frontend_promql::lower_promql_workload;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataWorkload, DurationMs, Evidence, Query,
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
                accuracy: AccuracyRequirement::Explicit(AccuracyTarget::Exact),
                ..Default::default()
            },
            predictability: Default::default(),
            invocations: 1,
            execute_at: None,
            time_selection: Default::default(),
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(1_000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    let pre_asap = lower_promql_workload(&workload, 0)?;
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
example, see [the CLI frontend example](../../crates/devtools/src/bin/show_logical_dag.rs).

## Plan a workload

`e2e_plan(UserInput) -> Result<PlanOutput, PlanError>` lowers a
`PlanningWorkload` with the frontend its language names and runs the stage
pipeline. `UserInput::new(&workload, frontend_input, models)` takes:

| Argument | Choices |
| --- | --- |
| `FrontendInput` | `Promql { now_ms, histograms }`, `Sql { catalog }` or `Metricsql` |
| `PlanningModels` | `PlanningModels::builtin()`, refined with `with_accuracy`, `with_calibration` and `with_capabilities` ([Models](#models)) |

`PlanOutput::plans` holds one `QueryPlan { entry_index, root }` per workload
entry, in `QueryWorkload::entries()` order; `PlanOutput::selection` reports
which candidate was selected, the priced and rejected ones, and whether the
selection is guaranteed optimal. A caller that already holds Pre-ASAP IR calls
`asap_planner::optimize` instead.

```rust
use asap_planner::{e2e_plan, FrontendInput, PlanningModels, UserInput};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataWorkload, DurationMs, Evidence, PlanningWorkload,
    Query, QueryLanguage, QueryRequirements, QueryWorkload,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query("quantile(0.99, latency)".into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(AccuracyTarget::Epsilon(0.01)),
                    ..Default::default()
                },
                predictability: Default::default(),
                invocations: 1,
                execute_at: None,
                time_selection: Default::default(),
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(1_000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    let input = UserInput::new(
        &workload,
        FrontendInput::Promql { now_ms: 0, histograms: None },
        PlanningModels::builtin(),
    );
    let output = e2e_plan(input).await?;
    for plan in &output.plans {
        println!("entry {}: {:#?}", plan.entry_index, plan.root);
    }
    Ok(())
}
```

## Run the stage pipeline on Pre-ASAP roots

```text
asap_plan_selection::plan_stages<Id: Clone>(
    roots: Vec<(Id, QueryRoot)>,
    demand: &[RootDemand],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    display: usize,
) -> Result<StagePipelineRun<Id>, SelectionError>
```

| Argument | Meaning |
| --- | --- |
| `roots` | One Pre-ASAP root per query, with a caller ID |
| `demand` | Per root: accuracy target, recurrence, predictability and latency bound |
| `data` | Data arrival and ingestion evidence Stage 2 and Stage 3 price with |
| `models` | [Models](#models) |
| `display` | Also build and price up to this many candidates for display (0: none) |

`StagePipelineRun::stage1` is Stage 1's inventory, `plan` the selected
`SelectedPlan` (its `logical` roots, `physical` candidate and `selection`
report), and `enumeration` the displayed candidates. `select_plan` and
`select_exhaustive` run Stages 2 and 3 over an existing Stage 1 inventory.

## Inspect Stage 1 alternatives

`asap_logical_optimizer::pass2::identical_expressions::stage1_logical_candidates(roots, &metric_types, &demand)`
returns one `SharingVariant` per Pass 2 sharing form (independent,
identical expressions, summary capability), each with its Pass 1
`LocalLogicalCandidates`: the alternatives (`Realization`) of every target
aggregate. `compose_logical_candidate(&inventory, &choice)` builds the
Pre-ASAP-plus-summary roots for one choice per target. See
[local logical candidates](local-logical-candidates.md). Alternatives are
unranked and carry no accuracy certificate; Stage 3 checks accuracy.

## Models

`PlanningModels` holds the planning logic a deployment can replace:

| Field / builder | Built-in value | Meaning |
| --- | --- | --- |
| `accuracy` / `with_accuracy(&dyn AccuracyModel)` | `asap_plan_selection::DefaultAccuracyModel` | Each estimate's guarantee (`local_guarantee`) and whether it meets the query's target (`satisfies`); Stage 3 rejects an estimate whose family has no model |
| `calibration` / `with_calibration(Stage3Calibration)` | `Stage3Calibration::ILLUSTRATIVE` | Weights that turn modeled resources into cost; illustrative, not measured |
| `capabilities` / `with_capabilities(&DeploymentCapabilities)` | Unrestricted | What the deployment can build, read out and keep; candidates needing more are rejected |
| `evidence` / `with_evidence(&dyn AccuracyEvidenceProvider)` | `NoAccuracyEvidence` | Planning-time accuracy evidence; the stage pipeline does not read it yet |

## Workload inputs and defaults

`PlanningWorkload` holds `QueryWorkload` and optional `DataWorkload` as peer
inputs. `QueryWorkload` contains the language and optional batch/repeating
entries. Entries carry requirements, predictability, recurrence and time
selection. These facts are separate: repeated queries can read data at rest.
`DataWorkload::validate()` checks the independent data evidence: ingestion
rates must be finite and nonnegative, and data at rest cannot have a positive
ingestion rate. `PlanningWorkload::validate()` shares these checks.

| Type/input | Current behavior | Caller responsibility |
| --- | --- | --- |
| `QueryRequirements::default()` | `ImplicitExact`, unspecified response latency | Pass approximation explicitly; each root's requirement becomes its `RootDemand` |
| `DataWorkload::default()` | Unknown arrival, unknown evidence | Supply facts needed for the requested comparisons |
| `Evidence<T>::default()` | No value, unknown source | Unknown/stale evidence is not zero; provide scoped valid observations |
| `Stage3Calibration::ILLUSTRATIVE` | Illustrative, uncalibrated cost weights | Supply a calibration measured for your deployment |

`Default` is a Rust constructor contract, not a general serde omission rule.
Several workload fields require explicit serialized values. A struct field being
optional also does not guarantee every planning operation can succeed without it.

## Export

| Function/type | Purpose |
| --- | --- |
| `asap_types::ir::apply_materialization_timings(&root, &assignment, &mut TimingMemo::new())` | Write execution timing into every node from a `MaterializationAssignment` (default: all query time) and validate the data-state edges; `PlanOutput::execution_timed_dag()` applies the default to a planned workload |
| `asap_types::ir::flat::flatten(&roots)` | A DAG as a flat, serializable node list (`FlatDag`, children as node ids) for inspection; not a physical plan |
| `asap_types::ir::physical_export::compile_physical_asap_dag(&timed_root)` | Export a timed DAG as the `PhysicalASAPDAG` a deployment binds; rejects an untimed node |

Choose the export matching your intended handoff, and preserve the selection
report a downstream needs instead of exporting only a bare DAG. For public
symbol details, build local API documentation with:

```sh
cargo doc -p asap-planner -p asap-logical-optimizer -p asap-plan-selection -p asap-types --no-deps
```

## Source references

- [Frontend PromQL](../../crates/frontend-promql/src/lib.rs), [SQL](../../crates/frontend-sql/src/lib.rs), [MetricsQL](../../crates/frontend-metricsql/src/lib.rs)
- [Facade](../../crates/planner/src/lib.rs)
- [Stage 1](../../crates/logical-optimizer/src/lib.rs), [Stages 2 and 3 entry point](../../crates/plan-selection/src/lib.rs)
- [Workload types](../../crates/types/src/workload/mod.rs)
- [Planner-runtime contract](../design_docs/architecture/planner-runtime-contract.md)
