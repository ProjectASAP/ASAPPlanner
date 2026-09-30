# Sharing Operators Between Pre-ASAP IR and Post-ASAP IR

> - Status: proposed, not implemented. 
> - Problem statement: [#468](https://github.com/ProjectASAP/ASAPPlanner/issues/468). 
> - Builds on [Decoupling operators from scalar expressions](decoupling_op_and_expr.md) (same PR), which splits `QueryExpr` into `NonASAPOp` and `ScalarExpr`. 
> - Revised: node timing comes from an applied summary maintenance lifecycle assignment, not from derivation over the IR; see [Output layers](../architecture/input-output-workflow.md#output-layers).

**The idea.** Today a post-ASAP plan is glued together from two sets of operator types. 
This proposal keeps one operator language and makes summary operators extra node kinds in it: any relational operator can sit above a summary, and a summary can read any relational subtree. 
Nothing is wrapped and nothing is duplicated.

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
| one category | for all `NonASAP` operators, guarantee is derived from the children | implementation specified to one enum branch of `Operator` |
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
/// In the current design, it will be used to wrap the `timing` and `guarantee` values: 
/// `guarantee` is set by derivation, `timing` by applying a lifecycle assignment.
pub enum Slot<T> { Unset, Set(T) }

/// Memo of one pass (derivation or assignment), keyed by (node pointer, timing).
/// One is shared by every root of a workload, so a node shared by two roots stays one `Rc`.
pub struct DerivationMemo { .. }

/// Build the accuracy guarantee of one DAG root by derivation
pub fn derive_guarantees(
  root: &Rc<Operator>,
  model: &dyn AccuracyModel,                // accuracy model used for derivation
  evidence: &dyn AccuracyEvidenceProvider,  // evidence provider used for derivation
  memo: &mut DerivationMemo,
) -> Result<Rc<Operator>, AccuracyError>;
/// Record the timings of a lifecycle assignment in the `timing` slots of one DAG root,
/// then validate them (§5). The summary maintenance lifecycle layer chooses the assignment;
/// timing is never inferred from operator kinds.
pub fn apply_lifecycle_timings(
  root: &Rc<Operator>,
  assignment: &LifecycleAssignment,
  memo: &mut DerivationMemo,
) -> Result<Rc<Operator>, ExecutionDataStateError>;
```

- **Recomputation**: `derive_guarantees` keeps the values set at construction (§2.2) and
  recomputes every other slot; `apply_lifecycle_timings` overwrites every `timing` slot.
  Both are safe to call again after a rewrite.
- **Equality**: both slots take part in `PartialEq` and hashing, so CSE never merges two
  nodes that differ in timing or guarantee.

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
    ScalarBridge(Rc<ScalarExpr<C>>),   // the `2` in PromQL `v * 2`
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
Filter { pred: Predicate(Rc<ScalarExpr>),        Filter { pred: Predicate(Rc<ScalarExpr>),
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
| `timing` | execution time | pre-ASAP: none<br>post-ASAP, stored: a field on `BinaryOp` / `ValueOperation` / `SummaryMerge`<br>post-ASAP, not stored: `KeepPreAsap` from the consuming edge, `SummaryAgg` from the child. | can be obtained by `timing()`<br>set only by `apply_lifecycle_timings()` from a lifecycle assignment (§2.3); `Unset` until one is applied |

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
  SummaryEstimate     p99 ±1%    ← local ±1%, composed with the child's
    SummaryAgg(Kll)   None       ← state has no guarantee
      Scan t          exact      ← no ASAP descendant
```

Per node kind:

| Node | Today | After |
|---|---|---|
| `SummaryEstimate` | stored at binding: the sketch's own error composed with the child's (`compose_guarantee`) | **derived**: `local_guarantee` composed with the child's. `local_guarantee` is set at binding: the sketch's error over an exact input, `None` when the model has no error model for the family |
| `SummaryAgg` | stored: ExactAggregate family composed with the child's; sketch families `None` | **derived**: ExactAggregate family: exact, composed with the child's under `exact_rule`, except `ExactKind::Count`, exact whatever the child (as today); sketch families `Set(None)`, state has no guarantee |
| `NonASAPOp` | pre-ASAP `QueryExpr`: none<br>post-ASAP `KeepPreAsap`: exact<br>post-ASAP `ValueOperation` / `BinaryOp` / `RelationalJoin` copies: composed at construction | **derived**: composed from the children; exact if no `ASAP` descendant |
| `FinalizeExactAccumulator` | copies the child's | **derived**: the child's |
| `MaintainPopulation` / `ReadPopulation` | stored: exact | **derived**: exact |
| unused variants | `None`: state has no guarantee of its own | unimplemented (§1.3) |
  
### 2.3 Timing: recorded from a lifecycle assignment

The logical layer (PlanSpace, binding, assembly) decides *what* to compute, not where it
runs. The summary maintenance lifecycle layer chooses a lifecycle per unique summary state;
a chosen assignment determines every node's timing (plus window framework and retention).
It is the only source of timing, and the deployment chooses among assignments with its own
costs ([Output layers](../architecture/input-output-workflow.md#output-layers)).

`apply_lifecycle_timings` writes the assignment into the slots, top-down per root with one
shared memo. `Unset` means no assignment has been applied; physical compilation and export
reject it. A node assigned two timings is copied (§4).

```
                    assembly        after an assignment is applied
Project             Unset           QueryTime
  SummaryEstimate   Unset           QueryTime
    SummaryAgg      Unset           IngestionTime   ← the lifecycle maintains the state
      Scan t        Unset           IngestionTime
```

Applying validates, not derives: the assignment is rejected if, e.g., ingestion work depends
on a query-time result, or a node's kind cannot run at its assigned timing.

Some logical candidates hard-code timing today: exact compositions (`OperationPlacement::Read`
/ `Maintenance`, `ValueOperationAtQueryTime` / `ValueOperationAtIngestionTime`),
maintained populations, and the grouped `Rate`→`Sum` placement pair from #472. These become
lifecycle choices over one logical candidate (§8 stage 5).

Per node kind:

| Node | Today | After |
|---|---|---|
| `NonASAPOp` | pre-ASAP `QueryExpr`: none<br>post-ASAP `KeepPreAsap`: from the consuming edge<br>post-ASAP `ValueOperation` / `BinaryOp` copies: a stored field | **assigned**; validated against its consuming edges (§5) |
| `SummaryAgg` | from the child; ingestion time under `KeepPreAsap` | **assigned** by the state's lifecycle |
| `FinalizeExactAccumulator` | a stored field, set by the planner | **assigned**: the same position allows either time |
| `SummaryEstimate` | query time, fixed by the kind | **assigned**; validated: query time only |
| `MaintainPopulation` / `ReadPopulation` | a stored field, always ingestion / query time | **assigned**; validated: ingestion / query time only |
| unused variants | `SummaryMerge`: a stored field; `Join` / `Subtract` / `Delete`: ingestion time | unimplemented (§1.3) |

Unlike a guarantee, a timing depends on the whole DAG and its assignment, so it is applied
only after assembly.

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
                   no timing; checks accuracy on a derived copy, then drops the copy
assembly           builds each root; kept NonASAP nodes stay as they are
derive_guarantees  per root, one shared memo: fills guarantees bottom-up
lifecycle          reads the derived guarantee; chooses an assignment;
                   apply_lifecycle_timings writes and validates timings, copies a
                   node assigned two timings
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

**Candidates stay bottom-up, as today**: a candidate is built on a concrete child plan
(`realize_child_with`, or each child candidate in `prepare_compositions`), so a chosen
plan is complete. Where `realize_child_with` falls back to `keep_pre_asap` today, it
returns the child's original subtree, and assembly keeps it as is.

**Accuracy check during search**: binding sets no `guarantee` slot (§2.2), so the
candidate filter in `search_workload_with_targets` and `prepare_compositions` run
`derive_guarantees` on the candidate alone, with a fresh memo, then check its accuracy target. The derived
copy is only read, then dropped: PlanSpace keeps the original candidate, whose nodes are
shared with other queries.

**Assembly** — one rule replaces `assemble_residual`:

```rust
fn assemble(&self, t: &Rc<Operator>) -> Rc<Operator> {
    memo by ptr;                                          // shared children stay one Rc
    let chosen = if query_time_nested_sum(t) { None }     // as today: keep the outer SUM so the
                 else { self.chosen(t) };                 // inner target's own choice is assembled
    match chosen {
        Some(Subtree(r))           => r,                  // a complete plan, used as is
        Some(ExactComposition{..}) => composition.plan,
        None => t.map_children(|c| if is_target(c) { self.assemble(c) } else { c }),
                                                          // keep the node, assemble its children — assemble_residual does this for four operators only
    }
}
```

Then `GlobalSelection::assemble_selected_dag` runs `derive_guarantees` (§2.2) on each
root it assembles. The `DerivationMemo` lives on
`GlobalSelection` next to `assembled_nodes`, so roots assembled one call at a time still
share nodes. `assemble_selected_dag_with_summary_maintenance_lifecycles` plans lifecycles
on that result, as today: lifecycle planning holds `Rc`s into the plan and reads the
root's guarantee, so it must see the derived tree. It then applies the chosen assignment
with `apply_lifecycle_timings` (§2.3, §5).

- **Illegal child** (e.g. a query-time `SummaryEstimate` under a `SummaryAgg`): an
  assignment that places them so is rejected when applied. The lifecycle layer offers only
  assignments it has checked (as `relink_summary` checks with
  `validate_execution_data_states_at` today), so an error from `apply_lifecycle_timings`
  is a bug, and planning fails with that error.
- **A shared subtree assigned two timings**, e.g. a query-time `Aggregate` and an
  ingestion-time `SummaryAgg` reading one `Scan`: both choices are legal, only the sharing
  is not. `apply_lifecycle_timings` memoizes by (pointer, timing), so it builds one copy per
  timing; a subtree read at one timing stays one `Rc`, within a root or across roots.

`map_children` is `rebuild_children` from
`pre_asap/cse.rs`, dispatching to `NonASAPOp::map_children` / `ASAPOp::map_children`.
Deleted: `assemble_residual`, `keep_pre_asap` / `keep_pre_asap_rc`, and the
`KeepPreAsap` branch of `finalize_exact_accumulator`. Kept: `relink_summary` and the
`query_time_nested_sum` special case, which pick a child after selection today.

| #468 problem | Resolution |
|---|---|
| 1. A `Project` is a `QueryExpr` inside `KeepPreAsap` and a `ValueOperation` outside | one set of types |
| 2. Nothing outside `KeepPreAsap` can reference the `Scan` inside, so an exact aggregate and a sketch cannot share a scan | `Aggregate` and `SummaryAgg` can point to the same `Scan`. This holds when both run at the same time; otherwise the scan is copied (above). They share under a lifecycle assignment that places both at query time (§2.3). Splitting a multi-measure `Aggregate` into exact + sketch is a binding rule, out of scope (§9) |
| 3. `SetOp` and similar have no post-ASAP copy, so no summary below them | `SetOp` takes `None => t`; both children are assembled |

## 5. Timing: execution data states

`validate_execution_data_states` becomes the validation step of `apply_lifecycle_timings`:
instead of deriving timings into the `ExecutionDataStateAssignment` side table (deleted),
it checks the timings the assignment wrote into the slots. Nothing is derived from the
consuming edge any more:

| Node | Produced state |
|---|---|
| `NonASAPOp` | `{assigned timing, Raw / …}`, checked against each consuming edge (`QUERY_ROWS` at the root) |
| `SummaryAgg` | `{assigned timing, SummaryState}` (§2.3) |
| other `ASAPOp` | unchanged rules, checked against the assigned timing |

A data state is the `timing` slot plus a primitive (`Raw` / `SummaryState` / …) fixed by
the kind; only the timing is stored.

The `KeepPreAsap` / `BinaryOp` / `ValueOperation` / `RelationalJoin` arms of today's
`validate_execution_data_states` merge into one `NonASAP` arm of that validation:

- Check each child's assigned state against the edge; an `ASAP` child is checked by the
  `ASAP` edge rules. Ingestion work cannot depend on a query-time result.
- `check_plain_operands` stays: referenced columns must be `FieldType::DataType`
  (`Project` / `Filter` / `Sort` / `Limit` may pass `ExactAggregate` columns through).
  This rejects `Project(ASAP(SummaryAgg))`.
- `BinaryOp`'s ingestion-side constraints move into this arm.
- `AmbiguousKeepPreAsap` is deleted: a subtree assigned two timings is copied (§4).

## 6. Export: fragments in the post-ASAP DAG

The four original-operator payloads (`fallback{expression: QueryExpr}`, `binary`,
`value`, `relational_join`) become one:

```rust
PostAsapOperatorPayload::Relational {
    /// No ASAP node inside. Leaves are Scans, or Scan { source: Source::DagInput { role } }
    /// for an incoming edge whose schema is the edge's intermediate_schema.
    expression: Operator,
}
```

`compile_post_asap_dag` takes each **largest connected subtree without `ASAP`** as one
fragment, cutting an edge with a `DagInput` leaf wherever it meets an `ASAP` node.
`ASAP` nodes map one-to-one onto the existing summary payloads;
`FinalizeExactAccumulator` / `MaintainPopulation` / `ReadPopulation` stay
`value{operation}`. A backend lowers every fragment with its existing `QueryExpr`
lowering plus a `DagInput` arm (an incoming edge as a materialized table); the
`binary` / `value::Project` / `relational_join` lowerings go.

- **Wire 5 → 6**: three fewer payloads; `fallback` becomes `relational` with `DagInput`
  leaves; `output_schema` / `intermediate_schema` become `Schema`. One cutover (§8
  stage 4), together with the downstream readers.
- **Timing and guarantee** are read from the node slots: timing as written by the applied
  lifecycle assignment, guarantee as derived. An `Unset` slot is rejected; for timing it
  means no assignment was applied. An edge's `data_state` is its producer's assigned
  timing plus the primitive of its kind (§5). `compile_post_asap_dag` splits precompute and
  query DAGs by that timing and no longer re-runs data-state validation.
- **`SummaryMerge`** stays a wire payload, although its planner-side variant is
  unimplemented (§1.3, §10).
- **Phases** become per fragment. Switching phase inside a fragment would need a
  materialization point, and those are `ASAP` nodes, so nothing is lost.

## 7. Other consumers

| Location | Change |
|---|---|
| `post_asap/cse.rs` | delete; `share_common_subtrees` covers `ASAPOp` (derives `PartialEq` + serde) |
| `dag_export.rs` | delete `build_summary` / `build_summary_hybrid` / `summary_kind_tag`; one exporter with an `ASAP` arm; update the viewer's `node-style.js` and the pin test `viewer_categorizes_exactly_the_exported_node_kinds` |
| `summary_maintenance_cost/estimator.rs` (80 sites) | `KeepPreAsap` branches (`query_source_selections`, `retained_queries`) use the §6 fragment; `exact_binary` / `value_operation` costs fold into it. Also fixes the missing `RelationalJoin` arm in `summary_operation_evidence` |
| `physical_plan_cost_model.rs::estimate_candidate` | every fragment goes through `lower_query_physical_dag` |
| `summary_maintenance_lifecycle.rs` | `selected_raw_recompute` becomes `!contains_asap(root)`; the `keep_pre_asap(target)` fallback in `assemble_selected_dag_with_summary_maintenance_lifecycles` becomes `target` |
| `maintained_population.rs` | `KeepPreAsap(source)` becomes `source`; `population.matches_input` reads an `NonASAP` child directly |
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
| 2 Two levels | §1.1, §1.4: `Operator<C>`, an empty `ASAPOp`, `contains_asap()`, `expect_non_asap()`; child slots become `Rc<Operator<C>>`; every variant gets `timing` / `guarantee` slots, and nodes are built through constructors that leave both `Unset` (`timing` stays `Unset` until a lifecycle assignment is applied) | every crate; the same mechanical change everywhere |
| 3 One schema | §2.1: `Column` → `Field` and `Schema.columns` → `fields` (serde keeps the name `columns` until stage 4); `FieldType`, `ASAPType`, `PlainField`, `Schema` everywhere except the `post_asap_dag.rs` wire types, which keep `SummarySchema` until stage 4. **No wire change** | `asap-types` + schema construction in every crate |
| 4 New types | fill `ASAPOp`; `ASAP` arms of `output_schema`; `derive_guarantees` and `apply_lifecycle_timings` (§2.2, §2.3, §5); the entry check (§3); `flatten(&SummaryNode) -> Rc<Operator>` so export runs on the new types, copying each node's guarantee into its slot and applying each node's current timing as the initial lifecycle assignment, so the export is unchanged; wire types become `Schema`, and `Schema.fields` serializes as `fields`. Wire → 6. **The only wire-breaking stage**; merged together with ASAPQuery-backend and ASAPCollector | `asap-types`, `devtools`, viewer |
| 5 Planner | §4: candidates and assembly on `Rc<Operator>`; §7 moves to the new types; delete `flatten`. Timing switches to lifecycle-applied: binding and candidates set none, and candidates that hard-code timing (§2.3) become lifecycle choices; paths without lifecycle planning apply a default assignment that reproduces today's timings | `asap-aware-mapping` |
| 6 Cleanup | delete `SummaryExpr`, `SummaryNode`, extra `ValueOperation` variants, `ExactOperation`, `post_asap/cse.rs`; delete today's timing fallbacks (`produced_data_state` defaults, `validate_execution_data_states_at`); update `post-asap-ir.md` (execution phase from the lifecycle assignment), `physical-plan-integration.md`, developer and viewer docs | docs |

Wrapping pre-ASAP operators in `ValueOperation` first is not planned: stage 4 gives
the same early flat export, on the final types.

**Tests**:

- One integration test per #468 problem:
  1. `WITH metric AS (SELECT avg(CASE WHEN l_quantity BETWEEN 1 AND 50 THEN 1.0 ELSE 0.0 END) AS in_range FROM lineitem) SELECT in_range, in_range = 1.0 AS ok FROM metric` — no post-ASAP-only node besides `ASAP`; all `Project`s are one variant.
  2. `SELECT avg(l_extendedprice), approx_percentile_cont(l_discount, 0.99) FROM lineitem` — the `avg` `Aggregate` and the KLL `SummaryAgg` share one `Scan` by `Rc::ptr_eq` when both run at the same time (once a binding rule splits measures); under the default assignment (sketch at ingestion time) the `Scan` is copied.
  3. `SELECT approx_distinct(l_partkey) FROM lineitem UNION ALL SELECT approx_distinct(l_suppkey) FROM lineitem` — each side of the `SetOp` has a `SummaryEstimate`.
- A shared `Scan` assigned two timings is copied once per timing; assigned one timing, it stays one `Rc`.
- A node shared by two roots, assembled in two calls, is still one `Rc` after `derive_guarantees` and `apply_lifecycle_timings`.
- After both, no slot is `Unset`; export rejects a tree with an `Unset` timing (no assignment applied).
- Applying an assignment where ingestion work reads a query-time result is rejected.
- Exported timings of today's plans are unchanged under the default assignment.
- A kept `NonASAP` node (e.g. a `SetOp`) reports the guarantee composed from its assembled children.
- Each unused branch (§1.3) returns `Unimplemented` from `output_schema`, `derive_guarantees`, `apply_lifecycle_timings` and export.
- `search_workload*` panics on a root containing `ASAP`.
- Rewrite the 117 `SummaryExpr::` assertions in `sql_to_post_asap.rs` / `promql_to_post_asap.rs` / `exact_composition.rs`.
- Wire 6 round trip with a `DagInput` fragment; a version-5 document is rejected.
- The 52 `execution_data_state.rs` tests keep their shapes; assertions read the `timing` slot after the default assignment is applied, instead of `ExecutionDataStateAssignment`.

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

## 10. Open questions

- Does ASAPQuery insert `SummaryMerge` only on the exported post-ASAP DAG, or through ASAPPlanner's
  post-ASAP types? The planner-side variant is unimplemented (§1.3).
- Which type carries a `LifecycleAssignment` (derived from `SummaryMaintenanceLifecyclePlan`
  or a new one), and how the lifecycle layer expands a per-state choice to per-node timing.
