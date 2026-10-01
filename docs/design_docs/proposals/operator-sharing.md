# Sharing Operators Between Pre-ASAP IR and Post-ASAP IR

> Status: proposal, not implemented. Audience: planner developers and designers.
> Addresses [#468](https://github.com/ProjectASAP/ASAPPlanner/issues/468) and builds on
> [Decoupling operators from scalar expressions](decoupling_op_and_expr.md).
> Code references use `main` at `5a32b8b` (after #472, #478 and #510).

Use one operator language before and after ASAP optimization. Relational operators
and summary operators can be children of each other, without `KeepPreAsap` wrappers
or duplicate relational variants.

`Operator` has two categories: `NonASAP(NonASAPOp)` and `ASAP(ASAPOp)`. This separates
relational and summary operations while giving both the same child type,
`Rc<Operator>`. Frontend inputs must contain only `NonASAP` nodes (§3).

Guarantees are derived from the DAG. Execution timings come from an explicit
lifecycle assignment and are validated before export (§2).

```
Today                                            Proposed
ValueOperation(Project)         ← a copy         NonASAP(Project)
  SummaryEstimate                                  ASAP(SummaryEstimate)
    SummaryAgg(Kll)                                  ASAP(SummaryAgg(Kll))
      KeepPreAsap(Scan lineitem) ← a black box         NonASAP(Scan lineitem)
```

| Part | Sections |
|---|---|
| I. New IR | §1 Types, §2 Schema, guarantee, and timing |
| II. Changes, in data-flow order | §3 Entry → §4 Planner → §5 Timing → §6 Export → §7 Other consumers |
| III. Implementation | §8 Stages and tests, §9 Out of scope, §10 Open questions |

---

# I. New IR

## 1. Types

### 1.1 Unified `Operator` type

Common operations—children, schema, guarantee and timing—are methods on `Operator`.
Each enum branch implements them for its category; variant-specific data, such as
`Aggregate.measures` and `SummaryAgg.family`, stays on the variant.

```rust
pub enum Operator<C: ColState = ColumnId> {
    NonASAP(NonASAPOp<C>),     // today's relational and timeseries operators in `QueryExpr` (§1.2)
    ASAP(ASAPOp<C>),           // summary operators (§1.3)
}

impl<C: ColState> Operator<C> {   // implemented for every operator
    pub fn children(&self) -> Vec<&Rc<Operator<C>>>;
    pub fn map_children(&self, f: impl FnMut(&Rc<Operator<C>>) -> Rc<Operator<C>>) -> Self;
    pub fn output_schema(&self) -> Result<Schema, SchemaError>;  // schema: §2.1
    pub fn guarantee(&self) -> &Slot<Option<ResultGuarantee>>;   // accuracy guarantee: §2.2
    pub fn timing(&self) -> &Slot<ExecutionTiming>;              // execution timing: §2.3
    pub fn with_guarantee(&self, guarantee: Option<ResultGuarantee>) -> Self;
    pub fn with_timing(&self, timing: ExecutionTiming) -> Self;
}

/// `Slot` represents a value that may be unset or set.
/// `guarantee` is filled by derivation (§2.2), `timing` by applying a lifecycle
/// assignment (§2.3). Both are `Unset` on a freshly built node.
pub enum Slot<T> { Unset, Set(T) }

/// Memo of one pass over a workload, keyed by node pointer.
/// One is shared by every root of a workload, so a node shared by two roots stays one `Rc`.
pub struct DerivationMemo { .. }

/// Build the accuracy guarantee of one DAG root by derivation
pub fn derive_guarantees(
  root: &Rc<Operator>,
  model: &dyn AccuracyModel,                // accuracy model used for derivation
  evidence: &dyn AccuracyEvidenceProvider,  // evidence provider used for derivation
  memo: &mut DerivationMemo,
) -> Result<Rc<Operator>, AccuracyError>;
/// Write the timings of one lifecycle assignment into the `timing` slots of one DAG
/// root, top-down, then validate them (§5). Summary materialization chooses the
/// assignment (§2.3); nothing is inferred from operator kinds. A shared node reached
/// with two different timings is an error (§4).
pub fn apply_lifecycle_timings(
  root: &Rc<Operator>,
  assignment: &LifecycleAssignment,   // per-node timings, expanded from per-state lifecycle choices (§10)
  memo: &mut DerivationMemo,
) -> Result<Rc<Operator>, ExecutionDataStateError>;
```

- **Immutable passes:** both passes return a new DAG and preserve sharing with one
  memo per pass across all workload roots. Lifecycle planning uses the
  guarantee-derived DAG; export uses the timed DAG. Earlier pointers do not identify
  nodes in those results.
- **Recomputation:** `derive_guarantees` fills guarantee slots from local evidence
  and child guarantees. `apply_lifecycle_timings` overwrites timing slots from the
  assignment. Run rewrites before these passes.
- **Equality:** timing participates in `PartialEq` and `Hash`; derived guarantees do
  not. Equal subtrees derive equal guarantees under the same accuracy model and
  evidence. `ResultGuarantee` contains `f64` and has no `Hash`.

```text
Operator<C>
├─ NonASAP(NonASAPOp<C>)
│   ├─ children:   Rc<Operator<C>>                  → back to Operator<C>: NonASAP or ASAP
│   ├─ timing / guarantee
│   └─ scalar expressions: Predicate<C> / ProjectItem<C> / SortKey<C> / ScalarBridge / ...
│                              └─ ScalarExpr<C>: never contains an Operator
└─ ASAP(ASAPOp<C>)
    ├─ children:   Rc<Operator<C>>                  → back to Operator<C>: NonASAP or ASAP
    └─ timing / guarantee
```

### 1.2 `NonASAPOp`

`NonASAPOp` contains the operator variants split from `QueryExpr` in the
[decoupling proposal](decoupling_op_and_expr.md#2-types).

```rust
pub enum NonASAPOp<C: ColState = ColumnId> {
    Scan          { .. },
    Filter        { pred: Predicate<C>, child: Rc<Operator<C>> },
    Project       { cols: Vec<ProjectItem<C>>, child: Rc<Operator<C>> },
    Aggregate     { reduction, measures, having: Option<Predicate<C>>, child: Rc<Operator<C>> },
    Join          { kind, pred: Predicate<C>, left: Rc<Operator<C>>, right: Rc<Operator<C>> },
    SetOp         { kind, all, left: Rc<Operator<C>>, right: Rc<Operator<C>> },
    Concat        { children: Vec<Rc<Operator<C>>> },
    Sort          { keys: Vec<SortKey<C>>, child: Rc<Operator<C>> },
    Limit         { n, offset, child: Rc<Operator<C>> },
    BinaryOp      { op, lhs, rhs },
    SQLWindowFunc { args: Vec<ScalarExpr<C>>, order_by: Vec<SortKey<C>>, child: Rc<Operator<C>>, .. },
    Dedup { .. }, TimeRange { .. }, TimeShift { .. }, Promql* { .. },
    ScalarBridge(ScalarExpr<C>),       // the `2` in PromQL `v * 2`
    EvalTimestamp,                     // PromQL time()
}
```

Every variant also carries the `timing` and `guarantee` slots (§1.1), omitted above.

### 1.3 `ASAPOp`

`ASAPOp` contains today's summary variants from `SummaryExpr` and the
summary-specific variants of `ValueOperation`.

```rust
pub enum ASAPOp<C: ColState = ColumnId> {
    SummaryAgg      { child: Rc<Operator<C>>, family: ASAPType, input, reduction, grouping,
                      exact_rule: Option<CompositionOperator> },
    SummaryEstimate { child: Rc<Operator<C>>, query: SketchQuery,
                      local_guarantee: Option<ResultGuarantee> },
    SummaryMerge    { children: Vec<Rc<Operator<C>>> },
    SummarySubtract { left: Rc<Operator<C>>, right: Rc<Operator<C>> },
    SummaryDelete   { child: Rc<Operator<C>>, key: C },
    SummaryJoin     { outer: Rc<Operator<C>>, inner: Rc<Operator<C>>, key: C, family: ASAPType },
    FinalizeExactAccumulator { child: Rc<Operator<C>> },
    MaintainPopulation       { child: Rc<Operator<C>>, population },
    ReadPopulation           { child: Rc<Operator<C>>, readout },
    Extension                { child: Rc<Operator<C>>, name: String },
}
```

Every variant also carries the `timing` and `guarantee` slots (§1.1), omitted above.

`SummaryMerge`, `SummarySubtract`, `SummaryDelete`, `SummaryJoin` and `Extension`
are currently built only in tests. Migrate their variants, but return `Unimplemented`
from schema, guarantee, timing and export operations.

Legacy types map to the new IR as follows:

| Legacy types | Expressed as |
|---|---|
| `SummaryExpr::KeepPreAsap(q)` | `q` itself, an `NonASAP(..)` subtree |
| `ValueOperation::{Project, Filter, Sort, Limit}` | `NonASAPOp::{Project, Filter, Sort, Limit}` |
| `SummaryExpr::{BinaryOp, RelationalJoin}` | `NonASAPOp::{BinaryOp, Join}` |
| `ValueOperation::Exact(Aggregate)`, `ExactOperation` | `NonASAPOp::Aggregate` |
| `SummaryNode` | `Operator` itself: `schema` is computed, `timing` / `guarantee` are slots on every variant (§2) |

### 1.4 Child field

Non-ASAP operators now sit on the same level as ASAP operators, so their children must
be `Rc<Operator>` to allow free placement:

```rust
// After the decoupling doc                      // After this proposal
Filter { pred: Predicate(ScalarExpr),            Filter { pred: Predicate(ScalarExpr),
         child: Rc<NonASAPOp> }                           child: Rc<Operator> }   // NonASAP(..) or ASAP(SummaryEstimate ..)
```

Now an original operator can also sit on ASAP operators, e.g. a `SetOp` sitting on two `SummaryEstimate` operators.

`Concat.children` is `Vec<QueryExpr>` today: branches are stored by value and have no `Rc` identity, so the planner (§4), which identifies targets by pointer,
cannot replace a branch — e.g. the branches of SQL `ROLLUP` or PromQL `histogram_quantiles`. It becomes `Vec<Rc<Operator>>` (§8 stage 0).

## 2. Schema, Guarantee, and Timing

| Attribute | Proposed rule |
|---|---|
| `schema` | Computed by `output_schema()` using one schema type for all nodes |
| `guarantee` | Binding records local evidence; `derive_guarantees()` fills the slots bottom-up |
| `timing` | Unset during assembly; `apply_lifecycle_timings()` fills the slots from an explicit assignment |

A set guarantee of `None` means no guarantee is available. It is different from an
`Unset` slot, which means the pass has not run.

### 2.1 Schema: fused into one type

Pre-ASAP uses `Schema` with plain `DataType` columns; post-ASAP stores a separate
`SummarySchema` that also supports summary state. Use `Schema` for both:

- Rename `Column` to `Field` and `Schema.columns` to `Schema.fields`.
- Widen `Field.dtype` to `FieldType`, covering plain values and ASAP state.
- Delete `SummarySchema` and `SummaryField`.

`Schema` keeps the reserved column `PROMQL_SERIES_IDENTITY` (`"$promql_series_identity"`,
`pre_asap/schema.rs`) and `has_promql_series_identity()`: `maintained_population.rs` uses
it to decide whether a closed PromQL schema still identifies a series (§7).

```rust
pub enum FieldType { DataType(DataType), ASAPType(ASAPType) }
pub enum ASAPType {       // SummaryFamilyType without Plain
    ExactAggregate(ExactKind, ExactParams), Sketch(SketchKind, GroupingStrategy),
    Sample(SamplingKind, SamplingParams), Wavelet(WaveletKind, WaveletParams), StatModel(StatModelKind, StatModelParams),
}
pub struct Schema { pub fields: Vec<Field>, pub time_index, pub unique_keys, pub closed }
pub struct Field { pub name, pub dtype: FieldType, pub nullable, pub table: Option<String> }
impl Field {
    pub fn plain(name, DataType) -> Self;
    pub fn plain_dtype(&self) -> Option<&DataType>;   // None for a state column
    pub fn expect_plain_dtype(&self) -> &DataType;    // frontends, scalar type inference; panics on state
}
```

| Node | Today | After |
|---|---|---|
| `NonASAPOp` | post-ASAP `KeepPreAsap`: `QueryExpr` schema lifted to `SummarySchema` and stored<br>post-ASAP `ValueOperation` / `BinaryOp` / `RelationalJoin` copies: stored at construction | using the same logic as `QueryExpr::output_schema()` |
| `SummaryAgg` | the replaced `Aggregate`'s output with the measure column retyped to `family` | grouping columns + one `ASAPType(family)` column |
| `SummaryEstimate` | the replaced operator's output schema | the child's grouping columns + the value columns of the `SketchQuery` |
| `FinalizeExactAccumulator` | the logical operator's output, lifted | the child's schema, `ASAPType(ExactAggregate ..)` columns changed into `DataType(..)` |
| `MaintainPopulation` / `ReadPopulation` | the source's schema / the replaced aggregate's output | the same rules, computed from the child and the `readout` |
| unused variants | one field typed `family` | unimplemented |

### 2.2 Guarantee: always derived

1. Binding records `SummaryEstimate.local_guarantee` (error over exact input) and
   exact `SummaryAgg.exact_rule`, leaving guarantee slots unset. To size a sketch, it derives
   the child's guarantee on a temporary copy, reads it, then drops the copy.
2. `derive_guarantees` fills slots bottom-up using the rules below.

```
Project               p99 ±1%    ← the child's
  SummaryEstimate     p99 ±1%    ← local ±1%, composed with Scan t's: looks through the SummaryAgg
    SummaryAgg(Kll)   None       ← state has no guarantee
      Scan t          exact      ← no ASAP descendant
```

| Node | Guarantee rule |
|---|---|
| `SummaryEstimate` | Compose `local_guarantee` with the guarantee of the input below `SummaryAgg`; sketch state itself has no guarantee. The local guarantee is `None` if the family has no error model. |
| `SummaryAgg` | ExactAggregate: compose with the child under `exact_rule`; `ExactKind::Count` stays exact regardless of the child. Sketch state: `Set(None)`. |
| `NonASAPOp` | Compose child guarantees; exact when there is no ASAP descendant. |
| `FinalizeExactAccumulator` | Copy the child's guarantee. |
| `MaintainPopulation` / `ReadPopulation` | Exact. |
| Unused variants | Return `Unimplemented` (§1.3). |

### 2.3 Timing: written from a lifecycle assignment

Logical planning decides what to compute. Summary materialization chooses a lifecycle
for each summary state, including execution timing and retention. One logical DAG can
have several assignments; the planner selects among them using deployment-provided
costs, consistent with the [planning-stages proposal](https://github.com/ProjectASAP/ASAPPlanner/pull/509).

Assembly leaves every timing slot `Unset`. `apply_lifecycle_timings` writes the
assignment with one memo across all roots and validates node and edge constraints
(§5). It rejects ingestion work that depends on query-time results, fixed-kind timing
violations, and conflicting timings on a shared node. Export and physical compilation
reject unset slots.

```
                    assembly        after apply_lifecycle_timings
Project             Unset           QueryTime
  SummaryEstimate   Unset           QueryTime
    SummaryAgg      Unset           IngestionTime   ← the state's lifecycle: maintained
      Scan t        Unset           IngestionTime   ← feeds a maintained state
```

Exact compositions (`OperationPlacement::Read` / `Maintenance`), maintained
populations and #472's grouped `Rate`→`Sum` pair currently fix timing during
construction. Each becomes a logical candidate whose placement is a lifecycle choice
(§8, stage 5).

The default assignment aims to preserve today's timings. It cannot do so when one
shared node requires both phases; resolving that conflict remains open (§10).

| Node | Timing constraint |
|---|---|
| `NonASAPOp` | Assigned timing must satisfy every consuming edge. |
| `SummaryAgg` | Assigned by the state's lifecycle. |
| `FinalizeExactAccumulator` | Either phase, subject to its inputs and consumers. |
| `SummaryEstimate` | Query time only. |
| `MaintainPopulation` / `ReadPopulation` | Ingestion time / query time only. |
| Unused variants | Return `Unimplemented` (§1.3). |

Apply timing after assembly because it depends on the whole DAG and its lifecycle.

### 2.4 Workflow

Today, construction and assembly compose guarantees, and export derives remaining
timings into a side table. The proposed sequence is:

```text
search / binding   record local accuracy evidence; leave timing unset
                   check accuracy on a temporary derived copy (§4)
assembly           build the selected roots, preserving shared nodes
derive_guarantees  fill guarantees bottom-up, with one memo across roots
lifecycle          choose per-state lifecycles using the derived DAG
apply_lifecycle_timings
                   write and validate timing, with one memo across roots
export             read filled slots; reject any Unset slot
```

---

# II. Changes, in data-flow order

## 3. Optimizer entry

Frontends and `resolve` build `NonASAP` trees only and access children with
`expect_non_asap()`. `search_cse_workload_with`, which every `search_workload*` entry
reaches, panics on a root that `contains_asap()`: an ASAP node there is a caller bug.

```rust
impl<C> Operator<C> {
    pub fn contains_asap(&self) -> bool;
    pub fn expect_non_asap(&self) -> &NonASAPOp<C>;   // an ASAP node here is a bug: panic
}
```

Library users do not call `search_workload*` themselves. Since #478 the external
boundary is `asap_aware_mapping::pass::optimize` (reached from `asap_planner::e2e_plan`),
and since #510 a pass always produces lifecycle-aware plans:

```
asap_planner::e2e_plan
  → asap_aware_mapping::pass::optimize
    → MajorPass::optimize
      → search_workload_with_targets
      → global_selection_with_summary_maintenance_lifecycles
      → assemble_selected_dag_with_summary_maintenance_lifecycles   (one call per root)
```

```rust
pub struct QueryLifecyclePlan { pub entry_index: usize, pub plan: SummaryMaintenanceLifecyclePlan }
pub struct PlanOutput { pub plans: Vec<QueryLifecyclePlan> }   // one per workload entry
impl PlanOutput { pub fn dags(&self) -> Vec<Rc<SummaryNode>> }  // each plan.root; becomes Rc<Operator>
```

`ParsedWorkload` (`asap_types::parsed_workload`) holds the roots as `Vec<Rc<QueryExpr>>`
and becomes `Vec<Rc<Operator>>`. The entry check above stays in `search_cse_workload_with`;
the pass layer adds no check of its own. See
[`updated_interface_with_pluggable_optimization.md`](../architecture/updated_interface_with_pluggable_optimization.md).

A compile-time alternative — an associated type on `ColState` with
`ColumnRef::ASAP = Never` — only protects frontend code before `resolve`: frontends
already return `ColumnId` trees, where `ASAP` is allowed. The entry check covers every
input (frontends after `resolve`, deserialized plans, test IR) with simpler types.

## 4. Planner: search and assembly

```rust
pub enum Replacement {
    Subtree(Rc<Operator>),     // formerly Summary(Rc<SummaryNode>) and Rewrite(Rc<QueryExpr>)
    ExactComposition { .. },   // its plan becomes Rc<Operator>
}
```

**Runtime support.** `runtime_support_evidence` dispatches on the replacement root:
`ASAP` uses `summary_support_evidence`; `NonASAP` returns `Some(true)`. Nested ASAP
nodes were checked when their own candidates were built.

**Candidate construction.** Candidates remain bottom-up and contain concrete child
plans (`realize_child_with`, `prepare_compositions`). The old `keep_pre_asap` fallback
returns the original child subtree directly.

**Accuracy checks.** Search derives each candidate's guarantee with a fresh memo,
checks its target, and stores the root guarantee on `ReplacementSubDAG` for selection.
It drops the derived copy and keeps the original shared candidate in `PlanSpace`.

**Assembly** — one rule replaces `assemble_residual`:

```rust
fn assemble(&self, t: &Rc<Operator>) -> Rc<Operator> {
    memo by ptr;                                          // shared children stay one Rc
    let composed = matches!(self.chosen(t),               // the chosen summary already folds the
        Some(Subtree(r)) if r is ASAP(SummaryAgg { child, .. })   // inner aggregate: nothing is hidden
            && !child.contains_asap() && !contains_aggregate(child));
    let chosen = if query_time_nested_sum(t) && !composed { None }  // as today: keep the outer SUM so the
                 else { self.chosen(t) };                           // inner target's own choice is assembled
    match chosen {
        Some(Subtree(r))           => r,                  // a complete plan, used as is
        Some(ExactComposition{..}) => composition.plan,
        None => t.map_children(|c| if is_target(c) { self.assemble(c) } else { c }),
                                                          // keep the node, assemble its children — assemble_residual does this for four operators only
    }
}
```

`GlobalSelection::assemble_selected_dag` derives guarantees after assembly, sharing
one `DerivationMemo` across roots. Lifecycle planning reads that derived DAG, then
`apply_lifecycle_timings` applies the chosen assignment.

`assemble_selected_query` remains the query-result boundary (#472): it calls
`assemble_selected_dag`, then `finalize_query_candidate` to insert a
`FinalizeExactAccumulator` where needed. Binding and the `BinaryOp` / `Join` callers
keep this finalization step. The lifecycle assembly path still bypasses it (§9).

Timing validation moves from candidate construction to lifecycle assignment:

- Materialization must offer only valid assignments. Failure when applying a selected
  assignment is a planning error.
- A shared node must have one timing across all roots. The pass rejects a conflict;
  it does not split the node by phase. Plans needing both phases must already contain
  distinct nodes (§10).

`map_children` is `rebuild_children` from
`pre_asap/cse.rs`, dispatching to `NonASAPOp::map_children` / `ASAPOp::map_children`.
Deleted: `assemble_residual`, `keep_pre_asap` / `keep_pre_asap_rc`, and the
`KeepPreAsap` branch of `finalize_exact_accumulator`. Kept: `relink_summary`,
`assemble_selected_query` / `finalize_query_candidate`, and the `query_time_nested_sum`
special case with its #472 exception — the chosen candidate is a `SummaryAgg` whose
child is a `NonASAP` subtree without an `Aggregate` — where `contains_aggregate` takes an
`Operator` instead of a `QueryExpr`.

| #468 problem | Resolution |
|---|---|
| 1. A `Project` is a `QueryExpr` inside `KeepPreAsap` and a `ValueOperation` outside | one set of types |
| 2. Exact and summary operators cannot share a scan hidden in `KeepPreAsap` | Both can reference one `Scan` when their timings agree. Different phases require distinct nodes. Splitting a multi-measure aggregate remains out of scope (§9). |
| 3. `SetOp` and similar have no post-ASAP copy, so no summary below them | `SetOp` takes `None => t`; both children are assembled |

## 5. Timing: validating the assignment

`apply_lifecycle_timings` replaces timing derivation with validation. It checks the
assigned timing against node kinds and consuming edges; the old
`ExecutionDataStateAssignment` side table is removed.

| Node | Produced state |
|---|---|
| `NonASAPOp` | `{assigned timing, Raw / …}`; checked against every consuming edge (`QUERY_ROWS` at the root) |
| `SummaryAgg` | `{assigned timing, SummaryState}` (§2.3) |
| other `ASAPOp` | today's rules, checked against the assigned timing |

A data state is the `timing` slot plus a primitive (`Raw` / `SummaryState` / …) fixed by
the kind; only the timing is stored.

The `KeepPreAsap` / `BinaryOp` / `ValueOperation` / `RelationalJoin` arms of today's
`validate_execution_data_states` merge into one `NonASAP` arm of that validation:

- Check the edge to each child; an `ASAP` child is checked by the `ASAP` edge rules.
  Ingestion work cannot depend on a query-time result.
- `check_plain_operands` stays: referenced columns must be `FieldType::DataType`
  (`Project` / `Filter` / `Sort` / `Limit` may pass `ExactAggregate` columns through).
  This rejects `Project(ASAP(SummaryAgg))`.
- `BinaryOp`'s ingestion-side constraints move into this arm.
- `AmbiguousKeepPreAsap` is deleted: a shared subtree assigned two timings is rejected by
  `apply_lifecycle_timings` (§4).

## 6. Export: one post-ASAP node per operator

The relational cases of `fallback{expression: QueryExpr}`, `binary`, `value` and
`relational_join` become one payload:

```rust
PostAsapOperatorPayload::Relational {
    /// One non-ASAP operator: its kind, expressions and parameters, without its child
    /// fields. The children are this node's incoming edges, in child-role order.
    operator: NonASAPOpKind,
}
```

Export emits one node per operator, with children represented by edges. A shared
operator is exported once. Each relational node stores its kind, scalar expressions
and parameters in `NonASAPOpKind`; it embeds no child subtree or `DagInput` placeholder.

Physical compilation lowers each node to one operator or a few helper operators.
Relational lowering reads inputs from edges, replacing the old whole-expression
`fallback` and the `binary`, `value::Project` and `relational_join` special cases.
ASAP nodes retain their summary payloads; `FinalizeExactAccumulator`,
`MaintainPopulation` and `ReadPopulation` retain `value{operation}`.

- **Wire version 6:** replace relational payloads with per-operator `relational`
  nodes and use `Schema` for output and intermediate schemas. Update downstream
  readers together in stage 4 (§8).
- **Attributes:** export reads derived guarantees and assigned timings, rejecting
  unset slots. Edge data state combines the producer's timing with its primitive (§5).
- **Compilation:** `compile_post_asap_dag` splits precompute and query DAGs by assigned
  timing without repeating validation.
- **Phase boundaries:** assignments must satisfy §5. This proposal materializes only
  ASAP state; assignments requiring materialization elsewhere are rejected.
- **`SummaryMerge`:** keep its wire payload; planner-side support remains open (§10).

## 7. Other consumers

| Location | Change |
|---|---|
| `post_asap/cse.rs` | delete; `share_common_subtrees` covers `ASAPOp` (derives `PartialEq` + serde) |
| `dag_export.rs` | delete `build_summary` / `build_summary_hybrid` / `summary_kind_tag`; one exporter with an `ASAP` arm; update the viewer's `node-style.js` and the pin test `viewer_categorizes_exactly_the_exported_node_kinds` |
| `summary_maintenance_cost/estimator.rs` (90 `SummaryExpr::` sites, 13 `KeepPreAsap`) | `KeepPreAsap` branches (`query_source_selections`, `retained_queries`) use the §6 `Relational` nodes; `exact_binary` / `value_operation` costs fold into it |
| `summary_maintenance_cost/evidence.rs` | `summary_operation_evidence` gets an `ASAP` arm; also fixes its missing `RelationalJoin` case |
| `physical_plan_cost_model.rs::estimate_candidate` | every `Relational` node goes through the per-operator lowering (§6) in `lower_query_physical_dag` |
| `summary_maintenance_lifecycle.rs` | `SummaryMaintenanceLifecyclePlan.root` becomes `Rc<Operator>`; `selected_raw_recompute` becomes `!contains_asap(root)`; the `keep_pre_asap(target)` fallback in `assemble_selected_dag_with_summary_maintenance_lifecycles` becomes `target` |
| `pass/mod.rs`, `pass/major.rs` | `PlanOutput::dags()` returns `Vec<Rc<Operator>>`; `MajorPass` otherwise unchanged (§3) |
| `types/parsed_workload.rs` | `ParsedWorkload` roots become `Rc<Operator>` (§3) |
| `asap-planner` (`planner/src/lib.rs`, `tests/e2e_plan.rs`) | follows `PlanOutput` |
| `maintained_population.rs` | `KeepPreAsap(source)` becomes `source`; `population.matches_input` reads an `NonASAP` child directly and keeps the `has_promql_series_identity()` check (§2.1) |
| `replacement.rs::enumerate_candidate_dags`, `CandidateDagInventory` | walk `Rc<Operator>` instead of `SummaryNode`; internal and test use only |
| `exact_composition.rs` | `ExactOperation::Aggregate` becomes a `NonASAP(Aggregate)`, built over each child candidate as `prepare_compositions` does today |
| `RelationalJoin.pruning` | never set to `Some` in production; delete. Candidate pruning can return as an `ASAPOp` variant |

---

# III. Implementation

## 8. Stages and tests

`main` builds and passes all tests after every stage.

| Stage | Change | Main consumers |
|---|---|---|
| 0. Prepare | Give `Concat.children` `Rc` identity; rename `rebuild_children` to `map_children`; add `Column::plain`. | `asap-types` |
| 1. Split | Separate `NonASAPOp` and `ScalarExpr`; keep `Rc<NonASAPOp>` children. | [Scalar consumers](decoupling_op_and_expr.md#3-changes) |
| 2. Unify operators | Add `Operator`, an empty `ASAPOp`, entry helpers, and unset attribute slots. Widen children to `Rc<Operator>`. | All crates |
| 3. Unify schemas | Introduce `Field`, `FieldType`, `ASAPType` and the shared `Schema`. Keep wire types and serialized names unchanged. | Schema consumers |
| 4. Switch export | Fill `ASAPOp`, add attribute passes and entry validation, and export the new IR with wire version 6. | Types, devtools, viewer, backend, collector |
| 5. Migrate planning | Build candidates and assemble directly on `Rc<Operator>`; move timing choices to lifecycle planning; migrate §7 consumers. | Mapping and planner crates |
| 6. Remove legacy code | Delete old IR types, duplicate operators, CSE and timing fallbacks; update architecture and developer docs. | All remaining consumers |

Stage 4 is the only wire-breaking stage and must land with ASAPQuery-backend and
ASAPCollector updates. A temporary `flatten(&SummaryNode) -> Rc<Operator>` adapter
copies existing guarantees and applies existing timings where valid. Wire schemas
switch from `SummarySchema` to `Schema`, and `Schema.fields` serializes as `fields`
instead of `columns`.

Stage 5 deletes `flatten`. Binding leaves timing unset; lifecycle planning handles
the placement choices listed in §2.3. Paths without lifecycle selection use the
default assignment, subject to the shared-node conflict in §10.

Stage 6 removes `SummaryExpr`, `SummaryNode`, duplicate `ValueOperation` variants,
`ExactOperation`, `post_asap/cse.rs`, `produced_data_state` defaults and
`validate_execution_data_states_at`. `PlanOutput::dags()` and `ParsedWorkload` finish
moving to `Operator`. Update `post-asap-ir.md`, `physical-plan-integration.md`,
`updated_interface_with_pluggable_optimization.md`, and developer/viewer docs.

**Tests**:

- One integration test per #468 problem:
  1. `WITH metric AS (SELECT avg(CASE WHEN l_quantity BETWEEN 1 AND 50 THEN 1.0 ELSE 0.0 END) AS in_range FROM lineitem) SELECT in_range, in_range = 1.0 AS ok FROM metric` — no post-ASAP-only node besides `ASAP`; all `Project`s are one variant.
  2. `SELECT avg(l_extendedprice), approx_percentile_cont(l_discount, 0.99) FROM lineitem` — the `avg` `Aggregate` and the KLL `SummaryAgg` share one `Scan` by `Rc::ptr_eq` under an assignment that runs both at query time (once a binding rule splits measures).
  3. `SELECT approx_distinct(l_partkey) FROM lineitem UNION ALL SELECT approx_distinct(l_suppkey) FROM lineitem` — each side of the `SetOp` has a `SummaryEstimate`.
- A shared `Scan` assigned two timings is rejected; assigned one timing, it stays one `Rc`.
- A node shared by two roots, assembled in two calls, is still one `Rc` after `derive_guarantees` and `apply_lifecycle_timings`.
- After both passes no slot is `Unset`; export rejects a tree with one, and a tree with no assignment applied.
- The default assignment preserves existing timings where valid; mixed-phase sharing is covered by the open question in §10.
- `apply_lifecycle_timings` rejects an assignment that puts ingestion work over a query-time result, and one that gives a `SummaryEstimate` ingestion time.
- A kept `NonASAP` node (e.g. a `SetOp`) reports the guarantee composed from its assembled children.
- Each unused branch (§1.3) returns `Unimplemented` from `output_schema`, `derive_guarantees`, `apply_lifecycle_timings` and export.
- `search_workload*` panics on a root containing `ASAP`.
- Rewrite the 110 `SummaryExpr::` assertions in `sql_to_post_asap.rs` / `promql_to_post_asap.rs` / `exact_composition.rs`.
- Wire 6 round trip of a DAG with `Relational` nodes both above and below `ASAP` nodes; a version-5 document is rejected.
- The 16 `execution_data_state.rs` tests keep their shapes; they apply an assignment and read the `timing` slot instead of `ExecutionDataStateAssignment`.

## 9. Out of scope

- The binding rule splitting a multi-measure `Aggregate` into exact + summary over one child.
- Candidate pruning as an `ASAPOp` variant.
- Accuracy through `SummaryMerge` / `Subtract` / `Delete` / `Join` (§2.2): an accuracy
  descriptor on state, or composing error along the state chain at readout.
- Holes: letting a chosen plan's child be filled by that child target's own choice at
  assembly, instead of fixing it when the candidate is built. A search-strategy change,
  independent of the types here.
- Folding `ExactComposition` into `Subtree` (both of its forms become expressible);
  deferred until stage 5 is stable.
- `assemble_selected_dag_with_summary_maintenance_lifecycles`, the only assembly `MajorPass`
  uses, calls `assemble_selected_dag` rather than `assemble_selected_query` (§4), so pass
  output carries no root `FinalizeExactAccumulator`. Not changed here.

## 10. Open questions

- **Assignment type:** reuse the type returned by #482's
  `SummaryMaintenanceLifecyclePlan::execution_timed_dag()` to expand state lifecycles
  into per-node timings, rather than define a competing type.
- **Summary merge:** does ASAPQuery insert `SummaryMerge` only in the exported DAG,
  or through planner-side types? The latter remains unimplemented.
- **Mixed-phase sharing:** one shared `Scan` may feed an ingestion-time `SummaryAgg`
  and a query-time `Aggregate`. The default assignment then conflicts. Decide whether
  assembly creates separate subtrees or materialization gives the shared state one
  lifecycle. The timing pass itself must not silently split it.
