# Sharing Operators Between Pre-ASAP IR and Post-ASAP IR

> Status: proposal, not implemented. Audience: planner designers and architects.
> Addresses [#468](https://github.com/ProjectASAP/ASAPPlanner/issues/468).
> Companion: [Decoupling operators from scalar expressions](decoupling_op_and_expr.md).

## Goal and problem

Use one operator model before and after ASAP optimization, so ordinary query
operations and summary operations can form one visible computation DAG.

Today, the post-ASAP representation wraps relational subplans and duplicates some
relational operators outside those wrappers. This causes three problems:

- A projection above a summary needs a different representation from a projection
  below it, although both perform the same operation.
- An exact aggregate cannot directly share a scan hidden inside a summary's input.
- An operator without a post-ASAP counterpart cannot naturally contain summary-based
  children.

For example, consider a p99 latency query that projects its input columns, builds a
KLL summary, and projects the estimated result. The DAGs below read from the result
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
inside the wrapped subplan. In the proposed DAG, both projections use the same
operator definition and the scan is directly visible. A union can likewise consume
summary estimates without needing a separate post-ASAP union definition.

The design removes these representation barriers. It makes composition and sharing
possible; whether a particular rewrite or shared computation is valid still depends
on query semantics, accuracy and execution timing.

## 1. Operator model

### 1.1 Unified `Operator` type

`Operator` describes an ordinary or ASAP operation; `OperatorNode` combines it
with common planning properties. `ScalarExpr` describes value computation. The
following overview and payload definitions are the canonical resolved interfaces
used by both proposals. The companion document defines `ScalarExpr` and its
operator-field wrappers; it does not define a second operator model.

**Proposed data structures — overview.** The complete outer structure is below;
operation variants and schema internals are expanded afterward. These declarations
are shared by the detailed sections, not separate abbreviated types.

```rust
// A DAG node combines its operation with common planning properties (§2).
struct OperatorNode {
    operator: Operator,
    result_kind: OperatorResultKind,
    schema: Schema,
    guarantee: Option<ResultGuarantee>,
    timing: Option<ExecutionTiming>,
}

// Operation payloads: each variant below defines its own inputs and parameters.
enum Operator {
    NonASAP(NonASAPOp),
    ASAP(ASAPOp),
}

// NonASAPOp / ASAPOp: detailed below; their inputs are Rc<OperatorNode>.
// ScalarExpr: an owned, unshared value expression, defined in the companion proposal.
// Schema / OperatorResultKind: defined in §2.1.
```

An operator owns its scalar expressions and references input nodes through
`Rc<OperatorNode>`. Either operation category can consume the other's outputs when
the input contract permits it. `NonASAP` describes one operation, not its entire
sub-DAG. Frontend DAGs contain only NonASAP operations; ASAP optimization may
introduce state construction and readout.

| Category | Meaning | All operations |
|---|---|---|
| `Operator::NonASAP(NonASAPOp)` | Ordinary query operations that transform, combine or aggregate data | `Scan`, `Values`, `Filter`, `Project`, `Aggregate`, `Join`, `SetOp`, `Concat`, `Dedup`, `Sort`, `Limit`, `BinaryOp`, `SQLWindowFunc`, `TimeRange`, `TimeShift`, `PromqlVectorFromScalar`, `PromqlRelabel`, `PromqlInfoEnrich`, `PromqlSeriesSample`, `PromqlSubquery` |
| `Operator::ASAP(ASAPOp)` | Operations on summary state and its results, including reserved operations | `SummaryAgg`, `SummaryEstimate`, `SummaryMerge`, `SummarySubtract`, `SummaryDelete`, `SummaryJoin`, `FinalizeExactAccumulator`, `MaintainPopulation`, `ReadPopulation`, `Extension` |

`CurrentTimestamp`, `EvalTimestamp` and `PromqlScalarFromVector` belong to
`ScalarExpr`, defined in the [companion proposal](decoupling_op_and_expr.md#22-scalar-expressions).
A constant needs no bridge operator. The sketches use resolved `ColumnId`s and
`Schema`; name resolution precedes construction of these nodes.

`NonASAPOp` retains the query semantics needed before and after optimization:

```rust
enum NonASAPOp {
    Scan {
        source: Source, predicates: Vec<Predicate>, schema: Schema,
    },
    Values { rows: Vec<Vec<ScalarExpr>>, schema: Schema },
    Filter { child: Rc<OperatorNode>, pred: Predicate },
    Project {
        child: Rc<OperatorNode>, cols: Vec<ProjectItem>, qualifier: Option<String>,
    },
    Aggregate {
        child: Rc<OperatorNode>, reduction: Reduction, measures: Vec<AggIntent>,
        output_names: Vec<String>, having: Option<Predicate>,
    },
    Join { left: Rc<OperatorNode>, right: Rc<OperatorNode>, kind: JoinKind, pred: Predicate },
    SetOp { left: Rc<OperatorNode>, right: Rc<OperatorNode>, kind: RelationalSetOpKind, all: bool },
    Concat {
        children: Vec<Rc<OperatorNode>>, discriminator_unique_key: Option<ConcatDiscriminatorKey>,
    },
    Dedup { child: Rc<OperatorNode>, cols: Vec<ColumnId> },
    Sort { child: Rc<OperatorNode>, keys: Vec<SortKey>, partition_by: GroupKeys },
    Limit { child: Rc<OperatorNode>, n: Option<usize>, offset: usize, partition_by: GroupKeys },
    BinaryOp {
        lhs: Rc<OperatorNode>, rhs: Rc<OperatorNode>, operator: BinaryOperator, return_bool: bool,
    },
    SQLWindowFunc {
        child: Rc<OperatorNode>, func: WindowFuncKind, args: Vec<ScalarExpr>,
        partition_by: GroupKeys, order_by: Vec<SortKey>,
        frame: Option<WindowFrame>, output_name: String,
    },
    TimeRange { child: Rc<OperatorNode>, range: Duration, kind: TimeRangeKind },
    TimeShift { child: Rc<OperatorNode>, shift: TimeShift },
    PromqlVectorFromScalar(ScalarExpr),
    PromqlRelabel { child: Rc<OperatorNode>, dst: String, value: ScalarExpr },
    PromqlInfoEnrich { child: Rc<OperatorNode>, selector: Vec<InfoMatcher> },
    PromqlSeriesSample { child: Rc<OperatorNode>, by: GroupKeys, kind: SampleKind },
    PromqlSubquery { child: Rc<OperatorNode>, range: Duration, resolution: Option<Duration> },
}

enum TimeRangeKind { Instant, Range }
```

Ordinary payload fields have these roles:

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
`FieldDataType` (§2.1) types every output field; state-producing operations use its
summary or exact-accumulator cases, never its `Plain` case.

```rust
enum ASAPOp {
    SummaryAgg {
        child: Rc<OperatorNode>, family: FieldDataType, input: SummaryUpdate,
        reduction: Reduction, grouping: GroupingStrategy,
    },
    SummaryEstimate {
        summary_input: Rc<OperatorNode>, query: SketchStatistic,
    },
    FinalizeExactAccumulator { child: Rc<OperatorNode> },
    MaintainPopulation { child: Rc<OperatorNode>, population: MaintainedPopulation },
    ReadPopulation { child: Rc<OperatorNode>, readout: PopulationStatistic },

    // Reserved operations; semantics and support require further design.
    SummaryMerge { children: Vec<Rc<OperatorNode>> },
    SummarySubtract { left: Rc<OperatorNode>, right: Rc<OperatorNode> },
    SummaryDelete { summary_input: Rc<OperatorNode>, key: ColumnId },
    SummaryJoin {
        outer: Rc<OperatorNode>, inner: Rc<OperatorNode>, key: ColumnId, family: FieldDataType,
    },
    Extension { child: Rc<OperatorNode>, name: String },
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

The common `OperatorNode` fields are declared in the overview above and explained
in §2. The operation variants do not repeat them. Reserved ASAP variants require
further semantic and capability design before use.

### 1.2 Operators and scalar expressions

A filter is an operator because it transforms a table. Its predicate, such as
`latency > 100`, is a scalar expression evaluated in that table's schema.

Scalar expressions belong to an operator field or a scalar query. Predicates,
projection expressions and sort keys describe value computation in that context.
Explicit scalar conversions and subqueries may reference operators; those are
visible DAG dependencies with defined cardinality rules. This prevents an
arbitrary expression from being mistaken for a table-producing plan. The
[companion proposal](decoupling_op_and_expr.md) defines this distinction.

The companion's `ScalarExpr` uses `Rc<OperatorNode>` for `PromqlScalarFromVector`,
`ScalarSubquery`, `Exists` and `InSubquery`, so those expressions already reference
this common DAG before and after optimization.

In `scalar(sum(up))`, `scalar()` is Prometheus PromQL's built-in vector-to-scalar
function, explicitly written by the query author. This proposal does not insert
it automatically: `sum(up)` alone is a valid query returning an instant vector.
The scalar expression `PromqlScalarFromVector` represents that function and references its
result to obtain one number. A valid ASAP rewrite may replace that producer with
a summary readout, preserving the required vector and accuracy semantics; it cannot
substitute raw summary state. Ordinary expressions such as `price * 2` reference
columns and literals, not a query sub-DAG.

These are **query sub-DAGs referenced by scalar expressions**, with the same
producer identity as any other operator dependency.

### 1.3 Example: composing a logical DAG

Consider this SQL query, with integer `bytes` and `status` columns:

```sql
SELECT SUM(bytes) + 1 AS total_bytes
FROM requests
WHERE status = 200;
```

Before ASAP optimization, its logical DAG is composed as follows. Each named node is an
`OperatorNode`; arrows point from a consumer to its input producer. The scalar
expressions shown beside nodes are owned fields, not additional DAG nodes.

```text
Project node: OperatorNode
   operator = Operator::NonASAP(NonASAPOp::Project)
   cols[0].expr = ScalarExpr::Arithmetic(Column(sum_bytes), Add, Literal(1))
   │ child: Rc<OperatorNode>
   ▼
Aggregate node: OperatorNode
   operator = Operator::NonASAP(NonASAPOp::Aggregate)
   measures = [AggIntent::Sum(bytes)]
   │ child: Rc<OperatorNode>
   ▼
Filter node: OperatorNode
   operator = Operator::NonASAP(NonASAPOp::Filter)
   pred = Predicate(ScalarExpr::Compare(Column(status), Eq, Literal(200)))
   │ child: Rc<OperatorNode>
   ▼
Scan node: OperatorNode
   operator = Operator::NonASAP(NonASAPOp::Scan)
   source = requests
```

This is abbreviated structural notation: `Column` and `Literal` above are
`ScalarExpr` variants; column names stand for resolved `ColumnId`s. The arithmetic
and comparison use `ExprSemantics::Sql`. The aggregate has no grouping keys and
names its output `sum_bytes`; the projection names its output `total_bytes`.

An eligible ASAP rewrite can implement the sum using an exact accumulator. The
resulting logical DAG contains both operation categories:

```text
Project node: NonASAP(Project)
   expression: sum_bytes + 1
 │ child
 ▼
Finalize node: ASAP(FinalizeExactAccumulator)
   output: ordinary sum_bytes value
 │ child
 ▼
Summary build node: ASAP(SummaryAgg)
   output: exact SUM accumulator state
 │ child
 ▼
Filter node: NonASAP(Filter)
   predicate: status = 200
 │ child
 ▼
Scan node: NonASAP(Scan)
   source: requests
```

The second diagram abbreviates the same nesting: `ASAP(SummaryAgg)` means an
`OperatorNode` whose `operator` is `Operator::ASAP(ASAPOp::SummaryAgg { ... })`.
Its family is `FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)`;
its update reads `bytes`, and it uses the same ungrouped reduction. Finalization
must preserve SQL SUM's NULL and empty-input behavior. This example assumes the
existing capability and rewrite checks permit that exact implementation.

| Part of the design | Role in this example |
|---|---|
| `OperatorNode` | Every DAG node, holding its operation and common result/schema, guarantee and timing properties. |
| `Operator` | Selects the `NonASAP` or `ASAP` operation category in each node. |
| `NonASAPOp` | Scan, filter, aggregate and projection before optimization; scan, filter and projection still use these definitions afterward. |
| `ASAPOp` | Builds accumulator state and finalizes it after the rewrite. |
| `ScalarExpr` | Computes `status = 200` and `sum_bytes + 1` within the filter and projection; neither computation needs a bridge node. |
| `Rc<OperatorNode>` | Connects each consumer to its producer, including `Project.child` pointing to an ASAP finalization node. |

For this example, assume `bytes` is nullable `Int64`. The output metadata is:

| Node | `result_kind` | Output columns (`name: dtype`, nullability) |
|---|---|---|
| Aggregate before optimization | `Relation` | `sum_bytes: Plain(Int64)`, nullable |
| Summary build after optimization | `State` | `sum_state: ExactAggregate(Sum, Sum)`, non-null accumulator state |
| Finalize after optimization | `Relation` | `sum_bytes: Plain(Int64)`, nullable |
| Project in either DAG | `Relation` | `total_bytes: Plain(Int64)`, nullable |

The empty accumulator finalizes to SQL NULL; the accumulator itself is state, not
a nullable numeric value. The projection consumes the finalized column. Guarantees
follow the existing assessment rules, while `timing` may remain `None` until
physical planning. The topmost Project node produces the query result.

This illustrates the connection between the two proposals: scalar separation
makes predicates and value expressions explicit; operator unification lets those
same ordinary operations consume ASAP results through normal DAG edges.

### 1.4 Scope of operator sharing

Here, sharing means pre-ASAP and post-ASAP use the same operator definitions.
A `Project`, for example, has one representation whether its input is an ordinary
aggregate or a summary estimate. This proposal removes the representation boundary;
it does not introduce rules for sharing computations across queries.

## 2. Node properties and why they differ

Both operation categories use the `OperatorNode` declared in the §1.1 overview.
That resolved node follows the current
`SummaryNode` separation between an operation and its metadata, generalized to
all operators. The field is named `operator` because it holds `Operator` (§1.1),
not a scalar expression. The table below explains those common fields; individual
operation variants do not repeat them.

```rust
// Existing enum; the node's Option represents an unassigned phase.
enum ExecutionTiming {
    IngestionTime,
    QueryTime,
}
```

| Field | Meaning | How it is determined |
|---|---|---|
| `operator` | Operation category, parameters and dependencies | `Operator`, `NonASAPOp` and `ASAPOp` in §1 |
| `result_kind`, `schema` | The output category and fields, including identity/time metadata | Derived from `operator` and its actual inputs, then retained on the resolved node (§2.1) |
| `guarantee` | An established result-accuracy guarantee, when available | Existing `ResultGuarantee` and composition rules (§2.2); `None` never means exact |
| `timing` | The assigned ingestion/query execution phase | Physical planning under #509 (§2.3); `None` means not assigned |

`ResultGuarantee` retains its existing definition. `Operator`, `OperatorNode`,
`OperatorResultKind` and the common node layout are proposed; `Schema` is unified
as specified below. This is a resolved-plan interface: name resolution must finish
before producing these concrete `ColumnId`/`Schema` nodes.

| Plan stage | Required property state |
|---|---|
| Resolved frontend / logical candidate | Valid `result_kind` and `schema`; `guarantee` only where established; `timing` may be `None`. |
| Executable physical candidate | Valid output metadata, accuracy acceptable under the existing requirements, and `Some(timing)` for every executable operator. |

Changing an operation or dependency requires re-deriving its output metadata and
revalidating dependent guarantees and timing assignments. Derived fields must not
retain facts from the plan that was replaced. This defines consistency, not a new
caching or mutation mechanism.

### 2.1 One schema model for values and state

Every operator output, before and after optimization, uses one `Schema` whose
`Field`s are typed by `FieldDataType`: `Plain(DataType)` for a readable value, or
the family, algorithm and parameters of summary or exact-accumulator state.
`OperatorResultKind` marks state outputs, and state becomes a value only through
an explicit readout. The schema model, `ColumnRef` versus `ColumnId`, the
validation entry points and the readout boundary are specified in
[Schema and physical data for ASAP primitives](asap-primitive-schema.md).

### 2.2 Preserve existing accuracy semantics

The unified representation must preserve the existing accuracy model, composition
rules and result guarantees. An operation's guarantee must still account for its
actual inputs, including producers referenced by scalar expressions. Reading an
approximate result through `PromqlScalarFromVector` or a SQL scalar subquery does
not make it exact; unknown accuracy must not be treated as exactness.

The common node reuses `guarantee: Option<ResultGuarantee>` from `SummaryNode`.
`Some` records an established guarantee; `None` covers an unassessed or unknown
result, or state whose accuracy is only established at readout. Exactness must be
explicitly established using the existing model. This proposal introduces no new
accuracy metric or guarantee-calculation workflow.

### 2.3 Timing follows the planning-stage design

The [planning-stage design in #509](https://github.com/ProjectASAP/ASAPPlanner/pull/509)
separates logical decisions about what to compute from physical decisions about how
and when to compute it. This proposal follows that division.

For example, a KLL summary build may execute at ingestion time or query time,
depending on the materialization choice. `OperatorNode.timing` records that assignment
as `Some(ExecutionTiming::IngestionTime)` or `Some(ExecutionTiming::QueryTime)`.
Logical nodes may retain `None`; `validate_execution_timing` rejects unassigned
executable nodes. Timing is common node metadata rather than a separate payload
field on selected `ASAPOp` variants.

The representation must preserve the resulting execution constraints: ingestion-time
work cannot depend on query-time results, and consumers must receive values or state
that are available when needed. Materialization choices, retention and plan selection
remain governed by #509; this document does not define another lifecycle policy.
These constraints also apply to query sub-DAGs referenced by scalar expressions.

PromQL evaluation timestamps and SQL statement time are separate from these
execution phases. `TimeShift`, subquery grids and `EvalTimestamp` retain their
source-language evaluation context. A shared node identity alone does not permit
reusing a result across different evaluation times.

## 3. Acceptance criteria

The design is successful when:

- A projection uses the same semantics above and below summary computations.
- A union or another ordinary operator can consume summary estimates on its inputs.
- Unifying the representation preserves existing DAG dependencies, including
  any shared inputs; it does not introduce new sharing rules.
- Existing value/state, accuracy and execution constraints remain enforceable on
  the unified representation.
- Scalar expressions and conversions use the same representation before and after
  optimization, with no bridge nodes or hidden subplans.
- Structural and timing validation include query sub-DAGs referenced by scalar
  expressions; planner assessment includes their accuracy dependencies.
