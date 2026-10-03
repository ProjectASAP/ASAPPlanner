# Schema and Physical Data for ASAP Primitives

This document is the single source of truth for the schema, and column design for ASAP Primitives. This is used in the logical stage (LogicalASAPDAG), and physical stage (PhysicalASAPDAG). 

## 1. Goal, problem, and requirements

Unlike existing Database engines, which work on raw data or explicitly defined materialized tables with schema and column names provided by the users, ASAPPlanner is designed for querying and execution over the mix of raw data and ASAP Primitives. ASAP primitives are usually compact summaries over raw data. Therefore, it introduces new requirement when we design the schema and node definitions for LogicalASAPDAG and PhysicalASAPDAG.

Assuming we have the Logical DAG defined for a canonicalized representation for a batch of queries. [TODO: add links for this here. ]
The LogicalASAPDAG will share/reuse the NonASAP operator and ScalarExpr nodes in LogicalDAG [TODO: link PR 511's doc here], but replacing some operators in LogicalDAG with the operators operated with ASAP Primitives: SummaryCreation?, SummaryUpdate, SummaryMerge, SummaryDelete, SummarySubtraction [TODO: check what is the complete list or discuss with others about the list]. 
Each of the Summary operators also require the ASAP primitive information above to inter-operate correctly, preserving semantic correctness. 

Basically, the following information should be represented to preserve the equivalent query semantics when we introduce ASAP Primitives to logical query representation, and following physical one. 

- What type of the ASAP Primitive is
- What is the ASAP Primitive parameters
- What data sources a ASAP primitive summarizes
- What query intent the summarized ASAP Primitive can support, e.g., statistical aggregation intents, time window aggregation intents



And these information will be combined with relational or time series query operator information, such as group by/reduction, filtering, projection, join, time series selection, together. 

Therefore, these requirements drive the following schema and metadata, node information, and column design. 









--------don't read below-------------


## Schema Design

Schema represents the metadata of information flow along an edge between two nodes in a logical or physical DAG. The schema field is associated with a node in the DAG. 

Schema definition here is shared between LogicalDAG, LogicalASAPDAG, and PhysicalASAPDAG. The schema contain fields, and each field is mapping to a column in the physical data representation. 
Each field should contain the following information. 
1. **The data type of a column.** A state column can be a raw data type (e.g., numerical number, string). It can also be a [summary type](), e.g., the summary family is sketch, and the sketch type is quantile KLL sketch algorithm, and KLL sketch has K  as parameter.   It has a
   family, an algorithm and parameters.
2. ** **. 



## Node design 

Each node in the DAG should contain the information of the instance this nodes is computing, in addition to schema or metadata. 
4. **What a ASAP primitive summarizes.** Two states of the same type can hold different
   data. The planner must know which observations each holds before it combines or
   reuses them.

The schema alone answers the first two but not the third. Every example below
uses two states with **equal schemas**,
`Schema(job: Plain(Utf8), state: Sketch(KLL{k=200}))`.

**Example 1: time.**

| Input A | Input B | Merging A and B is… |
|---|---|---|
| `[00:00, 00:01)` | `[00:01, 00:02)` | correct: p99 over `[00:00, 00:02)` |
| `[00:00, 00:02)` | `[00:01, 00:03)` | **wrong**: `[00:01, 00:02)` is counted twice |
| `[00:00, 00:01)` | `[00:02, 00:03)` | correct only for `[0,1) ∪ [2,3)`, not for `[0,3)` |

`Schema.time_index` is a column position. A KLL state has no timestamp column.

**Example 2: population.**

| Input A | Input B | Merging A and B is… |
|---|---|---|
| `region='us'` | `region='eu'` | correct: `us ∪ eu` within each `job` |
| `region='us'` | `tier='premium'` | **wrong**: premium US requests are in both |
| `region='us'` | `region='us'` | **wrong**: everything is counted twice |

`region` is a filter label, not an output column. `job` says how the state is
grouped, not which rows contributed.

**Example 3: time and population together.** Merging `us × [0,1)` with
`eu × [1,2)` covers exactly those two blocks. One time range plus one label set
would give `{us,eu} × [0,2)`, which claims data that was never read.

**Example 4: reuse.** For `p99(latency) WHERE region='us' AND ts IN [10:00, 10:05)
GROUP BY job`, a stored state with a matching schema could hold the right data,
EU data, or only 10:00–10:03. The schema shows that the state *type* fits, not
that the *contents* fit.

These compositions arise in Pass 2 window composition
([planner layering](planner-layering.md)), in `SummaryMerge` of partial states,
and in sub-DAG sharing and pane reuse. Schema equality is necessary but not
sufficient. Without more metadata, the planner must refuse every composition or
accept silent double counting and missing data.

## 2. Design considerations

```text
OperatorNode
├── operator: Operator                 the operation; SummaryAgg holds input, reduction, filter   (C3)
├── result_kind: OperatorResultKind    Relation | InstantVector | RangeVector | State             (C2)
├── schema: Schema                     the outgoing edge: fields, time, keys, closedness           (C1)
│   └── fields[i].dtype: FieldDataType Plain(DataType) or an ASAP primitive state family          (C2)
├── guarantee, timing                  accuracy and execution phase (operator sharing §2.2, §2.3)
└── coverage: Option<SummaryCoverage>  which observations the state holds                          (C3)
```

### 2.1 Consideration 1: the schema is the edge between two nodes

A node's `schema` types its output edge. The DAG is type-checked: the schema is
derived from the operator and its inputs (`Operator::output_schema`), retained on
the node, and verifiable without surrounding context. One `Schema` type serves
every operator before and after ASAP optimization. It replaced the separate
pre-ASAP `Schema`/`Column` and post-ASAP `SummarySchema`/`SummaryField` (old
plans still deserialize).

| Schema member | Meaning and requirement |
|---|---|
| `fields: Vec<Field>` | Ordered fields. `Field = name + dtype: FieldDataType + nullable + table`. `table` preserves SQL qualified resolution through joins. A `Field` holds metadata, never data. |
| `time_index` | Position of the `Plain(Timestamp)` time column, if any. It does not distinguish an instant vector from a range vector. |
| `unique_keys` | Proven column combinations identifying rows; empty asserts no known key. Rewrites that change identity recompute them. |
| `closed` | Whether `fields` is complete. A schemaless PromQL leaf is open; the first `Aggregate`/`Project` that fully determines its output closes it. Open schemas skip closed-world validation. |

**`ColumnId` versus `ColumnRef`.** These are different roles, not competing
representations:

| Name | Role | Holds runtime values? |
|---|---|---|
| `Schema`, `Field` | Edge metadata | No |
| `ColumnRef` | Unresolved logical reference: `Named`, `Qualified`, `SampleValue`, `Wildcard` | No |
| `ColumnId = usize` | Resolved position in one particular input/output schema | No |
| Runtime batch | Values conforming to a schema (§3) | Yes |

Resolution binds a `ColumnRef` to a `ColumnId` before operator nodes are built.
The same position indexes `schema.fields` for type checking and selects the
value at execution: resolving `t.bytes` to `1` gives its type from
`schema.fields[1]`, and `ScalarExpr::Column(1)` reads `row[1]` in the native
executor (or array `1` in a columnar one). `time_index`, `unique_keys` and group
keys use the same positions. A `ColumnId` is local to its schema, not a stable
identity across projections or joins, so there is no `FieldId`. ASAP payloads
that refer to input data before resolution (`SummaryUpdate`, `SketchStatistic::PointCount`)
keep `ColumnRef`.

**Derivation and validation.**

- `Scan.schema` declares source columns and `Values.schema` the constructed rows;
  every other `OperatorNode.schema` is derived. Planning may override only output
  names and qualifiers (`OperatorNode::with_schema`); all structural metadata must
  equal derivation.
- `ScalarExpr::scalar_type(input)` types an expression against its column scope
  (child schema, both join inputs, or aggregate outputs for `HAVING`).
- `OperatorNode::validate_structure()` walks the reachable DAG: input contracts,
  scalar typing, retained-versus-derived schema and result kind, and coverage
  (§2.3). It permits `timing = None`.
- `OperatorNode::validate_execution_timing()` adds assigned timing and phase
  dependencies, for executable candidates. Neither method proves accuracy;
  guarantees stay with planner assessment (#509).

### 2.2 Consideration 2: a field can have an ASAP primitive type

`FieldDataType` types every field. `Plain` is an ordinary readable value; every
other variant is the state of one ASAP primitive family and carries the identity
and parameters required by that family:

```rust
enum FieldDataType {
    Plain(DataType),                          // readable value
    ExactAggregate(ExactKind, ExactParams),   // Sum, Count, Min, Max, Increase, Rate, IRate
    Sketch(SketchKind, GroupingStrategy),     // KLL, DDSketch, HLL, CMS, CountSketch, UnivMon, …
    Sample(SamplingKind, SamplingParams),
    Wavelet(WaveletKind, WaveletParams),
    StatModel(StatModelKind, StatModelParams),
}
```

**Identity levels.** A sketch has one more level than the other families, because
several algorithms serve one query category (KLL and DDSketch both answer
quantiles):

| Level | Type | Example |
|---|---|---|
| family | `FieldDataType` variant | `Sketch`, `Sample`, `Wavelet`, `StatModel`, `ExactAggregate` |
| category | `SketchCategory` | `Quantile`, `Cardinality`, `Frequency`, `TopK`, `Universal` |
| algorithm | `SketchAlgorithm` | `Kll`, `DDSketch`; `Hll`, `Theta`, `Kmv`; `Cms`, `CountSketch`, … |
| committed choice | `SketchKind` | one validated category + algorithm + `SketchParams` |

`SketchKind::new(algorithm, params)` is the only constructor: it rejects a
parameter variant from another algorithm and classifies the pair into its
category; `.category()`, `.algorithm()` and `.params()` expose the committed
values. `Sample`, `Wavelet` and `StatModel` use flat `(Kind, Params)` pairs;
`ExactParams` is per-kind so a mismatched pair is a type error.
`GroupingStrategy` records the physical layout across `by` subpopulations:
`PerSubpopulationInstance` (default) or `SharedMultiSubpopulation { HydraKind,
HydraParams }`. It is part of the type because a shared Hydra structure and
independent instances are not merge-compatible even with the same algorithm.

Because the full identity is in the type, incompatible states fail at plan
construction: a merge over `Sketch(Kll, …)` and `Sketch(Cms, …)`, or a `Sketch`
read as a `Sample`, is a schema error.

**Rules for state fields.**

- **Top-level only.** Nested `List`/`Struct` elements are `Field<DataType>`, not
  `Field<FieldDataType>`, so a nested field cannot carry state.
- **Produced only by state operators.** `SummaryAgg.family` is never `Plain`; its
  input must be values, not state. Its output is the grouping columns plus one
  non-nullable `state` field of that family.
- **State is not a value.** `scalar_type` rejects a state column ("read it out
  first"). `Filter`, `BinaryOp`, `Join`, `SetOp`, `Concat`, `Aggregate` and
  `Dedup` reject `State` inputs; a bare-column `Project` may pass a state field
  through unchanged (its result stays `State`).
  Copying a state column does not make it readable.
- **Result kind.** `OperatorResultKind::State` marks an output carrying
  unfinalized state; its schema may also contain plain grouping keys. Matching
  columns never make result kinds interchangeable.

**Readout / finalization boundary.** State becomes plain values only through an
explicit ASAP readout, which takes its input's relation/vector kind:

| Readout | Input | Output field |
|---|---|---|
| `SummaryEstimate { query: SketchStatistic }` | exactly one `Sketch` state field whose category supports `query` | `Plain`: `quantile`/`frequency_l2`/`frequency_entropy` `Float64`, `cardinality`/`count` `Int64`, `topk` `Utf8` |
| `FinalizeExactAccumulator` | `ExactAggregate` state | the finalized aggregate value |
| `EvaluatePopulation` | `MaintainPopulation` state | the requested population statistic |

For example, a KLL build outputs `State` with a `Sketch(KLL{k=200})` column; its
p99 readout outputs `Plain(Float64)`. A numeric predicate can use the readout but
not the state.

### 2.3 Consideration 3: the metadata preserves summary semantics

A state is only meaningful together with what it summarizes. The information a
summary's semantics depends on is:

| Concern | Required semantic information |
|---|---|
| Input computation | Source identities and schemas, filters, joins/transforms and their order, or the canonical input sub-DAG |
| Values and grouping | Value expressions, item identities and weights, group keys and types, null/duplicate handling |
| Time | Time column and interpretation, interval bounds, evaluation alignment, query range versus maintained panes |
| Summary computation | Exact operation or sketch family, algorithm and parameters, build/merge behavior |
| Output | State versus finalized value, output schema/type, readout parameters |

KLL over `latency_seconds` and KLL over `log(latency_seconds)` differ even with
identical source, filter, grouping and window. Weighted frequency state needs both
item and weight expressions. Four descriptive fields (`source`, `filter`,
`grouping`, `window`) cannot replace the computation DAG.

The design splits this information by what it varies with, and records each fact
once:

| Where | What it records | Why there |
|---|---|---|
| Field type (`FieldDataType`) | Family, algorithm, parameters, grouping layout | It determines merge compatibility and which readouts apply, so it gates schema equality. |
| Producer operator (`SummaryAgg`) | `input: SummaryUpdate` (item, weight, `weight_domain` proof), `reduction` (group keys or per-entity), `filter`, `grouping`; the child sub-DAG is the input computation | These are the operation's parameters; copying them elsewhere would need a consistency check. |
| Node (`OperatorNode.coverage`) | Which observations: a source and a union of time × population regions | It differs between states that must still merge, and it cannot be derived from `SummaryAgg` alone. |
| Result kind and readout node | State versus value, readout statistic | Derived from the operator (§2.2). |

Time alignment, panes and maintenance lifecycle are planning and deployment
concerns ([planner layering](planner-layering.md),
[physical planning](../physical-planning-and-deployment.md)).

#### Coverage is beside the schema, not inside it

Coverage is not part of `Schema`. `SummaryMerge` requires equal input schemas,
and the inputs of every useful merge (`[0,1)` + `[1,2)`) have different coverage.
Coverage also describes the whole state output, not one field. It is not an
operator parameter either: a merge *derives* it from its inputs, like the schema.

Requirements:

1. Represent time and population **jointly**, per region, never as independent
   bounds.
2. Accept a merge only when the inputs are **provably disjoint**; fail closed.
   Merging does not imply that a family can remove duplicates.
3. Leave `Schema` and its equality unchanged.
4. Duplicate nothing the operator already records.
5. Support sources without a time column (plain tables).

#### What coverage records

`SummaryCoverage` names one observation `source`, using the same `Source` as
`Scan` (a table or a time series), and holds a **union of regions**. Each
`CoverageRegion` pairs:

- `time_ms`: half-open bounds on the source's time column, or `None` for no time
  restriction;
- `population`: a conjunction of non-null `label = value` predicates; empty means
  all observations.

Every observation in a region contributes once to the state. `regions = []` means
known empty coverage. What each observation contributes and how states are
grouped stay on `SummaryAgg.input` and `SummaryAgg.reduction` (requirement 4);
`SummaryMerge` compares those on its producers directly (#560).

#### Composition is a provably disjoint union

`merge_disjoint` accepts inputs with the same source whose regions are pairwise
disjoint. Two regions are disjoint when their time ranges do not intersect, or
when they assign different values to the same label. Different labels prove
nothing, and a region without time bounds overlaps any region it is not
population-disjoint from. The union keeps gaps and the time/population pairing;
adjacent intervals coalesce only when their populations are identical.

| Case | Result |
|---|---|
| `[0,1)` + `[1,2)`, same population | one region `[0,2)` |
| `[0,1)` + `[2,3)` | two regions (gap kept) |
| `[0,2)` + `[1,3)` | rejected: possible overlap |
| `region=us` + `region=eu`, same time | two regions |
| `region=us` + `region=us`, or + `tier=premium` | rejected: possible overlap |
| `us×[0,1)` + `eu×[1,2)` | two regions, never `{us,eu}×[0,2)` |
| different source | rejected |

Equality conjunctions are a deliberately narrow proof vocabulary. A richer
predicate needs an explicit disjointness rule before it can be declared.

#### Lifecycle

- **Required on summary nodes.** `SummaryAgg` cannot pass `validate_structure`
  without coverage; `SummaryMerge` joins it in #560. Coverage on a non-`State`
  node is rejected. The field is an `Option` only because all operators share
  `OperatorNode`.
- **Declared at build.** The composition rule or catalog that builds a
  `SummaryAgg` declares it (`with_coverage`). No production builder declares it
  on this branch yet.
- **Derived at merge (#560).** `SummaryMerge` computes the disjoint union of its
  inputs, and validation rejects a retained value that differs.
- **Cleared on rewrite.** `map_children` rebuilds the node without coverage, like
  `guarantee` and `timing`. The rewriter must declare it again.
- **Preserved downstream (#537).** Logical export keeps coverage, and CSE shares
  two nodes only if their coverage is equal.

#### Trust boundary

Declarations are trusted. Population is not yet checked against
`SummaryAgg.filter`, `Filter` nodes or `Scan.predicates`, so a wrong declaration
passes:

```text
A = SummaryAgg(filter: region='us'), declared {region: eu} × [0,1)   ← wrong
B = SummaryAgg(filter: region='us'), declared {region: us} × [0,1)
merge_disjoint(A, B) is accepted, and every US observation is counted twice.
```

[#570](https://github.com/ProjectASAP/ASAPPlanner/issues/570) adds the check: the
declared population must equal the `column = literal` predicates between the
`SummaryAgg` and its `Scan`. Time bounds stay trusted, because `TimeRange` is
relative to the evaluation time.

## 3. Physical data: how a state column is carried

The runtime uses the same `Schema` as planning (`SchemaRef = Arc<Schema>`). The
native executor (`asap-physical-operators`) stores `Batch { schema, rows:
Vec<Vec<Value>> }`; the row/column layout is executor-specific. A state column
holds a typed value:

```rust
enum Value {
    Null, Bool(..), Int64(..), Float64(..), Utf8(..), Timestamp(..), Date(..),
    Interval { .. }, List(..), Struct(..), Map(..),
    Summary { family: FieldDataType, state: Arc<dyn AggregateCore> },
}
```

- **Typed at the boundary.** `Batch::try_new` checks each `Summary` value's
  `family` equals the field's `FieldDataType`, and that the payload's shape
  (algorithm and parameters, for example KLL `k` or CMS width × depth) matches it.
  State fields must be non-nullable and of a natively supported family.
- **Not a key.** A `Summary` value cannot be a grouping key or be ordered.
- **Kernels.** `summary_kernels` adapt `asap_sketchlib` structures and exact
  Planner state behind `AggregateCore`: `merge_with` (same family and shape),
  `estimate(SketchStatistic)`, and `approx_memory_bytes` for memory reservations.
  `create_planner_accumulator(family, input, grouping)` builds the updater a
  `SummaryAgg` declares and rejects a family/grouping disagreement.
- **Native coverage.** Exact Sum/Count/Min/Max/Rate/Increase, KLL, DDSketch, HLL,
  Count-Min (stored state only), and weighted CMS/CountSketch with heaps.
  `SharedMultiSubpopulation` grouping, `Sample`, `Wavelet` and `StatModel` have
  no native kernel and are rejected at binding.
- **No encoding here.** `Value::Summary` is not serialized; byte encodings belong
  to `asap_sketchlib` and deployments.

Coverage is plan metadata and is not carried in runtime values. Physical merge of
states by group key checks family equality only; disjointness is proven at
planning time (§2.3).

## 4. Alternatives considered

| Alternative | Why not |
|---|---|
| Separate pre-ASAP and post-ASAP schema types | A projection above a summary needs a different representation from one below it; one `Schema` removes the barrier. |
| An opaque "state" type without family identity | KLL + CMS merges, and sketch-versus-sample confusion, would only fail at runtime. |
| State inside `List`/`Struct` fields | Nested state would escape the readout boundary and state validation. |
| Put coverage in `Schema` | Schema equality gates merges; merge inputs always differ in coverage. |
| One time range plus one label set | Invents the missing blocks (Example 3). |
| Copy `input` and `reduction` into coverage | Duplicates `SummaryAgg` and needs a consistency check; producers already carry them. |
| Arbitrary predicates per region | No general disjointness proof; overlap would be silently accepted. |
| Free-form string `source` | Two spellings of one table compare unequal; `Scan` already has `Source`. |
| Snapshot `revision` field | Deployment concern; the planner does not own catalog versions. |

## 5. Key code interfaces

```rust
// crates/types/src/pre_asap/schema.rs
pub type ColumnId = usize;
pub struct Field<T = FieldDataType> { pub name: String, pub dtype: T, pub nullable: bool, pub table: Option<String> }
pub struct Schema {
    pub fields: Vec<Field>,
    pub time_index: Option<ColumnId>,
    pub unique_keys: Vec<Vec<ColumnId>>,
    pub closed: bool,
}
pub enum FieldDataType { Plain(DataType), ExactAggregate(..), Sketch(SketchKind, GroupingStrategy), Sample(..), Wavelet(..), StatModel(..) }
// DataType::List { element: Box<Field<DataType>> }, DataType::Struct { fields: Vec<Field<DataType>> }

// crates/types/src/post_asap/sketch.rs
impl SketchKind { pub fn new(algorithm: SketchAlgorithm, params: SketchParams) -> Self; }
pub enum GroupingStrategy { PerSubpopulationInstance, SharedMultiSubpopulation { kind: HydraKind, params: HydraParams } }
pub struct SummaryUpdate { pub item: Option<SummaryInputExpr>, pub weight: SummaryInputExpr, pub weight_domain: WeightDomain }

// crates/types/src/ir/asap.rs
pub enum ASAPOp {
    SummaryAgg { child, family: FieldDataType, input: SummaryUpdate, reduction: Reduction,
                 grouping: GroupingStrategy, filter: Option<Predicate> },
    SummaryEstimate { summary_input, query: SketchStatistic },
    FinalizeExactAccumulator { child },
    SummaryMerge { children },            // reserved here; structure in #560
    // MaintainPopulation, EvaluatePopulation, SummarySubtract, SummaryDelete, SummaryJoin, Extension
}

// crates/types/src/ir/summary_coverage.rs
pub struct SummaryCoverage { pub source: Source, pub regions: Vec<CoverageRegion> }
pub struct CoverageRegion {
    pub time_ms: Option<Range<i64>>,          // half-open; None = no time restriction
    pub population: BTreeMap<String, String>, // label = value AND …; empty = all
}
impl SummaryCoverage {
    pub fn validate(&self) -> Result<(), CoverageError>;
    pub fn merge_disjoint(inputs: &[Self]) -> Result<Self, CoverageError>;
}
pub enum CoverageError {
    InvalidInterval, InvalidPopulation, SourceMismatch, PossibleOverlap, EmptyMerge,
    NotState, Missing,
    // #560: UnknownInput, MergeOutputMismatch
}

// crates/types/src/ir/node.rs
pub enum OperatorResultKind { Relation, InstantVector, RangeVector, State }
pub struct OperatorNode {
    pub operator: Operator, pub result_kind: OperatorResultKind, pub schema: Schema,
    pub guarantee: Option<ResultGuarantee>, pub timing: Option<ExecutionTiming>,
    pub coverage: Option<SummaryCoverage>,
}
impl OperatorNode {
    pub fn with_schema(operator: Operator, schema: Schema) -> Self;
    pub fn with_coverage(self, c: SummaryCoverage) -> Result<Self, SchemaDerivationError>;
    pub fn requires_coverage(&self) -> bool;  // SummaryAgg; SummaryMerge in #560
    pub fn validate_structure(self: &Rc<Self>) -> Result<(), SchemaDerivationError>;
    pub fn validate_execution_timing(self: &Rc<Self>) -> Result<(), SchemaDerivationError>;
    // #560: pub fn summary_update(&self) -> Option<(&SummaryUpdate, &Reduction)>;
}
// SchemaDerivationError::Coverage(CoverageError) reports coverage failures.

// crates/asap-physical-operators/src/{values.rs, summary_kernels/traits.rs}
pub enum Value { /* plain variants */ Summary { family: FieldDataType, state: Arc<dyn AggregateCore> } }
pub trait AggregateCore {
    fn merge_with(&self, other: &dyn AggregateCore) -> Result<Box<dyn AggregateCore>, KernelError>;
    fn estimate(&self, query: &SketchStatistic) -> Result<f64, KernelError>;
    fn approx_memory_bytes(&self) -> usize;
}
```

Coverage composition is tested in `crates/types/tests/summary_coverage.rs`. The
documented examples are built as real `Scan → SummaryAgg → SummaryMerge` plans in
`crates/types/tests/summary_coverage_examples.rs` (#560).
