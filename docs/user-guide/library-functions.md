# Public library functions

Audience: developers embedding ASAPPlanner or adding strategies/models. This is
a compact reference for the public workflow APIs at revision `e7fdb24`, not an
exhaustive symbol reference. The [user guide](user-guide.md) explains which exit
point to choose; the [design overview](../design_docs/README.md) defines ownership.

ASAPPlanner's primary output is `PlanSpace` plus ranked legal candidates.
Downstream owns physical binding and commitment. Selection/materialization helpers
do not deploy a plan, and a serializable DAG is not evidence of runtime readiness.

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

## Generate and rank candidates

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

`default_strategies()` creates the built-in set. `default_strategies_with(model)`
constructs a model-aware set; the sets are not guaranteed to be identical apart
from their cost model (the current rewrite strategy differs). For exact control
over the supplied context-free strategies, construct a slice explicitly:

```rust
use asap_aware_mapping::{
    DefaultCostModel, ReplacementStrategy, SketchAlgorithmStrategy,
    SharedSubtreeStrategy,
};
let model = DefaultCostModel;
let strategies: Vec<Box<dyn ReplacementStrategy + '_>> = vec![
    Box::new(SketchAlgorithmStrategy::new(&model)),
    Box::new(SharedSubtreeStrategy),
];
// Pass &strategies to search_workload_with[_targets].
```

This restricts supplied strategies; it is not a switch disabling all other search
behavior. Workload search still performs canonical sharing/CSE and automatically
derives workload-dependent rollup. There is currently no single public policy
object that toggles every internal pass. Omitting a strategy may remove useful
candidates but must not waive legality or accuracy requirements.

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

For a runtime that can build fresh state for each invocation and retire it, but
cannot prepare, retain for reuse, or incrementally maintain state:

```rust
use asap_aware_mapping::SummaryMaintenanceLifecycleCapabilities;
let capabilities = SummaryMaintenanceLifecycleCapabilities {
    supports_ephemeral: true,
    supports_prepared: false,
    supports_shared: false,
    supports_continuously_maintained: false,
};
```

Pass this value to the lifecycle functions along with real demand and a model.
It is meaningful input, not a dummy argument. Data-at-rest alone does not determine
whether preparation or retained reuse is supported. A singleton legal alternative
can be validated and recorded without a meaningful search; unknown cost inputs
still prevent unsupported cost claims.

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
