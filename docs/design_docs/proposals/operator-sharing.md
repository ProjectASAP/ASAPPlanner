# Sharing Operators Between Pre-ASAP IR and Post-ASAP IR

> Status: proposal, not implemented. Audience: planner designers and architects.
> Addresses [#468](https://github.com/ProjectASAP/ASAPPlanner/issues/468).
> Companion: [Decoupling operators from scalar expressions](decoupling_op_and_expr.md).

## Goal and problem

Use one operator model before and after ASAP optimization, so ordinary query
operations and summary operations can form one visible computation graph.

Today, the post-ASAP representation wraps relational subplans and duplicates some
relational operators outside those wrappers. This causes three problems:

- A projection above a summary needs a different representation from a projection
  below it, although both perform the same operation.
- An exact aggregate cannot directly share a scan hidden inside a summary's input.
- An operator without a post-ASAP counterpart cannot naturally contain summary-based
  children.

For example, consider a p99 latency query that projects its input columns, builds a
KLL summary, and projects the estimated result. The trees below read from the result
at the top to the data source at the bottom:

```text
Today                                      Proposed
Post-ASAP projection                       Project
└─ Summary estimation                      └─ Summary estimation
   └─ KLL summary build                       └─ KLL summary build
      └─ Wrapped relational subplan              └─ Project
         └─ Ordinary projection                     └─ Scan latency
            └─ Scan latency
```

Today the two projections need separate representations, and the scan is hidden
inside the wrapped subplan. In the proposed graph, both projections use the same
operator definition and the scan is directly visible. A union can likewise consume
summary estimates without needing a separate post-ASAP union definition.

The design removes these representation barriers. It makes composition and sharing
possible; whether a particular rewrite or shared computation is valid still depends
on query semantics, accuracy and execution timing.

## 1. Operator model

### 1.1 Unified `Operator` type

Relation, vector and summary-state computations are represented by `Operator`
nodes. Scalar computations use the companion proposal's `ScalarExpr`. The operator
type has two categories:

```rust
enum Operator {
    NonASAP(NonASAPOp),
    ASAP(ASAPOp),
}

enum QueryRoot {
    Operator(Rc<Operator>),
    Scalar(ScalarExpr),
}
```

The table lists all operator kinds in this proposal. Aggregate functions, join
kinds and scalar functions are choices within these operations, not additional
operator kinds.

| Category | Meaning | All operations |
|---|---|---|
| `NonASAP(NonASAPOp)` | Ordinary query operations that transform, combine or aggregate data | `Scan`, `Values`, `Filter`, `Project`, `Aggregate`, `Join`, `SetOp`, `Concat`, `Dedup`, `Sort`, `Limit`, `BinaryOp`, `SQLWindowFunc`, `TimeRange`, `TimeShift`, `PromqlVectorFromScalar`, `PromqlRelabel`, `PromqlInfoEnrich`, `PromqlSeriesSample`, `PromqlSubquery` |
| `ASAP(ASAPOp)` | Operations on summary state and its results, including reserved operations | `SummaryAgg`, `SummaryEstimate`, `SummaryMerge`, `SummarySubtract`, `SummaryDelete`, `SummaryJoin`, `FinalizeExactAccumulator`, `MaintainPopulation`, `ReadPopulation`, `Extension` |

`CurrentTimestamp`, `EvalTimestamp` and `PromqlScalarFromVector` belong to scalar
expressions. Constants need no `PromqlScalarBridge`; query roots may directly hold
a scalar expression. The [companion proposal](decoupling_op_and_expr.md) defines
these boundaries and their SQL/PromQL semantic coverage. A scalar-only frontend
query, such as `time()`, therefore needs no operator node.

`SummaryMerge`, `SummarySubtract`, `SummaryDelete`, `SummaryJoin` and `Extension`
are reserved in the proposed planner model; listing them does not establish planner
or runtime support. Defining new summary-composition semantics is outside this
proposal (§6).

`NonASAPOp` and `ASAPOp` describe the operation performed by a node. Inputs in both
categories connect to `Operator` nodes, so either category can consume the other
when their result kind, schema and execution constraints permit it. `NonASAP`
classifies one node; it does not require all of that node's descendants to be non-ASAP.

Both categories use the same graph model. A projection can consume a summary
estimate, and a summary can consume the result of a filter or join. There is no
separate relational subtree hidden inside a summary-plan node.

The categories remain distinct because they have different semantic rules:
ordinary operators consume query values, while summary operations may produce or
consume state. A common graph lets planning reason about all dependencies; the
categories make state-specific accuracy and execution constraints explicit. A
frontend plan contains only `NonASAP` nodes. Optimization may introduce `ASAP`
nodes later.

**Relationship to the current code.** These structures are proposals, not copies
of the current definitions with two category labels added. The operations come from
the existing query and summary models, but the sketches combine several changes:

| Change | Purpose and scope |
|---|---|
| Group ordinary operations under `NonASAP` and summary operations under `ASAP` | Organize the common operator model into two semantic categories. |
| Give both categories inputs that refer to `Operator` nodes | Allow ordinary and summary operations to compose directly, without a wrapper hiding their dependencies. |
| Remove relational-subplan wrappers and duplicate relational operations | Make all dependencies visible and give each ordinary operation one definition. |
| Separate scalar expressions from operators | A related proposal, described in the [companion document](decoupling_op_and_expr.md); not a consequence of categorization alone. |
| Represent execution timing chosen during physical planning | Follow the planning-stage design in [#509](https://github.com/ProjectASAP/ASAPPlanner/pull/509), rather than introduce a new timing policy here (§2.3). |

**Naming and compatibility.** Reuse current names where their semantics match.
The companion proposal defines deliberate additions and changes, including
`Values`, scalar query roots and typed scalar conversions. Ordinary payloads such
as `Predicate`, `ProjectItem`, `AggIntent`, `SketchQuery` and `SummaryFamilyType`
keep their names; retaining a name does not establish complete language coverage.

The following changes are explicit:

- Operator inputs become references to the common `Operator`. The sketches use the
  existing `Rc` notation for shared inputs and omit column-state generics for
  readability; column IDs and scan schemas below show the resolved form.
- `Predicate`, `ProjectItem` and other scalar-bearing payloads keep their roles and
  own `ScalarExpr` values. The companion proposal defines owned scalar trees and
  shared operator inputs, including `Concat`. Explicit scalar conversions and SQL
  subqueries also reference the common `Operator`; their dependencies remain visible.
  This document uses those same structures.
- `BinaryOp` reuses the existing post-ASAP `BinaryOperator` payload. It carries the
  binary operation, vector matching and checked-division requirements; the pre-ASAP
  `op` and `vector_match` semantics must be preserved when mapped into it.
  The proposed `return_bool` field also retains PromQL comparison mode.
- `Limit.partition_by` comes from the existing post-ASAP limit. Applying that field
  to the unified operator remains an explicit design choice, not new functionality
  implied by a rename. Optional `Limit.n` adds offset-only queries, and
  `TimeRange.kind` distinguishes instant selection from a range window, as specified
  in the companion. `SQLWindowFunc.frame` retains its current optional form.

**Proposed data structures.** These sketches describe operation-specific data using
current names. `Rc<Operator>` represents a shared input edge; no new reference type
is introduced. Storage and traversal algorithms remain outside this design.

`NonASAPOp` retains the query semantics needed before and after optimization:

```rust
enum NonASAPOp {
    Scan {
        source: Source, predicates: Vec<Predicate>, schema: Schema,
    },
    Values { rows: Vec<Vec<ScalarExpr>>, schema: Schema },
    Filter { child: Rc<Operator>, pred: Predicate },
    Project {
        child: Rc<Operator>, cols: Vec<ProjectItem>, qualifier: Option<String>,
    },
    Aggregate {
        child: Rc<Operator>, reduction: Reduction, measures: Vec<AggIntent>,
        output_names: Vec<String>, having: Option<Predicate>,
    },
    Join { left: Rc<Operator>, right: Rc<Operator>, kind: JoinKind, pred: Predicate },
    SetOp { left: Rc<Operator>, right: Rc<Operator>, kind: RelationalSetOpKind, all: bool },
    Concat {
        children: Vec<Rc<Operator>>, discriminator_unique_key: Option<ConcatDiscriminatorKey>,
    },
    Dedup { child: Rc<Operator>, cols: Vec<ColumnId> },
    Sort { child: Rc<Operator>, keys: Vec<SortKey>, partition_by: GroupKeys },
    Limit { child: Rc<Operator>, n: Option<usize>, offset: usize, partition_by: GroupKeys },
    BinaryOp {
        lhs: Rc<Operator>, rhs: Rc<Operator>, operator: BinaryOperator, return_bool: bool,
    },
    SQLWindowFunc {
        child: Rc<Operator>, func: WindowFuncKind, args: Vec<ScalarExpr>,
        partition_by: GroupKeys, order_by: Vec<SortKey>,
        frame: Option<WindowFrame>, output_name: String,
    },
    TimeRange { child: Rc<Operator>, range: Duration, kind: TimeRangeKind },
    TimeShift { child: Rc<Operator>, shift: TimeShift },
    PromqlVectorFromScalar(ScalarExpr),
    PromqlRelabel { child: Rc<Operator>, dst: String, value: ScalarExpr },
    PromqlInfoEnrich { child: Rc<Operator>, selector: Vec<InfoMatcher> },
    PromqlSeriesSample { child: Rc<Operator>, by: GroupKeys, kind: SampleKind },
    PromqlSubquery { child: Rc<Operator>, range: Duration, resolution: Option<Duration> },
}
```

The fields describe what an operator does to its inputs:

- `child`, `left`, `right`, `lhs`, `rhs` and `children` are graph dependencies. They can lead to
  either operator category, subject to the input's schema and result-kind requirements.
  Dependencies referenced by scalar conversions/subqueries are edges in this same graph.
- `Predicate` describes a row-level condition; `ProjectItem` contains a scalar
  expression and its optional output alias.
- `reduction` describes whether aggregation combines groups or operates per entity;
  `measures` describes the requested aggregates. Grouping is distinct from ordering
  or limiting within groups, represented by `partition_by`.
- Join/set kinds, vector matching, window frames and time selections preserve
  source-language semantics. Output names, qualifiers and proven uniqueness also
  survive optimization. The optional concatenation key records a discriminator
  that distinguishes branches together with their within-branch key.

`ASAPOp` describes state construction, state operations and readout separately.
`SummaryFamilyType` retains its current name; state-producing operations use its
summary or exact-accumulator cases, never its `Plain` case.

```rust
enum ASAPOp {
    SummaryAgg {
        child: Rc<Operator>, family: SummaryFamilyType, input: SummaryUpdate,
        reduction: Reduction, grouping: GroupingStrategy,
    },
    SummaryEstimate {
        summary_input: Rc<Operator>, query: SketchQuery,
    },
    FinalizeExactAccumulator { child: Rc<Operator> },
    MaintainPopulation { child: Rc<Operator>, population: MaintainedPopulation },
    ReadPopulation { child: Rc<Operator>, readout: PopulationReadout },

    // Reserved operations; semantics and support require further design.
    SummaryMerge { children: Vec<Rc<Operator>> },
    SummarySubtract { left: Rc<Operator>, right: Rc<Operator> },
    SummaryDelete { summary_input: Rc<Operator>, key: ColumnRef },
    SummaryJoin {
        outer: Rc<Operator>, inner: Rc<Operator>, key: ColumnRef, family: SummaryFamilyType,
    },
    Extension { child: Rc<Operator>, name: String },
}
```

The summary fields distinguish state construction and readout:

| Field | Design meaning |
|---|---|
| `family` | The summary or exact accumulator chosen, including its family-specific parameters |
| `input` | The item identity and observation or weight supplied to a state update |
| `reduction` | Which input entities contribute to each logical result |
| `grouping` | Whether those groups use separate state instances or a supported shared structure |
| `query` / `readout` | The result requested from summary or maintained-population state |
| `population` | The population whose membership and values are maintained |

Result kind, schema, derived accuracy and execution timing describe `Operator`
nodes in both categories (§2). They are omitted from these operation-specific sketches. Timing
comes from lifecycle planning rather than a fixed field value implied by an operator
kind; the final accuracy assessment combines local evidence with the actual inputs.

For example, arrows below show data flowing from producer to consumer:

```text
NonASAP(Scan) → NonASAP(Filter) → ASAP(SummaryAgg)
             → ASAP(SummaryEstimate) → NonASAP(Project)
```

Each node describes its operation, inputs and output schema. An executable plan also
needs an accuracy assessment and an execution phase for each relevant computation.
These have different sources, described in §2; they are not all known when a logical
operator is first created.

### 1.2 Operators and scalar expressions

A filter is an operator because it transforms a table. Its predicate, such as
`latency > 100`, is a scalar expression evaluated in that table's schema.

Scalar expressions belong to an operator field or a scalar query root. Predicates,
projection expressions and sort keys describe value computation in that context.
Explicit scalar conversions and subqueries may reference operators; those are
visible graph dependencies with defined cardinality rules. This prevents an
arbitrary expression from being mistaken for a table-producing plan. The
[companion proposal](decoupling_op_and_expr.md) defines this distinction.

Its pre-ASAP `Rc<NonASAPOp<C>>` references become `Rc<Operator>` in the unified
model, including `QueryRoot` and the inputs to `PromqlScalarFromVector`,
`ScalarSubquery`, `Exists` and `InSubquery`. Their cardinality, NULL and NaN rules
remain unchanged.

In `scalar(sum(up))`, `scalar()` is Prometheus PromQL's built-in vector-to-scalar
function, explicitly written by the query author. This proposal does not insert
it automatically: `sum(up)` alone is a valid query returning an instant vector.
The scalar expression `PromqlScalarFromVector` represents that function and references its
result to obtain one number. A valid ASAP rewrite may replace that producer with
a summary readout, preserving the required vector and accuracy semantics; it cannot
substitute raw summary state. Ordinary expressions such as `price * 2` reference
columns and literals, not a query subgraph.

These are **query subgraphs referenced by scalar expressions**. The reference is
an edge to a producer in the common graph, not a copy of the subgraph embedded in
the expression. Scalar ownership does not change the producer's graph identity.

### 1.3 Scope of operator sharing

Here, sharing means pre-ASAP and post-ASAP use the same operator definitions.
A `Project`, for example, has one representation whether its input is an ordinary
aggregate or a summary estimate. This proposal removes the representation boundary;
it does not introduce rules for sharing computations across queries.

## 2. Node properties and why they differ

| Property | Meaning | How it is determined |
|---|---|---|
| Result kind and output schema | Whether the node produces a relation, instant vector, range vector or state, and its fields, types and identity/time information | From the operation and its inputs |
| Accuracy guarantee | What can be established about the result's accuracy | From local accuracy evidence and the guarantees of its inputs |
| Execution timing | Whether work runs at ingestion time or query time | From a lifecycle choice for the complete plan |

### 2.1 One schema model for values and state

A common graph needs a common description of its edges. Validate result kind as
well as schema: matching numeric columns do not make a relation, an instant vector
and a range vector interchangeable. Summary state is also distinct from ordinary
values. The operation determines the applicable input/output contract; this does
not require adding the same result-kind field to every node.

For example, a KLL build produces state; its p99 estimation produces a numeric value.
A numeric predicate can consume the estimate, but cannot treat the KLL state itself
as a number. Ordinary operators may carry state through only where their semantics
permit it; exact aggregate state must be finalized before use as an ordinary value.

Schema information must preserve grouping fields, time information, uniqueness and
series identity where relevant. Sharing operators must not change SQL or PromQL
meaning. After a rewrite, schemas must describe the new inputs rather than the plan
that was replaced. Scalar expressions instead have value types and evaluation
contexts. A scalar root does not need a fabricated relation schema.

### 2.2 Preserve existing accuracy semantics

The unified representation must preserve the existing accuracy model, composition
rules and result guarantees. An operation's guarantee must still account for its
actual inputs, including producers referenced by scalar expressions. Reading an
approximate result through `PromqlScalarFromVector` or a SQL scalar subquery does
not make it exact; unknown accuracy must not be treated as exactness.

This proposal adds no accuracy fields or new guarantee-calculation workflow.
Changing the operator representation must not change the accuracy meaning of the
same computation.

### 2.3 Timing follows the planning-stage design

The [planning-stage design in #509](https://github.com/ProjectASAP/ASAPPlanner/pull/509)
separates logical decisions about what to compute from physical decisions about how
and when to compute it. This proposal follows that division.

For example, a KLL summary build may execute at ingestion time or query time,
depending on the materialization choice. The unified operator representation must
carry the chosen execution timing without requiring separate operator definitions
for the two phases.

The representation must preserve the resulting execution constraints: ingestion-time
work cannot depend on query-time results, and consumers must receive values or state
that are available when needed. Materialization choices, retention and plan selection
remain governed by #509; this document does not define another lifecycle policy.
These constraints also apply to query subgraphs referenced by scalar expressions.

PromQL evaluation timestamps and SQL statement time are separate from these
execution phases. `TimeShift`, subquery grids and `EvalTimestamp` retain their
source-language evaluation context. A shared node identity alone does not permit
reusing a result across different evaluation times.

## 3. Planning responsibilities

These are the stages defined in
[#509](https://github.com/ProjectASAP/ASAPPlanner/pull/509), shown here only to explain
how they use the common operator model:

| Stage from #509 | Use of the unified representation |
|---|---|
| Frontends | Produce an operator or scalar query root; any referenced operator nodes are `NonASAP`. Preserve source-language semantics. |
| Logical ASAP-aware optimization | Form candidate graphs containing ordinary and summary operators, with no wrappers hiding their dependencies. |
| Physical ASAP-aware optimization | Determine executable alternatives, including materialization and execution timing, for those candidate graphs. |
| Plan selection | Evaluate complete physical candidates using workload requirements and deployment-provided models and capabilities. |
| Deployment execution | Execute the selected graph, preserving its dependencies and assigned phases. |

Ordinary operators remain the same operations when their inputs are replaced by
summary computations. Replacing an aggregate below a projection, for example, does
not require a separate post-ASAP projection definition.

This document changes the representation used by these stages, not their search,
accuracy, costing or selection policies.

## 4. Export preserves the graph

Export one node per operator and represent its input dependencies as edges. Export
a shared producer once, with edges to all its consumers. Include dependencies
referenced by scalar conversions and subqueries. Preserve scalar query roots as
expressions and their operator dependencies; do not invent a bridge node for export.

This keeps the graph visible to costing, physical compilation, execution and plan
inspection. Embedding a whole relational subtree in one exported node would hide
its internal sharing and recreate the original boundary problem.

Export preserves result kinds, resolved schemas, scalar value types and evaluation
context, together with the applicable guarantees and assigned execution phases.
Physical compilation may lower one logical operation to several physical operations, but must preserve its dependencies and meaning. The execution layer
does not invent missing planning decisions.

Changing the exported representation requires coordinated adoption by the planner
and downstream readers while preserving existing query semantics and the selected
plan's execution requirements.

## 5. Acceptance criteria

The design is successful when:

- A projection uses the same semantics above and below summary computations.
- A union or another ordinary operator can consume summary estimates on its inputs.
- Unifying the representation preserves existing graph dependencies, including
  any shared inputs; it does not introduce new sharing rules.
- Existing value/state, accuracy and execution constraints remain enforceable on
  the unified representation.
- Scalar query roots and conversions use the same representation before and after
  optimization, with no bridge nodes or hidden subplans.
- Validation includes result kind and query subgraphs referenced by scalar
  expressions when checking schema, accuracy and execution constraints.
- Export preserves visible dependencies and shared producers.

## 6. Scope and compatibility

This proposal defines the common operator structure, its operation-specific data
and how dependencies remain visible through export. Existing names and semantics
are retained except for the structural changes identified in §1.1.

The pre-ASAP and post-ASAP versions of some operations carry different information.
The unified `BinaryOp` must retain existing checked-division requirements, and
`Limit` must retain the existing ability to limit within groups. These compatibility
requirements belong in this design because removing duplicate operator definitions
must not remove existing behavior. If a rewrite moves arithmetic into a scalar
expression, it must preserve applicable checked-division guards and exact fallback;
`ExprSemantics` selects language rules and does not replace those proof conditions.

The [companion semantic tables](decoupling_op_and_expr.md#3-semantic-requirements)
use DataFusion 55.1.0 and Prometheus 3.15.0 as design targets. Their gaps also apply
here: a common `Operator` type does not supply missing aggregate modifiers, value
types or function contracts. This proposal changes neither repository dependencies
nor the set of implemented language features.

New accuracy fields, accuracy-composition rules, computation-sharing algorithms and
lifecycle policies are outside this proposal. Planning responsibilities follow #509.
The scalar/operator separation is specified in the companion document. Storage,
traversal algorithms, serialization fields and a code migration sequence are also
outside this document.
