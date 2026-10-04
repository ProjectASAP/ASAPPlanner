# Schema and Physical Data for ASAP Primitives

This document is the single source of truth for the schema, and column design for ASAP Primitives. This is used in the logical stage (LogicalASAPDAG), and physical stage (PhysicalASAPDAG). 

## 1. Goal, problem, and requirements

Unlike existing Database engines, which work on raw data or explicitly defined materialized tables with schema and column names provided by the users, ASAPPlanner is designed for querying and execution over the mix of raw data and ASAP Primitives. ASAP primitives are usually compact summaries over raw data. Therefore, it introduces new requirement when we design the schema and node definitions for LogicalASAPDAG and PhysicalASAPDAG.

Assuming we have the Logical DAG defined for a canonicalized representation for a batch of queries. [TODO: add links for this here. ]
The LogicalASAPDAG will share/reuse the NonASAP operator and ScalarExpr nodes in LogicalDAG [TODO: link PR 511's doc here], but replacing some operators in LogicalDAG with the operators operated with ASAP Primitives: SummaryCreation?, SummaryUpdate, SummaryMerge, SummaryDelete, SummarySubtraction, SummaryEstimate [TODO: check what is the complete list or discuss with others about the list]. 
Each of the Summary operators also require the ASAP primitive information above to inter-operate correctly, preserving semantic correctness. 

Basically, the following information should be represented to preserve the equivalent query semantics when we introduce ASAP Primitives to logical query representation, and following physical one. 

- What type of the ASAP Primitive is
- What is the ASAP Primitive parameters
- What data sources a ASAP primitive summarizes
- What query intent the summarized ASAP Primitive can support, e.g., statistical aggregation intents, time window aggregation intents



And these information will be combined with relational or time series query operator information, such as group by/reduction, filtering, projection, join, time series selection, together. 

Therefore, these requirements drive the following schema and metadata, node information, and column design. 



## 2. Existing database terminology for schema, table, column, and physical data layout


## 3. Proposed schema design 
Schema represents the **metadata** of information flow along an **edge** between two nodes in a logical or physical DAG. The schema field is associated with the node in the DAG. The consumer of the node in the DAG takes the schema from the producer node as input. 

Schema definition here is shared between LogicalDAG, LogicalASAPDAG, and PhysicalASAPDAG. The schema contain fields, and each field is mapping to a column in the physical data representation. 
Based on our requirement, each field should contain the following information.
1. **What type of the ASAP Primitive is** A state column can be a raw data type (e.g., numerical number, string). It can also be a [summary type](TODO: add link), e.g., the summary family is sketch, and the sketch type is quantile KLL sketch algorithm, and KLL sketch has K  as parameter as the schema. (TODO: confirm the terminology with corresponding code/doc)  It has a family, an algorithm and parameters. 
2. **What query intent the summarized ASAP Primitive can support, e.g., statistical aggregation intents, time window aggregation intents** This information is being mapped based on the primitive type. 


## 4. Proposed Node field design 

A node in the physical data will represent the data or summary instance, so a node has a field for **What data sources a ASAP primitive summarizes**.

Based on the above the proposed OperatorNode interface is as below:
```rust
```

## 5. Examples on how OperatorNode, schema, and physical data information are being used with Summary operators

Given that these information requirements are introduced by summary operators to work correctly semantically, we show the examples of how the defined OperatorNode, schema, and physical data information work with each kind of summary operators. 

Notation: an edge is written `──Kind(field Type, …)──▶`. Schemas are the ones `output_schema()` derives. Planning may rename fields through `OperatorNode::with_schema`, but types, nullability, `time_index`, `unique_keys` and `closed` must match the derivation. All examples use a table source, so values are `Relation`; with a `TimeSeries` source the value side is `InstantVector`.

### 5.1 `SummaryAgg`: values → state

Scenario: p99 latency by job, from KLL(k=200), over one minute of table `t`.

```text
Scan(t: job Utf8, latency Float64)
  ──Relation(job Utf8, latency Float64)──▶
SummaryAgg(family = Sketch(KLL{k=200}, PerSubpopulationInstance),
           input = SummaryUpdate::column(Named("latency")), reduction = by[job],
           grouping = PerSubpopulationInstance, filter = None)
  ──State(job Utf8, state Sketch(KLL{k=200}, PerSubpopulationInstance))──▶
  coverage = { source: Table "t", regions: [{ time_ms: 0..60_000, population: {} }] }
```

- Output schema: the `by` keys followed by one non-nullable field `state` typed `family`; `unique_keys = [[0]]`, `closed = true`, no `time_index`. With `Reduction::PerEntity` the input columns are kept and the sample-value column is replaced by `state`.
- Checks: `family` is not `Plain`; the child is not `State`; the `weight`/`item` columns resolve against the child schema; `filter`, if present, types as `Bool`.
- Coverage: **required** and **declared**. `OperatorNode::new` leaves it `None`, `validate_structure` fails with `CoverageError::Missing`, and the planner attaches it with `with_coverage`.
- Boundary: this is where values become state. The sketch family, algorithm and parameters are committed in the field type, and `guarantee` stays `None` because state is not a caller-visible value.

### 5.2 `SummaryEstimate`: sketch state → value

Scenario: read p99 from the state in 5.1.

```text
──State(job Utf8, state Sketch(KLL{k=200}))──▶
SummaryEstimate(query = SketchStatistic::Quantile { q: 0.99 })
  ──Relation(job Utf8, quantile Float64)──▶      (planner may rename to p99)
```

- Output schema: the input schema with the one non-plain field replaced by a non-nullable plain field. Its name and type come from the statistic: `quantile`/`frequency_l2`/`frequency_entropy` Float64, `cardinality`/`count` Int64 (Float64 if the producer is a `PerEntity` `SummaryAgg`). Keys and metadata pass through. A top-k readout is the exception: it returns the selected rows, one per ranked item, with the partition keys, the item identity columns, and a `value` Float64 score (#579). This is the same row shape as an exact Sort → Limit top-k, so the plans for one query share a root schema.
- Result kind: the value kind of the source the state was built from (`Relation` here).
- Checks: input is `State` with exactly one non-plain field, that field is `Sketch`, and its category accepts the statistic (§3). For example, `Cardinality` on KLL is rejected.
- Coverage: **absent**. The output is a value, and `with_coverage` returns `NotState`.
- Boundary: state is consumed and a value is produced; `guarantee` on this node carries the readout's error bound.

### 5.3 `FinalizeExactAccumulator`: exact state → value

Scenario: total bytes by host with an exact Sum accumulator.

```text
Scan(t: host Utf8, bytes Float64)
  ──Relation(host Utf8, bytes Float64)──▶
SummaryAgg(family = ExactAggregate(Sum, Sum), input = column(Named("bytes")), reduction = by[host])
  ──State(host Utf8, state ExactAggregate(Sum, Sum))──▶   coverage: required, declared
FinalizeExactAccumulator
  ──Relation(host Utf8, state Float64)──▶
```

- Output schema: each `ExactAggregate` field keeps its name (`state`) and takes the type and nullability the equivalent `NonASAPOp::Aggregate` would give: Sum/Min/Max follow the input column, Count is Int64, and Rate/IRate/Increase are Float64. If the child is not a `SummaryAgg` directly, Count falls back to Int64 and the others to Float64. `unique_keys`, `closed` and `time_index` are preserved (`schema_rebuilding.rs`).
- Checks: the input is `State` and contains an `ExactAggregate` field; a sketch is rejected (`structure_contract.rs`).
- Coverage: **absent** on the output.
- Boundary: this is the explicit maintenance-to-read boundary for exact state. Exact state is never read through `SummaryEstimate`.

### 5.4 `MaintainPopulation`: values → maintained membership (state)

Scenario: keep the full latency population per job, so that p99 and top-10 can be evaluated later.

```text
Scan(t: job Utf8, latency Float64)  [closed schema]
  ──Relation(job Utf8, latency Float64)──▶
MaintainPopulation(population = MaintainedPopulation {
    input: PopulationInput::Rows { input: <that Scan>, value_column: 1, grouping: by[job] },
    max_k: 10, quantiles: true })
  ──State(job Utf8, latency Float64)──▶
```

- Output schema: identical to the child's, all plain. Only `result_kind = State` marks it as maintained state.
- Checks: `population.matches_node(child)`. For `Rows`, the child must be the same closed table `Scan`, the value column must be non-null Float64, and grouping must be `by` with in-range keys. For `CurrentSeries`, it must be a `TimeSeries` scan with the same metric, matchers and grouping labels, under an instant `TimeRange` of `lookback_ms` (which may be omitted only for the default 300 s lookback).
- Coverage: **not required**. `with_coverage` accepts it because the output is `State`.
- Boundary: the output is state because it must also track membership changes; downstream operators can only read it through `EvaluatePopulation`.

### 5.5 `EvaluatePopulation`: maintained membership → value

Scenario: p99 by job from the population in 5.4.

```text
──State(job Utf8, latency Float64)  [from MaintainPopulation]──▶
EvaluatePopulation(evaluation = PopulationStatistic::Quantile { q: 0.99 })
  ──Relation(job Utf8, quantile_0_99 Float64)──▶
```

- Output schema: the schema of `Aggregate(by grouping, measure)` over the maintained source. Quantile gives `quantile_<q>` Float64, Sum gives `sum` (value type), Count gives `count` Int64 and Average gives `avg` Float64; `unique_keys = [[0]]`, `closed`. `TopK { k }` instead returns the source schema unchanged (the selected rows).
- Checks: the child is a `MaintainPopulation` node whose `supports(evaluation)` holds: `quantiles` must be set for `Quantile`, and `k <= max_k` for `TopK`.
- Coverage: **absent**.
- Boundary: maintained membership is read as a value; the result kind is the source's (`Relation`).

### 5.6 `SummaryMerge` (#560): state × N → state

On this branch, `SummaryMerge { children }` is **reserved**. `is_unimplemented()` returns true, and `output_schema()` and `validate_inputs()` return `UNIMPLEMENTED_ASAP_OP`, so `OperatorNode::new` fails. Only `output_kind()` (= `State`), `children`, `map_children` and `kind_name` work. #560 enables it as follows (`summary_merge_structure.rs` in #560).

Scenario: combine two one-minute KLL panes over `Scan(t: value Float64)` into a two-minute state.

```text
SummaryAgg(KLL k=200, column(SampleValue), by[]) ──State(state Sketch(KLL{k=200}))── coverage {t, [0..60_000)} ─┐
SummaryAgg(KLL k=200, column(SampleValue), by[]) ──State(state Sketch(KLL{k=200}))── coverage {t, [60_000..120_000)} ─┴▶
SummaryMerge
  ──State(state Sketch(KLL{k=200}))──▶   coverage = { t, [0..120_000) }   (derived)
```

- Output schema: `children[0].schema`.
- Checks: at least one input; exactly one state column; every input is `State` with an identical schema (so family, params, grouping strategy and key positions match); every input has the same `summary_update()` (update expression and reduction); and `merged_coverage()` succeeds. Merging k=200 with k=300 fails, and so does merging raw rows.
- Coverage: **required** and **derived**. `OperatorNode::new` sets it to `SummaryCoverage::merge_disjoint` of the input coverages. An input without coverage gives `UnknownInput`, and overlapping inputs give `PossibleOverlap`. `validate_structure` rejects a retained coverage that differs from the derived one (`MergeOutputMismatch`). Gapped inputs stay as two regions.
- Boundary: state in, state out. No value is produced until a readout.

### 5.7 Reserved operators (not implemented)

These variants exist so that plans can name them, but `output_schema()`/`validate_inputs()` return `UNIMPLEMENTED_ASAP_OP`, so no node can be built. `output_kind()` already returns `State` for each of them. The intended edge shapes below follow from their fields; none of them is implemented.

| Operator | Fields | Intended edge shape |
|---|---|---|
| `SummarySubtract` | `left, right` | State × State → State: remove one window's contribution, e.g. [0,10) − [0,5) |
| `SummaryDelete` | `summary_input, key: ColumnId` | State → State with the entries for `key` removed |
| `SummaryJoin` | `outer, inner, key, family` | State × State → State typed `family` (`produced_state()` returns it), e.g. join-size estimation |
| `Extension` | `child, name` | deployment-named state operator |

### 5.8 Summary

| Operator | Input kind | Output kind | Output carries state | Coverage on output | Status |
|---|---|---|---|---|---|
| `SummaryAgg` | value (not `State`) | `State` | yes (one `family` field) | required, declared | implemented |
| `SummaryEstimate` | `State` (one `Sketch` field) | source's value kind | no | absent | implemented |
| `FinalizeExactAccumulator` | `State` (`ExactAggregate`) | source's value kind | no | absent | implemented |
| `MaintainPopulation` | `Relation` (table) / `InstantVector` (series) | `State` | yes (by kind; fields plain) | optional, not required | implemented |
| `EvaluatePopulation` | `State` from `MaintainPopulation` | source's value kind | no | absent | implemented |
| `SummaryMerge` | `State` × N | `State` | yes | required, derived | reserved; enabled by #560 |
| `SummarySubtract` | `State` × 2 | `State` | yes | — | reserved |
| `SummaryDelete` | `State` | `State` | yes | — | reserved |
| `SummaryJoin` | `State` × 2 | `State` | yes | — | reserved |
| `Extension` | any | `State` | yes | — | reserved |

## 6. Key code interfaces

`OperatorNode`, `OperatorResultKind` and coverage are in §4. Bodies and serde/derive attributes are elided below.

### 6.1 Schema and field types (`crates/types/src/pre_asap/schema.rs`)

```rust
pub type ColumnId = usize;

pub struct Schema {
    pub fields: Vec<Field>,
    pub time_index: Option<ColumnId>,     // must point at a plain Timestamp field
    pub unique_keys: Vec<Vec<ColumnId>>,
    pub closed: bool,                     // true = fields enumerate every column
}
impl Schema {
    pub fn new(fields: Vec<Field>) -> Self;
    pub fn with_time_index(fields: Vec<Field>, time_index: ColumnId, unique_keys: Vec<Vec<ColumnId>>) -> Self;
    pub fn lifted(fields: Vec<Field>, time_index: Option<ColumnId>) -> Self;   // closed = true
    pub fn is_all_plain(&self) -> bool;
    pub fn column_id(&self, name: &str) -> Option<ColumnId>;
    pub fn column_id_qualified(&self, table: &str, name: &str) -> Option<ColumnId>;
}

pub struct Field<T = FieldDataType> {
    pub name: String,
    pub dtype: T,
    pub nullable: bool,
    pub table: Option<String>,
}
impl Field<FieldDataType> {
    pub fn plain(name: impl Into<String>, dtype: DataType, nullable: bool) -> Self;
    pub fn plain_dtype(&self) -> Option<&DataType>;
    pub fn is_plain(&self) -> bool;
}

/// A column's type: a plain value, or summary state of one family.
pub enum FieldDataType {
    Plain(DataType),
    ExactAggregate(ExactKind, ExactParams),
    Sketch(SketchKind, GroupingStrategy),
    Sample(SamplingKind, SamplingParams),
    Wavelet(WaveletKind, WaveletParams),
    StatModel(StatModelKind, StatModelParams),
}

pub enum DataType {
    Null, Int64, Float64, Utf8, Bool, Timestamp, Interval, Date,
    List { element: Box<Field<DataType>> },
    Struct { fields: Vec<Field<DataType>> },
    Map { key: Box<DataType>, value: Box<DataType>, value_nullable: bool },
}
```

### 6.2 State-family parameters (`crates/types/src/post_asap/sketch.rs`)

```rust
pub enum ExactKind   { Sum, Count, Min, Max, Increase, Rate, IRate }
pub enum ExactParams { Sum, Count, Min, Max, Increase, Rate, IRate }   // no knobs; mirrors kind

pub struct SketchKind { category: SketchCategory, algorithm: SketchAlgorithm, params: SketchParams }
impl SketchKind {
    /// The only constructor; classifies the category and panics on mismatched params.
    pub fn new(algorithm: SketchAlgorithm, params: SketchParams) -> Self;
    pub fn category(&self) -> SketchCategory;
    pub fn algorithm(&self) -> &SketchAlgorithm;
    pub fn params(&self) -> &SketchParams;
}
pub enum SketchCategory { Universal, Quantile, Cardinality, Frequency, TopK }
// Universal: UnivMon | Quantile: Kll, DDSketch | Cardinality: Hll, Theta, Kmv
// Frequency: Cms, CountSketch | TopK: CmsWithHeap, CountSketchWithHeap
pub enum SketchAlgorithm { UnivMon, Kll, Cms, Hll, DDSketch, CmsWithHeap, Kmv, Theta, CountSketch, CountSketchWithHeap }
pub enum SketchParams {
    UnivMon { heap_size: u32, sketch_rows: u32, sketch_cols: u32, layers: u8 },
    Kll { k: u32 },
    Cms { width: u32, depth: u32 },
    Hll { precision: u8 },
    DDSketch { alpha: f64 },
    CmsWithHeap { width: u32, depth: u32, heap_size: u32 },
    Kmv { k: u32 },
    Theta { k: u32 },
    CountSketch { width: u32, depth: u32 },
    CountSketchWithHeap { width: u32, depth: u32, heap_size: u32 },
}

/// How grouped state is instantiated across `by` subpopulations. Orthogonal to family.
pub enum GroupingStrategy {
    PerSubpopulationInstance,                                       // Default
    SharedMultiSubpopulation { kind: HydraKind, params: HydraParams },
}
pub enum HydraKind { HydraKll /* experimental, no error bound */, HydraCms, HydraCountSketch }
pub enum HydraParams {
    HydraKll { k: u32, shared_buckets: u32 },
    HydraCms { width: u32, depth: u32, shared_rows: u32, shared_columns: u32 },
    HydraCountSketch { width: u32, depth: u32, shared_rows: u32, shared_columns: u32 },
}
pub fn hydra_kind_for(a: &SketchAlgorithm) -> Option<HydraKind>;   // Cms, CountSketch only

pub enum SamplingKind  { Reservoir }    pub enum SamplingParams  { Reservoir { size: u32 } }
pub enum WaveletKind   { Haar }         pub enum WaveletParams   { Haar { coefficients: u32 } }
pub enum StatModelKind { Parametric }   pub enum StatModelParams { Parametric { family: String } }
```

### 6.3 Update input and readouts (`post_asap/sketch.rs`, `post_asap/maintained_population.rs`)

```rust
/// One state update: `item` keys the update for keyed families; `weight` is applied to state.
pub struct SummaryUpdate {
    pub item: Option<SummaryInputExpr>,
    pub weight: SummaryInputExpr,
    pub weight_domain: WeightDomain,          // serde default: UnknownOrSigned
}
impl SummaryUpdate { pub fn column(c: ColumnRef) -> Self; }   // item None, UnknownOrSigned
pub enum WeightDomain {
    UnknownOrSigned,                                       // Default; never assumed non-negative
    NonNegative { proof: NonNegativeWeightProof },
}
pub enum NonNegativeWeightProof { UnitCount, ResetAwareCounterDerivative }
pub enum SummaryInputExpr {
    Constant(f64), Column(ColumnRef), Tuple(Vec<SummaryInputExpr>), EntityIdentity(EntityIdentity),
}
pub enum EntityIdentity { PromqlLabelSet { excluding: Vec<ColumnRef> } }

/// Readout of sketch state, carried by SummaryEstimate.
pub enum SketchStatistic {
    FrequencyL2, FrequencyEntropy,
    Quantile { q: f64 },
    PointCount { key: ColumnRef, value: Option<String> },
    Cardinality,
    TopK { k: usize },
}

/// Readout of a maintained population, carried by EvaluatePopulation.
pub enum PopulationStatistic { Quantile { q: f64 }, TopK { k: usize }, Sum, Count, Average }
pub struct MaintainedPopulation<N = QueryExpr> {
    pub input: PopulationInput<N>,
    pub max_k: usize,        // largest TopK it supports
    pub quantiles: bool,     // whether Quantile is supported
}
pub enum PopulationInput<N = QueryExpr> {
    CurrentSeries(CurrentSeriesInput),    // metric, matchers, grouping, without, lookback_ms
    Rows { input: Rc<N>, value_column: usize, grouping: GroupKeys },
}
```

A finalized value's accuracy statement is `ResultGuarantee { metric, bound, failure_probability, provenance }` (`post_asap/guarantee.rs`). It is attached to readout and finalized nodes, never to raw state.

### 6.4 ASAP operators (`crates/types/src/ir/asap.rs`)

```rust
pub const UNIMPLEMENTED_ASAP_OP: &str =
    "this ASAP operator is reserved: schema, accuracy, timing and export are not implemented";

pub enum ASAPOp {
    SummaryAgg {
        child: Rc<OperatorNode>,
        family: FieldDataType,             // never Plain
        input: SummaryUpdate,
        reduction: Reduction,              // Reduce(GroupKeys) | PerEntity
        grouping: GroupingStrategy,
        filter: Option<Predicate>,         // serde default None
    },
    SummaryEstimate { summary_input: Rc<OperatorNode>, query: SketchStatistic },
    FinalizeExactAccumulator { child: Rc<OperatorNode> },
    MaintainPopulation { child: Rc<OperatorNode>, population: MaintainedPopulation<OperatorNode> },
    EvaluatePopulation { child: Rc<OperatorNode>, evaluation: PopulationStatistic },
    // Reserved on this branch; #560 implements SummaryMerge.
    SummaryMerge { children: Vec<Rc<OperatorNode>> },
    SummarySubtract { left: Rc<OperatorNode>, right: Rc<OperatorNode> },
    SummaryDelete { summary_input: Rc<OperatorNode>, key: ColumnId },
    SummaryJoin { outer: Rc<OperatorNode>, inner: Rc<OperatorNode>, key: ColumnId, family: FieldDataType },
    Extension { child: Rc<OperatorNode>, name: String },
}

impl ASAPOp {
    pub fn children(&self) -> Vec<&Rc<OperatorNode>>;     // SummaryAgg includes its filter's subquery nodes
    pub fn map_children(&self, f: impl FnMut(&Rc<OperatorNode>) -> Rc<OperatorNode>) -> Self;
    pub fn kind_name(&self) -> &'static str;
    /// Merge, Subtract, Delete, Join, Extension on this branch; #560 removes Merge.
    pub fn is_unimplemented(&self) -> bool;
    /// SummaryAgg/SummaryJoin `family`; #560 adds SummaryMerge (its inputs' state type).
    pub fn produced_state(&self) -> Option<&FieldDataType>;
    /// #560: merge_disjoint of the children's coverage; fails closed.
    pub fn merged_coverage(&self) -> Result<SummaryCoverage, SchemaDerivationError>;
    pub fn output_schema(&self) -> Result<Schema, SchemaDerivationError>;
    pub fn output_kind(&self) -> OperatorResultKind;
    pub fn validate_inputs(&self) -> Result<(), SchemaDerivationError>;
}
```
