# Public library functions

Audience: developers embedding ASAPPlanner or adding strategies/models. This is
a compact reference for the public workflow APIs at revision `e7fdb24`, not an
exhaustive symbol reference. The [CLI guide](../user-guide/user-guide.md) covers command-line inspection; the [design overview](../design_docs/README.md) defines ownership.

ASAPPlanner's primary output is `PlanSpace` plus ranked legal candidates.
Downstream owns physical binding and commitment. Selection/materialization helpers
do not deploy a plan, and a serializable DAG is not evidence of runtime readiness.

## Choose a library workflow

| Desired result | Calls | Example |
| --- | --- | --- |
| Pre-ASAP IR | Frontend `lower_*` | [Lower a query](#lower-a-query-into-pre-asap-ir) |
| All ranked candidates | `search_workload_with_targets` -> `cost_sorted` | [Generate and rank](#generate-and-rank-candidates) |
| Custom optimization set | Construct `Vec<Box<dyn ReplacementStrategy>>`, then search | [Strategies and models](#choose-strategies-and-models) |
| Lifecycle-aware comparison | Lifecycle-aware selection -> lifecycle materialization | [Lifecycle recipe](#lifecycle-and-capabilities) |
| Selected semantic DAG / export | `global_selection` -> `materialize` -> export | [Selection example](#optional-whole-plan-selection-and-materialization) |

Each recipe ends at a different artifact. Use only the stages needed for that
artifact, while preserving the checks required by its intended consumer.

## Dependencies

Inside this workspace, depend on the frontend you need, `asap-aware-mapping`,
and `asap-types`. External users can use Git dependencies pinned to a compatible
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
| `asap_frontend_promql::lower_promql` | Query string, `AccuracyTarget` | `Result<QueryExpr, PromqlError>` |
| `asap_frontend_metricsql::lower_metricsql` | Query string, `AccuracyTarget` | `Result<QueryExpr, MetricsqlError>` |
| `asap_frontend_sql::lower_sql` | Query string, `SqlCatalog`, accuracy | Async `Result<QueryExpr, SqlError>`; default SQL dialect is DataFusionSQL |
| `asap_frontend_sql::lower_sql_dialect` | Same inputs plus `SqlDialect` | Async resolved Pre-ASAP query or error |
| `lower_promql_batch` / `lower_sql_batch` in their frontend crates | `QueryWorkload`; SQL additionally needs catalog | Per-query results for `query_batch`; these helpers do not iterate `repeating_queries` |

Lowering resolves the supported source language into the canonical query
representation. It does not enumerate Post-ASAP alternatives. A frontend may
reject unsupported syntax or semantics; a declared language/dialect enum does
not imply complete support. For mixed one-time/repeating workloads, use normalized
`QueryWorkload::entries()` and the appropriate single-query frontend, preserving
entry-to-root associations for later workload-aware operations.

### Definition and example

PromQL's public signature (types are imported from their respective crates):

```text
lower_promql(query: &str, accuracy: AccuracyTarget)
    -> Result<QueryExpr, PromqlError>
```

`query` and `accuracy` are required. These are the accuracy argument's choices:

| Value | Meaning | Example |
| --- | --- | --- |
| `AccuracyTarget::Exact` | No approximation permitted | Exact aggregation/unchanged-query alternatives only |
| `AccuracyTarget::Epsilon(e)` | An epsilon error requirement interpreted by the relevant accuracy rule | `Epsilon(0.01)` |
| `AccuracyTarget::EpsilonDelta { epsilon, delta }` | Error requirement with a failure-probability bound | `{ epsilon: 0.01, delta: 0.05 }` |

Epsilon does not mean the same error quantity for every statistic. Inspect the
candidate's guarantee and its error metric; a target is a requirement, not proof
that a supported candidate exists.

```rust
use asap_frontend_promql::lower_promql;
use asap_types::types::AccuracyTarget;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pre_asap = lower_promql("sum(latency)", AccuracyTarget::Exact)?;
    println!("{pre_asap:#?}");
    Ok(())
}
```

For SQL, the corresponding signatures are:

```text
async lower_sql(query: &str, catalog: &SqlCatalog, accuracy: AccuracyTarget)
    -> Result<QueryExpr, SqlError>
async lower_sql_dialect(query: &str, catalog: &SqlCatalog,
    dialect: SqlDialect, accuracy: AccuracyTarget) -> Result<QueryExpr, SqlError>
```

| `SqlDialect` value | Current behavior |
| --- | --- |
| `DataFusionSQL` | Default of `lower_sql`; uses DataFusion's supported SQL |
| `ClickhouseSQL` | Supported ClickHouse subset; not all ClickHouse functions |
| `ElasticSQL` | Returns `UnsupportedDialect` |

The catalog is required and describes your tables. For a complete schema-building
example, see [the CLI frontend example](../../crates/devtools/src/bin/show_pre_asap_ir.rs).

## Generate and rank candidates

### API definition

```text
search_workload_with_targets<'s, Id>(
    roots: Vec<(Id, Rc<QueryExpr>, Option<AccuracyTarget>)>,
    strategies: &[Box<dyn ReplacementStrategy + 's>],
    accuracy_model: &dyn AccuracyModel,
) -> PlanSpace<Id>

PlanSpace::cost_sorted(&self, cost_model: &dyn CostModel)
    -> Vec<RankedGroup<'_>>
```

| Argument | Choices / meaning | Required? |
| --- | --- | --- |
| `roots` | One tuple per query: caller ID, canonical IR, and root target | Yes |
| Root target | `Some(AccuracyTarget::…)` applies an explicit end-to-end requirement; `None` adds no explicit root target | Tuple field required; value optional |
| `strategies` | Default factory output or an explicit strategy vector; see option tables below | Yes; even an empty vector does not disable automatic workload strategies |
| `accuracy_model` | `DefaultAccuracyModel` or a custom `AccuracyModel` implementation | Yes |
| Ranking `cost_model` | `DefaultCostModel` or an evidence-backed/custom `CostModel` | Yes |

### Example


The following complete Rust example lowers one query, supplies an explicit root
accuracy target, and prints every ranked candidate instead of selecting a winner.
The default cost model is suitable for inspection, not deployment calibration.

```rust
use std::rc::Rc;
use asap_frontend_promql::lower_promql;
use asap_aware_mapping::{
    default_strategies_with, search_workload_with_targets,
    DefaultAccuracyModel, DefaultCostModel,
};
use asap_types::types::AccuracyTarget;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let accuracy = AccuracyTarget::Epsilon(0.01);
    let root = Rc::new(lower_promql("quantile(0.99, latency)", accuracy.clone())?);
    let cost_model = DefaultCostModel;
    let strategies = default_strategies_with(&cost_model);
    let space = search_workload_with_targets(
        vec![("q1", root, Some(accuracy))],
        &strategies,
        &DefaultAccuracyModel,
    );
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
| `search_workload` | `(query_id, Rc<QueryExpr>)` roots | `PlanSpace` with built-in strategies/model; no explicit per-root target argument |
| `search_workload_with` | Roots, strategy slice | `PlanSpace`; callers choose context-free replacement strategies |
| `search_workload_with_targets` | Roots with optional end-to-end targets, strategies, accuracy model | Candidate space with supplied root-target checks; `None` does not supply a root-level requirement |
| `PlanSpace::cost_sorted` | Cost model | `Vec<RankedGroup>`; retains alternatives and pairs `candidates[i]` with `costs[i]` |
| `PlanSpace::cost_sorted_with_recurrence` | Cost model, recurrence profiles, optional horizon | Ranked groups or `RecurrenceError`; uses recurrence for applicable share/recompute comparisons |
| `SketchAlgorithmStrategy::replacements` through `ReplacementStrategy` | One `TargetSubDAG` | Alternatives at that target; not whole-workload search |

`cost_sorted` is a ranking view, not a request to discard all but the first
candidate. Display costs follow model hooks and may be unavailable/non-finite;
they are not necessarily a globally sortable physical-cost scalar. Unavailable
cost alternatives may remain for explanation. Inspect eligibility and evidence
before physical selection; do not treat their presence as deployment permission.

## Choose strategies and models

### Strategy options

The `strategies` argument takes Rust objects implementing `ReplacementStrategy`,
not string names or a closed enum. These built-in context-free choices can be
combined in one vector; each proposes candidates where its applicability checks
pass. An omitted strategy contributes no proposals of its own.

| Value to put inside `Box::new(...)` | Meaning | In default factories? |
| --- | --- | --- |
| `SketchAlgorithmStrategy::new(&model)` | Enumerates supported exact/sketch implementations and parameter choices for aggregate targets | Yes |
| `HydraGroupingStrategy::new(&model)` | Considers a shared multi-subpopulation structure for supported grouped sketch families, subject to accuracy evidence | Yes |
| `SharedSubtreeStrategy` | Proposes sharing versus independent recomputation at reused subtrees | Yes |
| `SemanticEquivalentRewriteStrategy` | Proposes supported equivalent aggregate rewrites, including decomposing average into sum/count | Yes |
| `ExactCompositionStrategy::new(&model)` | Proposes supported exact operations around summary readouts or in maintenance | Yes |
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
| `default_strategies()` | Exploring with built-in defaults | Built-in cost/accuracy/allocation defaults |
| `default_strategies_with(&model)` | Supplying deployment-specific costing/sizing | Supplied cost model; default accuracy/allocation |
| `default_strategies_with_evidence(&model, &evidence)` | Supplying planning-time accuracy evidence as well | Supplied cost and evidence; default accuracy/allocation |
| Explicit vector | Controlling which context-free strategies are supplied | Models passed into each constructor |

### Example: supply two strategies and run search

```rust
use std::rc::Rc;
use asap_frontend_promql::lower_promql;
use asap_aware_mapping::{
    search_workload_with_targets, DefaultAccuracyModel, DefaultCostModel,
    ReplacementStrategy, SketchAlgorithmStrategy, SharedSubtreeStrategy,
};
use asap_types::types::AccuracyTarget;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let accuracy = AccuracyTarget::Epsilon(0.01);
    let root = Rc::new(lower_promql("quantile(0.99, latency)", accuracy.clone())?);
    let model = DefaultCostModel;
    let strategies: Vec<Box<dyn ReplacementStrategy + '_>> = vec![
        Box::new(SketchAlgorithmStrategy::new(&model)),
        Box::new(SharedSubtreeStrategy),
    ];
    let space = search_workload_with_targets(
        vec![("q1", root, Some(accuracy))], &strategies, &DefaultAccuracyModel,
    );
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
| `&dyn CostModel` | `empirical_cost::EmpiricalCostModel::new(provider)` | Offline sketch-benchmark model: ranks algorithms using matching offline measurements and supplies partial lifecycle costs |
| `&dyn CostModel` | `physical_plan_cost_model::PhysicalPlanCostModel::new(&provider, calibration)?` | Deployment-specific physical-plan model: compares complete physical alternatives using provider evidence and resource calibration; evidence may be offline or online |
| `&dyn AccuracyModel` | `DefaultAccuracyModel` | Built-in guarantee rules and satisfaction checks |
| `&dyn AccuracyBudgetAllocator` | `EqualSplitAllocator` | Built-in allocation of composition accuracy budgets |
| `&dyn AccuracyEvidenceProvider` | `NoAccuracyEvidence` | No extra planning-time statistics; evidence-dependent claims remain unavailable |
| `&dyn AccuracyEvidenceProvider` | `WorkloadAccuracyEvidence { data: &data, now_ms }` | Uses fresh data-workload evidence at the planning time |
| Any provider trait above | Your implementation | Supplies alternative models/evidence under the same contracts |

### Offline measurements versus physical-plan costing

These models differ in scope, not simply in whether they are offline or online.

| Model | Evidence and comparison | Missing evidence / limits |
| --- | --- | --- |
| `EmpiricalCostModel` | Offline sketch benchmarks matched to exact parameters, distribution, environment and validity interval; current algorithm ranking uses measured update CPU nanoseconds | If the measurements required for ranking are incomplete, preserves the incoming algorithm order. Supplies partial build/update lifecycle costs; `estimate_cost()` still uses `DefaultCostModel` structural scores |
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
    DefaultAccuracyModel, DefaultCostModel, EqualSplitAllocator,
    NoAccuracyEvidence, ReplacementStrategy, SketchAlgorithmStrategy,
};

fn main() {
    let cost = DefaultCostModel;
    let accuracy = DefaultAccuracyModel;
    let allocation = EqualSplitAllocator;
    let evidence = NoAccuracyEvidence;
    let strategies: Vec<Box<dyn ReplacementStrategy + '_>> = vec![Box::new(
        SketchAlgorithmStrategy::with_models_and_evidence(
            &cost, &accuracy, &allocation, &evidence,
        ),
    )];
    // Use &strategies and &accuracy in search_workload_with_targets.
    println!("{} explicitly configured strategy", strategies.len());
}
```

Constructor definition:

```text
SketchAlgorithmStrategy::with_models_and_evidence(
    cost_model: &dyn CostModel,
    accuracy_model: &dyn AccuracyModel,
    allocator: &dyn AccuracyBudgetAllocator,
    evidence: &dyn AccuracyEvidenceProvider,
) -> SketchAlgorithmStrategy
```

All provider arguments are required for this constructor. They must outlive the
strategy vector. `SketchAlgorithmStrategy::new(&cost_model)` is the shorter
constructor using default accuracy/allocation and no extra evidence.

| Extension point | What it controls | What it cannot establish alone |
| --- | --- | --- |
| `ReplacementStrategy` | Proposed semantic alternatives | Permission to violate query semantics or downstream support |
| `CostModel` | Candidate ordering/sizing hooks, recurrence/lifecycle and complete-cost evidence hooks | Correctness, measured costs without evidence, or installed runtime support |
| `AccuracyModel` | Derivation, propagation and satisfaction of guarantees | A meaningful guarantee without its required assumptions/evidence |
| `AccuracyBudgetAllocator` | Local accuracy requirements proposed within composition | End-to-end correctness without subsequent validation |
| `AccuracyEvidenceProvider` | Planning-time statistics used by supported strategies | Authority to change query requirements |

Models may be consumed during generation as well as ranking. Construct strategies
with the intended model/evidence; replacing only the final sorting model does not
regenerate parameter choices. For evidence-aware defaults, use
`asap_aware_mapping::replacement::default_strategies_with_evidence`.
For custom accuracy/allocation/evidence on sketches,
`SketchAlgorithmStrategy::with_models_and_evidence` exposes these providers.
Keep each provider's evidence scope and freshness valid for the query population.

## Workload inputs and defaults

`QueryWorkload` holds language, optional batch/repeating entries, and optional
`DataWorkload`. Entries carry requirements, predictability, recurrence and time
selection. These facts are separate: repeated queries can read data at rest.
`WorkloadDemand` associates a target with the relevant workload entry indices.

| Type/input | Current behavior | Caller responsibility |
| --- | --- | --- |
| `QueryRequirements::default()` | `ImplicitExact`, unspecified response latency | Pass approximation explicitly and thread per-root requirements into search |
| `DataWorkload::default()` | Unknown arrival, unknown evidence | Supply facts needed for the requested comparisons |
| `Evidence<T>::default()` | No value, unknown source | Unknown/stale evidence is not zero; provide scoped valid observations |
| `DefaultCostModel` | Built-in ordering/sizing and structural cost hooks | Supply deployment evidence for calibrated comparisons |
| `SummaryMaintenanceLifecycleCostInputs::default()` | All primitive costs unknown | Implement the required lifecycle cost hooks; structural defaults are insufficient |
| `horizon: None` in lifecycle planning | Horizon-dependent alternatives are unselectable | Supply a positive horizon when comparing rates/amortized reuse |
| Lifecycle capabilities default | All four modes enabled | Override with the actual runtime support |
| Per-summary maintenance capabilities default | Incremental update, merge, delete all false | Advertise supported operations for the concrete state representation |

`Default` is a Rust constructor contract, not a general serde omission rule.
Several workload fields require explicit serialized values. A struct field being
optional also does not guarantee every planning operation can succeed without it.

## Lifecycle and capabilities

Two capabilities are distinct: the runtime can orchestrate a lifecycle, and the
chosen summary representation supports the required state operations. Both must
hold. Workload legality and known cost evidence can further restrict alternatives.

### API definition and options

```text
global_selection_with_summary_maintenance_lifecycles<'a, Id>(
    space: &'a PlanSpace<Id>, workload: &QueryWorkload,
    root_workload_entries: &[usize], now_ms: u64, horizon: Option<Horizon>,
    capabilities: SummaryMaintenanceLifecycleCapabilities, cost_model: &dyn CostModel,
) -> Result<GlobalSelection<'a>, SummaryMaintenanceLifecycleSelectionError>

materialize_with_summary_maintenance_lifecycles(
    selection: &GlobalSelection<'_>, target: &Rc<QueryExpr>,
    demand: WorkloadDemand<'_>, now_ms: u64, horizon: Option<Horizon>,
    capabilities: SummaryMaintenanceLifecycleCapabilities, cost_model: &dyn CostModel,
) -> Result<Option<SummaryMaintenanceLifecyclePlan>, MaterializeSummaryMaintenanceLifecycleError>
```

| Argument | Values / requirements |
| --- | --- |
| `space`, `workload` | Actual candidate space and its workload; keep their root associations |
| `root_workload_entries` | One normalized workload entry index for each `space.roots` entry |
| `target` | A root from `space.roots`, after canonical sharing |
| `demand` | `WorkloadDemand::new(&workload, &indices)` for the entries consuming that target |
| `now_ms` | Actual planning time in Unix milliseconds for evidence freshness |
| `horizon` | `Some(Horizon(seconds))` with positive finite seconds, or `None` when horizon-dependent comparisons are unavailable |
| `capabilities` | Explicit Boolean fields below; several may be true |
| `cost_model` | A model supplying required lifecycle and raw-comparison evidence; default structural estimates are not enough |

| Capability field | `true` permits consideration of… | `false` means… |
| --- | --- | --- |
| `supports_ephemeral` | Fresh build per invocation, retired afterward | Exclude that lifecycle |
| `supports_prepared` | Build before a predictable execution and retain until it | Exclude that lifecycle |
| `supports_shared` | Retain state for multiple reads | Exclude that lifecycle |
| `supports_continuously_maintained` | Keep state current as updates arrive | Exclude that lifecycle |

All flags default to true; integrations should pass real support. Enabling a
flag does not override workload, algorithm-operation or evidence checks.

### Example: lifecycle-aware planning for a batch-only runtime

This helper takes the real workload and cost provider from your application.
It supports one searched root mapped to one workload entry, and returns a typed
plan/error rather than making up costs. For a shared root consumed by several
entries, construct demand using all applicable indices.

```rust
use asap_aware_mapping::{
    global_selection_with_summary_maintenance_lifecycles,
    materialize_with_summary_maintenance_lifecycles, CostModel, Horizon, PlanSpace,
    SummaryMaintenanceLifecycleCapabilities, SummaryMaintenanceLifecyclePlan,
    WorkloadDemand,
};
use asap_types::workload::QueryWorkload;

fn plan_batch_root(
    space: &PlanSpace<&str>,
    workload: &QueryWorkload,
    entry_index: usize,
    now_ms: u64,
    horizon: Option<Horizon>,
    model: &dyn CostModel,
) -> Result<Option<SummaryMaintenanceLifecyclePlan>, Box<dyn std::error::Error>> {
    if space.roots.len() != 1 {
        return Err("this example requires exactly one root".into());
    }
    let capabilities = SummaryMaintenanceLifecycleCapabilities {
        supports_ephemeral: true,
        supports_prepared: false,
        supports_shared: false,
        supports_continuously_maintained: false,
    };
    let indices = [entry_index];
    let selection = global_selection_with_summary_maintenance_lifecycles(
        space, workload, &indices, now_ms, horizon, capabilities, model,
    )?;
    let plan = materialize_with_summary_maintenance_lifecycles(
        &selection, &space.roots[0].1, WorkloadDemand::new(workload, &indices),
        now_ms, horizon, capabilities, model,
    )?;
    if let Some(plan) = &plan {
        println!("raw_recompute={}, deployments={:#?}",
            plan.selected_raw_recompute, plan.deployments);
    }
    Ok(plan)
}
```

Use this helper with the `space` built by the search example and the corresponding
workload/provider. No incremental lifecycle is permitted, but unknown evidence
can still prevent choosing summary state. If only one legal alternative remains,
recording it is a complete lifecycle decision. Data-at-rest alone does not imply
that prepared or retained shared state is supported.

| Function | Inputs | Output / promise |
| --- | --- | --- |
| `plan_summary_maintenance_lifecycles` | Materialized semantic root, `WorkloadDemand`, `now_ms`, optional horizon, runtime capabilities, cost model | `Result<SummaryMaintenanceLifecyclePlan, …>` for that fixed root; does not revisit all semantic candidates |
| `global_selection_with_summary_maintenance_lifecycles` | `PlanSpace`, workload/root-entry associations, time, horizon, capabilities, cost model | Lifecycle-aware compatible selection/error, using eligible cost evidence |
| `materialize_with_summary_maintenance_lifecycles` | Selection, target root and lifecycle context | Optional lifecycle plan/error; attaches state deployment decisions |

Inspect `deployments`, their selected lifecycle/alternatives/rejections,
`selected_raw_recompute`, and optional summary/raw costs. Success of a function
call alone is not a certificate that every desired summary was selected or fully
costed. A raw alternative remains a downstream execution obligation.

Lifecycle feasibility and costs must affect final deployment comparison. Running
lifecycle analysis after structural selection can evaluate the selected root,
but does not make the earlier selection lifecycle-optimal. An application may
consume ranked candidates and perform this comparison downstream instead.

## Optional whole-plan selection and materialization

| Method on `PlanSpace` / `GlobalSelection` | Behavior |
| --- | --- |
| `PlanSpace::global_selection(&model)` | Compatible structural selection across groups; no recurrence or lifecycle planning implied |
| `PlanSpace::global_selection_with_recurrence(...)` | Compatible selection using supplied recurrence profiles/horizon; no lifecycle commitments implied |
| `GlobalSelection::materialize(&target)` | `Result<Option<Rc<SummaryNode>>, ImplementError>`; constructs semantic IR, not stored summary data |

Use a target associated with the searched space; materialization can return `None`
when that target is absent. A downstream integration can use these convenience
APIs when its supplied model/evidence supports the intended comparison. Neither
plain structural selection nor taking each group's first candidate substitutes
for checking complete physical alternatives and deployment constraints.

### API definition and example

```text
PlanSpace::global_selection(&self, cost_model: &dyn CostModel) -> GlobalSelection<'_>
GlobalSelection::materialize(&self, target: &Rc<QueryExpr>)
    -> Result<Option<Rc<SummaryNode>>, ImplementError>
```

For structural inspection only, this complete example selects a semantic root
and exports its inspection graph. It performs no lifecycle or deployment planning.
Use lifecycle-aware selection above when the comparison needs those decisions.

```rust
use std::rc::Rc;
use asap_frontend_promql::lower_promql;
use asap_aware_mapping::{search_workload, DefaultCostModel};
use asap_types::types::AccuracyTarget;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = Rc::new(lower_promql("sum(latency)", AccuracyTarget::Exact)?);
    let space = search_workload(vec![("q1", root)]);
    let selection = space.global_selection(&DefaultCostModel);
    // Search may canonicalize roots; use the root returned by PlanSpace.
    if let Some(summary) = selection.materialize(&space.roots[0].1)? {
        let graph = asap_types::dag_export::export_summary(&summary);
        println!("{graph:#?}");
    }
    Ok(())
}
```

## Export and explain

| Function/type | Purpose |
| --- | --- |
| `asap_types::dag_export::export(&query)` | Pre-ASAP inspection graph |
| `asap_types::dag_export::export_summary(&summary)` | Post-ASAP inspection graph |
| `asap_types::post_asap::compile_executable_dag(&root)` | Compile a semantic DAG with execution-data-state validation; not a physical plan |
| `PostAsapDagDocument::new(dag)` and `.validate()` | Versioned semantic envelope and explicit validation; constructing it alone does not validate |
| `asap_aware_mapping::export_summary_maintenance_plan(&plan)` | Graph plus lifecycle deployments, alternatives and available cost/guarantee information |
| `explain_replacements` / `explain_replacements_with` | Findings from default/custom-strategy search; not a complete physical feasibility report |

Choose the export matching your intended handoff: an inspection graph is not
interchangeable with a versioned execution contract. Preserve lifecycle and
cost/guarantee evidence needed downstream instead of exporting only a bare DAG.
For public symbol details, build local API documentation with:

```sh
cargo doc -p asap-aware-mapping -p asap-types --no-deps
```

## Source references

- [Frontend PromQL](../../crates/frontend-promql/src/lib.rs), [SQL](../../crates/frontend-sql/src/lib.rs), [MetricsQL](../../crates/frontend-metricsql/src/lib.rs)
- [Search, ranking and selection](../../crates/asap-aware-mapping/src/replacement.rs)
- [Cost models](../../crates/asap-aware-mapping/src/cost_model.rs)
- [Lifecycle APIs](../../crates/asap-aware-mapping/src/summary_maintenance_lifecycle.rs)
- [Workload types](../../crates/types/src/workload.rs)
- [Downstream boundary](../design_docs/asapplanner-downstream-boundary.md)
