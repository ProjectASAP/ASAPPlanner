# Schema and Physical Data for ASAP Primitives

This document is the single source of truth for the schema, and column design for ASAP Primitives. This is used in the logical stage (LogicalASAPDAG), and physical stage (PhysicalASAPDAG). 

## 1. Goal, problem, and requirements

Unlike existing Database engines, which work on raw data or explicitly defined materialized tables with schema and column names provided by the users, ASAPPlanner is designed for querying and execution over the mix of raw data and ASAP Primitives. ASAP primitives are usually compact summaries over raw data. Therefore, it introduces new requirement when we design the schema and node definitions for LogicalASAPDAG and PhysicalASAPDAG.

Assuming we have the Logical DAG defined for a canonicalized representation for a batch of queries (the `LogicalDAG` produced by the frontends, [planning stages §0](planner-layering.md#0-language-specific-frontends); the stages that follow are in [planner-layering.md](planner-layering.md#stages)).
The LogicalASAPDAG will share/reuse the NonASAP operator and ScalarExpr nodes in LogicalDAG ([decoupling operators from scalar expressions](decoupling_op_and_expr.md), with the unified operator type in [operator sharing §1.1](operator-sharing.md#11-unified-operator-type)), but replacing some operators in LogicalDAG with the operators operated with ASAP Primitives. The complete list is `ASAPOp` in `crates/types/src/ir/asap.rs`:

| Operator | Input → output | Status |
|---|---|---|
| `SummaryAgg` | values → summary state (creates and updates the state; `SummaryUpdate` is its input mapping, not a separate operator) | implemented |
| `SummaryEstimate` | sketch state → value | implemented |
| `FinalizeExactAccumulator` | exact accumulator state → value | implemented |
| `MaintainPopulation` | values → maintained membership (state) | implemented |
| `EvaluatePopulation` | maintained membership → value | implemented |
| `SummaryMerge` | state × N → state | implemented (#560: identical schemas); coverage check in #646 |
| `SummarySubtract` | state × state → state | reserved |
| `SummaryDelete` | state → state without one key | reserved |
| `SummaryJoin` | state × state → state | reserved |
| `Extension` | state → state, named by an extension | reserved |

§5 walks through each of them.
Each of the Summary operators also require the ASAP primitive information above to inter-operate correctly, preserving semantic correctness. 

Basically, the following information should be represented to preserve the equivalent query semantics when we introduce ASAP Primitives to logical query representation, and following physical one. 

- What type of the ASAP Primitive is
- What is the ASAP Primitive parameters
- What data sources a ASAP primitive summarizes
- What query intent the summarized ASAP Primitive can support, e.g., statistical aggregation intents, time window aggregation intents



And these information will be combined with relational or time series query operator information, such as group by/reduction, filtering, projection, join, time series selection, together. 

Therefore, these requirements drive the following schema and metadata, node information, and column design. 



## 2. Existing database terminology for schema, table, column, and physical data layout

This section fixes the words used below. They follow relational databases and Apache Arrow / DataFusion, which ASAPPlanner's frontend already uses.

| Term | Meaning in existing systems | In ASAPPlanner |
|---|---|---|
| **Relation / table** | A set (bag) of rows with the same columns. A base table is stored; a derived relation is the output of a query operator. | Every edge in the DAG carries a relation. A `Scan` reads a base table (SQL table or PromQL metric); every other operator outputs a derived relation. |
| **Row / tuple** | One element of a relation: one value per column. | One output row of a node. For PromQL, one sample of one series at one time. |
| **Column** | One position in every row, with a name and a type. Qualified as `table.column` when names can collide (DataFusion `Column { relation, name }`). | `ColumnId` refers to a column of the input schema; `(table, name)` identifies it across nodes (§4.4). |
| **Schema** | The ordered list of columns of a relation: name, data type, nullability (Arrow `Schema` of `Field { name, data_type, nullable }`; DataFusion `DFSchema` adds the table qualifier). The schema is *metadata*: it describes rows, it contains none. | `Schema` of `Field { name, dtype, nullable, table }` in `crates/types/src/pre_asap/schema.rs`. Unlike Arrow, `dtype` can be a summary state type (§3). |
| **Data type** | The type of a column's values (`Int64`, `Utf8`, `Timestamp`, …). | `DataType`, wrapped as `FieldDataType::Plain`. |
| **Aggregate state** | The intermediate value of an aggregate function before its final result, e.g. `(sum, count)` for `AVG` (DataFusion `Accumulator::state`, partial/final aggregation). It is never exposed as a column type to users. | Summary state *is* a column type here (`FieldDataType::Sketch`, `ExactAggregate`, …), so state can flow along edges and be merged, stored and read by later operators. |
| **View / materialized view** | A view is a named query (its *definition*). A materialized view also stores the query's result rows; a query can then be answered from it when its definition matches (view matching, §4.1). | A built summary state is a materialized aggregation view whose aggregate is a summary family. Its definition and which rows it took are its coverage (§4). |
| **Physical data layout** | How rows are stored: row-oriented or columnar (Arrow `RecordBatch`: one array per column), split into partitions (hash or range) and batches. | Decided in physical planning ([planning stages §2](planner-layering.md#2-physical-asap-aware-optimization)) and by the executing backend. The logical schema does not depend on it. |

Two consequences for the design:

- A schema says what *kind* of values flow along an edge, never *which* rows. Which rows a relation contains is decided by the operators below it (its definition). This is why coverage is a node property and not part of the schema (§4).
- Existing systems keep aggregate state internal to one operator. ASAPPlanner makes it a first-class column type so that one state can be shared, merged and stored across queries, which is what §3 and §4 add.

## 3. Proposed schema design 
Schema represents the **metadata** of information flow along an **edge** between two nodes in a logical or physical DAG. The schema field is associated with the node in the DAG. The consumer of the node in the DAG takes the schema from the producer node as input. 

Schema definition here is shared between LogicalDAG, LogicalASAPDAG, and PhysicalASAPDAG. The schema contain fields, and each field is mapping to a column in the physical data representation. 
Based on our requirement, each field should contain the following information.
1. **What type of the ASAP Primitive is.** The field's type is a [`FieldDataType`](#61-schema-and-field-types-cratestypessrcpre_asapschemars) (`crates/types/src/pre_asap/schema.rs`). A column is either a raw value or a summary state:

   - **Raw value**: `Plain(DataType)`, e.g., a number or a string.
   - **Summary state**: described from coarse to fine by four levels:

   | Level | Code | Values | KLL example |
   |---|---|---|---|
   | Family | `FieldDataType` variant | `ExactAggregate`, `Sketch`, `Sample`, `Wavelet`, `StatModel` | `Sketch` |
   | Category (sketches only) | `SketchCategory` | `Quantile`, `Frequency`, `Cardinality`, `TopK`, `Universal` | `Quantile` |
   | Algorithm | `SketchAlgorithm` (other families: `ExactKind`, `SamplingKind`, …) | `Kll`, `Cms`, `Hll`, `DDSketch`, … | `Kll` |
   | Parameters | `SketchParams` (other families: `ExactParams`, `SamplingParams`, …) | per algorithm | `Kll { k: 200 }` |

   - For a sketch, category, algorithm and parameters are bundled as one `SketchKind` ([§6.2](#62-state-family-parameters-cratestypessrcpost_asapsketchrs)). A sketch also records its `GroupingStrategy`: one instance per group, or one shared structure (Hydra) for all groups.
   - So a quantile KLL sketch with `k = 200`, one instance per group, has the type `Sketch(SketchKind { Quantile, Kll, Kll { k: 200 } }, PerSubpopulationInstance)`.

2. **What query intent the summarized ASAP Primitive can support.** This is not stored in the field: it follows from the type in item 1. There are two kinds of intent:

   - **Statistical aggregation intents** (`AggIntent`, `crates/types/src/pre_asap/agg_intent.rs`): which aggregate the state can answer, and how it is read out.

     | Intent | Candidate types | Readout |
     |---|---|---|
     | `Quantile` | `Sketch`: `Kll`, `DDSketch` | `SummaryEstimate(Quantile { q })` |
     | `Cardinality` | `Sketch`: `Hll`, `Theta`, `Kmv`, `UnivMon` (one column only) | `SummaryEstimate(Cardinality)` |
     | `Count` (approximate) | `Sketch`: `Cms`, `CountSketch`, `UnivMon` | `SummaryEstimate(PointCount { .. })` |
     | `TopK` | `Sketch`: `CmsWithHeap`, `CountSketchWithHeap` | `SummaryEstimate(TopK { k })` |
     | `FrequencyL2`, `FrequencyEntropy` | `Sketch`: `UnivMon` | `SummaryEstimate(FrequencyL2 \| FrequencyEntropy)` |
     | `Sum`, `Count`, `Min`, `Max`, `Rate`, `Increase` (exact) | `ExactAggregate(ExactKind, …)` | `FinalizeExactAccumulator` |

     The sketch candidates are `summary_candidates(intent)` in `crates/asap-aware-mapping/src/replacement.rs`; the readouts are `SketchStatistic` ([§6.3](#63-update-input-and-readouts-post_asapsketchrs-post_asapmaintained_populationrs)).

   - **Time window aggregation intents**: whether states built over smaller windows can answer a larger one. This depends on how the family combines states:
     - **Merge** (`SummaryMerge`, §5.6): states over disjoint panes combine into the state of their union, e.g. two 1-minute KLL states answer a 2-minute quantile. Requires a mergeable family.
     - **Subtract** (`SummarySubtract`, reserved): a sliding window as a larger state minus an older one. Only families with an inverse (e.g. exact `Sum`/`Count`, CMS) can do this.

     Which inputs may be merged is decided by coverage (§4), not by the schema. 


## 4. Proposed Node field design 

A node in the physical data will represent the data or summary instance, so a node has a field for **What data sources a ASAP primitive summarizes**. This field is the node's **coverage**.

**At a glance**

| Question | Answer | Section |
|---|---|---|
| Why not put it in the schema? | States worth merging cover different data but must have the same schema | below |
| What is it based on? | Goldstein & Larson view matching (SIGMOD 2001), explained with an example | §4.1 |
| What does it store? | `definition` (what is computed) + `selection` (which rows were taken) | §4.2 |
| What do we take from the paper, and what do we add? | computation, aggregate and `GROUP BY` → `definition`; ranges → `selection`; plus unions and summary families | §4.3 |
| Who sets it? | Nobody: it is derived from the sub-DAG | §4.4 |
| What uses it? | merge, rollup, slice, reuse, subtract | §4.5 |
| Where is it in the code? | `OperatorNode::coverage()`, `SummaryCoverage::derive` | §4.6 |
| What is left out? | the deployment and runtime implementation, e.g. SDS | §4.7 |

**Why coverage is not part of the schema.**

- `SummaryMerge` requires all inputs to have the same schema; that check is how it knows they are the same kind of state (same sketch, parameters and grouping).
- Two summary states worth merging always cover different data. For example, two KLL states for "latency by job", built from minute 0–1 and minute 1–2:

  | | State A | State B | Equal? |
  |---|---|---|---|
  | schema | `(job: Utf8, state: KLL{k=200})` | `(job: Utf8, state: KLL{k=200})` | yes, so the merge is allowed |
  | what it summarizes | time `[0,1)` | time `[1,2)` | no, which is why merging them is useful |

- If coverage were part of the schema, these two schemas would differ and the merge would be rejected. The only merge left would be a state with an exact copy of itself, which counts every observation twice.

So the schema says *what kind of state* this is, and coverage says *which data it was built from*.

**Why coverage is a sub-DAG, not a few table columns.** A summary state summarizes the result of a whole computation: a KLL over `rate(requests_total[5m])` summarizes rate outputs, and a KLL over a join summarizes join rows. So coverage describes the state by the sub-DAG below it, split into *what is computed* and *which of its rows were taken*.

### 4.1 Background: view matching (Goldstein & Larson)

Our design is based on this paper:

> J. Goldstein and P.-Å. Larson. *Optimizing Queries Using Materialized Views: A Practical, Scalable Solution.* SIGMOD 2001. <https://dsg.uwaterloo.ca/seminars/notes/larson-paper.pdf>

**The problem it solves.** A materialized view is a query whose result is stored. When a new query arrives, the optimizer wants to answer it from the stored result instead of the base tables. It has to decide two things: does the view contain every row the query needs, and can the answer be computed from the view's output?

**Example.** View `V` is stored; query `Q` arrives:

```sql
-- V: stored
SELECT region, job, day, SUM(bytes) AS s
FROM t WHERE day BETWEEN 1 AND 31
GROUP BY region, job, day;

-- Q: new query
SELECT job, SUM(bytes)
FROM t WHERE day BETWEEN 5 AND 10 AND region = 'us'
GROUP BY job;
```

**Step 1: split each `WHERE` into three parts.** Each part is a list of `AND`-ed conditions:

| Part | What it is | Comes from | In `V` | In `Q` |
|---|---|---|---|---|
| **Equivalence classes** | sets of columns that are equal in every row | column equalities, e.g. the join condition `orders.cust_id = customers.id` | none | none |
| **Ranges** | for each column, the interval of values it may have | comparisons with a constant: `x > 5`, `x = 5`, `BETWEEN` | `day ∈ [1, 31]` | `day ∈ [5, 10]`, `region ∈ ['us', 'us']` |
| **Residuals** | every other condition; the algorithm does not try to understand them | e.g. `a + b > 10`, `lower(name) LIKE 'a%'`, `x = 1 OR y = 2` | none | none |

**Step 2: does `V` contain every row `Q` needs?** (§3.1.2 of the paper)

- **Ranges:** each range of `Q` lies inside the same column's range in `V`. `day ∈ [5, 10]` is inside `[1, 31]` ✓. `V` has no range on `region`, so any `region` is in `V` ✓.
- **Residuals:** each residual of `V` also appears in `Q`. Since residuals are not understood, the only safe case is when `Q` has the same condition.
- **Equivalence classes:** each column equality of `V` also holds in `Q`.

**Step 3: can the answer be computed from `V`'s output?** (§3.3)

- **Compensating filter:** where `Q` is narrower than `V`, the extra condition is applied to `V`'s rows. So its columns must be in `V`'s output: `day` and `region` are ✓.
- **Regrouping:** `Q`'s `GROUP BY` must be a subset of `V`'s. `{job}` ⊆ `{region, job, day}` ✓, so each group of `Q` is the sum of some groups of `V`.

**Result:**

```sql
SELECT job, SUM(s) FROM V
WHERE day BETWEEN 5 AND 10 AND region = 'us'
GROUP BY job;
```

**Limits that matter for us:**

- It answers a query from **one** view. Combining several views (a union) is left out (§3.1).
- It supports only `SUM` and `COUNT`, whose groups can be added up again.

**Existing implementation.** The `WHERE` split is implemented for DataFusion in [`datafusion-contrib/datafusion-materialized-views`](https://github.com/datafusion-contrib/datafusion-materialized-views), `src/rewrite/normal_form.rs` (`SpjNormalForm`, `Predicate { eq_classes, ranges_by_equivalence_class, residuals }`). It rejects plans that contain an `Aggregate` or a `Join`.

### 4.2 Summary Coverage = Summary definition + selection

A summary state is a stored aggregation, like `V` above, whose aggregate is a sketch. So we describe it the way the paper describes a view, in two parts:

| Part | Question it answers | What it is |
|---|---|---|
| **`definition`** | *What* is computed? | the `SummaryAgg` node with its row filters taken out: the sub-DAG below it, the summary family and parameters, its input column, and its `GROUP BY` |
| **`selection`** | *Which rows* went in? | the row filters that were taken out, as simple conditions on columns |

**Example:**

```text
state = KLL(latency) by[job] over Filter(region = 'us' AND latency < 100, Scan t)

definition: KLL(latency) by[job] over Scan t
selection:  region ∈ {us}, latency ∈ (−∞, 100)
```

**Why this is enough.** Two states with the same `definition` come from the same computation. If their selections do not overlap, no row is in both, so merging them counts every row once. This holds whatever the computation contains (joins, unions, `rate`, dedup), so we need no special rule per operator.

**More examples** of what goes where:

| Sub-DAG below `SummaryAgg` | `definition` keeps | `selection` takes |
|---|---|---|
| `Filter(region = 'us', Scan t)` | `Scan t` | `region ∈ {us}` |
| `Filter(latency < 100, Scan t)` | `Scan t` | `latency ∈ (−∞, 100)` |
| `TimeRange(1m, TimeShift(2m, Scan m))` | `Scan m` | the last 3 to 2 minutes before evaluation, `(−3m, −2m]` |
| `Filter(job = 'api', rate(TimeRange(5m, Scan m)))` | `rate(TimeRange(5m, Scan m))` | `job ∈ {api}` |
| `Filter(rate > 0, rate(TimeRange(5m, Scan m)))` | `rate(TimeRange(5m, Scan m))` and the condition `rate > 0` | nothing |

In the last two rows `TimeRange(5m)` stays in `definition`: it is the input window of `rate` and changes the rate values, so it does not just pick rows. `rate > 0` stays too, because it filters on a computed value (§4.4).

Formally, a state means `family(input(σ(C)))` for each group of `G`, where `C` is the sub-DAG without its row filters, `σ` is the selection, and `G` the grouping.

### 4.3 What we take from Goldstein & Larson, and what we add

| Goldstein & Larson | Summary coverage |
|---|---|
| tables, joins and residuals of the view | the sub-DAG `C` below the `SummaryAgg`, in `definition` |
| the aggregate and its argument | the summary family and its input, in `definition` |
| `GROUP BY` | the `SummaryAgg` grouping `G`, in `definition` |
| ranges | `selection` (§4.4) |
| compensating filter on the view's output | slicing: allowed only on a column of `G` (§4.5) |
| regrouping to a smaller `GROUP BY` | rollup (§4.5) |

**What we add:**

- **Unions of states.** The paper uses one view at a time. `SummaryMerge` combines several states, so we must also check that their selections do not overlap.
- **Summary families.** The paper only re-adds `SUM` and `COUNT`. Each summary family says how its inputs may overlap (§4.5).
- **More kinds of conditions:** value sets (`IN`, `NOT IN`), hash partitions, and time relative to the evaluation time (§4.4).

### 4.4 Deriving the selection

Nobody declares coverage: the planner computes it from the sub-DAG. It starts at the `SummaryAgg`, walks down, and looks at each filter condition on the way (split at `AND`). A condition moves into `selection` only when both rules below hold. Otherwise it stays in `definition`, like a residual in the paper.

**Rule 1: the condition can move up to the `SummaryAgg` without changing its meaning.** This is the reverse of filter pushdown (DataFusion's `PushDownFilter`).

| Operator between the condition and the `SummaryAgg` | The condition can pass when |
|---|---|
| `Filter`, `Scan.predicates`, `SummaryAgg.filter` | always |
| range `TimeRange`, `TimeShift` without `@` | always |
| `Project` | the column is passed through as is (a rename is fine) |
| `Aggregate` (later) | it uses only group columns |
| window function, or a per-series function such as `rate` (later) | it uses only partition columns (series labels) |
| anything else | never |

**Rule 2: the condition is a simple condition on one column.**

| Kind | Written as | Example | DataFusion analogue |
|---|---|---|---|
| value set | `=`, `!=`, `IN`, `NOT IN`, `OR` of `=` on the same column | `region IN ('us', 'eu')` | `LiteralGuarantee` |
| interval | `<`, `<=`, `>`, `>=` | `latency < 100` | `Interval` |
| hash partition (later) | `hash(columns) mod n = k` | `hash(job) mod 4 = 1` | `Partitioning::Hash` |

A column equality such as `a = b` is not a simple condition, so it stays in `definition`.

**Which column a condition is on.**

- A column is named by its source table and name, `(table, name)`. So `shipping.region` and `billing.region` are different columns.
- A rename keeps the original name.
- If two output columns have the same `(table, name)`, the condition cannot tell them apart and stays in `definition`.
- Values of different types are never treated as different: `1` and `1.0` might be equal.

**Time.** There are two kinds:

| Kind | Where it comes from | Example |
|---|---|---|
| **Absolute** | an interval on the timestamp column | `ts >= t0 AND ts < t1` → `ts ∈ [t0, t1)` |
| **Relative** to the evaluation time | a range `TimeRange(w)` over a `TimeShift(s)` | `TimeRange(1m)` over `TimeShift(2m)` → `(−3m, −2m]` |

- PromQL windows exclude their start, so relative windows are open on the left.
- The IR cannot yet write a timestamp constant, so absolute SQL time filters stay in `definition` for now.
- Stage 2 builds its tumbling panes from `TimeRange` and `TimeShift`, so pane times are derived, not declared (#601).
- An instant `TimeRange` takes the latest sample of each series. That does not pick rows by time, so it stays in `definition`.
- Absolute and relative time are never compared. Two states that differ only in the kind of time are treated as possibly overlapping.

### 4.5 Operations

Coverage tells the planner which states can be combined, and what the result covers.

| Operation | Example | Allowed when |
|---|---|---|
| **merge** (`SummaryMerge`) | minute 0–1 + minute 1–2; `region='us'` + `region='eu'` | all `definition`s are equal, and the selections relate as the family requires (below) |
| **rollup** (`SummaryMerge` with `group_by`, later) | `by[region, job]` → `by[job]` | the new grouping is a subset of the old one. Different groups never share a row, so no overlap check is needed |
| **slice** | read `region = 'us'` from a `by[region, job]` state | the condition is on a grouping column. A sketch cannot be filtered on any other column |
| **reuse** for a query | a stored state answers a new query | same `definition`, and the query's selection lies inside the state's (as in the paper) |
| **subtract** (`SummarySubtract`, reserved) | `[0, 10) − [0, 5)` | same `definition`, and the right selection lies inside the left |

**Merge and rollup are one operator,** `SummaryMerge { children, group_by }`. With the children's own grouping it is a plain merge; with a smaller one it is a rollup. The result's `definition` is the children's, with the new grouping; its `selection` is the union of theirs. Adjacent ranges join into one (minute 0–1 + minute 1–2 = minute 0–2); gaps stay as separate pieces.

**How inputs may overlap** depends on the summary family (`FieldDataType::family_merges` and `merge_relation`, #592):

| Rule | Families | Why |
|---|---|---|
| **must not overlap** | counting families: KLL, Count-Min, exact `Sum`/`Count` | a row in both inputs would be counted twice |
| **may overlap** | HLL, exact `Min`/`Max`, distinct sets | adding the same row twice does not change the result |
| **right inside left** | subtraction | you can only remove what is there |

**When two `definition`s are equal.**

- They must have the same structure. Planning details are ignored: `timing`, `guarantee` and `coverage_cache`. So a state built at ingestion time can merge with one built at query time.
- `SummaryUpdate.weight_domain` is compared too. It is computed from the rest, so it differs only if something is wrong.
- States over different tables never merge. To combine tables, put a `UNION ALL` with a column that marks the source table below one `SummaryAgg`; that column can then be used in `selection` or in the grouping.

### 4.6 Interface

```rust
pub struct OperatorNode {
    pub operator: Operator,
    pub result_kind: OperatorResultKind,
    pub schema: Schema,
    pub guarantee: Option<ResultGuarantee>,
    pub timing: Option<ExecutionTiming>,
    /// Cache for `coverage()`. Lazily filled, never serialized, ignored by
    /// equality, emptied on clone. Not a source of truth: coverage is always
    /// re-derivable.
    coverage_cache: CoverageCache,
}

impl OperatorNode {
    /// `Some` for a `SummaryAgg` and a valid `SummaryMerge`.
    pub fn coverage(&self) -> Option<&SummaryCoverage>;
}

pub struct SummaryCoverage {
    /// The `SummaryAgg` (or rolled-up equivalent) with the selection removed.
    pub definition: Rc<OperatorNode>,
    /// Union of boxes over the output rows of the definition's computation.
    pub selection: Vec<SelectionBox>,
}

pub struct SelectionBox {
    pub columns: BTreeMap<ColumnIdentity, Constraint>, // missing column = unrestricted
    pub relative_time: Option<(Bound<i64>, Bound<i64>)>, // ms from evaluation; None = unrestricted
}

pub struct ColumnIdentity {
    pub table: Option<String>,
    pub name: String,
}

pub enum Constraint {
    In(Vec<ScalarValue>),    // ScalarValue has no total order (Float64)
    NotIn(Vec<ScalarValue>),
    Interval { lower: Bound<ScalarValue>, upper: Bound<ScalarValue> },
    // HashPartition { columns, of, index }: added with its first producer.
}

impl SummaryCoverage {
    pub fn derive(node: &OperatorNode) -> Result<Self, CoverageError>;
}
```

- `OperatorNode::new` rejects an invalid `SummaryMerge` (different definitions, or selections the family does not allow), but it does not store the result.
- A `SummaryAgg` always has coverage: what cannot go into `selection` stays in `definition`.

### 4.7 What coverage does not contain

Coverage only says what a state means and which rows it took. The deployment and runtime implementation is not part of coverage: it belongs to ASAPQuery-backend, for example the summary data store (SDS), reading source data, and building, storing and serving summary instances.

**SDS mapping.** The SDS split matches coverage:

- `SummaryDefinition` stores the serialized `definition`. Planner provides its serde; Backend owns the format version, definition id and hash.
- A `StoredSummary`'s coordinates are the `selection` bound to one evaluation, plus the group value.

## 5. Examples on how OperatorNode, schema, and physical data information are being used with Summary operators

Given that these information requirements are introduced by summary operators to work correctly semantically, we show the examples of how the defined OperatorNode, schema, and physical data information work with each kind of summary operators. 

Notation: an edge is written `──Kind(field Type, …)──▶`. Schemas are the ones `output_schema()` derives. Planning may rename fields through `OperatorNode::with_schema`, but types, nullability, `time_index`, `unique_keys` and `closed` must match the derivation. All examples use a table source, so values are `Relation`; with a `TimeSeries` source the value side is `InstantVector`.

### 5.1 `SummaryAgg`: values → state

Scenario: p99 latency by job, from KLL(k=200), over one minute of table `t`, US rows only.

```text
Scan(t: job Utf8, region Utf8, ts Timestamp [time_index], latency Float64)
  ──Relation(job Utf8, region Utf8, ts Timestamp, latency Float64)──▶
Filter(region = 'us' AND ts >= 0 AND ts < 60_000)
  ──Relation(job Utf8, region Utf8, ts Timestamp, latency Float64)──▶
SummaryAgg(family = Sketch(KLL{k=200}, PerSubpopulationInstance),
           input = SummaryUpdate::column(Named("latency")), reduction = by[job],
           grouping = PerSubpopulationInstance, filter = None)
  ──State(job Utf8, state Sketch(KLL{k=200}, PerSubpopulationInstance))──▶
  coverage() = { definition: this SummaryAgg over Scan(t)   (the Filter removed),
                 selection:  [{ columns: { t.region: In{'us'} },
                                time: Absolute [Included(0), Excluded(60_000)) }] }
```

- Output schema: the `by` keys followed by one non-nullable field `state` typed `family`; `unique_keys = [[0]]`, `closed = true`, no `time_index`. With `Reduction::PerEntity` the input columns are kept and the sample-value column is replaced by `state`.
- Checks: `family` is not `Plain`; the child is not `State`; the `weight`/`item` columns resolve against the child schema; `filter`, if present, types as `Bool`.
- Coverage: **always derived**, never declared (§4.4). Both conjuncts of the `Filter` lift into `selection`, so `definition` is this node over the bare `Scan`. A KLL over `latency` for `region = 'eu'` has the same `definition` and a disjoint selection, so the two can merge. A conjunct that cannot lift (say `latency * 2 > 10`) stays in `definition` as a residual; the node still has coverage.
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
- Coverage: **none**. The output is a value; `coverage()` returns `None`.
- Boundary: state is consumed and a value is produced; `guarantee` on this node carries the readout's error bound.

### 5.3 `FinalizeExactAccumulator`: exact state → value

Scenario: total bytes by host with an exact Sum accumulator.

```text
Scan(t: host Utf8, bytes Float64)
  ──Relation(host Utf8, bytes Float64)──▶
SummaryAgg(family = ExactAggregate(Sum, Sum), input = column(Named("bytes")), reduction = by[host])
  ──State(host Utf8, state ExactAggregate(Sum, Sum))──▶   coverage: derived
FinalizeExactAccumulator
  ──Relation(host Utf8, state Float64)──▶
```

- Output schema: each `ExactAggregate` field keeps its name (`state`) and takes the type and nullability the equivalent `NonASAPOp::Aggregate` would give: Sum/Min/Max follow the input column, Count is Int64, and Rate/IRate/Increase are Float64. If the child is not a `SummaryAgg` directly, Count falls back to Int64 and the others to Float64. `unique_keys`, `closed` and `time_index` are preserved (`schema_rebuilding.rs`).
- Checks: the input is `State` and contains an `ExactAggregate` field; a sketch is rejected (`structure_contract.rs`).
- Coverage: **none** on the output.
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
- Coverage: **none**. Maintained membership is not combined by `SummaryMerge`. If maintained populations are later materialized per pane, they derive coverage the same way as `SummaryAgg`.
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
- Coverage: **none**.
- Boundary: maintained membership is read as a value; the result kind is the source's (`Relation`).

### 5.6 `SummaryMerge`: state × N → state (merge and rollup)

Current state:

- **On `main` (since #560):** `SummaryMerge { children }` is implemented. `validate_inputs()` accepts it when there is at least one child, every child is `State` with exactly one state field, and all children have identical schemas. The output schema is the children's schema.
- **#646 (open):** adds the coverage check. `OperatorNode::new` and `validate_structure` also require equal `definition`s and disjoint selections, and `coverage()` returns the merged coverage.
- **Planned:** `group_by`, so one operator does both merge and rollup (§4.5):

```rust
SummaryMerge { children: Vec<C>, group_by: Reduction }
```

Scenario A, time panes: two one-minute KLL panes of PromQL `quantile_over_time(0.99, m[2m])` merged into the two-minute state. Each pane reads `TimeRange(1m)` over `TimeShift(s)` over the scan, as Stage 2 builds them.

```text
pane 0 = SummaryAgg(KLL k=200, column(SampleValue), by[]) over TimeRange(1m, TimeShift(0,  Scan m))
         coverage() = { definition: SummaryAgg(...) over Scan m, selection: [{ time: Relative (−1m, 0] }] }
pane 1 = SummaryAgg(KLL k=200, column(SampleValue), by[]) over TimeRange(1m, TimeShift(1m, Scan m))
         coverage() = { definition: same,                        selection: [{ time: Relative (−2m, −1m] }] }
SummaryMerge(children = [pane 0, pane 1], group_by = by[])
  ──State(state Sketch(KLL{k=200}))──▶
  coverage() = { definition: same, selection: [{ time: Relative (−2m, 0] }] }
```

Scenario B, populations: `KLL(latency) by[job]` for `region = 'us'` and for `region = 'eu'` (as in 5.1) merge into `selection: [{ t.region: In{'us', 'eu'} }]`.

Scenario C, rollup: one `KLL(latency) by[region, job]` state merged with `group_by = by[job]`. Every job's state is the merge of that job's per-region states. The output's `definition` is the same `SummaryAgg` with `reduction = by[job]`, and `selection` is unchanged.

- Output schema: the children's schema with the group key fields reduced to `group_by`.
- Checks:
  - at least one child, every child is `State` with exactly one state field;
  - all children have equal `definition`s (§4.5), so family, parameters, `input`, `C` and grouping match. Merging k=200 with k=300, KLL over `latency` with KLL over `size`, or states over different sources fails;
  - `group_by` ⊆ the children's `G`, and the family merges;
  - the children's selections relate as the family requires: disjoint for KLL, so pane 0 with pane 0 is rejected; overlap is allowed for HLL.
- Coverage: **derived**: the shared `definition` with `group_by`, and the union of the children's selections. Adjacent intervals join; gaps stay as separate boxes. Nested merges work because a child merge has coverage like any other summary node.
- Boundary: state in, state out. No value is produced until a readout.

### 5.7 Reserved operators (not implemented)

These variants exist so that plans can name them, but `output_schema()`/`validate_inputs()` return `UNIMPLEMENTED_ASAP_OP`, so no node can be built. `output_kind()` already returns `State` for each of them. The intended edge shapes below follow from their fields; none of them is implemented.

| Operator | Fields | Intended edge shape |
|---|---|---|
| `SummarySubtract` | `left, right` | State × State → State: remove one window's contribution, e.g. [0,10) − [0,5). Same `definition`; the right selection must lie inside the left (§4.5) |
| `SummaryDelete` | `summary_input, key: ColumnId` | State → State with the entries for `key` removed |
| `SummaryJoin` | `outer, inner, key, family` | State × State → State typed `family` (`produced_state()` returns it), e.g. join-size estimation |
| `Extension` | `child, name` | deployment-named state operator |

### 5.8 Summary

| Operator | Input kind | Output kind | Output carries state | Coverage on output | Status |
|---|---|---|---|---|---|
| `SummaryAgg` | value (not `State`) | `State` | yes (one `family` field) | derived: itself minus selection, plus selection | implemented |
| `SummaryEstimate` | `State` (one `Sketch` field) | source's value kind | no | none | implemented |
| `FinalizeExactAccumulator` | `State` (`ExactAggregate`) | source's value kind | no | none | implemented |
| `MaintainPopulation` | `Relation` (table) / `InstantVector` (series) | `State` | yes (by kind; fields plain) | none | implemented |
| `EvaluatePopulation` | `State` from `MaintainPopulation` | source's value kind | no | none | implemented |
| `SummaryMerge` | `State` × N | `State` | yes | derived: shared definition with `group_by`, union of selections | implemented (#560); coverage check in #646; `group_by` planned |
| `SummarySubtract` | `State` × 2 | `State` | yes | derived: left selection minus right (planned) | reserved |
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
    // Implemented since #560 (identical child schemas).
    SummaryMerge { children: Vec<Rc<OperatorNode>> },
    // Reserved: migrated but unimplemented.
    SummarySubtract { left: Rc<OperatorNode>, right: Rc<OperatorNode> },
    SummaryDelete { summary_input: Rc<OperatorNode>, key: ColumnId },
    SummaryJoin { outer: Rc<OperatorNode>, inner: Rc<OperatorNode>, key: ColumnId, family: FieldDataType },
    Extension { child: Rc<OperatorNode>, name: String },
}

impl ASAPOp {
    pub fn children(&self) -> Vec<&Rc<OperatorNode>>;     // SummaryAgg includes its filter's subquery nodes
    pub fn map_children(&self, f: impl FnMut(&Rc<OperatorNode>) -> Rc<OperatorNode>) -> Self;
    pub fn kind_name(&self) -> &'static str;
    /// Subtract, Delete, Join, Extension.
    pub fn is_unimplemented(&self) -> bool;
    /// SummaryAgg/SummaryJoin `family`; SummaryMerge: its inputs' state type.
    pub fn produced_state(&self) -> Option<&FieldDataType>;
    pub fn output_schema(&self) -> Result<Schema, SchemaDerivationError>;
    pub fn output_kind(&self) -> OperatorResultKind;
    pub fn validate_inputs(&self) -> Result<(), SchemaDerivationError>;
}
```
