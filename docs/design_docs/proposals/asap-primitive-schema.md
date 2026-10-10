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
| **Column** | One position in every row, with a name and a type. Qualified as `table.column` when names can collide (DataFusion `Column { relation, name }`). | `ColumnId` refers to a column of the input schema; `(table, name)` identifies it across nodes (§4.2.2). |
| **Schema** | The ordered list of columns of a relation: name, data type, nullability (Arrow `Schema` of `Field { name, data_type, nullable }`; DataFusion `DFSchema` adds the table qualifier). The schema is *metadata*: it describes rows, it contains none. | `Schema` of `Field { name, dtype, nullable, table }` in `crates/types/src/pre_asap/schema.rs`. Unlike Arrow, `dtype` can be a summary state type (§3). |
| **Data type** | The type of a column's values (`Int64`, `Utf8`, `Timestamp`, …). | `DataType`, wrapped as `FieldDataType::Plain`. |
| **Aggregate state** | The intermediate value of an aggregate function before its final result, e.g. `(sum, count)` for `AVG` (DataFusion `Accumulator::state`, partial/final aggregation). It is never exposed as a column type to users. | Summary state *is* a column type here (`FieldDataType::Sketch`, `ExactAggregate`, …), so state can flow along edges and be merged, stored and read by later operators. |
| **View / materialized view** | A view is a named query (its *definition*). A materialized view also stores the query's result rows; a query can then be answered from it when its definition matches (view matching, §4.1). | A built summary state is a materialized aggregation view whose aggregate is a summary family. Its definition and which rows it took are its coverage (§4). |
| **Physical data layout** | How rows are stored: row-oriented or columnar (Arrow `RecordBatch`: one array per column), split into partitions (hash or range) and batches. | Decided in physical planning ([planning stages §2](planner-layering.md#2-physical-asap-aware-optimization)) and by the downstream deployment runtime. The logical schema does not depend on it. |

Two consequences for the design:

- A schema says what *kind* of values flow along an edge, never *which* rows. Which rows a relation contains is decided by the operators below it (its definition). This is why coverage is a node property and not part of the schema (§4).
- Existing systems keep aggregate state internal to one operator. ASAPPlanner makes it a first-class column type so that one state can be shared, merged and stored across queries, which is what §3 and §4 add.

## 3. Proposed schema design 
Schema represents the **metadata** of information flow along an **edge** between two nodes in a logical or physical DAG. The schema field is associated with the node in the DAG. The consumer of the node in the DAG takes the schema from the producer node as input. 

Schema definition here is shared between LogicalDAG, LogicalASAPDAG, and PhysicalASAPDAG. The schema contain fields, and each field is mapping to a column in the physical data representation. 
Based on our requirement, each field should contain the following information.
1. **What type of the ASAP Primitive is.** The field's type is a [`FieldDataType`](#63-schema-and-field-types-cratestypessrcpre_asapschemars) (`crates/types/src/pre_asap/schema.rs`). A column is either a raw value or a summary state:

   - **Raw value**: `Plain(DataType)`, e.g., a number or a string.
   - **Summary state**: described from coarse to fine by four levels:

   | Level | Code | Values | KLL example |
   |---|---|---|---|
   | Family | `FieldDataType` variant | `ExactAggregate`, `Sketch`, `Sample`, `Wavelet`, `StatModel` | `Sketch` |
   | Category (sketches only) | `SketchCategory` | `Quantile`, `Frequency`, `Cardinality`, `TopK`, `Universal` | `Quantile` |
   | Algorithm | `SketchAlgorithm` (other families: `ExactKind`, `SamplingKind`, …) | `Kll`, `Cms`, `Hll`, `DDSketch`, … | `Kll` |
   | Parameters | `SketchParams` (other families: `ExactParams`, `SamplingParams`, …) | per algorithm | `Kll { k: 200 }` |

   - For a sketch, category, algorithm and parameters are bundled as one `SketchKind` ([§6.4](#64-summary-family-parameters-cratestypessrcpost_asapsketchrs)). A sketch also records its `GroupingStrategy`: one instance per group, or one shared structure (Hydra) for all groups.
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
     | `Sum`, `Count`, `Min`, `Max`, `Rate`, `IRate`, `Increase` (exact) | `ExactAggregate(ExactKind, …)` | `FinalizeExactAccumulator` |

     The sketch candidates are `summary_candidates(intent)` in `crates/asap-aware-mapping/src/replacement.rs`; exact accumulators come from `exact_realization` there (`Count`) and from `function_rules.rs` (the others); the readouts are `SketchStatistic` ([§6.5](#65-update-input-and-readouts-post_asapsketchrs-post_asapmaintained_populationrs)).

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
| What does it store? | `definition` (what is computed) + `selection` (which rows were taken) | §4.2.1 |
| How is it computed? | by the planner, from the sub-DAG the node covers, in one walk that yields both the definition and the selection | §4.2.2 |
| How expensive is it? | proportional to the few operators directly under the `SummaryAgg`, not to the whole sub-DAG; cached per node | §4.2.3 |
| What uses it? | merge (in #646); rollup, slice, reuse (planned); subtract (reserved) | §5.6, §5.7 |
| Where is it in the code? | `OperatorNode::coverage()`, `SummaryCoverage::derive` | §6.1, §6.6 |
| What is left out? | the deployment and runtime implementation, e.g. SDS | §4.3 |

**Why coverage is not part of the schema.**

- `SummaryMerge` requires all inputs to have the same schema; that check is how it knows they are the same kind of state (same sketch, parameters and grouping).
- Two summary states worth merging always cover different data. For example, `quantile_over_time(0.99, latency[2m])` (p99 of each series of metric `latency`, whose series are identified by label `job`) can be answered from two one-minute KLL states, one for minute 0–1 and one for minute 1–2 (§5.6, Example A):

  | | State A | State B | Equal? |
  |---|---|---|---|
  | schema | `(job: Utf8, state: KLL{k=200})` | `(job: Utf8, state: KLL{k=200})` | yes, so the merge is allowed |
  | what it summarizes | time `(0, 1m]` | time `(1m, 2m]` | no, which is why merging them is useful |

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

**Step 2: does `V` contain every row `Q` needs?**

- **Ranges:** each range of `Q` lies inside the same column's range in `V`. `day ∈ [5, 10]` is inside `[1, 31]` ✓. `V` has no range on `region`, so any `region` is in `V` ✓.
- **Residuals:** each residual of `V` also appears in `Q`. Since residuals are not understood, the only safe case is when `Q` has the same condition.
- **Equivalence classes:** each column equality of `V` also holds in `Q`.

**Step 3: can the answer be computed from `V`'s output?**

- **Compensating filter:** where `Q` is narrower than `V`, the extra condition is applied to `V`'s rows. So its columns must be in `V`'s output: `day` and `region` are ✓.
- **Regrouping:** `Q`'s `GROUP BY` must be a subset of `V`'s. `{job}` ⊆ `{region, job, day}` ✓, so each group of `Q` is the sum of some groups of `V`.

**Result:**

```sql
SELECT job, SUM(s) FROM V
WHERE day BETWEEN 5 AND 10 AND region = 'us'
GROUP BY job;
```

**Limits that matter for us:**

- It answers a query from **one** view. Combining several views (a union) is left out.
- It supports aggregates whose groups can be added up again: `SUM` and `COUNT` (and `AVG` computed from them).

**Existing implementation.** The `WHERE` split is implemented for DataFusion in [`datafusion-contrib/datafusion-materialized-views`](https://github.com/datafusion-contrib/datafusion-materialized-views), `src/rewrite/normal_form.rs` (`SpjNormalForm`, `Predicate { eq_classes, ranges_by_equivalence_class, residuals }`). It rejects plans that contain an `Aggregate` or a `Join`.

### 4.2 Summary Coverage = Summary definition + selection

#### 4.2.1 Why coverage has two parts

A summary state is a stored aggregation, like `V` in §4.1, whose aggregate is a sketch. Before merging or reusing a state, the planner must answer two different questions about it[^gl]:

| Part | Question it answers |
|---|---|
| **`definition`** | *What* is computed? |
| **`selection`** | *Which rows* went in? |

**Example.** Three queries over table `t`:

```sql
-- S_us: p99 latency per job, US rows
SELECT job, approx_percentile_cont(latency, 0.99) FROM t WHERE region = 'us' GROUP BY job;
-- S_eu: p99 latency per job, EU rows
SELECT job, approx_percentile_cont(latency, 0.99) FROM t WHERE region = 'eu' GROUP BY job;
-- S_size: p99 size per job, EU rows
SELECT job, approx_percentile_cont(size, 0.99) FROM t WHERE region = 'eu' GROUP BY job;
```

With an error target, the planner answers each quantile from a KLL state. All three states have the same schema `(job Utf8, state Sketch(KLL k=200))`. `KLL(latency) by[job]` means a KLL sketch of `latency` for each `job`:

| State | Sub-DAG | `definition` | `selection` |
|---|---|---|---|
| `S_us` | `KLL(latency) by[job]` over `Filter(region = 'us', Scan t)` | `KLL(latency) by[job]` over `Scan t` | `region ∈ {us}` |
| `S_eu` | `KLL(latency) by[job]` over `Filter(region = 'eu', Scan t)` | `KLL(latency) by[job]` over `Scan t` | `region ∈ {eu}` |
| `S_size` | `KLL(size) by[job]` over `Filter(region = 'eu', Scan t)` | `KLL(size) by[job]` over `Scan t` | `region ∈ {eu}` |

- `S_us + S_eu` ✓: same computation, different rows. The result is the KLL of latency by job for US and EU, with every row counted once.
- `S_us + S_us` ✗: same computation, same rows. Every US row would be counted twice.
- `S_eu + S_size` ✗: same rows, different computation. One summarizes `latency`, the other `size`; the schema cannot tell them apart.

If coverage were one thing, for example the whole sub-DAG compared as a unit, `S_us` and `S_eu` would look different and could never merge. With two parts, the planner checks each question on its own: the definitions must be equal, and the selections must not overlap.

**Why this is enough.** Two states with the same `definition` come from the same computation. If their selections do not overlap, no row is in both, so merging them counts every row once. This holds whatever the computation contains (joins, unions, `rate`, dedup), so we need no special rule per operator.

Formally, a state means `family(input(σ(C)))` for each group of `G`, where `C` is the sub-DAG without its row filters, `σ` is the selection, and `G` the grouping.

[^gl]: **What we take from Goldstein & Larson, and what we add.** The view's tables, joins and residuals become the sub-DAG `C` below the `SummaryAgg`; the aggregate and its argument become the summary family and its input; `GROUP BY` becomes the `SummaryAgg` grouping `G`. These three are in `definition`. The paper's ranges become `selection` (§4.2.2). A compensating filter on the view's output becomes slicing, allowed only on a column of `G`, and regrouping to a smaller `GROUP BY` becomes rollup (§5.6). We add three things: **unions of states** (the paper uses one view at a time; `SummaryMerge` combines several, so we also check that their selections do not overlap), **summary families** (the paper only re-aggregates `SUM` and `COUNT`; each family says how its inputs may overlap, §5.6), and **more kinds of conditions** (value sets, hash partitions, and time relative to the evaluation time, §4.2.2).


#### 4.2.2 Deriving the definition and the selection

The planner computes the coverage of a node from the sub-DAG the node covers, not from a declaration. One walk down the sub-DAG produces both parts: every filter condition either moves into `selection` or stays in `definition`. The definition is described first, then how the planner decides which conditions move into the selection.

**Worked example.** p99 latency per job over table `t` (columns `job`, `region`, `latency`):

```sql
SELECT job,
       approx_percentile_cont(latency, 0.99) FILTER (WHERE latency < 100)
FROM (SELECT job, region AS r, latency
      FROM t
      WHERE region = 'us' AND latency * 2 > 10)
GROUP BY job;
```

The SQL frontend lowers it as follows: `FILTER (WHERE …)` becomes the aggregate's filter, `region AS r` becomes a `Project`, and the `WHERE` conditions are folded into the `Scan`'s predicates. With an error target, the planner answers the quantile from a KLL `SummaryAgg`, read out by a `SummaryEstimate`. The sub-DAG under that `SummaryEstimate`:

```text
                 ( SummaryEstimate p99 )
                         ▲
                         │
              [[ SummaryAgg ]]     KLL(latency) by job,  filter: latency < 100
                         ▲
                         │
                 [ Project ]       job, region AS r, latency
                         ▲
                         │
                  [ Scan t ]       predicates: region = 'us' AND latency * 2 > 10
```

##### The definition


The `definition` is the `SummaryAgg` together with its sub-DAG, with every condition that moves into `selection` (below) taken out. Everything else stays exactly as it is:

| In the sub-DAG | In the `definition` |
|---|---|
| a `Filter` whose conditions all move into `selection` | removed |
| a `Filter` with some conditions that stay | kept, with only the conditions that stay |
| `Scan.predicates` and `SummaryAgg.filter` | trimmed the same way |
| the one range `TimeRange`, and any `TimeShift`s, when they become relative time | removed |
| a `TimeShift` with no range `TimeRange`, or two or more range `TimeRange`s | unchanged: no time is taken out |
| any other operator | unchanged |

So the `definition` holds what the state computes: the computation `C` with its remaining conditions, the summary family and its parameters, the input column, and the grouping `G`.

In the worked example, `latency < 100` and `region = 'us'` move into `selection` (below), and `latency * 2 > 10` stays:

```text
                 ( SummaryEstimate p99 )
                         ▲
                         │
              [[ SummaryAgg ]]     KLL(latency) by job            ← filter removed
                         ▲
                         │
                 [ Project ]       job, region AS r, latency      ← unchanged
                         ▲
                         │
                  [ Scan t ]       predicates: latency * 2 > 10   ← region = 'us' removed
```

**When two definitions are equal.** Merging (§5.6) requires equal definitions.

- They must have the same structure. Planning details are ignored: `timing`, `guarantee` and `coverage_cache`. So a pane built at ingestion time can merge with one built at query time.
- `SummaryUpdate.weight_domain` is compared too. It is computed from the rest, so it differs only if something is wrong.
- States over different tables never merge: a KLL over `m1` and one over `m2` have different definitions. To combine tables, put a `UNION ALL` with a column that marks the source table below one `SummaryAgg`; that column can then be used in `selection` or in the grouping.

##### The selection


**Goal.** Decide which filter conditions under the `SummaryAgg` move into `selection`: those that only choose *which rows* go into the state. All other conditions stay in the `definition` (above).

**Steps.**

1. **Collect the conditions.** Go down the sub-DAG from the `SummaryAgg` and collect every filter condition: from `Filter` nodes, from `Scan.predicates`, and from the `SummaryAgg`'s own `filter`. A condition `A AND B` counts as two conditions, `A` and `B`.
2. **Ask two questions about each condition:**
   - **Rule 1: would it pick the same rows if it were moved to just below the `SummaryAgg`?** `region = 'us'` below a `Project` that only renames columns: yes. `value > 5` below `rate`: no, because it filters the raw samples that `rate` reads, which changes the rate values.
   - **Rule 2: is it a simple condition on one column?** That is, a value set such as `region IN ('us', 'eu')`, or a range such as `latency < 100`. `latency * 2 > 10` is not: it is on an expression.
3. **Putting the two rules together.**
   - If both answers are yes: take the condition out of the sub-DAG and put it into `selection`.
   - If either answer is no: leave it in the sub-DAG, so it is part of `definition`. The paper calls such conditions *residuals*.
   - One more case stays in `definition`: a condition on a column that already has a lifted condition it cannot be combined with (a forbidden-value set and a range, or values of different types). Conditions are taken in walk order: the `SummaryAgg.filter`, then the `Filter`s from top to bottom, then `Scan.predicates`.

In the worked example:

| Condition | Found at | Rule 1: same rows at the `SummaryAgg`? | Rule 2: simple? | Result |
|---|---|---|---|---|
| `latency < 100` | `SummaryAgg.filter` | yes, it is already there | yes, an interval | `selection`: `latency ∈ (−∞, 100)` |
| `region = 'us'` | `Scan.predicates` | yes: `Project` passes `region` through (renamed `r`) | yes, a value set | `selection`: `r ∈ {us}` |
| `latency * 2 > 10` | `Scan.predicates` | yes | **no**: it is on an expression, not a column | stays in `definition` |

So the `selection` is `latency ∈ (−∞, 100)`, `r ∈ {us}`. A PromQL example with a time window is under **Time** below.

**Rule 1 in detail.** Imagine moving the condition up, one operator at a time, until it is just below the `SummaryAgg`. Every operator it passes must leave the picked rows unchanged. Whether it can pass depends on what the operator does:

| Operator it must pass | Can it pass? | Why | Example |
|---|---|---|---|
| another `Filter` (also `Scan.predicates`, `SummaryAgg.filter`) | yes | filters only drop rows, so their order does not matter | `region = 'us'` below `Filter(value < 100)` ✓ |
| `TimeRange` (range) or `TimeShift` (without `@`) | yes | they choose a time window, but do not change any row's values | `region = 'us'` below `TimeRange(1m)` ✓ |
| `Project` | only if its column is passed through unchanged (a rename is fine) | the column must still be there, with the same values, above the `Project` | `Project [job, region AS r]`: `region = 'us'` ✓, it becomes `r = 'us'`. `Project [job, value * 2 AS v2]`: `value > 5` ✗, `value` is gone |
| `Aggregate` (later) | only if it uses group columns | a group column has one value per group, so filtering before or after grouping keeps the same groups | below `SUM(value) by job`: `job = 'api'` ✓; `value > 5` ✗, it changes the sums |
| `rate`, a window function, or an instant `TimeRange` (later) | only if it uses series labels | a label is the same for every sample of a series | below `rate(...)`: `job = 'api'` ✓; `value > 5` ✗, dropping raw samples changes the rate. Below an instant selector: `job = 'api'` ✓; `value > 5` ✗, the latest sample with value > 5 is not the latest sample |
| any other operator, e.g. `Join`, `Limit`, a `TimeShift` with `@` | no | | |

A `Filter` *above* `rate` has nothing to pass: it keeps some rate outputs, and those are exactly the rows the `SummaryAgg` reads. A PromQL comparison such as `rate(m[5m]) > 0` is not lowered to a `Filter`, though, but to a comparison operator (`BinaryOp`), which the walk does not enter. So today it stays in `definition` (last row of **More examples** below).

(This is filter pushdown in reverse. DataFusion's `PushDownFilter` uses the same rules to move filters down.)

**Rule 2 in detail.** `selection` can hold only three shapes of condition, each on a single column compared with non-NULL constants (`region = 'us'` and `'us' = region` both work): allowed values, forbidden values, or a range. Hash partitions will be a fourth shape later.

| Shape | Written as | Example | Stored as |
|---|---|---|---|
| allowed values | `=`, `IN`, or `OR` of `=` on the same column | `region IN ('us', 'eu')` | `region ∈ {us, eu}` |
| forbidden values | `!=`, `NOT IN` | `region != 'test'` | `region ∉ {test}` |
| range | `<`, `<=`, `>`, `>=` | `value >= 10 AND value < 100` | `value ∈ [10, 100)` |
| hash partition (later) | `hash(columns) mod n = k` | `hash(job) mod 4 = 1` | partition 1 of 4 |

Any other shape stays in `definition`:

| Condition | Why it is not one of the shapes |
|---|---|
| `latency * 2 > 10` | it is on an expression, not a column |
| `a = b` | it compares two columns |
| `region = 'us' OR job = 'api'` | it uses two columns |
| `name LIKE 'web%'` | it is a pattern, not a set of values or a range |

**Which column a condition is on.**

- A column is named by its table and name, `(table, name)`, **as the `SummaryAgg` reads it** (in the schema of its child). Directly above a join, `shipping.region = 'us'` and `billing.region = 'us'` are different conditions.
- A `Project` gives its columns new names and replaces their table with its own qualifier (none by default): after `region AS r`, the column is `(none, r)`, so the condition becomes `r ∈ {us}`. Since the `definition` contains the same `Project`, two states that rename the same way still compare equal.
- If two columns the `SummaryAgg` reads have the same `(table, name)` (for example `Project [a AS k, b AS k]`), a condition on `k` cannot tell them apart and stays in `definition`.
- PromQL labels have no table, so a label is named by its name alone.
- Values of different types are never treated as different: `1` and `1.0` might be equal, so `x = 1` and `x = 1.0` are treated as possibly overlapping.

**Time.** Example: `quantile_over_time(0.99, m[1m] offset 2m)` lowers to `TimeRange(1m)` over `TimeShift(2m)` over `Scan m`. Its KLL state (one per series) has `definition` = the `SummaryAgg` over `Scan m`, and `selection` = time `(−3m, −2m]`, the minute that ended 2 minutes before evaluation. There are two kinds of time:

| Kind | Comes from | Example |
|---|---|---|
| **Relative** to the evaluation time | a range `TimeRange(w)` over a `TimeShift(s)` → `(−(s+w), −s]` | `TimeRange(1m)` alone → `(−1m, 0]`; over `TimeShift(1m)` → `(−2m, −1m]` |
| **Absolute** (later) | an interval on the timestamp column | `ts >= t0 AND ts < t1` → `ts ∈ [t0, t1)` |

- PromQL windows exclude their start, so relative windows are open on the left.
- Window composition ([Pass 2](planner-layering.md#pass-2-asap-aware-common-subexpression-elimination) of logical optimization) builds its tumbling panes this way: a 3-minute window as panes `(−1m, 0]`, `(−2m, −1m]`, `(−3m, −2m]`, so pane times are derived, not declared (#601).
- An instant `TimeRange` (latest sample per series) does not pick rows by time, so it stays in `definition`. The walk also stops there, so for now the label matchers of an instant selector stay in `definition` too; lifting them is planned together with `rate` (Rule 1).
- The IR cannot yet write a timestamp constant, so absolute SQL time filters stay in `definition` for now.
- Absolute and relative time are never compared: a state over `(−1m, 0]` and one over `ts ∈ [t0, t1)` are treated as possibly overlapping.

**More examples** of what goes where. Each query's quantile is answered from a KLL `SummaryAgg`:

| Query | Sub-DAG below `SummaryAgg` | `definition` keeps | `selection` takes |
|---|---|---|---|
| SQL `… FROM t WHERE region = 'us'` | `Scan t {region = 'us'}` | `Scan t` | `region ∈ {us}` |
| SQL `… FROM t WHERE latency < 100` | `Scan t {latency < 100}` | `Scan t` | `latency ∈ (−∞, 100)` |
| PromQL `quantile_over_time(0.99, m[1m] offset 2m)` | `TimeRange(1m, TimeShift(2m, Scan m))` | `Scan m` | the last 3 to 2 minutes before evaluation, `(−3m, −2m]` |
| PromQL `quantile(0.99, rate(m{job="api"}[5m]))` | `rate(TimeRange(5m, Scan m {job = 'api'}))` | all of it | nothing: `job = 'api'` is under `rate`, which the walk cannot pass yet (Rule 1, later) |
| PromQL `quantile(0.99, rate(m[5m]) > 0)` | `BinaryOp(>, rate(TimeRange(5m, Scan m)), 0)` | all of it | nothing: a PromQL comparison is a `BinaryOp`, not a `Filter` |

`Scan t {…}` is a `Scan` with those predicates. In the last two rows `TimeRange(5m)` stays in `definition` in any case: it is the input window of `rate` and changes the rate values, so it does not just pick rows.

#### 4.2.3 Cost of deriving coverage

**When it runs.** `coverage()` derives a node's coverage the first time it is called and caches it on the node, so each node pays once. Building a `SummaryMerge` (`OperatorNode::new`) derives its coverage to reject an invalid merge and caches the result, so `coverage()` and `validate_structure` do not derive it again. `validate_structure` derives it only for a merge that was not built through `new`.

**Sizes used below.**

| Symbol | Meaning | Typical size |
|---|---|---|
| `d` | operators on the walk: the `Filter`, `Project`, `TimeRange` and `TimeShift` nodes directly under the `SummaryAgg` (§4.2.2) | a few |
| `c` | filter conditions on the walk (after splitting at `AND`), including `SummaryAgg.filter` and `Scan.predicates` | a few |
| `f` | columns in the widest schema on the walk | tens |
| `v` | values in one value set (`IN` list) | a few |
| `n` | inputs of a `SummaryMerge` | panes per window, regions, … |
| `b` | boxes in one input's selection | 1 for a `SummaryAgg` |
| `N` | nodes in a `definition` | the sub-DAG size |

**`SummaryAgg`: `O(c · (d·f + v²) + d·f)`.**

- Each condition is checked once. Following its column up through the `Project`s costs `O(d·f)`; checking that the column's name is unique costs `O(f)`; building and intersecting a value set costs `O(v²)`, because values are compared by type, not hashed.
- Rebuilding the definition creates at most `d` new nodes, each copying a schema (`O(f)`). Everything below the walk is shared, not copied.
- So the cost depends only on the few operators directly under the `SummaryAgg`, **not on the size of the sub-DAG below them**. A `SummaryAgg` over a large join costs the same as one over a `Scan`.

**`SummaryMerge`: `O(n·N + n²·b²·f·v² + (n·b)³)` in the worst case.**

| Step | Cost | Why |
|---|---|---|
| inputs' coverage | at most once per input | usually already cached; otherwise derived and cached now |
| equal definitions | `O(n·N·f)`: `n·N` node comparisons, each comparing an operator and a schema; often `O(n·f)` | structural comparison of each input's definition with the first one. Shared nodes (`Rc`) compare in `O(1)`, and node pairs already proven equal are remembered |
| no overlap | `O(n²·b²·f·v²)` | every pair of inputs, every pair of boxes, every shared column |
| union of selections | `O((n·b)³)` box comparisons, worst case | joins boxes that differ in only one dimension whose union is again one constraint (touching time windows, touching value ranges, or value sets of one column), until nothing more joins; each join restarts the scan |

For the common cases this is small: `n` one-minute panes in time order have one box each with no columns, so the merge costs `O(n·N·f)` for the definitions, `O(n²)` for overlap and `O(n²)` for the union. Nested merges keep `n` small: a merge of merges compares only its direct inputs, whose coverage is cached.

### 4.3 What coverage does not contain

Coverage only says what a state means and which rows it took. The deployment and runtime implementation is not part of coverage: it belongs to the downstream deployment runtime, for example the summary data store (SDS), reading source data, and building, storing and serving summary instances.

TODO: the SDS definition, including how it stores a summary's `definition` and `selection`, will be specified in a separate doc.

## 5. Examples on how OperatorNode, schema, and physical data information are being used with Summary operators

This section walks through each summary operator with one small example. For each operator it answers four questions:

| Question | What it tells you |
|---|---|
| **What comes out?** | the output schema the planner derives (`output_schema()`) |
| **When is it rejected?** | the checks the planner runs when it builds the node |
| **What is its coverage?** | `coverage()` of the output (§4.2) |
| **State or value?** | whether the output is summary state or a readable value |

**How to read the diagrams.** Data flows from bottom to top, along the `▲` arrows. Each edge is labelled with the schema it carries, written `Kind: field Type, …`.

| Notation | Meaning |
|---|---|
| `[ Op ]` | operator whose output is a value (`Relation`) |
| `[[ Op ]]` | operator whose output is summary state (`State`) |
| `( next operator )` | whatever consumes the result |
| `selection: …` next to a node | the `selection` part of that node's `coverage()` |

All examples read a table, so values are `Relation`. For PromQL series they would be `InstantVector`. A *closed schema* lists every column of the table. For readability, a filter on a table is drawn as a `Filter` over the `Scan`; the frontend folds it into `Scan.predicates`, which gives the same coverage (§4.2.2).

### 5.1 `SummaryAgg`: values → state

**Operator definition.** Turns rows into summary state: one state per group.

**Example.** p99 latency by job, using only US rows with latency under 10 s:

```sql
SELECT job, approx_percentile_cont(latency, 0.99)
FROM t WHERE region = 'us' AND latency < 10000
GROUP BY job;
```

With an error target, the planner answers the quantile from a KLL sketch with `k = 200`: a `SummaryAgg` builds the state, and a `SummaryEstimate` (5.2) reads p99 from it.

```text
                 ( next operator )
                         ▲
                         │  State: job Utf8, state Sketch(KLL k=200)
                         │
              [[ SummaryAgg ]]   KLL k=200, input latency, by job
                         ▲
                         │  Relation: job Utf8, region Utf8, ts Timestamp, latency Float64
                         │
                  [ Filter ]     region = 'us' AND latency < 10000
                         ▲
                         │  Relation: job Utf8, region Utf8, ts Timestamp, latency Float64
                         │
                  [ Scan t ]

coverage() of the SummaryAgg
┌──────────────────────────────────────────────────────────────────┐
│ definition: this SummaryAgg over Scan t   (the Filter removed)   │
│ selection:  region ∈ {us},  latency ∈ (−∞, 10000)                │
└──────────────────────────────────────────────────────────────────┘
```

| Question | Answer |
|---|---|
| What comes out? | the group columns (`job`), then one field `state` whose type is the summary type, here `Sketch(KLL k=200)`. Each `job` appears once. With `PerEntity` (one state per series), the input columns are kept and `state` replaces the value column |
| When is it rejected? | the summary type is a plain value type; the input is already state; the input or item column (`latency`) is not in the child's schema; `filter` is not a boolean |
| What is its coverage? | always present. Both `Filter` conditions are simple, so they move into `selection`, and the `definition` is the `SummaryAgg` over the bare `Scan t`. A condition like `latency * 2 > 10` would stay in the `definition` (§4.2.2) |
| State or value? | state. This is where values become state, so the result has no error bound yet |

### 5.2 `SummaryEstimate`: sketch state → value

**Operator definition.** Reads a number out of a sketch, for example a quantile or a count.

**Example.** Two queries over the same rows as 5.1, one for p99 and one for p50:

```sql
SELECT job, approx_percentile_cont(latency, 0.99) FROM t WHERE region = 'us' AND latency < 10000 GROUP BY job;
SELECT job, approx_percentile_cont(latency, 0.5)  FROM t WHERE region = 'us' AND latency < 10000 GROUP BY job;
```

Both need the same KLL state, so the planner builds it once (sub-DAG sharing, [Pass 2](planner-layering.md#pass-2-asap-aware-common-subexpression-elimination)) and reads it twice.

```text
       ( next operator )                       ( next operator )
               ▲                                       ▲
               │  Relation: job Utf8,                  │  Relation: job Utf8,
               │  quantile Float64                     │  quantile Float64
               │                                       │
   [ SummaryEstimate ]                     [ SummaryEstimate ]
     Quantile q = 0.99                       Quantile q = 0.5
               ▲                                       ▲
               │                                       │
               └───────────────────┬───────────────────┘
                                   │  State: job Utf8, state Sketch(KLL k=200)
                                   │
                        [[ SummaryAgg ]]      KLL k=200, input latency, by job
                                   ▲
                                   │
                            [ Filter ]        region = 'us' AND latency < 10000
                                   ▲
                                   │
                            [ Scan t ]
```

| Question | Answer |
|---|---|
| What comes out? | the same columns, with `state` replaced by the answer: `quantile` Float64 here. Counts and cardinalities are Int64 (Float64 when the state was built per series, `PerEntity`). A top-k readout is a `topk` Utf8 field today; #579 changes it to return the selected rows: the partition keys, the item identity columns and a Float64 `value` score |
| When is it rejected? | the input is not a sketch, or the sketch cannot answer the question. For example, asking a KLL for a cardinality |
| What is its coverage? | none: the output is a value |
| State or value? | value. The node carries the readout's error bound |

### 5.3 `FinalizeExactAccumulator`: exact state → value

**Operator definition.** Turns an exact accumulator (sum, count, min, max, rate, …) into its final value.

**Example.** Total bytes by host:

```sql
SELECT host, SUM(bytes) FROM t GROUP BY host;
```

The planner answers `SUM` with an exact `Sum` accumulator: a `SummaryAgg` builds it, and `FinalizeExactAccumulator` reads the total.

```text
                 ( next operator )
                         ▲
                         │  Relation: host Utf8, state Float64
                         │
         [ FinalizeExactAccumulator ]
                         ▲
                         │  State: host Utf8, state ExactAggregate(Sum)
                         │
              [[ SummaryAgg ]]   ExactAggregate Sum, input bytes, by host
                         ▲
                         │  Relation: host Utf8, bytes Float64
                         │
                  [ Scan t ]

coverage() of the SummaryAgg
┌──────────────────────────────────────────────────┐
│ definition: this SummaryAgg over Scan t          │
│ selection:  everything (no filter)               │
└──────────────────────────────────────────────────┘
```

| Question | Answer |
|---|---|
| What comes out? | the same columns, with `state` turned into the value type an ordinary `Aggregate` would give: a sum of Float64 is Float64, a count is Int64, a rate is Float64 |
| When is it rejected? | the input has no exact accumulator, for example a sketch (sketches are read with `SummaryEstimate`) |
| What is its coverage? | none on the output. The `SummaryAgg` below has coverage as in 5.1, whose selection has no conditions (all rows), because there is no filter |
| State or value? | value. Exact state is only ever read through this operator |

### 5.4 `MaintainPopulation`: values → maintained membership (state)

**Operator definition.** Keeps every value of a population (not a sketch), so that exact quantiles and top-k can be computed later, and tracks rows entering and leaving.

**Example.** Exact p99 latency per job, and the top-10 latencies per job:

```sql
SELECT job, percentile_cont(latency, 0.99) FROM t GROUP BY job;   -- exact target
SELECT job, latency
FROM (SELECT job, latency,
             ROW_NUMBER() OVER (PARTITION BY job ORDER BY latency DESC) AS rn
      FROM t)
WHERE rn <= 10;
```

With an exact target, the planner can keep all latencies per job in one maintained population, and answer both queries from it (5.5):

```text
                 ( next operator )
                         ▲
                         │  State: job Utf8, latency Float64   (all fields plain)
                         │
          [[ MaintainPopulation ]]   input: rows of that Scan, value latency, by job
                         ▲                  max_k: 10, quantiles: true
                         │  Relation: job Utf8, latency Float64
                         │
                  [ Scan t ]         closed schema
```

| Question | Answer |
|---|---|
| What comes out? | the same columns as the input, all plain. Only the result kind `State` marks it as maintained |
| When is it rejected? | the population description does not match the child. For table rows: the child must be that same table `Scan`, the value column a non-null Float64, and the grouping valid. For PromQL series: a scan of the same metric, labels and grouping, under an instant `TimeRange` of the lookback (which may be left out for the default 5-minute lookback) |
| What is its coverage? | none. Maintained populations are not merged today |
| State or value? | state, because it must also track membership changes. It is read only through `EvaluatePopulation` |

### 5.5 `EvaluatePopulation`: maintained membership → value

**Operator definition.** Computes an exact statistic from a maintained population.

**Example.** The two queries of 5.4 (exact p99 per job, top-10 latencies per job), both read from the one population:

```text
       ( next operator )                       ( next operator )
               ▲                                       ▲
               │  Relation: job Utf8,                  │  Relation: job Utf8,
               │  quantile_0_99 Float64                │  latency Float64 (the top rows)
               │                                       │
  [ EvaluatePopulation ]                  [ EvaluatePopulation ]
     Quantile q = 0.99                       TopK k = 10
               ▲                                       ▲
               │                                       │
               └───────────────────┬───────────────────┘
                                   │  State: job Utf8, latency Float64
                                   │
                    [[ MaintainPopulation ]]     by job, max_k: 10, quantiles: true
                                   ▲
                                   │  Relation: job Utf8, latency Float64
                                   │
                            [ Scan t ]           closed schema
```

| Question | Answer |
|---|---|
| What comes out? | the same shape as an ordinary `Aggregate` by the grouping: `quantile_0_99` Float64 here, or `sum`, `count`, `avg`. Top-k instead returns the selected rows |
| When is it rejected? | the child is not a `MaintainPopulation`, or the population was not set up for the question: `quantiles` must be on for a quantile (and `q` finite), and `k` must be at most `max_k` for top-k |
| What is its coverage? | none |
| State or value? | value |

### 5.6 `SummaryMerge`: state × N → state (merge and rollup)

**Operator definition.** Combines several states of the same kind into one. With `group_by` (planned) it can also make the grouping coarser (rollup).

**Status.**

- **On `main` (since #560):** implemented. All children must be state with exactly one state field and identical schemas.
- **#646 (open):** also requires equal `definition`s and selections that do not overlap, and computes the merged coverage.
- **Planned:** a `group_by` field, so one operator does both merge and rollup:

```rust
SummaryMerge { children: Vec<C>, group_by: Reduction }
```

**Example A: time panes.** p99 of each series of metric `m` over the last 2 minutes:

```promql
quantile_over_time(0.99, m[2m])
```

Window composition (Pass 2) can answer it from two one-minute panes, one KLL per series in each, merged. Both panes read the same `Scan`.

```text
                           ( next operator )
                                   ▲
                                   │  State: series labels, state Sketch(KLL k=200)
                                   │
                          [[ SummaryMerge ]]
                                   ▲             selection: time (−2m, 0]
                                   │
                 ┌─────────────────┴─────────────────┐
                 │  State: series labels,            │  State: series labels,
                 │  state Sketch(KLL k=200)          │  state Sketch(KLL k=200)
                 │                                   │
     [[ SummaryAgg ]]  pane 0             [[ SummaryAgg ]]  pane 1
     KLL k=200, per series                KLL k=200, per series
     selection: time (−1m, 0]             selection: time (−2m, −1m]
                 ▲                                   ▲
                 │                                   │
         [ TimeRange 1m ]                    [ TimeRange 1m ]
                 ▲                                   ▲
                 │                                   │
         [ TimeShift 0 ]                     [ TimeShift 1m ]
                 ▲                                   ▲
                 │                                   │
                 └─────────────────┬─────────────────┘
                                   │
                              [ Scan m ]
```

Both panes have the same `definition` (`SummaryAgg` over `Scan m`), and their time ranges touch, so the merge covers one continuous range:

```text
time      −2m           −1m            0
pane 1     (─────────────]
pane 0                   (─────────────]
merge      (───────────────────────────]
```

**Example B: regions.** Two queries for p99 latency per job, one per region, and a third for both regions:

```sql
SELECT job, approx_percentile_cont(latency, 0.99) FROM t WHERE region = 'us' GROUP BY job;
SELECT job, approx_percentile_cont(latency, 0.99) FROM t WHERE region = 'eu' GROUP BY job;
SELECT job, approx_percentile_cont(latency, 0.99) FROM t WHERE region IN ('us', 'eu') GROUP BY job;
```

The first two build `KLL(latency) by[job]` states. The third can be answered by merging them into one state for both regions:

```text
                           ( next operator )
                                   ▲
                                   │  State: job Utf8, state Sketch(KLL k=200)
                                   │
                          [[ SummaryMerge ]]
                                   ▲             selection: region ∈ {us, eu}
                                   │
                 ┌─────────────────┴─────────────────┐
                 │                                   │
     [[ SummaryAgg ]]                     [[ SummaryAgg ]]
     KLL k=200, by job                    KLL k=200, by job
     selection: region ∈ {us}             selection: region ∈ {eu}
                 ▲                                   ▲
                 │                                   │
     [ Filter region = 'us' ]             [ Filter region = 'eu' ]
                 ▲                                   ▲
                 │                                   │
                 └─────────────────┬─────────────────┘
                                   │
                              [ Scan t ]
```

**Example C: rollup (planned).** p99 latency per region and job, and per job:

```sql
SELECT region, job, approx_percentile_cont(latency, 0.99) FROM t GROUP BY region, job;
SELECT job, approx_percentile_cont(latency, 0.99) FROM t GROUP BY job;
```

One `by[region, job]` state answers the first query directly. Rolled up to `by[job]`, it answers the second: each job's state is the merge of its per-region states.

```text
       ( next operator )                       ( next operator )
               ▲                                       ▲
               │  Relation: job Utf8,                  │  Relation: region Utf8, job Utf8,
               │  quantile Float64                     │  quantile Float64
               │                                       │
   [ SummaryEstimate ]                     [ SummaryEstimate ]
     p99 by job                              p99 by region, job
               ▲                                       ▲
               │  State: job Utf8,                     │
               │  state Sketch(KLL k=200)              │
               │                                       │
     [[ SummaryMerge ]]                                │
     group_by: by job                                  │
               ▲                                       │
               │                                       │
               └───────────────────┬───────────────────┘
                                   │  State: region Utf8, job Utf8, state Sketch(KLL k=200)
                                   │
                        [[ SummaryAgg ]]      KLL k=200, by region, job
                                   ▲
                                   │
                            [ Scan t ]
```

```text
input groups                       output groups
(us, api) ─┐
(eu, api) ─┴─ merge ─────────────▶ api
(us, web) ─┐
(eu, web) ─┴─ merge ─────────────▶ web
```

**Which merges are allowed.** Each state below is the per-series KLL that answers a PromQL query; `A`, `B` and `C` have the definition `KLL(value) per series over Scan m`:

| State | Query | Selection |
|---|---|---|
| `A` | `quantile_over_time(0.99, m[1m])` | time `(−1m, 0]` |
| `B` | `quantile_over_time(0.99, m[1m] offset 1m)` | time `(−2m, −1m]` |
| `C` | `quantile_over_time(0.99, m[1m] offset 30s)` | time `(−90s, −30s]` |
| `D` | `quantile_over_time(0.99, n[1m])` | time `(−1m, 0]`, but over metric `n`: definition `KLL(value) per series over Scan n` |

| Merge | Allowed? | Why | Result's selection |
|---|---|---|---|
| `A + B` | ✓ | same definition, no overlap | `(−2m, 0]` (touching time windows join) |
| `A + C` | ✗ | `(−60s, −30s]` is in both, so those rows would be counted twice | |
| `A + A` | ✗ | every row is in both | |
| `A + D` | ✗ | different definitions: `D` reads metric `n`, not `m` | |
| `A` + a KLL with `k = 400` | ✗ | different schema: `k` is part of the state type, so the merge is rejected before coverage is checked | |
| `(A + B)` + the state of `m[1m] offset 2m`, time `(−3m, −2m]` | ✓ | a merge has coverage like any state, so merges nest | `(−3m, 0]` |

**Whether inputs may overlap** depends on the summary type (family and algorithm, §3). This is #592 (open), which adds `FieldDataType::family_merges` and `merge_relation`; #646 alone requires every merge to be overlap-free:

| Rule | Summary types | Example |
|---|---|---|
| **must not overlap** | KLL, DDSketch, Count-Min, Count Sketch, UnivMon, exact `Sum`/`Count` | KLL `A + C` ✗: the rows in `(−60s, −30s]` would be counted twice |
| **may overlap** | HLL, exact `Min`/`Max` | HLL over `A`'s and `C`'s rows ✓: a value seen twice is still one distinct value. The result keeps both boxes, `(−1m, 0]` and `(−90s, −30s]`, which together cover `(−90s, 0]` |
| **cannot merge (yet)** | `CmsWithHeap`, `CountSketchWithHeap`, Theta, KMV, exact `Rate`/`IRate`/`Increase`, samples, wavelets, models | no sound merge is modeled yet, so #592 rejects them |
| **right inside left** | subtraction | 5.7 |

| Question | Answer |
|---|---|
| What comes out? | the children's schema; with `group_by`, only the remaining group columns |
| When is it rejected? | no children; a child is not state; the schemas differ (e.g. different sketch parameters); the definitions differ (different column, filters or source); the selections may overlap (#646; with #592, only where the family does not allow it); the family cannot merge at all (#592); `group_by` is not a subset of the children's grouping (planned) |
| What is its coverage? | the shared `definition` (with the new grouping, once `group_by` exists), and the union of the children's selections. Two boxes join when they differ in only one dimension and their union is again one constraint, as in the paper's one range per column: touching time windows (`(−2m, −1m]` and `(−1m, 0]` give `(−2m, 0]`), touching value ranges (`latency < 100` and `latency >= 100` give `latency ∈ (−∞, ∞)`, which still excludes NULL), or value sets of one column (`{us}` and `{eu}` give `{us, eu}`). Gaps stay as separate boxes |
| State or value? | state in, state out |

**Other uses of coverage.** The planner also uses coverage to read or reuse a state without merging:

- **Slice:** from a `by[region, job]` state, a query for `region = 'us'` by job reads only the `us` groups ✓. A query for `value < 50` ✗: `value` is not a grouping column, and a sketch cannot be filtered after it is built.
- **Reuse:** a stored state answers a query when the definitions are equal and the query's rows are all in the state, with any difference covered by a slice. A stored `by[region, job]` state over `(−5m, 0]` answers p99 by job over the last 5 minutes for `region = 'us'` ✓, but not over the last 1 minute ✗: time is not a grouping column, so minutes 2–5 cannot be taken out.

### 5.7 Reserved operators (not implemented)

These operators exist in the code so that plans can name them, but the planner cannot build them yet. Their intended shapes:

| Operator | Inputs | What it would do |
|---|---|---|
| `SummarySubtract` | `left`, `right` | remove one state from another, e.g. a window minus its oldest part. Same `definition`; the right selection must be inside the left |
| `SummaryDelete` | `summary_input`, `key` | remove the entries for one key |
| `SummaryJoin` | `outer`, `inner`, `key`, `family` | combine two states into a new one, e.g. to estimate a join's size |
| `Extension` | `child`, `name` | a state operator named by the deployment |

`SummarySubtract`, for a family that allows it (e.g. an exact `Sum`):

```text
time      −10m                 −5m                 0
left       (───────────────────────────────────────]
right      (───────────────────]
result                         (───────────────────]
```

## 6. Key code interfaces

Every struct field and enum-variant field below has a comment saying what it holds. Function bodies and serde/derive attributes are left out.

**How the subsections connect.** The subsections follow one DAG node from the outside in:

```text
OperatorNode                                         §6.1
├── operator: Operator::ASAP(ASAPOp)                 §6.2
│     ├── SummaryAgg
│     │     ├── family: FieldDataType ───────────┐   §6.3 → §6.4
│     │     └── input:  SummaryUpdate            │   §6.5  (what each row adds)
│     ├── SummaryEstimate.query: SketchStatistic │   §6.5  (what is read out)
│     ├── MaintainPopulation.population          │   §6.5
│     └── EvaluatePopulation.evaluation          │   §6.5
├── schema: Schema                               │   §6.3
│     └── fields[i].dtype: FieldDataType ◀───────┘   §6.3  (the same type: the output
│           └── Sketch(SketchKind, GroupingStrategy)        `state` field has type `family`)
│                 └── category, algorithm, params    §6.4
├── guarantee: ResultGuarantee                       §6.5  (accuracy of a readout)
└── coverage() → SummaryCoverage                     §6.6
      └── definition: OperatorNode   (again a node, so the same structure)
```

| Subsection | Defines | Used by |
|---|---|---|
| §6.1 DAG node | `OperatorNode` | the DAG itself |
| §6.2 ASAP operators | `ASAPOp` | `OperatorNode.operator` |
| §6.3 Schema and field types | `Schema`, `Field`, `FieldDataType` | every node's `schema`; `SummaryAgg.family` |
| §6.4 Summary-family parameters | `SketchKind`, `SketchParams`, `GroupingStrategy`, exact/sample/wavelet/model params | the payload of each non-plain `FieldDataType` variant |
| §6.5 Update input and readouts | `SummaryUpdate`, `SketchStatistic`, `MaintainedPopulation`, `PopulationStatistic` | fields of the `ASAPOp` variants in §6.2 |
| §6.6 Summary coverage | `SummaryCoverage`, `SelectionBox`, `Constraint` | `OperatorNode::coverage()` on summary nodes |

**Example: the p99 latency state of §5.1.** The `SummaryAgg` node (§6.1) holds `ASAPOp::SummaryAgg` (§6.2) with `family = Sketch(SketchKind::new(Kll, Kll { k: 200 }), PerSubpopulationInstance)` (§6.3, §6.4) and `input = SummaryUpdate::column(latency)` (§6.5). Its `schema` (§6.3) is `(job Utf8, state Sketch(KLL k=200))`: the `state` field has the same type as `family`. A `SummaryEstimate` above it holds `query = Quantile { q: 0.99 }` (§6.5), and its node carries the `guarantee` of the readout. `coverage()` on the `SummaryAgg` node returns a `SummaryCoverage` (§6.6).

### 6.1 DAG node (`crates/types/src/ir/node.rs`)

Every operator in a DAG is wrapped in an `OperatorNode`. `coverage()` is explained in §6.6.

Shown as of #646. On `main` today the node still has a declared `pub coverage: Option<SummaryCoverage>` field (with `with_coverage()` and `requires_coverage()`); #646 replaces it with the derived `coverage()` below.

```rust
/// One node of the DAG. Immutable and shared through `Rc`.
pub struct OperatorNode {
    /// What the node does: an ordinary operator or an ASAP operator.
    pub operator: Operator,
    /// What kind of output it has: `Relation`, `InstantVector`,
    /// `RangeVector`, or `State` (summary state). Derived from `operator`.
    pub result_kind: OperatorResultKind,
    /// The output columns. Derived from `operator` and its children.
    pub schema: Schema,
    /// The accuracy statement of the output, once known. `None` does not
    /// mean exact.
    pub guarantee: Option<ResultGuarantee>,
    /// When the node runs: `IngestionTime` or `QueryTime`. `None` until
    /// planning assigns it.
    pub timing: Option<ExecutionTiming>,
    /// Cache for `coverage()`. Filled on first use, never serialized,
    /// ignored by equality, emptied on clone. Not a source of truth:
    /// coverage can always be derived again.
    coverage_cache: CoverageCache,
}

impl OperatorNode {
    /// The node's coverage (§4.2), derived on first use and then cached.
    /// `Some` for a `SummaryAgg` and a valid `SummaryMerge`; `None` for every
    /// other node.
    pub fn coverage(&self) -> Option<&SummaryCoverage>;
}
```

### 6.2 ASAP operators (`crates/types/src/ir/asap.rs`)

`ASAPOp<C>` is generic over how it refers to its inputs: `C` is `Rc<OperatorNode>` by default, and can also be a node id (e.g. in a flat DAG).

```rust
pub const UNIMPLEMENTED_ASAP_OP: &str =
    "this ASAP operator is reserved: schema, accuracy, timing and export are not implemented";

pub enum ASAPOp<C = Rc<OperatorNode>> {
    SummaryAgg {
        /// The input rows.
        child: C,
        /// The summary type of the output `state` field. Never `Plain`.
        family: FieldDataType,
        /// What each input row adds to the state (item and weight).
        input: SummaryUpdate,
        /// The grouping: `Reduce(by columns)`, or `PerEntity` for one state per
        /// input series without grouping.
        reduction: Reduction,
        /// Whether each group gets its own sketch or all groups share one.
        grouping: GroupingStrategy,
        /// Rows to include, applied before updating the state. `None`: all rows.
        filter: Option<Predicate<C>>,
    },
    SummaryEstimate {
        /// The node that produces the sketch state.
        summary_input: C,
        /// What to read out of it.
        query: SketchStatistic,
    },
    FinalizeExactAccumulator {
        /// The node that produces the exact accumulator state.
        child: C,
    },
    MaintainPopulation {
        /// The input rows; must match `population.input`.
        child: C,
        /// What population to keep and which reads it supports.
        population: MaintainedPopulation<OperatorNode>,
    },
    EvaluatePopulation {
        /// The `MaintainPopulation` node.
        child: C,
        /// What to compute from it.
        evaluation: PopulationStatistic,
    },
    // Implemented since #560 (identical child schemas).
    SummaryMerge {
        /// The states to merge; all have the same schema.
        children: Vec<C>,
    },
    // Reserved: migrated but unimplemented.
    SummarySubtract {
        left: C,                // the state to subtract from
        right: C,               // the state to remove from `left`
    },
    SummaryDelete {
        summary_input: C,       // the state
        key: ColumnId,          // the key column whose entries are removed
    },
    SummaryJoin {
        outer: C,               // one input state
        inner: C,               // the other input state
        key: ColumnId,          // the join key column
        family: FieldDataType,  // the summary type of the result
    },
    Extension {
        child: C,               // the input
        name: String,           // the deployment-defined operator name
    },
}

impl<C> ASAPOp<C> {
    /// The input nodes. For `SummaryAgg` this also includes nodes used by
    /// subqueries inside its `filter`.
    pub fn children(&self) -> Vec<&C>;
    /// The same operator with each input replaced by `f(input)`; `f` may
    /// change the reference type, e.g. from `Rc<OperatorNode>` to a node id.
    pub fn map_children<D>(&self, f: impl FnMut(&C) -> D) -> ASAPOp<D>;
    /// The operator's name, e.g. `"SummaryAgg"`, for messages and display.
    pub fn kind_name(&self) -> &'static str;
}

impl ASAPOp {
    /// Whether the operator is reserved and cannot be built yet: Subtract,
    /// Delete, Join, Extension.
    pub fn is_unimplemented(&self) -> bool;
    /// The summary type this operator outputs: `family` for SummaryAgg and
    /// SummaryJoin, the inputs' state type for SummaryMerge, `None` otherwise.
    pub fn produced_state(&self) -> Option<&FieldDataType>;
    /// The output schema, derived from the operator and its inputs. An error
    /// for a reserved operator or an invalid input.
    pub fn output_schema(&self) -> Result<Schema, SchemaDerivationError>;
    /// The output kind: `State` for operators that output state, otherwise
    /// the value kind of the input.
    pub fn output_kind(&self) -> OperatorResultKind;
    /// Checks the inputs (§5, "When is it rejected?"). An error if they do
    /// not fit the operator.
    pub fn validate_inputs(&self) -> Result<(), SchemaDerivationError>;
}
```

### 6.3 Schema and field types (`crates/types/src/pre_asap/schema.rs`)

```rust
/// Position of a column in one schema (0-based). Local to that schema:
/// the same column can have a different `ColumnId` after a projection or join.
pub type ColumnId = usize;

/// The columns that flow along one DAG edge. Metadata only: it holds no data.
pub struct Schema {
    /// The columns, in order.
    pub fields: Vec<Field>,
    /// The column that holds each row's timestamp, if any. Must point at a
    /// plain `Timestamp` field. PromQL inputs always have one.
    pub time_index: Option<ColumnId>,
    /// Sets of columns whose values together identify at most one row,
    /// e.g. `[[0]]` when column 0 is unique (one row per `job`).
    pub unique_keys: Vec<Vec<ColumnId>>,
    /// `true`: `fields` lists every column. `false`: more columns may exist
    /// that are not listed (e.g. PromQL labels not yet known).
    pub closed: bool,
}
impl Schema {
    /// A schema with only these fields: no time column, no unique key, not
    /// closed. Used for a table `Scan` without key metadata.
    pub fn new(fields: Vec<Field>) -> Self;
    /// A schema with a time column and unique keys, not closed. Used for
    /// PromQL inputs, whose rows are unique per (time, label set).
    pub fn with_time_index(fields: Vec<Field>, time_index: ColumnId, unique_keys: Vec<Vec<ColumnId>>) -> Self;
    /// A closed schema with an optional time column and no unique key: the
    /// shape of a summary operator's output.
    pub fn lifted(fields: Vec<Field>, time_index: Option<ColumnId>) -> Self;
    /// Whether every field holds a plain value (no summary state).
    pub fn is_all_plain(&self) -> bool;
    /// Position of the first field with this name, or `None`.
    pub fn column_id(&self, name: &str) -> Option<ColumnId>;
    /// Position of the field with this table and name, or `None`. Tells apart
    /// `a.k` and `b.k` after a join.
    pub fn column_id_qualified(&self, table: &str, name: &str) -> Option<ColumnId>;
}

/// One column of a `Schema`. `T` is `FieldDataType` on DAG edges, and plain
/// `DataType` for fields nested inside a `List` or `Struct`.
pub struct Field<T = FieldDataType> {
    /// Column name as the producer outputs it: a SQL column name, or a PromQL
    /// label name, `value` or `timestamp`.
    pub name: String,
    /// The column's type: a plain value, or summary state (`FieldDataType`).
    pub dtype: T,
    /// Whether the column may contain NULL. PromQL value columns are never NULL.
    pub nullable: bool,
    /// Table or alias the column comes from (`t` in `t.col`), so two columns
    /// with the same name from a join stay apart. `None` for PromQL labels and
    /// unqualified columns.
    pub table: Option<String>,
}
impl Field<FieldDataType> {
    /// An unqualified field holding a plain value of type `dtype`.
    pub fn plain(name: impl Into<String>, dtype: DataType, nullable: bool) -> Self;
    /// The value type if the field is plain; `None` for summary state.
    pub fn plain_dtype(&self) -> Option<&DataType>;
    /// Whether the field holds a plain value (not summary state).
    pub fn is_plain(&self) -> bool;
}

/// A column's type: a plain value, or summary state of one family (§3).
pub enum FieldDataType {
    /// A readable value of this type.
    Plain(DataType),
    /// Exact accumulator state: which accumulator, and its parameters.
    ExactAggregate(ExactKind, ExactParams),
    /// Sketch state: the chosen sketch (category, algorithm, parameters), and
    /// whether each group has its own sketch or all groups share one.
    Sketch(SketchKind, GroupingStrategy),
    /// Sample state: the sampling method, and its parameters.
    Sample(SamplingKind, SamplingParams),
    /// Wavelet state: the transform, and its parameters.
    Wavelet(WaveletKind, WaveletParams),
    /// Statistical-model state: the model kind, and its parameters.
    StatModel(StatModelKind, StatModelParams),
}

/// Types of plain values.
pub enum DataType {
    Null,       // only NULL values
    Int64,      // 64-bit integer
    Float64,    // 64-bit float
    Utf8,       // string
    Bool,       // boolean
    Timestamp,  // point in time
    Interval,   // a duration; used for literals, never as a column type
    Date,       // calendar date without time of day
    /// A list. `element`: name, type and nullability of each element.
    List { element: Box<Field<DataType>> },
    /// A record. `fields`: its named fields, in order.
    Struct { fields: Vec<Field<DataType>> },
    /// A SQL map. `key`: key type (keys are never NULL). `value`: value type.
    /// `value_nullable`: whether values may be NULL.
    Map { key: Box<DataType>, value: Box<DataType>, value_nullable: bool },
}
```

### 6.4 Summary-family parameters (`crates/types/src/post_asap/sketch.rs`)

```rust
/// Which exact accumulator. None of them has parameters, so `ExactParams`
/// mirrors `ExactKind` one to one.
pub enum ExactKind   { Sum, Count, Min, Max, Increase, Rate, IRate }
pub enum ExactParams { Sum, Count, Min, Max, Increase, Rate, IRate }

/// A chosen sketch. Built only through `new`, so the three fields always agree.
pub struct SketchKind {
    /// Which aggregation intents the sketch answers: `Quantile` (quantiles),
    /// `Cardinality` (distinct counts), `Frequency` (item counts), `TopK`
    /// (heavy hitters), or `Universal` (frequency moments such as L2 and
    /// entropy, plus counts, distinct counts and top-k). Derived from `algorithm`.
    category: SketchCategory,
    /// Which sketch algorithm.
    algorithm: SketchAlgorithm,
    /// That algorithm's size parameters; must match `algorithm`.
    params: SketchParams,
}
impl SketchKind {
    /// The only constructor. Derives the category from the algorithm, and
    /// panics if `params` belong to a different algorithm.
    pub fn new(algorithm: SketchAlgorithm, params: SketchParams) -> Self;
    /// Which aggregation intents the sketch answers (see the `category` field).
    pub fn category(&self) -> SketchCategory;
    /// The sketch algorithm.
    pub fn algorithm(&self) -> &SketchAlgorithm;
    /// The algorithm's size parameters.
    pub fn params(&self) -> &SketchParams;
}
pub enum SketchCategory { Universal, Quantile, Cardinality, Frequency, TopK }
// Universal: UnivMon | Quantile: Kll, DDSketch | Cardinality: Hll, Theta, Kmv
// Frequency: Cms, CountSketch | TopK: CmsWithHeap, CountSketchWithHeap
pub enum SketchAlgorithm { UnivMon, Kll, Cms, Hll, DDSketch, CmsWithHeap, Kmv, Theta, CountSketch, CountSketchWithHeap }

/// Size parameters of each algorithm. Larger values: more memory, less error.
pub enum SketchParams {
    /// `layers`: number of sampling levels. Each level has a Count Sketch of
    /// `sketch_rows` hash rows × `sketch_cols` counters, and a heap of the
    /// `heap_size` heaviest items.
    UnivMon { heap_size: u32, sketch_rows: u32, sketch_cols: u32, layers: u8 },
    /// `k`: compactor capacity; error shrinks roughly as 1/k.
    Kll { k: u32 },
    /// `width`: counters per row. `depth`: number of hash rows.
    Cms { width: u32, depth: u32 },
    /// `precision`: log2 of the number of registers.
    Hll { precision: u8 },
    /// `alpha`: relative error of each quantile.
    DDSketch { alpha: f64 },
    /// Count-Min `width` × `depth`, plus a heap of the `heap_size` heaviest items.
    CmsWithHeap { width: u32, depth: u32, heap_size: u32 },
    /// `k`: number of smallest hash values kept.
    Kmv { k: u32 },
    /// `k`: number of hash values kept (nominal entries).
    Theta { k: u32 },
    /// `width`: counters per row. `depth`: number of hash rows.
    CountSketch { width: u32, depth: u32 },
    /// Count Sketch `width` × `depth`, plus a heap of the `heap_size` heaviest items.
    CountSketchWithHeap { width: u32, depth: u32, heap_size: u32 },
}

/// How a grouped sketch is laid out across the `by` groups. Independent of the family.
pub enum GroupingStrategy {
    /// One separate sketch per group. The default.
    PerSubpopulationInstance,
    /// One shared structure for all groups (Hydra). `kind`: which shared
    /// structure. `params`: its sizes.
    SharedMultiSubpopulation { kind: HydraKind, params: HydraParams },
}
pub enum HydraKind { HydraKll /* experimental, no error bound */, HydraCms, HydraCountSketch }
/// Sizes of the shared structure.
pub enum HydraParams {
    /// `k`: KLL `k` each group sees. `shared_buckets`: size of the one structure
    /// shared by all groups.
    HydraKll { k: u32, shared_buckets: u32 },
    /// `width` × `depth`: the sketch each group sees. `shared_rows` ×
    /// `shared_columns`: the one physical grid all groups hash into.
    HydraCms { width: u32, depth: u32, shared_rows: u32, shared_columns: u32 },
    /// Same fields as `HydraCms`, for Count Sketch.
    HydraCountSketch { width: u32, depth: u32, shared_rows: u32, shared_columns: u32 },
}
/// The shared (Hydra) version of an algorithm, if one with a known error
/// bound exists: `Cms` → `HydraCms`, `CountSketch` → `HydraCountSketch`;
/// `None` for all others.
pub fn hydra_kind_for(a: &SketchAlgorithm) -> Option<HydraKind>;

/// `size`: number of rows kept in the reservoir.
pub enum SamplingKind  { Reservoir }    pub enum SamplingParams  { Reservoir { size: u32 } }
/// `coefficients`: number of wavelet coefficients kept.
pub enum WaveletKind   { Haar }         pub enum WaveletParams   { Haar { coefficients: u32 } }
/// `family`: name of the parametric distribution, e.g. a normal distribution.
pub enum StatModelKind { Parametric }   pub enum StatModelParams { Parametric { family: String } }
```

### 6.5 Update input and readouts (`post_asap/sketch.rs`, `post_asap/maintained_population.rs`)

This part of the code answers two questions about summary state:

1. **Update input** (`SummaryUpdate`): when a `SummaryAgg` reads one input row, what does it add to the state? It adds a **weight**, optionally under an **item** key.
2. **Readouts**: once the state is built, what can be asked of it? `SketchStatistic` is what `SummaryEstimate` asks a sketch (§5.2). `MaintainedPopulation` and `PopulationStatistic` are the exact-population counterpart, used by `MaintainPopulation` and `EvaluatePopulation` (§5.4, §5.5).

**Update input examples** (as the planner builds them in `asap-aware-mapping`):

| Query | Sketch | `item` | `weight` | `weight_domain` |
|---|---|---|---|---|
| p99 of `latency` | KLL | none: KLL has no keys | `Column(latency)`: the value itself | `UnknownOrSigned` |
| how often each `endpoint` occurs | Count-Min | `Column(endpoint)` | `Constant(1.0)`: each row counts once | `NonNegative(UnitCount)` |
| `topk(5, rate(http_requests_total[5m]))` | Count-Min with heap | `Tuple(every column except value and the group columns)`: one item per series, including its timestamp | `Column(value)`: the rate | `NonNegative(ResetAwareCounterDerivative)` |

`weight_domain` matters because some sketches (e.g. Count-Min) are only accurate when weights are never negative. The planner records why a weight is non-negative; if it cannot prove it, the weight counts as possibly negative.

**Readout examples:** `Quantile { q: 0.99 }` reads p99 from a KLL; `PointCount { key: endpoint, value: Some("checkout") }` reads how often `checkout` occurred from a Count-Min; `TopK { k: 5 }` reads the 5 heaviest items.

```rust
/// What one input row adds to the state.
pub struct SummaryUpdate {
    /// The key the row is counted under, for keyed families (e.g. the item
    /// in a Count-Min or HLL). `None` for unkeyed families such as KLL.
    pub item: Option<SummaryInputExpr>,
    /// The value added to the state: the observed value for a KLL, or the
    /// count to add for a Count-Min (often `Constant(1.0)`).
    pub weight: SummaryInputExpr,
    /// Whether `weight` is proven never negative. Some algorithms need that.
    /// Missing proof is never treated as non-negative.
    pub weight_domain: WeightDomain,
}
impl SummaryUpdate {
    /// Add the value of column `c` for each row: no item, and the weight is
    /// not known to be non-negative.
    pub fn column(c: ColumnRef) -> Self;
}
pub enum WeightDomain {
    /// Nothing is known; the weight may be negative. The default.
    UnknownOrSigned,
    /// The weight is never negative. `proof`: why.
    NonNegative { proof: NonNegativeWeightProof },
}
pub enum NonNegativeWeightProof {
    UnitCount,                     // every row adds 1
    ResetAwareCounterDerivative,   // PromQL increase/rate over counters with reset correction
}
/// An expression that computes an item or a weight from the input row.
pub enum SummaryInputExpr {
    Constant(f64),                    // the same number for every row
    Column(ColumnRef),                // the value of one column
    Tuple(Vec<SummaryInputExpr>),     // several values combined into one item
    EntityIdentity(EntityIdentity),   // the identity of the row's series
}
pub enum EntityIdentity {
    /// A PromQL series identified by its labels. `excluding`: labels left out.
    PromqlLabelSet { excluding: Vec<ColumnRef> },
}

/// What `SummaryEstimate` reads out of a sketch.
pub enum SketchStatistic {
    FrequencyL2,        // sqrt of the sum of squared item frequencies
    FrequencyEntropy,   // entropy of the item frequencies, in bits
    /// `q`: the quantile rank, in (0, 1].
    Quantile { q: f64 },
    /// An estimated count. `key`: the column being counted. `value`: the item
    /// to look up (e.g. `"checkout"`), or `None` for the total count.
    PointCount { key: ColumnRef, value: Option<String> },
    Cardinality,        // number of distinct items
    /// `k`: how many top items to return.
    TopK { k: usize },
}

/// What `EvaluatePopulation` computes from a maintained population.
pub enum PopulationStatistic {
    Quantile { q: f64 },   // `q`: the quantile rank
    TopK { k: usize },     // `k`: how many top rows to return
    Sum, Count, Average,
}
/// A population kept in full (§5.4).
pub struct MaintainedPopulation<N = QueryExpr> {
    /// Which rows or series the population contains.
    pub input: PopulationInput<N>,
    /// The largest `k` a `TopK` read may ask for.
    pub max_k: usize,
    /// Whether `Quantile` reads are supported.
    pub quantiles: bool,
}
pub enum PopulationInput<N = QueryExpr> {
    /// The current value of each PromQL series (see `CurrentSeriesInput`).
    CurrentSeries(CurrentSeriesInput),
    /// Rows of a table. `input`: the table `Scan` node. `value_column`: the
    /// column whose values are kept. `grouping`: the `by` columns.
    Rows { input: Rc<N>, value_column: usize, grouping: GroupKeys },
}
pub struct CurrentSeriesInput {
    pub metric: String,                      // metric name
    pub matchers: Vec<CurrentSeriesMatcher>, // label matchers, e.g. job="api"
    pub grouping: Vec<String>,               // labels to group by
    pub without: bool,                       // true: group by all labels except `grouping`
    pub lookback_ms: u64,                    // how long a series stays current without new samples
}
```

A finalized value's accuracy statement is `ResultGuarantee` (`post_asap/guarantee.rs`): `metric` (which error is measured, e.g. rank error), `bound` (the error bound), `failure_probability` (the chance the bound does not hold) and `provenance` (which estimates the bound came from). It is attached to readout and finalized nodes, and to `MaintainPopulation` nodes (whose population is exact); never to sketch or accumulator state.

### 6.6 Summary coverage (`crates/types/src/ir/summary_coverage.rs`)

The code for §4, as of #646. On `main` today `SummaryCoverage` is still the older declared form (`source` plus `regions`).

```rust
pub struct SummaryCoverage {
    /// What the state computes: the `SummaryAgg` and its sub-DAG, with the
    /// conditions that went into `selection` taken out (§4.2.2).
    pub definition: Rc<OperatorNode>,
    /// Which rows went in: a union of boxes. A row is in the state if it is
    /// in at least one box.
    pub selection: Vec<SelectionBox>,
}

/// One box: a row is in it when it meets every constraint.
pub struct SelectionBox {
    /// One constraint per restricted column. A column not in the map is
    /// unrestricted.
    pub columns: BTreeMap<ColumnIdentity, Constraint>,
    /// The time window relative to the evaluation time, in ms, e.g.
    /// `(Excluded(-120000), Included(-60000))` for `(−2m, −1m]`.
    /// `None`: no time restriction.
    pub relative_time: Option<(Bound<i64>, Bound<i64>)>,
}

/// Names a column across nodes, independent of its position.
pub struct ColumnIdentity {
    /// The table or alias the column comes from; `None` if unqualified.
    pub table: Option<String>,
    /// The column name.
    pub name: String,
}

/// A constraint on one column.
pub enum Constraint {
    /// The value is one of these.
    In(Vec<ScalarValue>),
    /// The value is none of these.
    NotIn(Vec<ScalarValue>),
    /// The value lies between `lower` and `upper`. Each end is `Included`,
    /// `Excluded` or `Unbounded`.
    Interval { lower: Bound<ScalarValue>, upper: Bound<ScalarValue> },
    // HashPartition { columns, of, index }: added with its first producer.
}

impl SummaryCoverage {
    /// Computes the coverage of a summary node from its sub-DAG (§4.2.2).
    /// Errors: `NotSummary` for a node that is not a `SummaryAgg` or
    /// `SummaryMerge`, or a merge input that has no coverage; for a merge, `EmptyMerge` (no inputs),
    /// `DefinitionMismatch` (inputs compute different things) or
    /// `PossibleOverlap` (inputs may share rows).
    pub fn derive(node: &OperatorNode) -> Result<Self, CoverageError>;
}
```

- `OperatorNode::new` and `validate_structure` reject an invalid `SummaryMerge` (different definitions, or selections that may overlap; #592 relaxes the overlap check per family). The derived coverage is cached on the node.
- A `SummaryAgg` always has coverage: what cannot go into `selection` stays in `definition`.
