# Labels Once per Series

**Status:** Proposal, for review before any code.

**Audience:** Planner designers and implementers of Stage 0/Pass 2 (memo
groups of per-series states), the cost model (`asap-cost`) and the
executor's stored views (`asap-executor`, `DataStore`).

## Example

The dashboard at 10,000 series
(`tools/dag-viewer/workloads/typical-dashboard-10k/workload.json`) reads
`data_total` in four queries:

| Query | Labels its scan carries |
| --- | --- |
| `sum by (job) (rate(data_total[5m]))`, `topk(5, sum by (job) (rate(data_total[5m])))` | `job` |
| `sum by (region) (increase(data_total[1h]))` | `region` |
| `sum by (service) (rate(data_total[1h]))` | `service` |
| `topk(10, sum by (service, job) (increase(data_total[15m])))` | `job`, `service` |

Every one keeps a per-series counter state of `data_total`, so every one
can read the same tumbling-window chain. But the front end scans the metric
with only the labels the query reads, and a stored window row carries
those labels beside its state. The four scans are four memo groups, and so
are the counter chains over them: 4 chains where 1 would do. The same holds
for the sum, count, max and DDSketch chains of `data`. In all, the dashboard
keeps 11 narrow chains (and 27 wider levels over them) where 5 states would
do.

A chain over the scan of every label could serve all four queries. Today
it can never replace the narrow ones exactly:

- each of its windows stores `job`, `region` and `service`: 16 bytes per
  label per series per window more than a narrow chain;
- each reader reads it through a `Project` back to its own labels, an
  operator with its own cost per row.

So it is dearer than any one narrow chain on memory and on query work.
A plan that needs only one of the narrow chains pays more with it.

A series' labels are a function of its series identity, which every stored
row already carries (64 bytes per row). This proposal stores no label
beside a state: a window row is its window end, its series identity and its
state. A read names the labels it needs, and the store derives them from the
identity. Then the four chains are one memo group by construction, and no
plan pays more for sharing it.

## 1. Components

### 1.1 Today

- A per-series state over a PromQL metric scans the metric with
  `schema = (ts, value, <labels the query reads>, $promql_series_identity)`.
- A chain's stored rows are `(window_end, keys, state)`
  (`DataStore::read_materialized_view`), where `keys` are the scan's labels
  and the series identity.
- Cost: each stored row is priced at
  `state_row_bytes_beside_state(schema)`, which is 8 for the window time,
  16 per label and 64 for the series identity (`SERIES_IDENTITY_BYTES`),
  plus the state's bytes. A `Project` or `Filter` over states prices each
  row at the state and its own columns, plus its own measured cost per row
  (`lookup_at_scale` with the plain-row entry).

### 1.2 Proposed

- **Series-keyed per-series state.** A per-series state of a metric (a
  `SummaryAgg` with `Reduction::PerEntity` over a predicate-free scan)
  scans the metric with `(ts, value, $promql_series_identity)`, whatever
  labels its query reads. Its rows are `(window_end, identity, state)` for
  a chain, and `(identity, state)` otherwise.
- **Labels of a read.** The first operator above the state that needs
  labels gets them from a label read. It is a `Project` whose items are
  `ScalarExpr::SeriesLabel { identity, label }`: the
  label's value in the series identity, or null if the series lacks the
  label, as PromQL reads a missing label.
- **Label read folded into a view read.** A `Project` of only column
  references and `SeriesLabel` items, directly over a `TimeRange` over a
  view, is part of that view read. The read's schema lists the labels, and
  the store fills them from the identity. It is costed as the read, not as
  a separate operator (section 2.3).

### 1.3 The definitions touched

`crates/types/src/ir/scalar/mod.rs` (one variant added, the rest as
today):

```rust
pub enum ScalarExpr<C = Rc<OperatorNode>, Col = ColumnId> {
    Column(Col),
    Literal(ScalarValue),
    Negative { expr: Box<ScalarExpr<C, Col>>, semantics: ExprSemantics },
    Compare {
        left: Box<ScalarExpr<C, Col>>,
        op: CompareOpKind,
        right: Box<ScalarExpr<C, Col>>,
        semantics: ExprSemantics,
    },
    BoolAnd(Vec<ScalarExpr<C, Col>>),
    BoolOr(Vec<ScalarExpr<C, Col>>),
    Not(Box<ScalarExpr<C, Col>>),
    IsNull(Box<ScalarExpr<C, Col>>),
    IsNotNull(Box<ScalarExpr<C, Col>>),
    Cast { expr: Box<ScalarExpr<C, Col>>, to: DataType, try_cast: bool },
    InList { expr: Box<ScalarExpr<C, Col>>, list: Vec<ScalarExpr<C, Col>>, negated: bool },
    FunctionCall { name: String, args: Vec<ScalarExpr<C, Col>> },
    Arithmetic {
        op: ArithmeticOpKind,
        left: Box<ScalarExpr<C, Col>>,
        right: Box<ScalarExpr<C, Col>>,
        semantics: ExprSemantics,
    },
    Case {
        operand: Option<Box<ScalarExpr<C, Col>>>,
        branches: Vec<(ScalarExpr<C, Col>, ScalarExpr<C, Col>)>,
        else_expr: Option<Box<ScalarExpr<C, Col>>>,
    },
    CurrentTimestamp,
    EvalTimestamp,
    PromqlScalarFromVector(C),
    ScalarSubquery(C),
    Exists { subquery: C, negated: bool },
    InSubquery { expr: Box<ScalarExpr<C, Col>>, subquery: C, negated: bool },
    /// The value of label `label` in the PromQL series identity in column
    /// `identity`; null when the series has no such label.
    SeriesLabel { identity: Col, label: String },
}
```

`crates/executor/src/sources/data_store.rs`:

```rust
pub struct MaterializedViewRequest<'a> {
    /// `(start, end]` in milliseconds: the rows the read selects (as today).
    pub time_range_ms: (i64, i64),
    /// The slot's columns: the view node's schema, followed by the labels
    /// in `labels`.
    pub schema: &'a SchemaRef,
    /// The labels the read asks for beside each row, each filled from the
    /// row's series identity (null when the series lacks it).
    pub labels: &'a [String],
}
```

`crates/cost/src/node/state_bytes/mod.rs`, `state_row_bytes_beside_state`:
unchanged in form. A series-keyed state's schema has no label column, so
a stored row is `8 + 64 + state` bytes.

## 2. Algorithm

### 2.1 Front end and Stage 0

For each per-series state over a predicate-free metric scan, the scan's
schema keeps only `ts`, `value` and the series identity. The labels the
query reads are added by a `Project` of `SeriesLabel` items directly above
the state's readers that need them: the `Aggregate by (job)` above
`rate()`, the `TopK` above the sum, and so on. Hash-consing then makes the
four `data_total` scans one group (their schemas are equal), and the counter
state groups one group per range.

### 2.2 Pass 2 and Stage 2

Unchanged. Chains are built under state groups as today: one chain per
width under the one counter state group, read by all four queries, so no
widened scan and no `Project` candidate is ever offered.

Dashboard (expected): 11 narrow chains become 5 (counter 4 → 1, sum
2 → 1, count 2 → 1, max 2 → 1, DDSketch 2 → 1); wider levels 27 → about
15; covers and their merges halve.

### 2.3 Cost

- Stored row: `8 + 64 + state` bytes per window per series. Per counter
  window, `job`'s 16 bytes are gone from `sum by (job)`'s chain, and 32
  from `topk(10, sum by (service, job) ...)`'s.
- A label read folded into a view read costs, per row read, the measured
  cost of producing a label from an identity, `SeriesLabel` per label
  (calibrated as `ns_per_series_label`). It adds `16 × labels` bytes to
  the read's output rows. Not folded (over any other operator), it is a
  `Project` costed per row as today, with the same per-label cost.
- **Exactness of sharing.** Two queries whose per-series states differed
  only in carried labels now have one state group, built once. Any plan
  that kept both of the old chains now keeps one, at the same bytes per
  window and the same refresh work per row: a chain's update work does not
  depend on labels it no longer carries. Each reader pays its label read
  per row for its own labels, the same whichever reader shares the chain.
  So for every plan of the old memo there is a plan of the new one at no
  more of every cost, and no plan is offered a choice between a shared and
  a narrow chain. The replacement is not a rule; the narrow chains do not
  exist.
- **What it costs.** Each query reading a label pays the label read once
  per row per step at query time, where today it pays nothing (the label
  is stored). On the dashboard, `sum by (job)` over 10,000 series every
  15 s reads 10,000 labels per step. That is a measured quantity
  (`ns_per_series_label`) to add before this is costed.

### 2.4 Executor

- The kernels that build and seal per-series states key them by series
  identity only (they already carry it).
- `read_materialized_view` and `read_raw_data` fill `request.labels`
  from each row's identity (`decode_series_identity` in
  `physical_planner/promql_rows.rs`, or a per-store identity → labels
  cache).
- `SeriesLabel` in other expressions evaluates the same decoding.

## 3. Code interface

| Change | Where |
| --- | --- |
| `ScalarExpr::SeriesLabel { identity, label }` | `crates/types/src/ir/scalar/mod.rs`; evaluation in `crates/executor/src/expressions/` |
| Per-series states scan only `ts`, `value`, identity; labels added by a `Project` of `SeriesLabel` above them | PromQL lowering in `crates/frontend-promql/` |
| `MaterializedViewRequest::labels`, `RawDataRequest::labels` | `crates/executor/src/sources/data_store.rs` and every `DataStore` |
| A label `Project` over a view read folded into the read | `crates/executor/src/physical_planner/mod.rs` (slot schema) and `crates/cost/src/node/usage/mod.rs` (read priced with its labels) |
| `ns_per_series_label` (measured) | `crates/cost/src/measured.rs`, calibration files in `tools/dag-viewer/cost-inputs/` |

## 4. Decisions and open questions

- **Open:** whether labels a query only groups by (never outputs) could be
  read from the identity inside the aggregate itself, saving the label read.
- **Open:** a per-store identity → labels cache, against decoding each
  identity per read.
- **Open:** SQL tables are unaffected (no series identity); this applies to
  PromQL metrics only.

## 5. Test plan (failing tests first)

| # | Test | Checks | Fails today because |
| --- | --- | --- | --- |
| L1 | `per_series_states_over_different_labels_are_one_group` (stage0) | the four `data_total` scans of the example are one memo group, and so are their counter states per range | four groups |
| L2 | `a_series_label_reads_its_identity` (executor) | `SeriesLabel` returns each label of an encoded identity, null for a missing label, as PromQL does | no such expression |
| L3 | `a_view_read_fills_the_labels_it_asks_for` (executor) | a chain's read with `labels = ["job"]` returns each window row with its series' `job`, byte for byte equal to today's stored `job` | no such field |
| L4 | `sharing_a_series_keyed_chain_answers_as_before` (integration) | the dashboard's answers with one counter chain equal today's answers with four, every step | one chain cannot serve four label sets |
| L5 | `a_series_keyed_chain_costs_no_more_than_a_narrow_one` (cost) | stored bytes per window and refresh work per row of the shared chain equal those of each old narrow chain, less the labels' bytes | labels priced per window |
| L6 | dashboard measurement | memo groups and chain groups fall (chains 11 → 5); cost model 1 and 3 planning time and plan cost against `main` | measurement, not a failing test |
