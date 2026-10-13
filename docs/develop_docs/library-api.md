# Public library functions

Audience: developers embedding ASAPPlanner or supplying deployment models. This
is a compact reference for the public workflow APIs, not an exhaustive symbol
reference. The [CLI guide](../user_guide_docs/run-a-query.md) covers
command-line inspection; the [design overview](../design_docs/architecture/README.md)
defines ownership.

ASAPPlanner runs the planning stages (#509): Stage 0, the query-language
frontends, lowers each query; Stage 1, logical ASAP-aware optimization, lists
each computation's alternatives (Pass 1) and composes whole-workload
candidates; Stage 2, physical ASAP-aware optimization, builds the physical
candidates; Stage 3, plan selection, checks accuracy and selects the cheapest.
Downstream owns physical binding and commitment: a selected DAG is not evidence
of runtime readiness.

## Choose a library workflow

| Desired result | Calls | Example |
| --- | --- | --- |
| Pre-ASAP IR | Frontend `lower_*` | [Lower a query](#lower-a-query-into-pre-asap-ir) |
| Selected plan for a workload | `asap_planner::e2e_plan` | [Plan a workload](#plan-a-workload) |
| Selected plan from Pre-ASAP IR | `asap_aware_mapping::optimize` with `Stage1ToStage3Pass` | [Plan a workload](#plan-a-workload) |
| Every stage's candidates | `asap_aware_mapping::run_stage1_to_stage3` | [Planning stages](#run-the-planning-stages-on-pre-asap-roots) |
| Pass 1 alternatives only | `generate_per_computation_candidates` | [Pass 1](#inspect-pass-1-alternatives) |
| Exported DAG | `PlanOutput::execution_timed_dag` | [Export](#export) |

## Dependencies

Inside this workspace, depend on the frontend you need, `asap-planner` (the
facade) or `asap-aware-mapping` (the planning stages), and `asap-types`.
External users can use Git dependencies pinned to a compatible revision; use
the same revision across these crates.

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

Epsilon does not mean the same error quantity for every statistic. Inspect the
candidate's guarantee and its error metric; a target is a requirement, not proof
that a supported candidate exists.

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

`e2e_plan(UserInput) -> Result<PlanOutput, PlanError>` (async) lowers a
`PlanningWorkload` with the frontend its language names and runs the default
optimization pass, `Stage1ToStage3Pass`. `UserInput::new(&workload, frontend_input, models)` takes:

| Argument | Choices |
| --- | --- |
| `FrontendInput` | `Sql { catalog }`, `Promql { now_ms, histograms }` or `Metricsql { now_ms }` |
| `PlanningModels` | `PlanningModels::builtin()`, refined with `with_accuracy` and `with_evidence` ([Models](#models)) |

`PlanOutput::plans` holds one `QueryPlan { entry_index, root }` per workload
entry, in `QueryWorkload::entries()` order; `PlanOutput::selection` reports
which candidate was selected, the priced candidates and the rejected ones with
their reasons. A caller that already holds Pre-ASAP IR builds a
`ParsedWorkload` and calls
`asap_aware_mapping::optimize(&Stage1ToStage3Pass, OptimizationInput::new(&parsed, models))`
instead. `UserInput::with_pass` plugs in another `OptimizationPass`.

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

## Run the planning stages on Pre-ASAP roots

```text
asap_aware_mapping::run_stage1_to_stage3<Id: Clone + Debug>(
    roots: Vec<(Id, QueryRoot)>,
    targets: &[Option<AccuracyTarget>],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<Stage1ToStage3Run<Id>, PerComputationCandidateError>
```

| Argument | Meaning |
| --- | --- |
| `roots` | One Pre-ASAP root per query, with a caller ID |
| `targets` | Per root: its accuracy target, or `None` for none |
| `data` | Data arrival and ingestion evidence Stage 3 prices with |
| `models` | [Models](#models) |

`Stage1ToStage3Run` holds every stage's output: `stage1_pass1_per_computation`
(Pass 1's inventory), `combinations` (how many whole-workload candidates
exist), `stage1_logical_asap_optimization` (every composed logical
candidate), `stage2_physical_asap_optimization` (Stage 2's candidates),
`stage2_pruned` (logical candidates Stage 2 could not implement, with the
reason) and `stage3_plan_selection` (Stage 3's `Selection`, or why selection
failed).
Every combination is enumerated and priced.

## Inspect Pass 1 alternatives

`asap_aware_mapping::per_computation_candidates::generate_per_computation_candidates(roots)`
returns `PerComputationCandidates`: the roots and one `ComputationTarget` per
eligible computation, each with its unranked candidates (exact, exact
accumulator, or a summary family). `compose_logical_candidate(&inventory, &choice)`
builds the roots for one choice per target. Alternatives carry no accuracy
certificate; Stage 3 checks accuracy.

## Models

`PlanningModels` holds what a deployment can replace:

| Field / builder | Built-in value | Meaning |
| --- | --- | --- |
| `accuracy` / `with_accuracy(&dyn AccuracyModel)` | `DefaultAccuracyModel` | Each summary's guarantee (`local_guarantee`) and whether a guarantee meets the query's target (`satisfies`) |
| `evidence` / `with_evidence(&dyn AccuracyEvidenceProvider)` | `NoAccuracyEvidence` | The deployment's empirical accuracy for a candidate's whole output (`output_guarantee`); Stage 3 accepts an output whose theoretical guarantee misses the target only when this shows it meets it |

Stage 3 prices candidates with its own analytical model; there is no cost
model input.

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
| `QueryRequirements::default()` | `ImplicitExact`, unspecified response latency | Pass approximation explicitly; each query's target is checked in Stage 3 |
| `DataWorkload::default()` | Unknown arrival, unknown evidence | Supply facts needed for the requested comparisons |
| `Evidence<T>::default()` | No value, unknown source | Unknown/stale evidence is not zero; provide scoped valid observations |
| Stage 3 pricing | Analytical model with illustrative statistics | Treat costs as illustrative, not measured |

`Default` is a Rust constructor contract, not a general serde omission rule.
Several workload fields require explicit serialized values. A struct field being
optional also does not guarantee every planning operation can succeed without it.

## Export

| Function/type | Purpose |
| --- | --- |
| `PlanOutput::execution_timed_dag()` | The planned workload as one `PhysicalASAPDAG`, every node at query time, shared sub-DAGs exported once |
| `asap_types::ir::apply_materialization_timings(&root, &assignment, &mut TimingMemo::new())` | Write execution timing into every node from a `MaterializationAssignment` (default: all query time) and validate the data-state edges |
| `asap_types::ir::physical_export::compile_physical_asap_dag(&timed_root)` | Export a timed DAG as a `PhysicalASAPDAG` (`PHYSICAL_ASAP_DAG_WIRE_VERSION`); rejects an untimed node; `compile_physical_asap_workload(&roots)` exports a batch with one root per query |
| `PhysicalASAPDAGDocument::new(dag)` and `.validate()` | Versioned semantic envelope and explicit validation; constructing it alone does not validate |
| `asap_types::ir::flat::flatten(&roots)` | A DAG as a flat node list (`FlatDag`) for inspection; not a physical plan |

Preserve the selection report a downstream needs instead of exporting only a
bare DAG. For public symbol details, build local API documentation with:

```sh
cargo doc -p asap-planner -p asap-aware-mapping -p asap-types --no-deps
```

## Source references

- [Frontend PromQL](../../crates/frontend-promql/src/lib.rs), [SQL](../../crates/frontend-sql/src/lib.rs), [MetricsQL](../../crates/frontend-metricsql/src/lib.rs)
- [Facade](../../crates/planner/src/lib.rs)
- [Planning stages](../../crates/asap-aware-mapping/src/pass/stage1_to_stage3.rs)
- [Workload types](../../crates/types/src/workload.rs)
- [Planner-runtime contract](../design_docs/architecture/planner-runtime-contract.md)
