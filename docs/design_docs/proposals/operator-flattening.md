# Sharing Operators Between Pre-ASAP IR and Post-ASAP IR

> Status: proposed, not implemented. Problem statement:
> [#468](https://github.com/ProjectASAP/ASAPPlanner/issues/468). Code
> references and counts are against `main` at `8acb472`.

**The idea.** Today the post-ASAP-plan is glued together by different operator types. 
This proposal keeps one operator language and makes summary operators extra node kinds in it: any relational operator can sit above a summary, and a summary can read any relational subtree.
Nothing is wrapped and nothing is duplicated.

```
Today                                            Proposed
ValueOperation(Project)         ← a copy         Project
  SummaryEstimate                                  Ext(SummaryEstimate)
    SummaryAgg(Kll)                                  Ext(SummaryAgg(Kll))
      KeepPreAsap(Scan lineitem) ← a black box         Scan lineitem
```

Concretely: split `QueryExpr` into operators (`OriginalOp`) and scalar
expressions (`ScalarExpr`), make every operator's children `Rc<Operator>` with
`Operator = Basic(OriginalOp) | Ext(ASAPOp)`, and delete the parallel post-ASAP
types (`SummaryExpr`, `SummaryNode`, `SummarySchema`, and the relational variants
of `ValueOperation`).

**Roadmap**:

| Part | Sections | Question answered |
|---|---|---|
| I. New IR | §1 Types, §2 Per-node information | Which node kinds exist, and what each carries |
| II. Upstream and downstream changes | §3 Frontends and entry → §4 Planner → §5 Validation → §6 Export → §7 Other consumers | How each stage changes, in data-flow order |
| III. Implementation plan | §8 Stages and tests, §9 Out of scope | How to land it, and what is deliberately left out |

---

# I. New IR

## 1. Types

### 1.1 Overview

```
Operator<C>                         operator: one node of the DAG; every child is Rc<Operator<C>>
├─ Basic(OriginalOp<C>)             original operator: today's relational QueryExpr variants (Scan / Filter / Join / Aggregate / SetOp …)
└─ Ext(ASAPOp<C>)                   ASAP operator: summary operators (SummaryAgg / SummaryEstimate …)

ScalarExpr<C>                       scalar expression: only appears inside operator fields (predicates, SELECT lists); never a node
```

- **Naming**: the suffix says what a type is — `*Op` is an operator, `*Expr` is an
  expression. `OriginalOp` holds the operators pre-ASAP already has, as opposed to
  the extension `ASAPOp`. The name `QueryExpr` goes away.
- **The generic `C`**: the existing column-reference stage of `QueryExpr` —
  `ColumnRef` (by name) out of the frontends, `ColumnId` (by position) after
  `resolve_root`. A parent and its children must be in the same stage, so all
  three types carry the same `C`.

### 1.2 `OriginalOp` and `ScalarExpr`: splitting `QueryExpr`

Today one `QueryExpr` enum holds two kinds of things:

```sql
SELECT l_quantity * 2 AS q2 FROM lineitem WHERE l_quantity > 10
```
```
Project { cols: [ProjectItem { expr: Arithmetic(Column(4) * Literal(2)) }],   ← scalar expression: computes one value per row
          child: Filter { pred: Predicate(Compare(Column(4) > Literal(10))),  ← scalar expression
                          child: Scan lineitem } }                             ← relational operator: rows in, rows out
```

`Filter.child` (rows) and the `Compare` inside `Filter.pred` (a value) are both
`QueryExpr`; only the field position tells them apart. The code already knows
which variants are scalars: `output_schema` returns `ScalarHasNoRowSchema` for all
of them (`types/src/pre_asap/query_expr.rs:1447`). Split them into two types:

```rust
pub enum OriginalOp<C: ColState = ColumnId> {
    Scan { .. }, Filter { pred: Predicate<C>, child: Rc<Operator<C>> }, Project { cols: Vec<ProjectItem<C>>, child },
    Aggregate { .. }, Join { .. }, SetOp { .. }, Concat { .. }, Dedup { .. }, Sort { .. }, Limit { .. }, BinaryOp { .. },
    SQLWindowFunc { .. }, TimeRange { .. }, TimeShift { .. }, Promql* { .. },
    ScalarBridge(Rc<ScalarExpr<C>>),   // formerly PromqlScalarBridge: a scalar in operator position (the `2` in PromQL `v * 2`, a bare scalar query)
    EvalTimestamp,                     // PromQL time(): operator position, one row, one column
}
pub enum ScalarExpr<C: ColState = ColumnId> {     // children are ScalarExpr only
    Column(C), Literal(ScalarValue), Compare { .. }, BoolAnd(..), BoolOr(..), Not(..), IsNull(..), IsNotNull(..),
    Cast { .. }, InList { .. }, FunctionCall { .. }, Arithmetic { .. }, Case { .. }, CurrentTimestamp,
}
pub struct Predicate<C>(pub Rc<ScalarExpr<C>>);
pub struct ProjectItem<C> { pub alias: Option<String>, pub expr: ScalarExpr<C> }
```

- **Ambiguous variants**: `PromqlScalarBridge`, `EvalTimestamp`,
  `PromqlScalarFromVector` and `PromqlVectorFromScalar` appear in operator position
  and have a row schema, so they belong to `OriginalOp`. `CurrentTimestamp` (SQL
  `NOW()`) is used both ways: it has a row schema (`query_expr.rs:1388`) and takes
  part in scalar type inference (`:1674`, e.g. `WHERE ts > NOW()`). It goes into
  `ScalarExpr` and is written `ScalarBridge(CurrentTimestamp)` in operator
  position. **Open**: confirm how the frontends place `CurrentTimestamp` in
  operator position.
- **Benefits**: a scalar in operator position (`Basic(Column(3))`) is no longer
  expressible; the `ScalarHasNoRowSchema` runtime error disappears; `canonicalize`
  and `pre_asap/cse` already walk only the relational skeleton and treat scalars as
  opaque data (see the `pre_asap/cse.rs` module docs) — the types now say so.

### 1.3 `ASAPOp`

```rust
pub enum ASAPOp<C: ColState = ColumnId> {
    SummaryAgg      { child: Rc<Operator<C>>, family: ASAPType, input, reduction, grouping,   // family was SummaryFamilyType, "never Plain" by convention; now by type
                      guarantee: Option<ResultGuarantee> },   // Some only for the ExactAggregate family
    SummaryEstimate { child, query: SketchQuery,
                      guarantee: Option<ResultGuarantee> },   // None = the AccuracyModel has no error model for this family
    SummaryMerge    { children: Vec<Rc<Operator<C>>>, timing: ExecutionTiming },
    SummarySubtract { left, right },
    SummaryDelete   { child, key: C },                        // was ColumnRef; now C like everything else
    SummaryJoin     { outer, inner, key: C, family: ASAPType },
    // The four summary-specific ValueOperation variants, lifted to the top level instead of { child, operation, timing }
    FinalizeExactAccumulator { child, timing: ExecutionTiming },
    MaintainPopulation       { child, population },           // always ingestion time
    ReadPopulation           { child, readout },              // always query time
    Extension                { child, name: String },
}
impl<C> ASAPOp<C> { pub fn guarantee(&self) -> Option<&ResultGuarantee>; }   // only the two variants above store one; see §2.2
```

**Deleted post-ASAP types** — each is expressed by an existing `OriginalOp` variant:

| Deleted | Expressed as |
|---|---|
| `SummaryExpr::KeepPreAsap(q)` | no wrapper: the subtree `q` is itself `Basic(..)` |
| `ValueOperation::{Project, Filter, Sort, Limit}` | `OriginalOp::{Project, Filter, Sort, Limit}` |
| `SummaryExpr::BinaryOp`, `SummaryExpr::RelationalJoin` | `OriginalOp::BinaryOp`, `OriginalOp::Join` |
| `ValueOperation::Exact(Aggregate)`, `ExactOperation` | `OriginalOp::Aggregate` |
| `SummaryNode` (the node wrapper) | not needed; per-node information is covered in §2 |

### 1.4 Child slots

```rust
// Today                                           // After
Filter {                                          Filter {
    pred: Predicate(Rc<QueryExpr>),   // scalar        pred: Predicate(Rc<ScalarExpr>),
    child: Rc<QueryExpr>,             // rows          child: Rc<Operator>,       // either Basic(..) or Ext(SummaryEstimate ..)
}                                                 }
```

**Rule**: every `OriginalOp` field that holds input rows has type `Rc<Operator<C>>`.
There are about 25 such fields — `Filter.child`, `Join.left` / `Join.right`, both
sides of `SetOp`, `Concat.children`, `BinaryOp.lhs` / `BinaryOp.rhs`, the `child` of
each `Promql*` operator, and so on.

**Effect**: an ASAP operator can sit directly under any original operator. Both
sides of a `SetOp`, for example, can be `SummaryEstimate`s.

**Exception: `Concat.children`**. Today it is `Vec<QueryExpr>`: branches are stored
by value and have no `Rc` identity. The planner's search and assembly (§4)
identify nodes by `Rc` pointer — targets are registered by pointer and holes are
recognized with `Rc::ptr_eq` — so a `Concat` branch can be neither a target nor a
hole. This affects, for example, the branches SQL `ROLLUP` and PromQL
`histogram_quantiles` are lowered into. As `Vec<Rc<Operator>>` it is handled like
any other child slot and §4 needs no special case (§8 stage 0).

## 2. Per-node information

A post-ASAP node carries three pieces of information today. After the change:

| Field | Meaning | Today | After |
|---|---|---|---|
| `schema` | which columns the node outputs, and their types | stored on every `SummaryNode` | not stored; `output_schema()` computes it on demand (§2.1) |
| `guarantee` | how far the node's output value can be off (metric, bound, failure probability, provenance) | stored on every `SummaryNode` | stored on `SummaryAgg` / `SummaryEstimate`; derived by `GuaranteeIndex` for everything else (§2.2) |
| `timing` | whether the node runs at ingestion time or at query time | stored on the `BinaryOp` / `ValueOperation` / `SummaryMerge` variants | not stored on `OriginalOp` (§5); kept on `SummaryMerge` and `FinalizeExactAccumulator` |

### 2.1 Schema: two types become one

A schema is a node's list of output columns: name, type, nullability. There are two today:

| | pre-ASAP | post-ASAP |
|---|---|---|
| Type | `Schema { columns: Vec<Column>, time_index, unique_keys, closed }` | `SummarySchema { fields: Vec<SummaryField>, time_index }` |
| Column type | `Column.dtype: DataType` — plain values only (`Int64`, `Float64`, `Utf8`, …) | `SummaryField.dtype: SummaryFamilyType` — a plain value `Plain(DataType)` or summary state (`Sketch(Kll, …)`, …) |
| Where it lives | not stored; `QueryExpr::output_schema()` computes it | stored on every `SummaryNode` |

In the new IR both kinds of operator share one tree, so `Operator::output_schema()`
must describe any node, and a column type must be able to hold either a plain
value or summary state:

```
SummaryEstimate(Quantile .99)   → [p99: DataType(Float64)]
  SummaryAgg(Kll)               → [state: ASAPType(Sketch(Kll, k=269))]
    Scan lineitem               → [l_orderkey: DataType(Int64), l_quantity: DataType(Float64), …]
```

So keep `Schema`, delete `SummarySchema` / `SummaryField`, and give `Column.dtype` a new type, `ColumnType`:

```rust
pub enum ColumnType {
    DataType(DataType),   // a plain value; the existing DataType, unchanged (Int64, Float64, Utf8, …)
    ASAPType(ASAPType),   // ASAP state
}
pub enum ASAPType {       // SummaryFamilyType without its Plain variant
    ExactAggregate(ExactKind, ExactParams), Sketch(SketchKind, GroupingStrategy),
    Sample(SamplingKind, SamplingParams), Wavelet(WaveletKind, WaveletParams), StatModel(StatModelKind, StatModelParams),
}

pub struct Column { pub name, pub dtype: ColumnType, pub nullable, pub table: Option<String> }
impl Column {
    pub fn plain(name, DataType) -> Self;                 // build a plain column (frontends, catalogs)
    pub fn plain_dtype(&self) -> Option<&DataType>;       // None for an ASAP-state column
    pub fn expect_plain_dtype(&self) -> &DataType;        // frontends and scalar type inference: a state column is a bug, panic
}
```

- **Naming**: as in §1, `DataType` / `ASAPType` mirror `OriginalOp` / `ASAPOp`.
  The name `SummaryFamilyType` goes away: once every column carries it, a plain
  column reading `SummaryFamilyType::Plain(Float64)` is a misnomer. With two
  levels, "is this column state?" is a check on the outer variant only — exactly
  the split `check_plain_operands` needs.
- **`DataType` itself is unchanged.** It remains part of the scalar type system
  (the type of a `Literal`, the result type of `Arithmetic`, catalog column
  declarations); it now sits inside `ColumnType::DataType(..)`. Code that reads
  `Column.dtype` expecting a `DataType` unwraps one more level: code that only
  ever sees plain columns (frontends, scalar type inference) uses
  `expect_plain_dtype()`; code that may see state (planner, validation, export)
  uses `plain_dtype()` or matches. A scalar expression never legitimately reads a
  state column — `check_plain_operands` (§5) rejects such a DAG first.
- **Elements of nested types**: `DataType::List { element: Box<Column> }` and
  `DataType::Struct { fields: Vec<Column> }` have `Column` elements. With
  `Column.dtype: ColumnType` an element could become ASAP state (a "list of KLL
  sketches"), which is not intended. These two use a plain-only field instead:

  ```rust
  pub struct PlainField { pub name: String, pub dtype: DataType, pub nullable: bool }   // nested fields are already unqualified (List's doc comment), so no table
  DataType::List   { element: Box<PlainField> }
  DataType::Struct { fields: Vec<PlainField> }
  ```

  `DataType::Map { key: Box<DataType>, value: Box<DataType>, .. }` already uses
  `DataType` only and is unchanged. There are 36 construction or match sites of
  `DataType::{List, Struct, Map}`.
- **Why keep `Schema`**: `ColumnType::DataType(..)` represents every plain type, so
  nothing is lost, and `Schema` additionally has `unique_keys` / `closed` /
  `Column.table`, which merging the other way would drop.
- **Computed on demand**: schemas are no longer stored on nodes; `Operator::output_schema()`
  computes them. `OriginalOp` keeps today's `QueryExpr::output_schema()` logic, and
  `ASAPOp` follows the table below. These rules are currently spread over
  `asap-aware-mapping/src/replacement.rs` and `types/src/post_asap/execution_data_state.rs`;
  they move into `asap-types`.
- **Deleted conversions**: `lift()` (`replacement.rs:3352`) and `lift_plain`
  (`execution_data_state.rs:725`), which turn a `Schema` into a `SummarySchema`, and
  the reverse `plain_schema` (`execution_data_state.rs:707`). With one schema type
  there is nothing to convert.
- **Size of the change**: 7 direct `Column { .. }` literals; the deleted
  `SummarySchema {}` (36) and `SummaryField {}` (19) literals. In non-test code
  `SummaryFamilyType` appears 197 times (42 of them with `Plain(`) and `.dtype` is
  read at 67 sites; all are rewritten against the new types.

| `ASAPOp` | Output schema |
|---|---|
| `SummaryAgg` | the grouping columns as-is + one `ASAPType(family)` column |
| `SummaryEstimate` | the row schema determined by the `SketchQuery` |
| `SummaryMerge` / `Subtract` / `Delete` / `Join` | one `ASAPType(family)` column |
| `FinalizeExactAccumulator` | the child's schema, with each `ASAPType(ExactAggregate ..)` column turned into the matching `DataType(..)` |
| `MaintainPopulation` / `ReadPopulation` | existing implementation |

### 2.2 Guarantee: some stored, some derived

| Node | Where its guarantee comes from |
|---|---|
| `SummaryAgg`, `SummaryEstimate` | **stored in a field**. Computed at binding time by the `AccuracyModel` from the summary's parameters and the evidence; it cannot be recovered afterwards, and selection needs it during search to decide whether a candidate meets the accuracy target (`replacement.rs:5831`) |
| `OriginalOp` (`Project`, `Join`, …), `FinalizeExactAccumulator` | composed from the children by rule (`relational_join_guarantee`; `exact_operation_rule` in `accuracy/composition.rs`). A subtree with no `Ext` descendant is exact |
| `MaintainPopulation`, `ReadPopulation` | always exact |
| `SummaryMerge` / `Subtract` / `Delete` / `Join` | output state, so no guarantee of their own — but they change the error of a later readout (see below) |

The derived part goes into one table:

```rust
/// One traversal, keyed by node pointer — the same approach as the ExecutionDataStateAssignment
/// returned by validate_execution_data_states.
pub struct GuaranteeIndex(HashMap<*const Operator, ResultGuarantee>);
pub fn guarantee_index(root: &Rc<Operator>) -> GuaranteeIndex;
```

- The composition rules also depend on the `AccuracyModel`, so the planner must
  compute `GuaranteeIndex` with the same model and hand it over together with the
  assembled root (`assemble_selected_dag`). `compile_executable_dag` uses it to fill
  `ExecutableDagNode.guarantee`.
- Not chosen: wrapping every node to store a guarantee — frontends and `resolve`
  would then have to build the wrapper too.
- **Gap for state nodes**: `SummaryMerge` / `Subtract` / `Delete` / `Join` change the
  error of a later readout. For a CMS, `a − b` has error bound `ε·(‖a‖₁ + ‖b‖₁)`, so
  the relative error is large when `a` and `b` are close. A readout's guarantee is
  computed today as if its input were a sketch built directly by a `SummaryAgg`,
  ignoring any intermediate operation. The planner on `main` never emits these four
  nodes (they are built only in tests, and `SummaryMerge` is documented as inserted by
  a deployment), so the gap is latent. Not addressed here (§9).

---

# II. Upstream and downstream changes

## 3. Frontends and the optimizer entry

Trees built by the frontends and by `resolve` contain only `Basic`. They access
children through `expect_basic()`; before a tree reaches the optimizer, the entry
checks it once and rejects any `Ext`:

```rust
impl<C> Operator<C> {
    pub fn contains_ext(&self) -> bool;
    pub fn expect_basic(&self) -> &OriginalOp<C>;   // an Ext here is a bug: panic
}
```

**Where the check goes**: on `main` the optimizer entries are `search_workload` /
`search_workload_with` / `search_workload_with_targets`. They do not return
`Result`, so the check starts as `assert!(!root.contains_ext())` at each entry. If
the single `ParsedWorkload` entry point (#429) lands on `main` first, the check
moves into `ParsedWorkload::new` and returns an error (it already checks the entry
count).

**Why not a compile-time guarantee** (an associated type `Ext` on `ColState`, with
`ColumnRef::Ext = Never`):

| | Compile time | Run time (this proposal) |
|---|---|---|
| Types | one more associated type on `ColState`, plus an empty enum | the `Ext` variant holds `ASAPOp` directly |
| What it catches | only frontend code before `resolve`. A frontend already returns a `ColumnId` tree, where `Ext` is allowed | **every input to the optimizer**: frontend code after `resolve`, deserialized plans, third-party frontends, hand-written test IR |
| Frontends matching children | `basic()` is total | `expect_basic()`, which can in principle panic |

The compile-time version protects only a small stretch inside the frontends; the
runtime check sits at the entry, covers more, and keeps the types simpler. So one
`Operator` type serves both pre- and post-ASAP, told apart by the entry check
rather than by a type parameter.

## 4. Planner: search and assembly

**Candidates**: three shapes become two.

```rust
pub enum Replacement {
    Subtree(Rc<Operator>),     // formerly Summary(Rc<SummaryNode>) and Rewrite(Rc<QueryExpr>); "introduces a summary" = contains_ext()
    ExactComposition { .. },   // unchanged, except its plan becomes Rc<Operator> instead of Rc<SummaryNode>
}
```

**Holes**: a subtree of a candidate that is `Rc::ptr_eq` to some target is a hole,
left for that target's own choice to fill. A candidate that needs a specific
implementation of a child (for example a `SummaryAgg` that needs an exact
accumulator value, as in `realize_temporal_average`) inlines the child as a new node;
the pointer differs, so it is not a hole. `realize_child_with` changes from
"realize the child recursively, else `keep_pre_asap`" to "else leave a hole".

**Assembly**: one generic rule replaces `assemble_residual`.

```rust
fn assemble(&self, t: &Rc<Operator>) -> Rc<Operator> {
    memo by ptr;                                          // a shared child stays one Rc — today's assembled_nodes
    let body = match self.chosen(t) {
        Some(Subtree(r))           => r,                  // Rewrites are assembled further down too (today replacement.rs:4571 wraps the whole rewrite in keep_pre_asap and ignores the choices below)
        Some(ExactComposition{..}) => composition.plan,
        None                       => t,                  // no candidate chosen: keep the node, recurse into its children — what assemble_residual does for only four operators
    };
    body.map_children(|c| if is_target(c) { self.assemble(c) } else { c })
}
```

After assembly, run the data-state validation (§5) once; if filling a hole is
illegal (e.g. a query-time `SummaryEstimate` under a `SummaryAgg`), that hole falls
back to its original subtree. This generalizes the fallback `relink_summary` does
today for `Aggregate` only. Finally compute `GuaranteeIndex` (§2.2).

- **`map_children`**: reuse `rebuild_children` from `pre_asap/cse.rs:576` as
  `Operator::map_children`, dispatching `Basic` to `OriginalOp::map_children` and
  `Ext` to `ASAPOp::map_children`.
- **Deleted**: `assemble_residual`, `relink_summary`, the `query_time_nested_sum` /
  `contains_aggregate` special cases, `keep_pre_asap` / `keep_pre_asap_rc`, and the
  branch of `finalize_exact_accumulator` that builds an empty schema for `KeepPreAsap`.

How the three problems in #468 are resolved:

| Problem | Resolution |
|---|---|
| 1. One operator, two spellings: a `Project` is a `QueryExpr` inside `KeepPreAsap` and a `ValueOperation` outside it | one set of types; `assemble` no longer builds `ValueOperation`s |
| 2. `KeepPreAsap` is opaque: nothing outside can reference the `Scan` inside, so an exact aggregate and a sketch cannot share one scan | `Scan` is no longer wrapped; `SummaryAgg{child: scan}` and `Aggregate{child: scan}` can point to the same `Rc`. Splitting a multi-measure `Aggregate` into `avg` + KLL is a binding-rule change; this proposal only makes the split expressible |
| 3. `SetOp` and similar operators have no post-ASAP counterpart and must stay inside `KeepPreAsap`, so no subtree below them can use a summary | `SetOp` takes the `None => t` path and both children are assembled independently |

## 5. Validation: execution data states

Today's rule `produced_data_state(KeepPreAsap) = None` (decided by the edge that
reaches the node) extends to every `OriginalOp`:

| Node | Produced state |
|---|---|
| `OriginalOp` | none of its own; assigned by the consuming edge (`QUERY_ROWS` at the root) and passed down unchanged |
| `SummaryAgg` | `{the child's timing, SummaryState}`; ingestion time when the child is an `OriginalOp` |
| other `ASAPOp` | unchanged |

The `KeepPreAsap` / `BinaryOp` / `ValueOperation` / `RelationalJoin` arms of
`validate_execution_data_states` merge into one `Basic` arm:

- Pass the node's state to each child; an `Ext` child is checked against the
  `Ext` edge rules (e.g. `SummaryEstimate` only on the query side).
- `check_plain_operands` stays: every column an `OriginalOp` references must be
  `ColumnType::DataType`; `Project` / `Filter` / `Sort` / `Limit` may pass
  `ExactAggregate` columns through (today's exception). This is what rejects
  `Project(Ext(SummaryAgg))`, i.e. projecting sketch state as if it were rows.
- The extra ingestion-side constraints on `BinaryOp` (arithmetic only, identical
  schemas on both sides, exactly one `Float64`) move into this arm, per variant.
- `AmbiguousKeepPreAsap` is renamed `AmbiguousDataState`; its meaning is unchanged.

## 6. Export: fragments in the executable DAG

`ExecutableDag` has four payloads for original operators today — `fallback{expression: QueryExpr}`,
`binary`, `value` and `relational_join`. Afterwards there is one:

```rust
ExecutableOperatorPayload::Relational {
    /// An Operator with no Ext (checked at construction). Leaves are real Scans, or
    /// Scan { source: Source::DagInput { role } } standing for an incoming edge whose
    /// schema is that edge's intermediate_schema.
    expression: Operator,
}
```

`Source` gains a variant `DagInput { role: EdgeRole }`. `compile_executable_dag` works as follows:

1. Starting from the root, take the **largest connected subtree containing no
   `Ext`** as one fragment node; wherever the fragment meets an `Ext`, cut an edge
   and put a `DagInput` leaf in the fragment.
2. Each `Ext` node maps one-to-one onto the existing `summary_agg` /
   `summary_estimate` / … payloads; `FinalizeExactAccumulator` / `MaintainPopulation` /
   `ReadPopulation` / `Extension` become `value{operation}`. `ValueOperation` stays as a
   wire type with only these four variants.
3. Today's `fallback` is a fragment with no `DagInput` leaf. A backend's existing
   path that recursively lowers a `QueryExpr` handles every fragment after unwrapping
   one `Basic` level and adding a `DagInput` arm (treat the incoming edge as a
   materialized table); the dedicated lowerings for `binary` / `value::Project` /
   `relational_join` can go.

- **Wire version 5 → 6**: three fewer payloads; `fallback` is renamed `relational`
  and allows `DagInput` leaves; `output_schema` / `intermediate_schema` change from
  `SummarySchema` to `Schema` (adding `unique_keys` / `closed` / `table`).
- **Phase granularity**: ingestion / query phase is now one per fragment, and
  `with_execution_phases` still assigns it per node. Switching phase inside a
  fragment would need a materialization point, and materialization points are
  exactly `Ext` nodes (`FinalizeExactAccumulator`, `MaintainPopulation`), so no real
  granularity is lost.

## 7. Other consumers

| Location | Change |
|---|---|
| `post_asap/cse.rs` | delete. `ASAPOp` derives `PartialEq` + serde, so `share_common_subtrees` in `pre_asap/cse.rs` covers it |
| `dag_export.rs` | delete `build_summary` / `build_summary_hybrid` / `summary_kind_tag`; the one exporter gains an `Ext` arm. The viewer's `node-style.js` drops `KeepPreAsap` / `SummaryBinaryOp` / `ValueOperation` / `RelationalJoin` and adds the `ASAPOp` variant names; the pin test at `dag_export.rs:1843` follows |
| `summary_maintenance_cost/estimator.rs` (the largest, 80 sites) | the `KeepPreAsap` branches through `query_source_selections` / `retained_queries` switch to "largest subtree with no `Ext`" (exactly the §6 fragment); `BinaryOp \| RelationalJoin => "exact_binary"` and `ValueOperation => "value_operation"` fold into the fragment cost. This also fixes the missing `RelationalJoin` arm at `evidence.rs:292`, which can raise `InconsistentOperatorStatistics` |
| `physical_plan_cost_model.rs::estimate_candidate` | `Summary(KeepPreAsap(q))` and `Rewrite(q)` already share `lower_query_physical_dag`; every fragment will go through it |
| `summary_maintenance_lifecycle.rs` | `selected_raw_recompute = matches!(root, KeepPreAsap)` becomes `!contains_ext(root)`; `keep_pre_asap(target)` at `:551` becomes `target` itself |
| `maintained_population.rs` | `KeepPreAsap(source)` becomes `source` itself; `MaintainPopulation`'s `population.matches_input(child)` inspects a `Basic` child directly |
| `exact_composition.rs` | the `ExactOperation::Aggregate` shell becomes a `Basic(Aggregate)` node whose `child` is a hole |
| `RelationalJoin.pruning` | production code never sets `Some`; delete it. Candidate pruning can return later as an `ASAPOp` variant; the `CandidateCompleteness` type stays |

---

# III. Implementation plan

## 8. Stages and tests

`main` builds and passes all tests after every stage.

| Stage | Content | Main changes |
|---|---|---|
| 0 Preparation | `Rc` for `Concat.children`; `rebuild_children` becomes `map_children`; add `Column::plain` | `asap-types`, mechanical |
| 1 Split operators and expressions | §1.2: split `QueryExpr` into `OriginalOp` + `ScalarExpr`; `Predicate` / `ProjectItem` etc. hold `ScalarExpr`; relational children stay `Rc<OriginalOp>` for now. No semantic change | every place that builds or matches scalars: the three frontends' expression lowering, column resolution in `resolve`, scalar rewrites in `canonicalize`, `scalar_signature.rs`, `column_resolution.rs`. Mechanical |
| 2 Two levels | §1.1, §1.4: add `Operator<C>`, `ASAPOp` (a placeholder with no producer yet), `contains_ext()` and `expect_basic()`; every relational child slot becomes `Rc<Operator<C>>`. No semantic change | every place that builds or matches children: the three frontends, `resolve`, `canonicalize`, `pre_asap/cse`, `output_schema`, `dag_export`, all of `asap-aware-mapping`. Each change is the same shape; afterwards the remaining work is local |
| 3 One schema | §2.1: `SummarySchema` → `Schema`, `SummaryFamilyType` → `ColumnType` + `ASAPType`, `PlainField` for `List` / `Struct` elements; delete `lift*` | all of `asap-types` plus schema construction in every crate. The wire format changes only in schema fields; no version bump yet |
| 4 New types alongside old | fill in the `ASAPOp` variants; add the `Ext` arms of `output_schema` and data-state validation (§5), `GuaranteeIndex` (§2.2) and the optimizer-entry check (§3); write `flatten(&SummaryNode) -> Rc<Operator>`; `compile_executable_dag` and `dag_export` consume the flat tree (planner output goes through `flatten` first). Wire → 6 | `asap-types`, `devtools`, viewer. The planner is untouched, but export and execution already run on the new types |
| 5 Planner switch | §4: candidates and assembly results become `Rc<Operator>`, `assemble` is rewritten; the cost / lifecycle / estimator code in §7 moves to the new types; delete `flatten` | `asap-aware-mapping`; the largest stage |
| 6 Cleanup | delete `SummaryExpr` / `SummaryNode` / the redundant `ValueOperation` variants / `ExactOperation` / `post_asap/cse.rs`; update `post-asap-ir.md`, `physical-plan-integration.md`, the developer guide and the viewer docs | mostly docs |

An alternative transition would first make `ValueOperation` wrap a pre-ASAP operator
without changing the structure. Its benefit — export and execution see a flat
structure early — is already delivered by stage 4, which is built on the final types
and so is not throwaway code. That transition is therefore not planned.

**Tests**:

- One integration test per problem in §4, asserting the shape of the assembled plan:
  1. `WITH metric AS (SELECT avg(CASE WHEN l_quantity BETWEEN 1 AND 50 THEN 1.0 ELSE 0.0 END) AS in_range FROM lineitem) SELECT in_range, in_range = 1.0 AS ok FROM metric`:
     no post-ASAP-only node other than `Ext`, and all three `Project`s are the same variant;
  2. `SELECT avg(l_extendedprice), approx_percentile_cont(l_discount, 0.99) FROM lineitem`:
     the `child` of the `avg` `Aggregate` and of the KLL `SummaryAgg` is the same `Scan` by `Rc::ptr_eq` (enabled once a binding rule splits measures);
  3. `SELECT approx_distinct(l_partkey) FROM lineitem UNION ALL SELECT approx_distinct(l_suppkey) FROM lineitem`:
     each side of the `SetOp` has a `SummaryEstimate`.
- The optimizer entry rejects a tree containing `Ext`.
- Rewrite the 117 `SummaryExpr::` assertions in `sql_to_post_asap.rs` /
  `promql_to_post_asap.rs` / `exact_composition.rs` against the new shape.
- Wire 6 round trip: a fragment with a `DagInput` leaf is equal after serialization
  and deserialization; a version-5 document is rejected by `deny_unknown_fields`.
- Keep the shapes of the 52 existing tests in `execution_data_state.rs`; only their
  construction changes.

## 9. Out of scope

- The binding rule that splits a multi-measure `Aggregate` into "exact + summary"
  sharing one child (the second half of problem 2 in §4).
- Candidate pruning as an `ASAPOp` variant.
- **Accuracy of summary state** (§2.2): making a readout's guarantee account for
  `SummaryMerge` / `Subtract` / `Delete` / `Join`. Either attach an accuracy
  descriptor to state (e.g. "ε relative to the L1 norm"), or compose the error along
  the state chain at readout. To be done once the planner starts emitting these nodes.
- Folding `ExactComposition` into `Subtree`. Semantically it equals an `Aggregate`
  fragment (query time) or `Finalize(SummaryAgg{ExactAggregate})` (ingestion time),
  both expressible afterwards; the cost machinery of `composition_plans` and
  `CompositionDecision` stays as is until stage 5 is stable.
