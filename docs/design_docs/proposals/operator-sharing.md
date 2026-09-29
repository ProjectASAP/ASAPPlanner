# Sharing Operators Between Pre-ASAP IR and Post-ASAP IR

> Status: proposed, not implemented. Problem statement:
> [#468](https://github.com/ProjectASAP/ASAPPlanner/issues/468). Implementation
> starts after the single entry point and the pluggable pass (#429, #430) land on
> `main`. Builds on [Decoupling operators from scalar expressions](decoupling_op_and_expr.md)
> (same PR), which splits `QueryExpr` into `NonASAPOp` and `ScalarExpr`. Code is
> referenced by file and function; counts are approximate, measured on `main` at `8acb472`.

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
| I. New IR | §1 Types, §2 Per-node information |
| II. Changes, in data-flow order | §3 Entry → §4 Planner → §5 Timing → §6 Export → §7 Other consumers |
| III. Implementation | §8 Stages and tests, §9 Out of scope, §10 Open questions |

---

# I. New IR

## 1. Types

### 1.1 Overview

Operator attributes differ in how widely they apply. Each is defined at the level
that matches its breadth:

| Applies to | Examples | Defined as |
|---|---|---|
| every operator | children, schema, timing, guarantee | methods of `Operator`; the values may be stored per variant |
| one category | for all `NonASAP` operators, timing is derived from the consuming edge, and guarantee from the children | a variant of `Operator` |
| one operator | `Aggregate.measures`, `SummaryAgg.family` | fields of that variant |

```rust
pub enum Operator<C: ColState = ColumnId> {         // category
    NonASAP(NonASAPOp<C>),                          // today's relational QueryExpr variants (§1.2)
    ASAP(ASAPOp<C>),                                // summary operators (§1.3)
}

impl<C: ColState> Operator<C> {                     // every operator
    pub fn children(&self) -> Vec<&Rc<Operator<C>>>;
    pub fn map_children(&self, f: impl FnMut(&Rc<Operator<C>>) -> Rc<Operator<C>>) -> Self;
    pub fn output_schema(&self) -> Result<Schema, SchemaError>;   // computed (§2.1)
    pub fn timing(&self) -> &Slot<ExecutionTiming>;              // §2.3
    pub fn guarantee(&self) -> &Slot<Option<ResultGuarantee>>;   // §2.2; Set(None): unknown, never read as exact
    pub fn with_timing(&self, timing: ExecutionTiming) -> Self;
    pub fn with_guarantee(&self, guarantee: Option<ResultGuarantee>) -> Self;
}
pub enum Slot<T> { Unset, Set(T) }

pub fn derive_guarantees(root: &Rc<Operator>, model: &dyn AccuracyModel,
                         evidence: &dyn AccuracyEvidenceProvider) -> Result<Rc<Operator>, AccuracyError>; // §2.2
pub fn derive_timings(root: &Rc<Operator>) -> Result<Rc<Operator>, ExecutionDataStateError>; // §2.3, §5
```

### 1.2 `NonASAPOp`

`NonASAPOp` and `ScalarExpr` come from splitting `QueryExpr`
([decoupling doc](decoupling_op_and_expr.md#2-types)). Here `NonASAPOp` becomes the
non-ASAP category of `Operator`: its child slots widen from `Rc<NonASAPOp>` to
`Rc<Operator>` (§1.4), and every variant gets the `timing` / `guarantee` slots (§1.1).
Scalar fields are unchanged; only `NonASAPOp` holds them.

```text
Operator<C>
├─ NonASAP(NonASAPOp<C>)
│   ├─ children:   Rc<Operator<C>>                  → back to Operator<C>: NonASAP or ASAP
│   ├─ timing / guarantee slots
│   └─ scalar fields: Predicate<C> / ProjectItem<C> / SortKey<C> / ScalarBridge / ...
│                       └─ ScalarExpr<C>: never contains an Operator
└─ ASAP(ASAPOp<C>)
    ├─ children:   Rc<Operator<C>>                  → back to Operator<C>: NonASAP or ASAP
    └─ timing / guarantee slots
```

### 1.3 `ASAPOp`

```rust
pub enum ASAPOp<C: ColState = ColumnId> {
    SummaryAgg      { child: Rc<Operator<C>>, family: ASAPType, input, reduction, grouping,
                      exact_rule: Option<CompositionOperator> },      // ExactAggregate only, §2.2
    SummaryEstimate { child, query: SketchQuery,
                      local_guarantee: Option<ResultGuarantee> },     // §2.2
    SummaryMerge    { children: Vec<Rc<Operator<C>>> },
    SummarySubtract { left, right },
    SummaryDelete   { child, key: C },                        // key was ColumnRef
    SummaryJoin     { outer, inner, key: C, family: ASAPType },
    // the summary-specific ValueOperation variants, lifted to the top level
    FinalizeExactAccumulator { child },
    MaintainPopulation       { child, population },           // always ingestion time
    ReadPopulation           { child, readout },              // always query time
    Extension                { child, name: String },
}
```

`family` was the original `SummaryFamilyType`.

Every variant also carries the `timing` and `guarantee` slots (§1.1), omitted above.

**Unexercised variants**: `SummaryMerge`, `SummarySubtract`, `SummaryDelete`, `SummaryJoin`
and `Extension` are built only in tests today. They are migrated, but all their methods
return `Unimplemented`.

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

`Concat.children` is `Vec<QueryExpr>` today: branches are stored by value and have no `Rc` identity, so the planner (§4), which identifies targets and holes by pointer,
cannot replace a branch — e.g. the branches of SQL `ROLLUP` or PromQL `histogram_quantiles`. It becomes `Vec<Rc<Operator>>` (§8 stage 0).

## 2. Per-node information

Besides children (§1.4), every operator has the three attributes below: the
every-operator level of §1.1. Each row says where the value lives and who supplies it.

| Field | Meaning | Today | After |
|---|---|---|---|
| `schema` | output columns and their types | stored on every `SummaryNode` | obtained by `output_schema()` (§2.1) |
| `guarantee` | how far the output value can be off | stored on every `SummaryNode` | obtained by `guarantee()`: derived for every node; binding stores only a sketch's own error, as a `SummaryEstimate` field (§2.2) |
| `timing` | ingestion time or query time | on `BinaryOp` / `ValueOperation` / `SummaryMerge` | obtained by `timing()`: set by binding and lifecycle (`SummaryAgg`) or the planner (`FinalizeExactAccumulator`); derived for the rest (§2.3) |

### 2.1 Schema: fused into one type

Today pre-ASAP uses `Schema { columns: Vec<Column>, time_index, unique_keys, closed }`
with `Column.dtype: DataType` (plain values only), and post-ASAP stores a
`SummarySchema { fields: Vec<SummaryField>, time_index }` on every node, with
`SummaryField.dtype: SummaryFamilyType` (`Plain(DataType)` or summary state). One
tree now needs one schema type whose columns can be either:

```
SummaryEstimate(Quantile .99)   → [p99: DataType(Float64)]
  SummaryAgg(Kll)               → [state: ASAPType(Sketch(Kll, k=269))]
    Scan lineitem               → [l_orderkey: DataType(Int64), l_quantity: DataType(Float64), …]
```

We merge semantics of the above two types into one `Schema` type. 
The struct of `Schema` stays, with two changes:

- `Column` is renamed `Field`, and `Schema.columns` `Schema.fields`: the struct describes
  a column and holds none of its data. Arrow and DataFusion use the same names.
- `Field.dtype` widens from `DataType` to an enum `FieldType`, so a field can describe either a
  plain value or ASAP state.

`SummarySchema` / `SummaryField` are then redundant and deleted:

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

Today every `SummaryNode` stores its schema, built at construction. After, every node
computes it with `output_schema()`:

| Node | Today | After |
|---|---|---|
| `NonASAPOp` | `QueryExpr::output_schema()` lifted to `SummarySchema` (`KeepPreAsap`), or stored on the `ValueOperation` / `BinaryOp` / `RelationalJoin` copy | today's `QueryExpr::output_schema()` logic |
| `SummaryAgg` | the replaced `Aggregate`'s output with the measure column retyped to `family` | grouping columns + one `ASAPType(family)` column |
| `SummaryEstimate` | the replaced operator's output schema | the child's grouping columns + the value columns of the `SketchQuery` |
| `FinalizeExactAccumulator` | the logical operator's output, lifted | the child's schema, `ASAPType(ExactAggregate ..)` columns turned into `DataType(..)` |
| `MaintainPopulation` / `ReadPopulation` | the source's schema / the replaced aggregate's output | the same rules, computed from the child and the `readout` |
| unexercised variants | one field typed `family` | unimplemented (§1.3) |

### 2.2 Guarantee: always derived

| Node | Today | After |
|---|---|---|
| `SummaryEstimate` | stored at binding: the sketch's own error composed with the child's (`compose_guarantee`) | **derived**: `local_guarantee` composed with the child's. `local_guarantee` is a field set at binding: the sketch's error over an exact input, `None` when the model has no error model for the family |
| `SummaryAgg` | stored: ExactAggregate family composed with the child's; sketch families `None` | **derived**: ExactAggregate family: exact, composed with the child's under `exact_rule`; sketch families `Set(None)`, state has no guarantee |
| `NonASAPOp` | `KeepPreAsap`: exact; the `ValueOperation` / `BinaryOp` / `RelationalJoin` copies: composed at construction | **derived**: composed from the children (`relational_join_guarantee`, `exact_operation_rule`); exact if no `ASAP` descendant |
| `FinalizeExactAccumulator` | copies the child's | **derived**: the child's |
| `MaintainPopulation` / `ReadPopulation` | stored: exact | **derived**: exact |
| unexercised variants | `None`: state has no guarantee of its own | unimplemented (§1.3) |

- **Only the local part is stored.** Today binding stores the composed value, built
  from the child it sees. Assembly can fill that child's hole with a different plan, and
  a stored composition would go stale (`relink_agg_child` copies it today, relying on the
  new child being exact).
- `derive_guarantees` uses the same `AccuracyModel` as binding, and the evidence for
  `propagate`'s `PropagationStats`. It reads only the subtree, so search runs it on a
  candidate to check its accuracy target (the candidate filter in
  `search_workload_with_targets`).
- The value travels with the node through cloning, CSE and serialization, as
  `SummaryNode.guarantee` does today.
  
### 2.3 Timing: set where position does not decide it

| Node | Today | After |
|---|---|---|
| `NonASAPOp` | `KeepPreAsap`: from the consuming edge; the `ValueOperation` / `BinaryOp` copies: a stored field | **derived** from the consuming edge (§5) |
| `SummaryAgg` | from the child; ingestion time under `KeepPreAsap` | **set**: binding sets `QueryTime`; a lifecycle decision may change it to `IngestionTime` |
| `FinalizeExactAccumulator` | a stored field, set by the planner | **set** by the planner: the same position allows either time |
| `SummaryEstimate` | query time, fixed by the kind | **derived** from the kind: query time |
| `MaintainPopulation` / `ReadPopulation` | a stored field, always ingestion / query time | **derived** from the kind: ingestion / query time |
| unexercised variants | `SummaryMerge`: a stored field; `Join` / `Subtract` / `Delete`: ingestion time | unimplemented (§1.3) |

Unlike a guarantee, a timing depends on the parents, so `derive_timings` needs the whole
DAG and runs only after assembly.

---

# II. Changes, in data-flow order

## 3. Optimizer entry

Frontends and `resolve` build `NonASAP` trees only and access children with
`expect_non_asap()`. `ParsedWorkload::new` rejects a tree that `contains_asap()`,
next to its existing entry-count check:

```rust
impl<C> Operator<C> {
    pub fn contains_asap(&self) -> bool;
    pub fn expect_non_asap(&self) -> &NonASAPOp<C>;   // an ASAP node here is a bug: panic
}
```

A compile-time alternative — an associated type on `ColState` with
`ColumnRef::ASAP = Never` — only protects frontend code before `resolve`: frontends
already return `ColumnId` trees, where `ASAP` is allowed. The entry check covers every
input (frontends after `resolve`, deserialized plans, third-party frontends, test IR)
with simpler types.

## 4. Planner: search and assembly

```rust
pub enum Replacement {
    Subtree(Rc<Operator>),     // formerly Summary(Rc<SummaryNode>) and Rewrite(Rc<QueryExpr>)
    ExactComposition { .. },   // its plan becomes Rc<Operator>
}
```

**Holes**: a subtree of a candidate that is `Rc::ptr_eq` to a target is filled by that
target's own choice. A candidate needing a specific child implementation (e.g.
`realize_temporal_average`) inlines a new node, so it is not a hole.
`realize_child_with` falls back to leaving a hole instead of `keep_pre_asap`.

**Assembly** — one rule replaces `assemble_residual`:

```rust
fn assemble(&self, t: &Rc<Operator>) -> Rc<Operator> {
    memo by ptr;                                          // shared children stay one Rc
    let body = match self.chosen(t) {
        Some(Subtree(r))           => r,                  // rewrites are assembled further down too; today assemble_target wraps them in keep_pre_asap
        Some(ExactComposition{..}) => composition.plan,
        None                       => t,                  // keep the node, recurse — assemble_residual does this for four operators only
    };
    body.map_children(|c| if is_target(c) { self.assemble(c) } else { c })
}
```

Then run `derive_timings` (§5) once:

- **Illegal fill** (e.g. a query-time `SummaryEstimate` under a `SummaryAgg`):
  `derive_timings` returns an error, and the hole falls back to its original subtree —
  `relink_summary`'s fallback, for every operator.
- **A shared subtree read at two timings**, e.g. a query-time `Aggregate` and an
  ingestion-time `SummaryAgg` reading one `Scan`: both choices are legal, only the sharing
  is not. `derive_timings` memoizes by (pointer, timing), so it builds one copy per
  timing; a subtree read at one timing stays one `Rc`.

Finally run `derive_guarantees` (§2.2). `map_children` is `rebuild_children` from
`pre_asap/cse.rs`, dispatching to `NonASAPOp::map_children` / `ASAPOp::map_children`.
Deleted: `assemble_residual`, `relink_summary`, the `query_time_nested_sum` /
`contains_aggregate` special cases, `keep_pre_asap` / `keep_pre_asap_rc`, and the
`KeepPreAsap` branch of `finalize_exact_accumulator`.

| #468 problem | Resolution |
|---|---|
| 1. A `Project` is a `QueryExpr` inside `KeepPreAsap` and a `ValueOperation` outside | one set of types |
| 2. Nothing outside `KeepPreAsap` can reference the `Scan` inside, so an exact aggregate and a sketch cannot share a scan | `Aggregate` and `SummaryAgg` can point to the same `Scan`. This holds when both run at the same time — always, without lifecycle decisions; otherwise the scan is duplicated (above). Splitting a multi-measure `Aggregate` into exact + sketch is a binding rule, out of scope (§9) |
| 3. `SetOp` and similar have no post-ASAP copy, so no summary below them | `SetOp` takes `None => t`; both children are assembled |

## 5. Timing: execution data states

`validate_execution_data_states` becomes `derive_timings`: instead of returning the
`ExecutionDataStateAssignment` side table (deleted), it writes each node's `timing` slot.
`produced_data_state(KeepPreAsap) = None` (set by the consuming edge) extends to all
`NonASAPOp`s:

| Node | Produced state |
|---|---|
| `NonASAPOp` | set by the consuming edge (`QUERY_ROWS` at the root), passed to its children |
| `SummaryAgg` | `{timing, SummaryState}` from its `timing` slot (§2.3); a `NonASAPOp` child takes the same timing |
| other `ASAPOp` | unchanged |

A data state is the `timing` slot plus a primitive (`Raw` / `SummaryState` / …) fixed by
the kind; only the timing is stored.

The `KeepPreAsap` / `BinaryOp` / `ValueOperation` / `RelationalJoin` arms of today's
`validate_execution_data_states` merge into one `NonASAP` arm of `derive_timings`:

- Pass the state to each child; an `ASAP` child is checked by the `ASAP` edge rules.
- `check_plain_operands` stays: referenced columns must be `FieldType::DataType`
  (`Project` / `Filter` / `Sort` / `Limit` may pass `ExactAggregate` columns through).
  This rejects `Project(ASAP(SummaryAgg))`.
- `BinaryOp`'s ingestion-side constraints move into this arm.
- `AmbiguousKeepPreAsap` is deleted: a subtree read at two timings is copied (§4).

## 6. Export: fragments in the executable DAG

The four original-operator payloads (`fallback{expression: QueryExpr}`, `binary`,
`value`, `relational_join`) become one:

```rust
ExecutableOperatorPayload::Relational {
    /// No ASAP node inside. Leaves are Scans, or Scan { source: Source::DagInput { role } }
    /// for an incoming edge whose schema is the edge's intermediate_schema.
    expression: Operator,
}
```

`compile_executable_dag` takes each **largest connected subtree without `ASAP`** as one
fragment, cutting an edge with a `DagInput` leaf wherever it meets an `ASAP` node.
`ASAP` nodes map one-to-one onto the existing summary payloads;
`FinalizeExactAccumulator` / `MaintainPopulation` / `ReadPopulation` stay
`value{operation}`. A backend lowers every fragment with its existing `QueryExpr`
lowering plus a `DagInput` arm (an incoming edge as a materialized table); the
`binary` / `value::Project` / `relational_join` lowerings go.

- **Wire 5 → 6**: three fewer payloads; `fallback` becomes `relational` with `DagInput`
  leaves; `output_schema` / `intermediate_schema` become `Schema`. One cutover (§8
  stage 4), together with the downstream readers.
- **Timing and guarantee** are read from the node slots; an `Unset` slot is rejected.
  An edge's `data_state` is its producer's timing plus the primitive of its kind (§5).
  `compile_executable_dag` no longer re-runs data-state validation.
- **Phases** become per fragment. Switching phase inside a fragment would need a
  materialization point, and those are `ASAP` nodes, so nothing is lost.

## 7. Other consumers

| Location | Change |
|---|---|
| `post_asap/cse.rs` | delete; `share_common_subtrees` covers `ASAPOp` (derives `PartialEq` + serde) |
| `dag_export.rs` | delete `build_summary` / `build_summary_hybrid` / `summary_kind_tag`; one exporter with an `ASAP` arm; update the viewer's `node-style.js` and the pin test `viewer_categorizes_exactly_the_exported_node_kinds` |
| `summary_maintenance_cost/estimator.rs` (80 sites) | `KeepPreAsap` branches (`query_source_selections`, `retained_queries`) use the §6 fragment; `exact_binary` / `value_operation` costs fold into it. Also fixes the missing `RelationalJoin` arm in `summary_operation_evidence` |
| `physical_plan_cost_model.rs::estimate_candidate` | every fragment goes through `lower_query_physical_dag` |
| `summary_maintenance_lifecycle.rs` | `selected_raw_recompute` becomes `!contains_asap(root)`; the `keep_pre_asap(target)` fallback in `assemble_selected_dag_with_summary_maintenance_lifecycles` becomes `target`; the lifecycle decision sets a `SummaryAgg` to `IngestionTime` with `with_timing` (§2.3) |
| `maintained_population.rs` | `KeepPreAsap(source)` becomes `source`; `population.matches_input` reads an `NonASAP` child directly |
| `exact_composition.rs` | `ExactOperation::Aggregate` becomes an `NonASAP(Aggregate)` whose child is a hole |
| `RelationalJoin.pruning` | never set to `Some` in production; delete. Candidate pruning can return as an `ASAPOp` variant |

---

# III. Implementation

## 8. Stages and tests

`main` builds and passes all tests after every stage.

| Stage | Content | Touches |
|---|---|---|
| 0 Preparation | `Rc` for `Concat.children`; `rebuild_children` → `map_children`; `Column::plain` | `asap-types` |
| 1 Split | [decoupling doc](decoupling_op_and_expr.md): `NonASAPOp` + `ScalarExpr`; children stay `Rc<NonASAPOp>` | scalar code ([decoupling doc §3](decoupling_op_and_expr.md#3-changes)) |
| 2 Two levels | §1.1, §1.4: `Operator<C>`, an empty `ASAPOp`, `contains_asap()`, `expect_non_asap()`; child slots become `Rc<Operator<C>>`; every variant gets `timing` / `guarantee` slots, left `Unset` | every crate; the same mechanical change everywhere |
| 3 One schema | §2.1: `Column` → `Field` and `Schema.columns` → `fields` (serde keeps the name `columns` until stage 4); `FieldType`, `ASAPType`, `PlainField`, `Schema` everywhere except the `executable_dag.rs` wire types, which keep `SummarySchema` until stage 4. **No wire change** | `asap-types` + schema construction in every crate |
| 4 New types | fill `ASAPOp`; `ASAP` arms of `output_schema`; `derive_guarantees` and `derive_timings` (§2.2, §5); the entry check (§3); `flatten(&SummaryNode) -> Rc<Operator>` so export runs on the new types; wire types become `Schema`, and `Schema.fields` serializes as `fields`. Wire → 6. **The only wire-breaking stage**; merged together with ASAPQuery-backend and ASAPCollector | `asap-types`, `devtools`, viewer |
| 5 Planner | §4: candidates and assembly on `Rc<Operator>`; §7 moves to the new types; delete `flatten` | `asap-aware-mapping` |
| 6 Cleanup | delete `SummaryExpr`, `SummaryNode`, extra `ValueOperation` variants, `ExactOperation`, `post_asap/cse.rs`; update `post-asap-ir.md`, `physical-plan-integration.md`, developer and viewer docs | docs |

Wrapping pre-ASAP operators in `ValueOperation` first is not planned: stage 4 gives
the same early flat export, on the final types.

**Tests**:

- One integration test per #468 problem:
  1. `WITH metric AS (SELECT avg(CASE WHEN l_quantity BETWEEN 1 AND 50 THEN 1.0 ELSE 0.0 END) AS in_range FROM lineitem) SELECT in_range, in_range = 1.0 AS ok FROM metric` — no post-ASAP-only node besides `ASAP`; all `Project`s are one variant.
  2. `SELECT avg(l_extendedprice), approx_percentile_cont(l_discount, 0.99) FROM lineitem` — the `avg` `Aggregate` and the KLL `SummaryAgg` share one `Scan` by `Rc::ptr_eq` (once a binding rule splits measures).
  3. `SELECT approx_distinct(l_partkey) FROM lineitem UNION ALL SELECT approx_distinct(l_suppkey) FROM lineitem` — each side of the `SetOp` has a `SummaryEstimate`.
- A shared `Scan` read at two timings is copied once per timing; read at one timing, it stays one `Rc`.
- After `derive_*`, no slot is `Unset`; export rejects a tree with one.
- A `SummaryEstimate` over a hole reports the error of the plan that fills the hole.
- Each unexercised variant (§1.3) returns `Unimplemented` from `output_schema`, `derive_*` and export.
- `ParsedWorkload::new` rejects a tree containing `ASAP`.
- Rewrite the 117 `SummaryExpr::` assertions in `sql_to_post_asap.rs` / `promql_to_post_asap.rs` / `exact_composition.rs`.
- Wire 6 round trip with a `DagInput` fragment; a version-5 document is rejected.
- The 52 `execution_data_state.rs` tests keep their shapes; assertions read the `timing` slot instead of `ExecutionDataStateAssignment`.

## 9. Out of scope

- The binding rule splitting a multi-measure `Aggregate` into exact + summary over one child.
- Candidate pruning as an `ASAPOp` variant.
- Accuracy through `SummaryMerge` / `Subtract` / `Delete` / `Join` (§2.2): an accuracy
  descriptor on state, or composing error along the state chain at readout.
- Folding `ExactComposition` into `Subtree` (both of its forms become expressible);
  deferred until stage 5 is stable.

## 10. Open questions

None at present.
