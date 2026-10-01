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

`Operator` describes an ordinary or ASAP operation; `OperatorNode` combines it
with common planning properties. `ScalarExpr` describes value computation. The
following overview and payload definitions are the canonical resolved interfaces
used by both proposals. The companion document defines `ScalarExpr` and its
operator-field wrappers; it does not define a second operator model.

**Proposed data structures — overview.** The complete outer structure is below;
operation variants and schema internals are expanded afterward. These declarations
are shared by the detailed sections, not separate abbreviated types.

```rust
// A graph node combines its operation with common planning properties (§2).
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
// ScalarExpr: an owned value-expression tree, defined in the companion proposal.
// Schema / OperatorResultKind: defined in §2.1.
```

An operator owns its scalar expressions and references input nodes through
`Rc<OperatorNode>`. Either operation category can consume the other's outputs when
the input contract permits it. `NonASAP` describes one operation, not its entire
subgraph. Frontend graphs contain only NonASAP operations; ASAP optimization may
introduce state construction and readout.

| Category | Meaning | All operations |
|---|---|---|
| `Operator::NonASAP(NonASAPOp)` | Ordinary query operations that transform, combine or aggregate data | `Scan`, `Values`, `Filter`, `Project`, `Aggregate`, `Join`, `SetOp`, `Concat`, `Dedup`, `Sort`, `Limit`, `BinaryOp`, `SQLWindowFunc`, `TimeRange`, `TimeShift`, `PromqlVectorFromScalar`, `PromqlRelabel`, `PromqlInfoEnrich`, `PromqlSeriesSample`, `PromqlSubquery` |
| `Operator::ASAP(ASAPOp)` | Operations on summary state and its results, including reserved operations | `SummaryAgg`, `SummaryEstimate`, `SummaryMerge`, `SummarySubtract`, `SummaryDelete`, `SummaryJoin`, `FinalizeExactAccumulator`, `MaintainPopulation`, `ReadPopulation`, `Extension` |

`CurrentTimestamp`, `EvalTimestamp` and `PromqlScalarFromVector` belong to
`ScalarExpr`, defined in the [companion proposal](decoupling_op_and_expr.md#22-scalar-expressions).
A constant needs no bridge operator. The sketches use resolved `ColumnId`s and
`Schema`; name resolution precedes construction of these nodes. Compatibility
changes from current types are collected in §6.

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
`FieldType` (§2.1) types every output field; state-producing operations use its
summary or exact-accumulator cases, never its `Plain` case.

```rust
enum ASAPOp {
    SummaryAgg {
        child: Rc<OperatorNode>, family: FieldType, input: SummaryUpdate,
        reduction: Reduction, grouping: GroupingStrategy,
    },
    SummaryEstimate {
        summary_input: Rc<OperatorNode>, query: SketchQuery,
    },
    FinalizeExactAccumulator { child: Rc<OperatorNode> },
    MaintainPopulation { child: Rc<OperatorNode>, population: MaintainedPopulation },
    ReadPopulation { child: Rc<OperatorNode>, readout: PopulationReadout },

    // Reserved operations; semantics and support require further design.
    SummaryMerge { children: Vec<Rc<OperatorNode>> },
    SummarySubtract { left: Rc<OperatorNode>, right: Rc<OperatorNode> },
    SummaryDelete { summary_input: Rc<OperatorNode>, key: ColumnId },
    SummaryJoin {
        outer: Rc<OperatorNode>, inner: Rc<OperatorNode>, key: ColumnId, family: FieldType,
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
visible graph dependencies with defined cardinality rules. This prevents an
arbitrary expression from being mistaken for a table-producing plan. The
[companion proposal](decoupling_op_and_expr.md) defines this distinction.

The companion's `ScalarExpr` uses `Rc<OperatorNode>` for `PromqlScalarFromVector`,
`ScalarSubquery`, `Exists` and `InSubquery`, so those expressions already reference
this common graph before and after optimization.

In `scalar(sum(up))`, `scalar()` is Prometheus PromQL's built-in vector-to-scalar
function, explicitly written by the query author. This proposal does not insert
it automatically: `sum(up)` alone is a valid query returning an instant vector.
The scalar expression `PromqlScalarFromVector` represents that function and references its
result to obtain one number. A valid ASAP rewrite may replace that producer with
a summary readout, preserving the required vector and accuracy semantics; it cannot
substitute raw summary state. Ordinary expressions such as `price * 2` reference
columns and literals, not a query subgraph.

These are **query subgraphs referenced by scalar expressions**, with the same
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
Its family is `FieldType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)`;
its update reads `bytes`, and it uses the same ungrouped reduction. Finalization
must preserve SQL SUM's NULL and empty-input behavior. This example assumes the
existing capability and rewrite checks permit that exact implementation.

| Part of the design | Role in this example |
|---|---|
| `OperatorNode` | Every graph node, holding its operation and common result/schema, guarantee and timing properties. |
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
| Project in either graph | `Relation` | `total_bytes: Plain(Int64)`, nullable |

The empty accumulator finalizes to SQL NULL; the accumulator itself is state, not
a nullable numeric value. The projection consumes the finalized column. Guarantees
follow the existing assessment rules, while `timing` may remain `None` until
physical planning. The topmost Project node produces the query result.

This illustrates the connection between the two proposals: scalar separation
makes predicates and value expressions explicit; operator unification lets those
same ordinary operations consume ASAP results through normal graph edges.

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

Use one `Schema` for operator outputs before and after optimization. Rename today's
`SummaryFamilyType` to `FieldType`: it types every field, and `Plain` is not a summary
family. Rename `Column` to `Field` and `Schema.columns` to `Schema.fields`: the struct
describes a column and holds none of its data. Retain the current `Schema` metadata.
The following is the proposed resolved interface; it is not the current Rust definition.

```rust
struct Field {
    name: String,
    dtype: FieldType,
    nullable: bool,
    table: Option<String>,
}

struct Schema {
    fields: Vec<Field>,
    time_index: Option<ColumnId>,
    unique_keys: Vec<Vec<ColumnId>>,
    closed: bool,
}

// Today's `SummaryFamilyType`, renamed; variants and payloads unchanged.
enum FieldType {
    Plain(DataType),
    ExactAggregate(ExactKind, ExactParams),
    Sketch(SketchKind, GroupingStrategy),
    Sample(SamplingKind, SamplingParams),
    Wavelet(WaveletKind, WaveletParams),
    StatModel(StatModelKind, StatModelParams),
}

// Proposed derived output classification, separate from column types.
enum OperatorResultKind {
    Relation,
    InstantVector,
    RangeVector,
    State,
}

impl Operator {
    fn output_schema(&self) -> Result<Schema, QueryExprError>;
    fn output_kind(&self) -> Result<OperatorResultKind, QueryExprError>;
    fn validate_inputs(&self) -> Result<(), QueryExprError>;
}

impl OperatorNode {
    fn validate_structure(&self) -> Result<(), QueryExprError>;
    fn validate_execution_timing(&self) -> Result<(), QueryExprError>;
}

impl ScalarExpr {
    fn scalar_type(&self, input: &Schema) -> Result<(DataType, bool), QueryExprError>;
}
```

**Relationship to current types.** `Field` is today's pre-ASAP `Column` with `dtype`
widened from `DataType` to `FieldType`. `FieldType` is today's `SummaryFamilyType`
under a name that also fits its `Plain` case. The proposed common `Schema` replaces
the separate operator-edge roles of pre-ASAP `Schema` and post-ASAP `SummarySchema` /
`SummaryField`; it does not rename `DataType`. A pre-ASAP value column becomes
`Plain(dtype)`.
Frontend validation permits only ordinary value columns, preserving the current
pre-ASAP restriction even though the common schema can also express state.

| Field | Meaning and requirement |
|---|---|
| `fields` | Ordered named fields. `Plain(DataType)` is a readable value; other variants retain the identity and parameters of summary or exact-accumulator state. |
| `Field.nullable`, `Field.table` | Preserve SQL nullability and qualified column resolution. |
| `time_index` | Identifies the time column when present; it does not by itself distinguish an instant vector from a range vector. |
| `unique_keys` | Proven column combinations identifying rows; an empty list asserts no known key. Recompute these proofs when a rewrite changes identity. |
| `closed` | Whether `fields` completely describes the output. An open PromQL schema must retain unlisted labels through the existing complete-series-identity contract. |

`OperatorResultKind` is derived from the operation and its inputs and retained as
`OperatorNode.result_kind`. `State` describes an output carrying unfinalized state; its
schema may also contain ordinary grouping keys. `SummaryEstimate`,
`FinalizeExactAccumulator` and other readouts derive the appropriate relation or
vector kind from their operation and input context. Matching numeric columns do
not make those kinds interchangeable.

**Interface contracts.** `Operator::output_schema` and `output_kind` derive output
metadata from the payload and validated inputs. `validate_inputs` checks local
producer/consumer compatibility, such as vector inputs for `BinaryOp` or the
required state family for a summary readout. Scalar typing checks the input-kind
contract of `PromqlScalarFromVector` and other scalar plan reads.

| Validation entry | Scope and stage |
|---|---|
| `OperatorNode::validate_structure()` | Walks the reachable operator graph, including scalar plan references; checks input contracts, scalar typing and agreement between retained and derived output metadata. Valid for logical and physical plans; permits `timing = None`. |
| `OperatorNode::validate_execution_timing()` | Includes structural validation, then requires assigned timing on every executable operator and checks phase dependencies. Used for executable physical candidates. |
| Existing planner assessment and selection (#509) | Establishes guarantees using the existing accuracy models and checks them against request requirements and deployment capabilities. Neither node method re-proves a guarantee or decides request feasibility. |

The two node methods need only the graph and its annotations. Request requirements
and deployment models remain inputs to the existing planning/selection workflow,
not implicit globals of `validate_structure`. Passing the timing check alone does
not establish that a physical candidate satisfies the query's accuracy requirement.

`Scan.schema` declares the source columns; `Values.schema` declares the constructed
row shape. `OperatorNode.schema` is the derived output for any operation. A scan's
predicates cannot change its declared output columns; a Values row must match the
declared arity, types and nullability. These leaf outputs retain the declaration's
column layout and time/identity information, with only justified metadata changes.
The declaration and derived output therefore have distinct roles, and structural
validation rejects disagreement rather than trusting two independent schemas.

`scalar_type` keeps the existing method name and `(DataType, nullable)` result.
Its `input` is the applicable column scope: the child schema for a projection,
both input schemas for a join predicate, or aggregate outputs for `HAVING`.
Explicit subquery/conversion expressions validate their referenced producer using
the contracts above. Numeric expressions cannot consume state columns as numbers.
A standalone scalar expression is checked with an empty column scope and needs no fabricated
relation output schema. `QueryExprError` retains the existing error-type name;
result-kind, state-family, schema and execution-phase mismatches require
corresponding validation errors.

For example, a KLL build outputs `State` with a
`Sketch(SketchKind, GroupingStrategy)` column identifying KLL and its parameters.
Its p99 readout outputs an ordinary `Plain(Float64)` column in the appropriate
relation/vector schema. A numeric predicate can use that readout, but not the KLL
state. Exact accumulator state similarly requires `FinalizeExactAccumulator`.
An ordinary operator may pass state through only where its input/output contract
permits it. A bare-column projection can preserve the field's `FieldType`
directly during `output_schema` derivation; `scalar_type` applies when that column
is used as a scalar value and rejects state. Copying a state column does not turn
it into a readable scalar.

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
| Frontends | Represent queries using operators and scalar expressions; any referenced operator nodes are `NonASAP`. Preserve source-language semantics. |
| Logical ASAP-aware optimization | Form candidate graphs containing ordinary and summary operators, with no wrappers hiding their dependencies. |
| Physical ASAP-aware optimization | Determine executable alternatives, including materialization and execution timing, for those candidate graphs. |
| Plan selection | Evaluate complete physical candidates using workload requirements and deployment-provided models and capabilities. |
| Deployment execution | Execute the selected graph, preserving its dependencies and assigned phases. |

## 4. Export preserves the graph

Export one node per operator and represent its input dependencies as edges. Export
a shared producer once, with edges to all its consumers. Include dependencies
referenced by scalar conversions and subqueries. Preserve scalar queries as
expressions and their operator dependencies; do not invent a bridge node for export.

Export preserves result kinds, resolved schemas, scalar value types and evaluation
context, together with the applicable guarantees and assigned execution phases.
Physical compilation may lower one logical operation to several physical
operations, but must preserve its dependencies and meaning. The execution layer
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
- Scalar expressions and conversions use the same representation before and after
  optimization, with no bridge nodes or hidden subplans.
- Structural and timing validation include query subgraphs referenced by scalar
  expressions; planner assessment includes their accuracy dependencies.
- Export preserves visible dependencies and shared producers.

## 6. Scope and compatibility

The two documents define one resolved interface: this document owns
`OperatorNode`, `Operator`, operation payloads and schema/validation interfaces;
the companion owns `ScalarExpr`, its wrappers and language-semantic mappings.
Existing type names are retained where their meanings still apply.

| Change from current code | Final representation and compatibility rule |
|---|---|
| Separate ordinary/summary models and relational wrappers | Both categories use `OperatorNode` dependencies; no wrapped relational subplan. |
| Mixed operator/scalar `QueryExpr` | Owned scalar expressions in operator fields; explicit scalar conversions/subqueries reference `OperatorNode`. |
| Different pre-/post-ASAP binary payloads | One `BinaryOp.operator: BinaryOperator`, retaining `kind`, `vector_match` and checked-division flags. `return_bool` on `BinaryOp` adds PromQL comparison mode. |
| Limit and time selection differences | Retain post-ASAP `Limit.partition_by`; optional `n` supports offset-only queries. `TimeRange.kind` distinguishes instant/range selection. |
| Reserved ASAP key references | `SummaryDelete.key` and `SummaryJoin.key` use resolved `ColumnId`s, like other column references in this graph. |
| Missing relation constructor | `Values` represents SQL literal rows and the one empty input row for SELECT without FROM. |
| Separate edge schemas | Common `Schema` uses `Field` / `FieldType` while keeping the existing identity/time metadata (§2.1). |
| `SummaryFamilyType`, `Column`, `Schema.columns` | Renamed `FieldType`, `Field`, `Schema.fields`; variants and payloads unchanged. |

A rewrite that moves arithmetic into a scalar expression must preserve applicable
checked-division guards and exact fallback. `ExprSemantics` selects language rules;
it does not replace those proof conditions. Reserved ASAP operations still require
their own semantic/capability design.

The [companion semantic tables](decoupling_op_and_expr.md#3-semantic-requirements)
use DataFusion 55.1.0 and Prometheus 3.15.0 as design targets. Their gaps also apply
here: a common `Operator` type does not supply missing aggregate modifiers, value
types or function contracts. This proposal changes neither repository dependencies
nor the set of implemented language features.

New accuracy metrics, accuracy-composition rules, computation-sharing algorithms and
lifecycle policies are outside this proposal. Planning responsibilities follow #509.
The scalar/operator separation is specified in the companion document. Storage,
traversal algorithms, serialization fields and a code migration sequence are also
outside this document.
