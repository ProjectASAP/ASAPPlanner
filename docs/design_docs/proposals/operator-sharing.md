# Sharing Operators Between Pre-ASAP IR and Post-ASAP IR

> - Status: proposed, not implemented. 
> - Problem statement: [#468](https://github.com/ProjectASAP/ASAPPlanner/issues/468). 
> - Builds on [Decoupling operators from scalar expressions](decoupling_op_and_expr.md), which splits `QueryExpr` into `NonASAPOp` and `ScalarExpr`. 
> - Code is referenced by file and function against `main` at `5a32b8b` (after #472, #478, #510).
> - Timing follows the planner layering of #480 / #509: a node's timing is written from the summary maintenance lifecycle assignment chosen for the DAG, never inferred from the IR (§2.3).

**The idea.** Today a post-ASAP plan is glued together from two sets of operator types. 
This proposal keeps one operator language and makes summary operators extra node kinds in it: any relational operator can sit above a summary, and a summary can read any relational subtree. 
Nothing is wrapped and nothing is duplicated.

`Operator` has two levels, `NonASAP(NonASAPOp)` and `ASAP(ASAPOp)`, rather than one flat
enum of every variant: frontends, `resolve` and the per-operator export (§6) work on `NonASAP`
DAGs only, and `NonASAPOp` gives them a precise type for that instead of a run-time check
on each node.

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

Operator attributes differ in how widely they apply. 
We define the `Operator` type structure based on the breadth of its attributes.

| Applies to | Examples | Defined as |
|---|---|---|
| every operator | children, schema, timing, guarantee | methods implemented for `Operator` |
| one category | for all `NonASAP` operators, guarantee is derived from the children, and the assigned timing is checked against the consuming edge | implementation specified to one enum branch of `Operator` |
| one operator | `Aggregate.measures`, `SummaryAgg.family` | fields of that variant |

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
    pub fn with_guarantee(&self, guarantee: Option<ResultGuarantee>) -> Self;  // Setter of accuracy guarantee
    pub fn with_timing(&self, timing: ExecutionTiming) -> Self;                // Setter of execution timing
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

- **Passes copy**: `derive_guarantees` and `apply_lifecycle_timings` return a new tree;
  nodes are immutable and `with_*` build new ones. Pointers held before a pass
  (`assembled_nodes`, planner memos) are not valid into its result; lifecycle selection
  reads the guarantee-derived tree, export reads the timed tree (§2.4).
- **Passes recompute**: `derive_guarantees` keeps the values set at construction (§2.2)
  and recomputes every other slot; `apply_lifecycle_timings` overwrites every `timing`
  slot. Every rewrite runs before them, so running either again gives the same slots.
- **Equality**: the `timing` slot takes part in `PartialEq` and `Hash`, so CSE never merges
  two nodes assigned different timings. The `guarantee` slot takes part in neither: it is
  a function of the subtree, so equal subtrees derive equal guarantees, and
  `ResultGuarantee` holds `f64` and has no `Hash`.

Following diagram conceptually displays the structure of `Operator<C>`:
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

`NonASAPOp` is the non-ASAP category of `Operator`.
It comes from splitting `QueryExpr` into "operator" and "scalar expression" parts ([decoupling doc](decoupling_op_and_expr.md#2-types)). 

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

`ASAPOp` is the ASAP category of `Operator`.
`ASAPOp` comes from today's `SummaryExpr`: its summary variants, and the summary-specific `ValueOperation` variants.

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

**Unused branches**: `SummaryMerge`, `SummarySubtract`, `SummaryDelete`, `SummaryJoin` and `Extension` are built only in tests today. They are migrated, but for safety, we have all their methods return `Unimplemented`.

Following table shows how some legacy types get expressed in the new framework.

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

This section discusses three key per-node attributes, `schema`, `guarantee`, and `timing`, as well as how they are stored and filled in the new framework.

| Field | Meaning | Today | After |
|---|---|---|---|
| `schema` | output columns and their types | pre-ASAP: computed by `QueryExpr::output_schema()`<br>post-ASAP: a `SummarySchema` stored on every `SummaryNode` | can be obtained by `output_schema()` |
| `guarantee` | accuracy bound | pre-ASAP: none<br>post-ASAP: stored on every `SummaryNode` | can be obtained by `guarantee()`<br>binding stores only each operator's own error<br> complete error bound need to be derived by `derive_guarantees()` |
| `timing` | execution time | pre-ASAP: none<br>post-ASAP, stored: a field on `BinaryOp` / `ValueOperation` / `SummaryMerge`<br>post-ASAP, not stored: `KeepPreAsap` from the consuming edge, `SummaryAgg` from the child. | can be obtained by `timing()`<br>`Unset` after assembly<br>written for every node by `apply_lifecycle_timings()` from the chosen lifecycle assignment (§2.3) |

### 2.1 Schema: fused into one type

Today schemas of pre-ASAP operators and post-ASAP operators are different:
- pre-ASAP uses `Schema { columns: Vec<Column>, time_index, unique_keys, closed }` with `Column.dtype: DataType` (plain values only),
- post-ASAP stores a `SummarySchema { fields: Vec<SummaryField>, time_index }` on every node, with `SummaryField.dtype: SummaryFamilyType` (`Plain(DataType)` or summary state). 
Now since the two operators types are unified into one, we need a unified schema type as well.

We implement the new schema type based on the original `Schema` type used in pre-ASAP operators, with two changes:

- `Column` is renamed `Field`, and `Schema.columns` `Schema.fields`: the struct describes
  a column and holds none of its data. (Arrow and DataFusion use the same names.)
- `Field.dtype` widens from `DataType` to an enum `FieldType`, which covers both plain data types and ASAP summary types.

`SummarySchema` / `SummaryField` are then redundant and deleted.

`Schema` keeps the reserved column `PROMQL_SERIES_IDENTITY` (`"$promql_series_identity"`,
`pre_asap/schema.rs`) and `has_promql_series_identity()`: `maintained_population.rs` uses
it to decide whether a closed PromQL schema still identifies a series (§7).

Detailed code design is shown below.
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

A guarantee is filled in two steps:

1. **Binding** records local accuracy guarantee: a `SummaryEstimate`'s `local_guarantee` (the sketch's error over an exact input) and an exact `SummaryAgg`'s `exact_rule`. No `guarantee` slot is set yet. To size a sketch and check its target, binding still needs the child's error, as today: it runs `derive_guarantees` on the child with a fresh memo, reads the result, and drops it.
2. **`derive_guarantees`** fills every slot bottom-up: a node without an `ASAP` descendant is exact, and every other node composes its children's guarantees by its own rule.

```
Project               p99 ±1%    ← the child's
  SummaryEstimate     p99 ±1%    ← local ±1%, composed with Scan t's: looks through the SummaryAgg
    SummaryAgg(Kll)   None       ← state has no guarantee
      Scan t          exact      ← no ASAP descendant
```

Per node kind:

| Node | Today | After |
|---|---|---|
| `SummaryEstimate` | stored at binding: the sketch's own error composed with the child's (`compose_guarantee`) | **derived**: `local_guarantee` composed with the guarantee of the state's input, i.e. the child of the `SummaryAgg` below (the `SummaryAgg` itself has none). `local_guarantee` is set at binding: the sketch's error over an exact input, `None` when the model has no error model for the family |
| `SummaryAgg` | stored: ExactAggregate family composed with the child's; sketch families `None` | **derived**: ExactAggregate family: exact, composed with the child's under `exact_rule`, except `ExactKind::Count`, exact whatever the child (as today); sketch families `Set(None)`, state has no guarantee |
| `NonASAPOp` | pre-ASAP `QueryExpr`: none<br>post-ASAP `KeepPreAsap`: exact<br>post-ASAP `ValueOperation` / `BinaryOp` / `RelationalJoin` copies: composed at construction | **derived**: composed from the children; exact if no `ASAP` descendant |
| `FinalizeExactAccumulator` | copies the child's | **derived**: the child's |
| `MaintainPopulation` / `ReadPopulation` | stored: exact | **derived**: exact |
| unused variants | `None`: state has no guarantee of its own | unimplemented (§1.3) |
  
### 2.3 Timing: written from a lifecycle assignment

The logical layer — PlanSpace, binding, assembly — decides *what* to compute, not when.
Timing is chosen by summary materialization: for every unique summary state it picks a
lifecycle (maintain at ingestion time or recompute at query time, with window and
retention), and that choice fixes the timing of every node that feeds or reads the state.
One logical DAG can therefore have several assignments; the deployment chooses among
them with its own costs ([Output layers](../architecture/input-output-workflow.md#output-layers), #480).

Nothing in the IR sets a timing. Binding sets no `SummaryAgg.timing`; the planner sets
none on `FinalizeExactAccumulator` (`finalize_query_candidate`, §4, still inserts the
node, its timing is assigned like any other). After assembly every `timing` slot is
`Unset`. `apply_lifecycle_timings` writes the chosen assignment into the slots, top-down
per root with one shared memo, and validates it (§5): an assignment under which
ingestion work depends on a query-time result, or a node of fixed kind gets the wrong
timing, is rejected, as is one that gives a shared node two timings (§4). `Unset` means
no assignment was applied; physical compilation and export reject it.

```
                    assembly        after apply_lifecycle_timings
Project             Unset           QueryTime
  SummaryEstimate   Unset           QueryTime
    SummaryAgg      Unset           IngestionTime   ← the state's lifecycle: maintained
      Scan t        Unset           IngestionTime   ← feeds a maintained state
```

Some candidates fix a timing when they are built today: exact compositions carry
`OperationPlacement::Read` / `Maintenance` (provenance `ValueOperationAtQueryTime` /
`ValueOperationAtIngestionTime`), maintained populations are built at ingestion time,
and #472's grouped `Rate`→`Sum` pair. Each becomes one logical candidate whose placement
is a lifecycle choice (§8 stage 5). The **default assignment** reproduces today's
timings — a `SummaryAgg` maintained at ingestion time, everything above a readout at
query time — so exported timings do not change until lifecycle selection chooses
otherwise.

Per node kind:

| Node | Today | After |
|---|---|---|
| `NonASAPOp` | pre-ASAP `QueryExpr`: none<br>post-ASAP `KeepPreAsap`: from the consuming edge<br>post-ASAP `ValueOperation` / `BinaryOp` copies: a stored field | **assigned**; validated against its consuming edges (§5) |
| `SummaryAgg` | from the child; ingestion time under `KeepPreAsap` | **assigned** by its state's lifecycle |
| `FinalizeExactAccumulator` | a stored field, set by the planner | **assigned**; the position allows either time |
| `SummaryEstimate` | query time, fixed by the kind | **assigned**; validated: query time only |
| `MaintainPopulation` / `ReadPopulation` | a stored field, always ingestion / query time | **assigned**; validated: ingestion / query time only |
| unused variants | `SummaryMerge`: a stored field; `Join` / `Subtract` / `Delete`: ingestion time | unimplemented (§1.3) |

Unlike a guarantee, a timing is a property of the whole DAG and its assignment, so it is
applied only after assembly.

### 2.4 Workflow of setting up `guarantee` and `timing`: today vs. after

Today:

```
search / binding   each SummaryNode's guarantee is composed when the node is built;
                   BinaryOp / ValueOperation store their timing
selection          reads each candidate's stored guarantee against its target
assembly           assemble_residual builds kept nodes and composes their guarantee;
                   relink_summary copies the old guarantee onto a relinked SummaryAgg
lifecycle          reads the root's guarantee
export             validate_execution_data_states, per root, derives the remaining
                   timings into a side table and writes them onto the edges
```

After:

```
search / binding   sets only what cannot be derived: local_guarantee, exact_rule;
                   no timing; checks accuracy on a derived copy, keeps the result on
                   the candidate record (§4), drops the copy
assembly           builds each root; kept NonASAP nodes stay as they are
derive_guarantees  per root, one shared memo: fills guarantees bottom-up
lifecycle          reads the derived guarantee; chooses a lifecycle per summary state
apply_lifecycle_   per root, one shared memo: writes the assignment's timings
  timings          top-down, validates them, rejects a shared node assigned two timings
export             reads the slots; rejects an Unset one
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

`runtime_support_evidence` (`replacement.rs`) asks the cost model whether the runtime
supports a candidate, per variant: `Summary` → `summary_support_evidence`, `Rewrite` →
`Some(true)`. With one `Subtree` variant it dispatches on the root: an `ASAP` root →
`summary_support_evidence`, a `NonASAP` root → `Some(true)`. `ASAP` nodes below a
`NonASAP` root were each asked when they were candidates themselves.

**Candidates stay bottom-up, as today**: a candidate is built on a concrete child plan
(`realize_child_with`, or each child candidate in `prepare_compositions`), so a chosen
plan is complete. Where `realize_child_with` falls back to `keep_pre_asap` today, it
returns the child's original subtree, and assembly keeps it as is.

**Accuracy check during search**: binding sets no `guarantee` slot (§2.2), so the
candidate filter in `search_workload_with_targets` and `prepare_compositions` run
`derive_guarantees` on the candidate alone, with a fresh memo, then check its accuracy target. The derived
copy is dropped; its root guarantee is stored on the candidate's `ReplacementSubDAG`, where
selection reads it (today's stored `guarantee` moved from the node to the candidate record).
PlanSpace keeps the original candidate, whose nodes are shared with other queries.

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

Then `GlobalSelection::assemble_selected_dag` runs `derive_guarantees` (§2.2) on each
root it assembles. The `DerivationMemo` lives on `GlobalSelection` next to
`assembled_nodes`, so roots assembled one call at a time still share nodes.
`assemble_selected_dag_with_summary_maintenance_lifecycles` plans lifecycles on that
result, as today: lifecycle planning holds `Rc`s into the plan and reads the root's
guarantee, so it must see the derived tree. The assignment it chooses is then written
with `apply_lifecycle_timings` (§2.3, §5).

`assemble_selected_query` stays the boundary for a query result (#472): it runs
`assemble_selected_dag`, then `finalize_query_candidate`, which puts a query-time
`FinalizeExactAccumulator` between maintained exact state and the query-time consumer
(§2.3). The other callers of `finalize_query_candidate` — binding and the two sides of
`BinaryOp` and `Join` — keep calling it, on `Rc<Operator>`. Today
`assemble_selected_dag_with_summary_maintenance_lifecycles` (`summary_maintenance_lifecycle.rs`)
calls `assemble_selected_dag` directly and skips that step; this proposal leaves that as
it is (§9).

- **Illegal placement** (e.g. a query-time `SummaryEstimate` under a `SummaryAgg`):
  candidates carry no timing, so nothing is checked when they are built. The check moves
  to `apply_lifecycle_timings`, which rejects an assignment that places a node illegally.
  Summary materialization offers only assignments it has validated (today
  `relink_summary` runs the same check through `validate_execution_data_states_at`), so an
  error from `apply_lifecycle_timings` on a chosen assignment is a bug, and planning fails
  with that error.
- **A shared subtree assigned two timings**, e.g. a query-time `Aggregate` and an
  ingestion-time `SummaryAgg` reading one `Scan`: both placements are legal, only the
  sharing is not. `apply_lifecycle_timings` memoizes by pointer and records the timing it
  wrote; reaching the node again with another timing is an error, and the assignment is
  rejected like any other illegal one. A subtree assigned one timing stays one `Rc`,
  within a root or across roots. Nothing is copied: a plan that needs the same subtree
  in both phases must hold two `Rc`s before the pass (§10).

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
| 2. Nothing outside `KeepPreAsap` can reference the `Scan` inside, so an exact aggregate and a sketch cannot share a scan | `Aggregate` and `SummaryAgg` can point to the same `Scan`. This holds only when both run at the same time, e.g. under an assignment that recomputes the sketch at query time; an assignment that runs them in different phases over one `Rc` is rejected (above). Splitting a multi-measure `Aggregate` into exact + sketch is a binding rule, out of scope (§9) |
| 3. `SetOp` and similar have no post-ASAP copy, so no summary below them | `SetOp` takes `None => t`; both children are assembled |

## 5. Timing: validating the assignment

`validate_execution_data_states` becomes the validation half of `apply_lifecycle_timings`.
The `ExecutionDataStateAssignment` side table is deleted; instead of deriving a state per
node, the pass checks the timing the assignment wrote into each slot against the node's
kind and its consuming edges. Nothing is derived from the consuming edge any more:

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

The four original-operator payloads (`fallback{expression: QueryExpr}`, `binary`,
`value`, `relational_join`) become one:

```rust
PostAsapOperatorPayload::Relational {
    /// One non-ASAP operator: its kind, expressions and parameters, without its child
    /// fields. The children are this node's incoming edges, in child-role order.
    operator: NonASAPOpKind,
}
```

Export emits **one post-ASAP node per `NonASAP` operator**, exactly as it already does
per `ASAP` operator: children become edges, leaves are `Scan` nodes. No subtree is
embedded in a node, so there is no `DagInput` placeholder and no fragment. A `NonASAP`
node shared by two consumers (one `Rc`, kept by CSE, §4) is exported once, with two
outgoing edges. Physical compilation then corresponds node by node: each post-ASAP node
lowers to one physical operator, or to a few helper operators numbered from it. `ASAP`
nodes map one-to-one onto the existing summary payloads;
`FinalizeExactAccumulator` / `MaintainPopulation` / `ReadPopulation` stay
`value{operation}`. The `fallback` whole-expression lowering and the `binary` /
`value::Project` / `relational_join` special cases go; every `Relational` node is lowered
by one per-operator lowering that reads its inputs from its edges.

- **Wire 5 → 6**: three fewer payloads; `fallback` becomes `relational` nodes, one per
  operator; `output_schema` / `intermediate_schema` become `Schema`. One cutover (§8
  stage 4), together with the downstream readers.
- **Timing and guarantee** are read from the node slots: timing as assigned, guarantee as
  derived. An `Unset` slot is rejected; for timing it means no assignment was applied.
  An edge's `data_state` is its producer's assigned timing plus the primitive of its kind
  (§5). `compile_post_asap_dag` splits the precompute and query DAGs by that timing and
  no longer re-runs data-state validation.
- **`SummaryMerge`** stays a wire payload, although its planner-side variant is
  unimplemented (§1.3, §10).
- **Phases** are per node, from the assigned timing. A phase switch between two
  `NonASAP` nodes must satisfy §5 (ingestion work cannot read a query-time result);
  summary materialization places materialization points only at `ASAP` state, so an
  assignment that would need one elsewhere is rejected when applied.

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

| Stage | Content | Touches |
|---|---|---|
| 0 Preparation | `Rc` for `Concat.children`; `rebuild_children` → `map_children`; `Column::plain` | `asap-types` |
| 1 Split | [decoupling doc](decoupling_op_and_expr.md): `NonASAPOp` + `ScalarExpr`; children stay `Rc<NonASAPOp>` | scalar code ([decoupling doc §3](decoupling_op_and_expr.md#3-changes)) |
| 2 Two levels | §1.1, §1.4: `Operator<C>`, an empty `ASAPOp`, `contains_asap()`, `expect_non_asap()`; child slots become `Rc<Operator<C>>`; every variant gets `timing` / `guarantee` slots, and nodes are built through constructors that leave both `Unset` (`timing` stays `Unset` until an assignment is applied) | every crate incl. `asap-planner`; the same mechanical change everywhere |
| 3 One schema | §2.1: `Column` → `Field` and `Schema.columns` → `fields` (serde keeps the name `columns` until stage 4); `FieldType`, `ASAPType`, `PlainField`, `Schema` everywhere except the `post_asap_dag.rs` wire types, which keep `SummarySchema` until stage 4. **No wire change** | `asap-types` + schema construction in every crate |
| 4 New types | fill `ASAPOp`; `ASAP` arms of `output_schema`; `derive_guarantees` and `apply_lifecycle_timings` (§2.2, §2.3, §5); the entry check (§3); `flatten(&SummaryNode) -> Rc<Operator>` so export runs on the new types, copying each node's guarantee into its slot and applying today's timings as the initial assignment, so the export is unchanged; wire types become `Schema`, and `Schema.fields` serializes as `fields`. Wire → 6. **The only wire-breaking stage**; merged together with ASAPQuery-backend and ASAPCollector | `asap-types`, `devtools`, viewer |
| 5 Planner | §4: candidates and assembly on `Rc<Operator>`; §7 moves to the new types; delete `flatten`. Timing moves to the lifecycle: binding and candidates set none; the candidates that fix a timing today (§2.3) become one logical candidate each, their placement a lifecycle choice; paths without lifecycle selection apply the default assignment that reproduces today's timings | `asap-aware-mapping`, `asap-planner` |
| 6 Cleanup | delete `SummaryExpr`, `SummaryNode`, extra `ValueOperation` variants, `ExactOperation`, `post_asap/cse.rs`, and today's timing fallbacks (`produced_data_state` defaults, `validate_execution_data_states_at`); `PlanOutput::dags()` and `ParsedWorkload` lose their `SummaryNode` / `QueryExpr` types; update `post-asap-ir.md` (execution phase comes from the assignment), `physical-plan-integration.md`, `updated_interface_with_pluggable_optimization.md`, developer and viewer docs | `asap-planner`, docs |

Wrapping pre-ASAP operators in `ValueOperation` first is not planned: stage 4 gives
the same early flat export, on the final types.

**Tests**:

- One integration test per #468 problem:
  1. `WITH metric AS (SELECT avg(CASE WHEN l_quantity BETWEEN 1 AND 50 THEN 1.0 ELSE 0.0 END) AS in_range FROM lineitem) SELECT in_range, in_range = 1.0 AS ok FROM metric` — no post-ASAP-only node besides `ASAP`; all `Project`s are one variant.
  2. `SELECT avg(l_extendedprice), approx_percentile_cont(l_discount, 0.99) FROM lineitem` — the `avg` `Aggregate` and the KLL `SummaryAgg` share one `Scan` by `Rc::ptr_eq` under an assignment that runs both at query time (once a binding rule splits measures).
  3. `SELECT approx_distinct(l_partkey) FROM lineitem UNION ALL SELECT approx_distinct(l_suppkey) FROM lineitem` — each side of the `SetOp` has a `SummaryEstimate`.
- A shared `Scan` assigned two timings is rejected; assigned one timing, it stays one `Rc`.
- A node shared by two roots, assembled in two calls, is still one `Rc` after `derive_guarantees` and `apply_lifecycle_timings`.
- After both passes no slot is `Unset`; export rejects a tree with one, and a tree with no assignment applied.
- Exported timings of today's plans are unchanged under the default assignment.
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

- `LifecycleAssignment` (§1.1): #482 adds `SummaryMaintenanceLifecyclePlan::execution_timed_dag()`,
  which expands per-state lifecycle choices into per-node timings. This proposal should
  take that type as the assignment rather than define its own.

- Does ASAPQuery insert `SummaryMerge` only on the exported post-ASAP DAG, or through ASAPPlanner's
  post-ASAP types? The planner-side variant is unimplemented (§1.3).

- Workload CSE can share one `Scan` between a query whose `SummaryAgg` is maintained at
  ingestion time and another whose `Aggregate` runs at query time. Today `KeepPreAsap`
  takes its timing from the consuming edge, so the two sides are effectively separate;
  after this proposal the default assignment is rejected on that `Rc` (§4). Either
  assembly un-shares a `NonASAP` subtree that `SummaryAgg` reads, or summary
  materialization must treat the shared scan as one state with one lifecycle. Not
  decided here.
