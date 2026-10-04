# Public library functions

Audience: developers embedding ASAPPlanner or adding strategies/models. This is
a compact reference for the public workflow APIs, not an
exhaustive symbol reference. The [CLI guide](../user_guide_docs/run-a-query.md) covers command-line inspection; the [design overview](../design_docs/architecture/README.md) defines ownership.

ASAPPlanner's primary output is `CandidateLogicalASAPDAGs`; ranking is a view over its candidates.
Downstream owns physical binding and commitment. Selection/DAG assembly helpers
do not deploy a plan, and a serializable DAG is not evidence of runtime readiness.

## Choose a library workflow

| Desired result | Calls | Example |
| --- | --- | --- |
| Pre-ASAP IR | Frontend `lower_*` | [Lower a query](#lower-a-query-into-pre-asap-ir) |
| All ranked candidates | `search_workload_with_targets` -> `cost_sorted` | [Generate and rank](#generate-and-rank-candidates) |
| Custom optimization set | Construct `Vec<Box<dyn ReplacementStrategy>>`, then search | [Strategies and models](#choose-strategies-and-models) |
| Selected semantic DAG / export | `global_selection` -> `assemble_selected_dag` -> export | [Selection example](#optional-whole-plan-selection-and-dag-assembly) |

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
example, see [the CLI frontend example](../../crates/devtools/src/bin/show_pre_asap_ir.rs).

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
) -> CandidateLogicalASAPDAGs<Id>

candidate_selection::cost_sorted<'a, Id>(space: &'a CandidateLogicalASAPDAGs<Id>, cost_model: &dyn CostModel)
    -> Vec<RankedTargetSubDAGCandidates<'a>>
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
    AccuracyRequirement, BatchEntry, DataWorkload, DurationMs, Evidence, Query,
    PlanningWorkload, QueryLanguage, QueryRequirements, QueryWorkload,
};
use asap_aware_mapping::plan_selection::candidate_selection::cost_sorted;
use asap_aware_mapping::{
    default_strategies, search_workload_with_targets,
    DefaultAccuracyModel, DefaultCostModel,
};
use asap_types::types::AccuracyTarget;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let accuracy = AccuracyTarget::Epsilon(0.01);
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
            query: Query("quantile(0.99, latency)".into()),
            requirements: QueryRequirements {
                accuracy: AccuracyRequirement::Explicit(accuracy.clone()),
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
    let root = lower_promql_workload(&workload, 0)?.remove(0);
    let cost_model = DefaultCostModel;
    let strategies = default_strategies();
    let space = search_workload_with_targets(
        vec![("q1", root, Some(accuracy))],
        &strategies,
        &DefaultAccuracyModel,
    );
    for group in cost_sorted(&space, &cost_model) {
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
| `candidate_selection::cost_sorted` | Cost model | `Vec<RankedTargetSubDAGCandidates>`; retains alternatives and pairs `candidates[i]` with `costs[i]` |
| `candidate_selection::cost_sorted_with_recurrence` | Cost model, recurrence profiles, optional horizon | Ranked per-target candidate sets or `RecurrenceError`; uses recurrence for applicable share/recompute comparisons |
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
comes from a `MaterializationAssignment` (all query time until Stage 2
materialization, #509, decides otherwise).

## Choose strategies and models

### Strategy options

The `strategies` argument takes Rust objects implementing `ReplacementStrategy`,
not string names or a closed enum. These built-in context-free choices can be
combined in one vector; each proposes candidates where its applicability checks
pass. An omitted strategy contributes no proposals of its own.

| Value to put inside `Box::new(...)` | Meaning | In default factories? |
| --- | --- | --- |
| `ASAPStrategies::default()` | Enumerates supported exact/sketch implementations and parameter choices for aggregate targets | Yes |
| `HydraGroupingStrategy::default()` | Considers a shared multi-subpopulation structure for supported grouped sketch families, subject to accuracy evidence | Yes |
| `SharedSubDAGStrategy` | Proposes sharing versus independent recomputation at reused sub-DAGs | Yes |
| `SemanticEquivalentRewriteStrategy` | Proposes supported equivalent aggregate rewrites, including decomposing average into sum/count | Yes |
| `ExactCompositionStrategy` | Proposes exact operations around summary evaluations or in maintenance; not filtered by runtime support; `global_selection` commits one only with positive (`Some(true)`) cost-model support evidence | Yes |
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
replacement::default_strategies_with_evidence<'a>(
    evidence: &'a dyn AccuracyEvidenceProvider,
) -> Vec<Box<dyn ReplacementStrategy + 'a>>
```

| Factory | Use when | Models used |
| --- | --- | --- |
| `default_strategies()` | Exploring with built-in defaults | Built-in accuracy/allocation defaults; no extra evidence |
| `default_strategies_with_evidence(&evidence)` | Supplying planning-time accuracy evidence | Supplied evidence; default accuracy/allocation |
| Explicit vector | Controlling which context-free strategies are supplied | Models passed into each constructor |

No factory takes a cost model: candidate generation is cost-model independent.
Pass the deployment cost model to `cost_sorted`/`global_selection`.

### Example: supply two strategies and run search

```rust
use asap_frontend_promql::lower_promql_workload;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataWorkload, DurationMs, Evidence, Query,
    PlanningWorkload, QueryLanguage, QueryRequirements, QueryWorkload,
};
use asap_aware_mapping::plan_selection::candidate_selection::cost_sorted;
use asap_aware_mapping::{
    search_workload_with_targets, DefaultAccuracyModel, DefaultCostModel,
    ReplacementStrategy, ASAPStrategies, SharedSubDAGStrategy,
};
use asap_types::types::AccuracyTarget;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let accuracy = AccuracyTarget::Epsilon(0.01);
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
            query: Query("quantile(0.99, latency)".into()),
            requirements: QueryRequirements {
                accuracy: AccuracyRequirement::Explicit(accuracy.clone()),
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
    let root = lower_promql_workload(&workload, 0)?.remove(0);
    let model = DefaultCostModel;
    let strategies: Vec<Box<dyn ReplacementStrategy + '_>> = vec![
        Box::new(ASAPStrategies::default()),
        Box::new(SharedSubDAGStrategy),
    ];
    let space = search_workload_with_targets(
        vec![("q1", root, Some(accuracy))], &strategies, &DefaultAccuracyModel,
    );
    println!("{:#?}", cost_sorted(&space, &model));
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
| `&dyn CostModel` | `DefaultCostModel` | Built-in ordering and structural estimates; no measured deployment guarantee |
| `&dyn CostModel` | `empirical_cost::EmpiricalCostModel::new(provider)` | Offline sketch-benchmark model: ranks algorithms using matching offline measurements |
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
    DefaultAccuracyModel, EqualSplitAllocator,
    NoAccuracyEvidence, ReplacementStrategy, ASAPStrategies,
};

fn main() {
    let accuracy = DefaultAccuracyModel;
    let allocation = EqualSplitAllocator;
    let evidence = NoAccuracyEvidence;
    let strategies: Vec<Box<dyn ReplacementStrategy + '_>> = vec![Box::new(
        ASAPStrategies::new_with_planning_inputs_and_evidence(
            &accuracy, &allocation, &evidence,
        ),
    )];
    // Use &strategies and &accuracy in search_workload_with_targets.
    println!("{} explicitly configured strategy", strategies.len());
}
```

Constructor definition:

```text
ASAPStrategies::new_with_planning_inputs_and_evidence(
    accuracy_model: &dyn AccuracyModel,
    allocator: &dyn AccuracyBudgetAllocator,
    evidence: &dyn AccuracyEvidenceProvider,
) -> ASAPStrategies
```

All provider arguments are required for this constructor. They must outlive the
strategy vector. `ASAPStrategies::default()` uses default accuracy/allocation
and no extra evidence.

| Extension point | What it controls | What it cannot establish alone |
| --- | --- | --- |
| `ReplacementStrategy` | Proposed semantic alternatives | Permission to violate query semantics or downstream support |
| `CostModel` | Selection-time ranking, cost, support-evidence and recurrence cost hooks | Correctness, measured costs without evidence, or installed runtime support |
| `AccuracyModel` | Derivation, propagation and satisfaction of guarantees | A meaningful guarantee without its required assumptions/evidence |
| `AccuracyBudgetAllocator` | Local accuracy requirements proposed within composition | End-to-end correctness without subsequent validation |
| `AccuracyEvidenceProvider` | Planning-time statistics used by supported strategies | Authority to change query requirements |

Accuracy models, allocators and evidence are consumed during generation; the cost
model is consumed only at selection (`cost_sorted`, `global_selection` and their
`_with_recurrence` variants). Sketch parameters come from the analytical
estimators, not the cost model. For evidence-aware defaults, use
`asap_aware_mapping::replacement::default_strategies_with_evidence`.
For custom accuracy/allocation/evidence on sketches,
`ASAPStrategies::new_with_planning_inputs_and_evidence` exposes these providers.
Keep each provider's evidence scope and freshness valid for the query population.

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
| `QueryRequirements::default()` | `ImplicitExact`, unspecified response latency | Pass approximation explicitly and thread per-root requirements into search |
| `DataWorkload::default()` | Unknown arrival, unknown evidence | Supply facts needed for the requested comparisons |
| `Evidence<T>::default()` | No value, unknown source | Unknown/stale evidence is not zero; provide scoped valid observations |
| `DefaultCostModel` | Built-in ordering and structural cost hooks | Supply deployment evidence for calibrated comparisons |

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
physical deployment feasibility. Stage 2 materialization (#509) will own
materialization; downstream still owns physical commitment.

| Function or method | Behavior |
| --- | --- |
| `candidate_selection::global_selection(&space, &model)` | Compatible structural selection across targets; no recurrence or materialization planning implied |
| `candidate_selection::global_selection_with_recurrence(...)` | Compatible selection using supplied recurrence profiles/horizon; no materialization commitments implied |
| `GlobalSelection::assemble_selected_dag(&target)` | `Result<Option<Rc<OperatorNode>>, RealizationError>`; constructs untimed semantic IR, not stored summary data |

Use a target associated with the searched space; DAG assembly can return `None`
when that target is absent. A downstream integration can use these convenience
APIs when its supplied model/evidence supports the intended comparison. Neither
plain structural selection nor taking each target's first candidate substitutes
for checking complete physical alternatives and deployment constraints.

### API definition and example

```text
candidate_selection::global_selection<'a, Id>(space: &'a CandidateLogicalASAPDAGs<Id>, cost_model: &dyn CostModel)
    -> CostedGlobalSelection<'a>  // derefs to GlobalSelection
GlobalSelection::assemble_selected_dag(&self, target: &Rc<OperatorNode>)
    -> Result<Option<Rc<OperatorNode>>, RealizationError>
```

For structural inspection only, this complete example selects a semantic root
and exports its inspection DAG. It performs no materialization or deployment
planning.

```rust
use asap_frontend_promql::lower_promql_workload;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataWorkload, DurationMs, Evidence, Query,
    PlanningWorkload, QueryLanguage, QueryRequirements, QueryWorkload,
};
use asap_aware_mapping::plan_selection::candidate_selection::global_selection;
use asap_aware_mapping::{search_workload, DefaultCostModel};
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
    let root = lower_promql_workload(&workload, 0)?.remove(0);
    let space = search_workload(vec![("q1", root)]);
    let selection = global_selection(&space, &DefaultCostModel);
    // Search may canonicalize roots; use the root returned by CandidateLogicalASAPDAGs.
    if let Some(summary) = selection.assemble_selected_dag(&space.roots[0].1)? {
        let dag = asap_types::dag_export::export_summary(&summary);
        println!("{dag:#?}");
    }
    Ok(())
}
```

## Export and explain

| Function/type | Purpose |
| --- | --- |
| `asap_types::dag_export::export(&query)` | Pre-ASAP inspection dag |
| `asap_types::dag_export::export_summary(&summary)` | Post-ASAP inspection dag |
| `asap_types::ir::apply_materialization_timings(&root, &assignment, &mut TimingMemo::new())` | Write execution timing into every node from a `MaterializationAssignment` (default: all query time) and validate the data-state edges; `PlanOutput::execution_timed_dag()` applies the default to a planned workload |
| `asap_types::ir::export::compile_post_asap_dag(&timed_root)` | Export a timed DAG as a `PostAsapDAG` (wire version 7); rejects an untimed node; not a physical plan |
| `PostAsapDAGDocument::new(dag)` and `.validate()` | Versioned semantic envelope and explicit validation; constructing it alone does not validate |
| `explain_replacements` / `explain_replacements_with` | Findings from default/custom-strategy search; not a complete physical feasibility report |

Choose the export matching your intended handoff: an inspection DAG is not
interchangeable with a versioned execution contract. Preserve
cost/guarantee evidence needed downstream instead of exporting only a bare DAG.
For public symbol details, build local API documentation with:

```sh
cargo doc -p asap-aware-mapping -p asap-types --no-deps
```

## Source references

- [Frontend PromQL](../../crates/frontend-promql/src/lib.rs), [SQL](../../crates/frontend-sql/src/lib.rs), [MetricsQL](../../crates/frontend-metricsql/src/lib.rs)
- [Search, ranking and selection](../../crates/asap-aware-mapping/src/replacement.rs)
- [Cost models](../../crates/asap-aware-mapping/src/cost_model.rs)
- [Workload types](../../crates/types/src/workload/mod.rs)
- [Planner-runtime contract](../design_docs/architecture/planner-runtime-contract.md)
