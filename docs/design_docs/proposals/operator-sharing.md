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
operator definition and the scan is directly visible. An exact aggregate can also
consume that scan when sharing is valid, as shown in §4. Similarly, a union can
consume summary estimates without needing a separate post-ASAP union definition.

The design removes these representation barriers. It makes composition and sharing
possible; whether a particular rewrite or shared computation is valid still depends
on query semantics, accuracy and execution timing.

## 1. Operator model

### 1.1 Unified `Operator` type

Every computation is represented by an `Operator` node. The type has two categories:

```rust
enum Operator {
    NonASAP(NonASAPOp),
    ASAP(ASAPOp),
}
```

The table lists all operator kinds in this proposal. Aggregate functions, join
kinds and scalar functions are choices within these operations, not additional
operator kinds.

| Category | Meaning | All operations |
|---|---|---|
| `NonASAP(NonASAPOp)` | Ordinary query operations that transform, combine or aggregate data | `Scan`, `Filter`, `Project`, `Aggregate`, `Join`, `SetOp`, `Concat`, `Dedup`, `Sort`, `Limit`, `BinaryOp`, `SQLWindowFunc`, `TimeRange`, `TimeShift`, `ScalarBridge`, `EvalTimestamp`, `PromqlVectorFromScalar`, `PromqlScalarFromVector`, `PromqlRelabel`, `PromqlInfoEnrich`, `PromqlSeriesSample`, `PromqlSubquery` |
| `ASAP(ASAPOp)` | Operations on summary state and its results, including reserved operations | `SummaryAgg`, `SummaryEstimate`, `SummaryMerge`, `SummarySubtract`, `SummaryDelete`, `SummaryJoin`, `FinalizeExactAccumulator`, `MaintainPopulation`, `ReadPopulation`, `Extension` |

`ScalarBridge` is the proposed name for the existing `PromqlScalarBridge`.
`CurrentTimestamp` belongs to scalar expressions, so it is not in this operator list.

`SummaryMerge`, `SummarySubtract`, `SummaryDelete`, `SummaryJoin` and `Extension`
are reserved in the proposed planner model; listing them does not establish planner
or runtime support. Summary composition remains an open design question (§7).

`NonASAPOp` and `ASAPOp` describe the operation performed by a node. Inputs in both
categories connect to `Operator` nodes, so either category can consume the other
when their schema and execution constraints permit it. `NonASAP` classifies one
node; it does not require all of that node's descendants to be non-ASAP.

Both categories use the same graph model. A projection can consume a summary
estimate, and a summary can consume the result of a filter or join. There is no
separate relational subtree hidden inside a summary-plan node.

The categories remain distinct because they have different semantic rules:
ordinary operators consume query values, while summary operations may produce or
consume state. A common graph lets planning reason about all dependencies; the
categories make state-specific accuracy and execution constraints explicit. A
frontend plan contains only `NonASAP` nodes. Optimization may introduce `ASAP`
nodes later.

**Proposed data structures.** The sketches below show the operation-specific data
carried by each category. They use Rust-like notation to describe the design, not
final API signatures. `NodeRef` means an edge to another `Operator` node; it does
not prescribe a pointer or storage type. Multiple edges may refer to one producer.
`ScalarExpr` means an expression within an operator, not another graph node.

`NonASAPOp` retains the query semantics needed before and after optimization:

```rust
enum NonASAPOp {
    Scan {
        source: Source, predicates: Vec<ScalarExpr>, schema: Schema,
    },
    Filter { child: NodeRef, predicate: ScalarExpr },
    Project {
        child: NodeRef, columns: Vec<NamedExpr>, qualifier: Option<String>,
    },
    Aggregate {
        child: NodeRef, reduction: Reduction, measures: Vec<AggregateMeasure>,
        output_names: Vec<String>, having: Option<ScalarExpr>,
    },
    Join { left: NodeRef, right: NodeRef, kind: JoinKind, predicate: ScalarExpr },
    SetOp { left: NodeRef, right: NodeRef, kind: SetOpKind, all: bool },
    Concat {
        children: Vec<NodeRef>, discriminator_unique_key: Option<UniqueKey>,
    },
    Dedup { child: NodeRef, columns: Vec<ColumnRef> },
    Sort { child: NodeRef, keys: Vec<SortKey>, partition_by: GroupKeys },
    Limit { child: NodeRef, count: usize, offset: usize, partition_by: GroupKeys },
    BinaryOp {
        left: NodeRef, right: NodeRef, operation: BinaryOpKind,
        vector_match: Option<VectorMatch>,
    },
    SQLWindowFunc {
        child: NodeRef, function: WindowFunction, args: Vec<ScalarExpr>,
        partition_by: GroupKeys, order_by: Vec<SortKey>,
        frame: WindowFrame, output_name: String,
    },
    TimeRange { child: NodeRef, range: Duration },
    TimeShift { child: NodeRef, shift: TimeShiftSpec },
    ScalarBridge { expression: ScalarExpr },
    EvalTimestamp,
    PromqlVectorFromScalar { child: NodeRef },
    PromqlScalarFromVector { child: NodeRef },
    PromqlRelabel { child: NodeRef, destination_label: String, value: ScalarExpr },
    PromqlInfoEnrich { child: NodeRef, selector: Vec<InfoMatcher> },
    PromqlSeriesSample { child: NodeRef, by: GroupKeys, kind: SampleKind },
    PromqlSubquery { child: NodeRef, range: Duration, resolution: Option<Duration> },
}
```

The fields describe what an operator does to its inputs:

- `child`, `left`, `right` and `children` are graph dependencies. They can lead to
  either operator category, subject to the input's schema requirements.
- Predicates and named expressions describe row-level calculations. A named
  expression contains a scalar expression and its optional output alias.
- `reduction` describes whether aggregation combines groups or operates per entity;
  `measures` describes the requested aggregates. Grouping is distinct from ordering
  or limiting within groups, represented by `partition_by`.
- Join/set kinds, vector matching, window frames and time selections preserve
  source-language semantics. Output names, qualifiers and proven uniqueness also
  survive optimization. The optional concatenation key records a discriminator
  that distinguishes branches together with their within-branch key.

`ASAPOp` describes state construction, state operations and readout separately:

```rust
enum ASAPOp {
    SummaryAgg {
        child: NodeRef, family: SummaryFamily, input: SummaryUpdate,
        reduction: Reduction, grouping: GroupingStrategy,
        exact_rule: Option<AccuracyCompositionRule>,
    },
    SummaryEstimate {
        child: NodeRef, query: SummaryQuery,
        local_guarantee: Option<AccuracyGuarantee>,
    },
    FinalizeExactAccumulator { child: NodeRef },
    MaintainPopulation { child: NodeRef, population: PopulationSpec },
    ReadPopulation { child: NodeRef, readout: PopulationReadout },

    // Reserved operations; semantics and support require further design.
    SummaryMerge { children: Vec<NodeRef> },
    SummarySubtract { left: NodeRef, right: NodeRef },
    SummaryDelete { child: NodeRef, key: ColumnRef },
    SummaryJoin {
        outer: NodeRef, inner: NodeRef, key: ColumnRef, family: SummaryFamily,
    },
    Extension { child: NodeRef, name: String },
}
```

The summary fields distinguish state construction, readout and accuracy evidence:

| Field | Design meaning |
|---|---|
| `family` | The summary or exact accumulator chosen, including its family-specific parameters |
| `input` | The item identity and observation or weight supplied to a state update |
| `reduction` | Which input entities contribute to each logical result |
| `grouping` | Whether those groups use separate state instances or a supported shared structure |
| `query` / `readout` | The result requested from summary or maintained-population state |
| `local_guarantee` / `exact_rule` | Local accuracy evidence or composition semantics; neither is the final guarantee of the complete subtree |
| `population` | The population whose membership and values are maintained |

For example, one KLL `SummaryAgg` can feed two `SummaryEstimate` nodes whose queries
request p50 and p99. The build operation and its state are shared; the requested
estimates differ.

Schema, derived accuracy and execution timing describe every `Operator`, regardless
of category (§2). They are omitted from these operation-specific sketches. Timing
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

Keep scalar expressions within their owning operators. Graph edges then represent
computation dependencies, while predicates, projection expressions and sort keys
describe how an operator processes its input. This prevents an expression from
being mistaken for a table-producing plan. The
[companion proposal](decoupling_op_and_expr.md) defines this distinction.

### 1.3 Two meanings of sharing

**Sharing the operator model** means pre-ASAP and post-ASAP use the same definitions
for ordinary operators. It does not mean those two planning stages execute together
or must reference the same node instances.

**Sharing a computation** means multiple consumers within a workload use one
producer. For example, two queries may read one scan, or two estimates may use one
summary:

```text
                         ┌→ p50 estimation → query A
Scan → KLL summary build ┤
                         └→ p99 estimation → query B
```

The shared producer must satisfy every consumer's input, window, accuracy and timing
requirements. Representing it once exposes reuse to planning and costing. Keeping
multiple query roots in one workload graph is therefore part of the design.

Adding accuracy information or execution timing must preserve that sharing. It must
also leave alternative candidate plans independent: assigning a lifecycle to one
candidate must not change another candidate's choices. How nodes are stored or
reused is outside this design.

## 2. Node properties and why they differ

| Property | Meaning | How it is determined |
|---|---|---|
| Output schema | What the node produces: field names, types and relevant identity/time information | From the operation and its inputs |
| Accuracy guarantee | What can be established about the result's accuracy | From local accuracy evidence and the guarantees of its inputs |
| Execution timing | Whether work runs at ingestion time or query time | From a lifecycle choice for the complete plan |

### 2.1 One schema model for values and state

A common graph needs a common description of its edges. Schemas must distinguish
ordinary values from summary state, so a consumer can determine whether an input is
usable.

For example, a KLL build produces state; its p99 estimation produces a numeric value.
A numeric predicate can consume the estimate, but cannot treat the KLL state itself
as a number. Ordinary operators may carry state through only where their semantics
permit it; exact aggregate state must be finalized before use as an ordinary value.

Schema information must preserve grouping fields, time information, uniqueness and
series identity where relevant. Sharing operators must not change SQL or PromQL
meaning. After a rewrite, schemas must describe the new inputs rather than the plan
that was replaced.

### 2.2 Accuracy follows the computation

A summary's local error describes its behavior over an exact input. That alone is
not the guarantee of a larger query: its input may already be approximate, and
later operations may change the error. The planner must compose accuracy through
the actual computation graph.

For example, a KLL estimate over exact input can carry the sketch's guarantee. If
that input is approximate, the estimate must also account for the upstream error.
The summary state itself is not a query answer and need not have a value-level
accuracy guarantee.

Ordinary exact computations remain exact when their inputs and operation semantics
justify it. Exact accumulator finalization preserves the established guarantee.
Special cases, such as exact counting of the rows actually received, retain their
operation-specific rules.

Distinguish an assessment that has not happened from an assessment that found no
supported guarantee. Missing accuracy evidence cannot be treated as exactness or
as proof that a query's accuracy target is met. Candidate assessment and final-plan
validation must use the same accuracy semantics.

### 2.3 Timing is a planning choice

The same logical summary can be maintained as data arrives or computed when a query
needs it. Its position in the graph alone does not choose between these behaviors.
Logical planning therefore leaves timing undecided. Physical planning chooses
materialization and lifecycle behavior, then determines execution phases across the
complete graph.

The planner evaluates alternatives using deployment-provided cost and accuracy
models and capabilities. The deployment executes the selected plan, consistent with
the [planning-stages proposal](https://github.com/ProjectASAP/ASAPPlanner/pull/509).

A valid timing assignment must satisfy these constraints:

- Ingestion-time work cannot depend on a query-time result.
- Summary estimation runs at query time. Population maintenance and readout run at
  ingestion time and query time, respectively.
- A shared computation has one execution phase compatible with all its consumers.
  A single producer cannot simultaneously mean two separate executions.
- Stored state remains available for as long as its consumers need it.

This proposal covers materialization at summary-state boundaries. Materializing
arbitrary ordinary intermediate results requires a separate design.

## 3. Planning responsibilities

The common representation separates what a plan computes from how it executes:

| Responsibility | Required result |
|---|---|
| Frontend translation | An ordinary query graph preserving source-language semantics |
| Logical optimization | Exact and summary-based alternatives, including legal shared computations |
| Candidate assessment | Accuracy and capability evidence for the actual candidate graph |
| Physical and lifecycle planning | Executable alternatives with materialization, retention and compatible timing |
| Plan selection | A valid plan chosen using workload-level costs and requirements |
| Export and execution | The selected graph with explicit dependencies and completed assessments |

Ordinary operators remain in the graph when their inputs are replaced by summary
computations. For example, replacing an aggregate below a projection must not require
replacing the projection with a separate post-ASAP operator.

Changing a candidate's inputs can change its schema, accuracy and legal timing.
Those properties must be checked against the resulting graph. A plan with unresolved
execution timing or an unfinished accuracy assessment is not ready for export.

This proposal changes the representation, not the search strategy. It does not
require a new enumeration algorithm or change when candidates commit to particular
child plans.

## 4. Example: one scan serving exact and approximate queries

Consider an exact average and an approximate p99 over the same input interval.
Once a rewrite exposes the two computations, the common model can express:

```text
       ┌→ Exact average ─────────────────────→ query A
Scan ──┤
       └→ KLL summary build → p99 estimation → query B
```

If both branches run at query time, they may share the scan. Computing accuracy and
assigning timing must retain that one producer for both consumers.

If the KLL is maintained at ingestion time while the exact average reads raw data at
query time, the depicted scan cannot serve both phases as one execution. The plan
needs separate scans or a different lifecycle that makes sharing valid. The planner
must represent that choice explicitly; timing validation rejects the conflicting
shared plan.

The representation enables the shared plan but does not supply the rewrite that
splits a multi-measure aggregate into these branches. That rewrite is outside this
proposal. How planning resolves the mixed-phase case remains open (§7).

## 5. Export preserves the graph

Export one node per operator and represent its input dependencies as edges. Export
a shared producer once, with edges to all its consumers.

This keeps the graph visible to costing, physical compilation, execution and plan
inspection. Embedding a whole relational subtree in one exported node would hide
its internal sharing and recreate the original boundary problem.

Export carries the resolved schemas, assessed guarantees and assigned execution
phases. Physical compilation may lower one logical operation to several physical
operations, but must preserve its dependencies and meaning. The execution layer
does not invent missing planning decisions.

Changing the exported representation requires coordinated adoption by the planner
and downstream readers. Representation unification must preserve query semantics;
it does not by itself guarantee unchanged timings for plans with unresolved sharing
conflicts.

## 6. Acceptance criteria

The design is successful when:

- A projection uses the same semantics above and below summary computations.
- An exact aggregate and a summary can share an input when all requirements agree.
- A union or another ordinary operator can consume summary estimates on its inputs.
- Shared producers remain shared when accuracy and timing are resolved, including
  across different queries in the workload.
- Invalid value/state combinations, incompatible timing and unfinished plan
  assessments are rejected before execution.
- Export preserves visible dependencies and shared producers.

## 7. Scope and open questions

This proposal defines a common operator model and its correctness constraints. It includes the operation-specific data needed to express those semantics, but
does not prescribe storage structures, public APIs, traversal algorithms,
serialization fields or a code migration sequence.

The following decisions remain separate or unresolved:

- **Mixed-phase sharing:** when consumers need different phases, should planning
  separate the producer or find a common materialized lifecycle? Existing timing
  behavior cannot be promised until this is resolved.
- **Summary composition:** where are merge operations introduced—during planner
  optimization or downstream? Accuracy rules for merge, subtract, delete and
  summary joins need a separate design. A representable operation is not a claim of
  runtime support.
- **Lifecycle integration:** reuse the lifecycle concepts being developed in
  [#482](https://github.com/ProjectASAP/ASAPPlanner/pull/482), without creating a second
  competing source of timing decisions.
- **Additional optimization:** rules that split exact and approximate measures,
  new pruning strategies and deferred child-plan choices are outside this proposal.
- **Finalization:** this representation change does not resolve the existing
  difference between query-result assembly and lifecycle-plan assembly in adding
  finalization of exact aggregate state.
