# L3 — intent algebra

The language- and deployment-independent canonical form. Pure intent at
this layer: no summary types, no summary parameters, no physical
operator choice — which concrete strategy answers a given piece of
intent is entirely an L4 decision (see
[`l4-summary-bound-ir.md`](./l4-summary-bound-ir.md)). Data-model
agnostic by design: the same tree shape is meant to cover both
time-series-style sources and tabular sources, so a source's data model
is a property of its scan node, not a fork in the tree type.

## Design rules

1. **One canonical form per plan.** No two variants whose semantics
   decompose into each other — e.g. a window over an aggregate is the
   only windowed-aggregate shape; there is no separate combined node
   for it. This keeps later rule-matching unambiguous. The one
   deliberate exception: an operation that has a genuinely different
   computational strategy available (e.g. heavy-hitter top-k, servable
   by a dedicated streaming primitive) gets its own first-class intent
   rather than being expressed purely as a composition of more generic
   operators — but the generic composition (sort + limit) still exists
   for cases that don't have that special strategy available.
2. **Language-orthogonal, except where an operation is genuinely
   semantically distinct.** Two operations that look similar in some
   source language's surface syntax but aren't actually interchangeable
   (e.g. a cumulative-bucket interpolation vs. a genuinely
   re-summarizable quantile) must be represented as separate intents,
   discriminated by what they actually require to compute — never by
   which language's syntax happened to produce them.
3. **Intent at L3, summary at L4.** An intent like "a quantile to this
   accuracy" says what to compute; it never says which summary computes
   it.
4. **A DAG, not a tree.** A producer can have more than one consumer
   sharing its computed result — from explicit naming in the source
   query (e.g. a named sub-query), from a later layer's decision to
   reuse a shared computation across two different queries, or from
   physical structure (a pre-aggregate feeding several downstream
   consumers). The canonical form supports this by construction, via a
   binding/reference pair of node kinds — a tree-only representation
   would have to duplicate the producer per consumer and lose the
   sharing.

## The intent vocabulary

Base relations, filtering/projection, aggregation (grouping keys +
aggregate intents), deduplication, windowing (including time-range and
time-shift), set/join operators, arithmetic, ordering, named/referenced
sub-plans, analytic (partitioned) window functions, scalar/vector
bridges for languages that distinguish the two, and a small number of
language-specific extension points with no general equivalent yet.
Grouping keys are one shared shape wherever "operate per group" is
needed (aggregation, ordering-with-partition, analytic windows) — a
grouping can either name its keys directly or specify them by exclusion
(everything *except* a given set), with the exclusion form only
resolvable once a schema is known.

There is deliberately no separate node for "top-k" or "windowed
aggregate" as syntactic shapes — see design rule 1; both fold into
composition of the generic operators instead, except for the one
strategy-driven exception noted there.

**Why a scan's source is a variant, not a separate tree type per data
model.** Most operators (filter, aggregate, …) have identical semantics
regardless of whether the input is a time-series window or a table scan
— only the leaf differs. One tree with a polymorphic leaf lets
data-model-agnostic rules apply uniformly across every data model;
rules that do care about data-model specifics gate on that leaf's
declared kind. Summaries themselves are meant to be data-model-agnostic
by construction — a streaming summary ingests a stream of values
regardless of what produced that stream.

## The intent catalog

What to compute, not how. Roughly: data-model-agnostic reducers (count,
sum, min, max, average, standard deviation, variance, quantile, top-k,
cardinality — each optionally carrying an accuracy target where
approximation is possible), counter-aware streaming derivatives (rate,
increase — genuinely distinct from a plain windowed sum, since they
must account for counter resets, which is why they're their own
intents), further counter-derivative functions, native-histogram
accessors, per-sample transforms, and a generic extension point for
deployment-specific intents the core vocabulary doesn't know the shape
of.

**Why a streaming derivative is its own intent, not "sum with a
different name."** Because it isn't computable the same way — it
requires awareness of counter resets that a plain windowed reduction
doesn't need. Folding it into a generic reducer would either lose that
awareness or force every reducer to carry it.

**Why there's no separate "quantile over a window" intent.** It would
duplicate "quantile" composed with "window" — the window's shape (kind,
size, slide) is already fully captured by the surrounding window
operator, and the quantile computation itself doesn't care whether its
input came from a windowed time series or a grouped table. One intent,
composed with the generic window operator, covers every language's
version of this idiom.

## Why column identity is positional

Everywhere the canonical form names a column — a grouping key, a
unique key, a designated time column — it is a position into a schema,
not a string. A layer-2 name means nothing without knowing which
schema it resolves against and at what offset; a position is settled
once, at bind time, and by construction can't fail to resolve for any
later pass. This also makes the canonical tree self-describing: every
scan carries its own schema, so any sub-tree's output schema is
computable purely from its inputs, without external context. A named
alias for the column-identity type (rather than a bare integer) is
still kept, so code that touches it can express "this is a column
position" as a distinct kind of value, not just any number.

## Schema flow

Every edge in the canonical tree carries a schema: its columns, which
column (if any) is the designated time index, its unique keys, and
whether it's closed (a complete enumeration) or open (a runtime row may
carry more). The tree is locally type-checked: a node's output schema
is a pure function of its inputs' schemas and its own parameters,
verifiable without consulting the rest of the tree.

Three distinct kinds of schema-shaped information are easy to conflate
and shouldn't be: the schema flowing along the canonical tree's own
edges; the schema of the underlying data source itself (what tables or
metrics actually exist, consulted only during L1 → L2 name resolution);
and a catalog of what summaries exist and what they can serve
(consulted only from L4 onward). Each is read by a different layer, for
a different purpose.

## Unique keys and cross-query sharing

A unique key is a set of column positions that, together, identify a
row uniquely; a schema can carry more than one such key. Each operator
derives its output's unique keys from its inputs' — most operators
either pass keys through unchanged, or (for a grouping aggregation)
manufacture a fresh key from the grouping columns, since grouping by a
column set makes that set unique in the result. An operator whose
output can no longer guarantee row-level uniqueness (a projection, a
label rewrite, a union, a join) resets its unique keys to empty rather
than guess.

**What this is for.** When several queries are planned together, a
shared sub-computation can be computed once and reused by every
consumer that needs it — but only if the producer is guaranteed to emit
the *same* rows for every consumer. A provable unique key is what
licenses that guarantee; structural identity between two consumers'
requested sub-computations tells you they *want* the same producer, but
not that its output is *stable* across reads. Without a provable unique
key, sharing is unsound and each consumer must recompute independently
— which is why a source with no way to prove a key (an open,
usage-derived schema) can never participate in this kind of sharing,
while a source with a declared key from an external catalog can.

## Interface

The canonical `QueryExpr` is one Rust enum covering the whole relational
vocabulary, every variant:

```rust
pub enum QueryExpr {
    // ── leaves ────────────────────────────────────────────────────────
    Scan { source: Source, predicates: Vec<Predicate>, schema: Schema },
    Ref { name: BindingName },              // a LetBinding reference
    Scalar(f64),                            // a constant scalar literal
    EvalTime,                               // the query evaluation time as a scalar

    // ── scalar/vector bridges ────────────────────────────────────────
    VectorFromScalar(Box<QueryExpr>),       // promote a scalar to a label-less vector
    ScalarFromVector(Box<QueryExpr>),       // collapse a single-series vector to a scalar

    // ── per-row transforms ───────────────────────────────────────────
    Relabel { dst: String, value: L3Expr, child: Box<QueryExpr> },
    InfoJoin { selector: Vec<InfoMatcher>, child: Box<QueryExpr> },
    Sample { by: GroupKeys, kind: SampleKind, child: Box<QueryExpr> },

    // ── core relational ──────────────────────────────────────────────
    Filter { pred: Predicate, child: Box<QueryExpr> },
    Project { cols: Vec<ProjectItem>, qualifier: Option<String>, child: Box<QueryExpr> },
    Aggregate {
        reduction: Reduction,
        aggs: Vec<AggIntent>,
        output_names: Vec<String>,
        having: Option<Predicate>,
        child: Box<QueryExpr>,
    },

    // ── windowing, dedup, set composition ────────────────────────────
    Window { kind: WindowKind, size: Duration, slide: Option<Duration>, child: Box<QueryExpr> },
    Distinct { cols: Vec<ColumnId>, child: Box<QueryExpr> },
    Merge { children: Vec<QueryExpr> },     // exact, n-ary UNION ALL
    Join { kind: JoinKind, pred: Predicate, left: Box<QueryExpr>, right: Box<QueryExpr> },
    SetOp { kind: SetOpKind, all: bool, left: Box<QueryExpr>, right: Box<QueryExpr> },

    // ── ordering / limiting ──────────────────────────────────────────
    Sort { keys: Vec<SortKey>, partition_by: GroupKeys, child: Box<QueryExpr> },
    Limit { n: usize, offset: usize, child: Box<QueryExpr> },

    // ── sharing and temporal wrappers ────────────────────────────────
    LetBinding { name: BindingName, expr: Box<QueryExpr>, child: Box<QueryExpr> },
    Subquery { range: Duration, resolution: Option<Duration>, child: Box<QueryExpr> },
    TimeRange { range: Duration, child: Box<QueryExpr> },
    TimeShift { shift: TimeShift, child: Box<QueryExpr> },

    // ── SQL analytic window functions ────────────────────────────────
    WindowFunc {
        func: WindowFuncKind,
        args: Vec<L3Expr>,
        partition_by: GroupKeys,
        order_by: Vec<SortKey>,
        output_name: String,
        child: Box<QueryExpr>,
    },

    // ── arithmetic / comparison / boolean composition ────────────────
    BinaryOp { op: BinaryOpKind, lhs: Box<QueryExpr>, rhs: Box<QueryExpr>, vector_match: Option<VectorMatch> },
}
```

`reduction` is a field *on* the `Aggregate` variant itself — not a
separate node in the tree, and not something any other variant carries.
It answers a question only `Aggregate` ever needs to ask: is this node
collapsing rows at all, and if so, by which (possibly empty) key set —
or does it have no grouping concept to begin with. Making that an
explicit field, rather than something a consumer infers from whether a
key list happens to be empty, is deliberate: the two cases are easy to
conflate (both can present as "empty keys") but require opposite
handling downstream — see [`l4-summary-bound-ir.md`](./l4-summary-bound-ir.md#interface)'s
`SummaryExecutor::find_candidates` for where that distinction is
actually load-bearing.

The two small types that field's shape turns on, in full — neither is a
tree node either; both are plain data reachable only through
`Aggregate.reduction`, and `GroupKeys` only exists at all when
`reduction` is `Reduce` (it's meaningless for `PerEntity`, which is
exactly why it isn't a sibling field instead):

```rust
pub enum Reduction {
    Reduce(GroupKeys),
    PerEntity,
}

pub struct GroupKeys { /* private fields */ }
impl GroupKeys {
    pub fn by(keys: Vec<ColumnId>) -> Self;       // keep these columns
    pub fn without(keys: Vec<ColumnId>) -> Self;  // exclude these columns
    pub fn is_without(&self) -> bool;
    pub fn keys(&self) -> &[ColumnId];
}
```

`AggIntent` — the full vocabulary:

```rust
pub enum AggIntent {
    // data-model-agnostic reducers
    Count { accuracy: AccuracyTarget },
    Sum { col: Option<ColumnId> },
    Min { col: Option<ColumnId> },
    Max { col: Option<ColumnId> },
    Avg { col: Option<ColumnId> },
    StdDev { col: Option<ColumnId>, population: bool },
    Variance { col: Option<ColumnId>, population: bool },
    Quantile { col: Option<ColumnId>, q: f64, accuracy: AccuracyTarget },
    TopK { k: usize, accuracy: AccuracyTarget },
    Cardinality { col: Option<ColumnId>, accuracy: AccuracyTarget },

    // counter-aware streaming derivatives
    Rate,
    Increase,

    // further counter-derivative / range-vector functions
    Changes, Delta, IDelta, Deriv, Resets,
    PredictLinear { seconds: f64 },
    DoubleExpSmoothing { smoothing: f64, trend: f64 },
    LastOverTime, FirstOverTime, MadOverTime,
    TsOfMinOverTime, TsOfMaxOverTime, TsOfFirstOverTime, TsOfLastOverTime,

    // native-histogram accessors
    HistogramCount, HistogramSum, HistogramAvg, HistogramStdDev, HistogramStdVar,
    HistogramFraction { lower: f64, upper: f64 },
    HistogramQuantile { q: f64 },

    // per-sample transforms
    Math(MathFunc),
    TimeFn(TimeFunc),

    // presence
    Absent, AbsentOverTime, PresentOverTime,

    // extended aggregation operators — grouped by *sample value* rather
    // than reducing to a single number per group, so both need a schema
    // shape no other reducer in this list uses:
    //
    // - `Group`: emits a constant `1` per group ("does this group have any
    //   members at all"), independent of the input values — the output
    //   carries no information about the samples beyond their presence.
    //   Kept as its own intent rather than folded into `Sum`/`Count`
    //   because the value has nothing to do with what's being summed or
    //   counted.
    // - `CountValues { label }`: groups the input further by each
    //   distinct *sample value* (not just the usual grouping keys),
    //   counting how many samples land in each. Since the sample value
    //   itself becomes part of the output's identity, this is the one
    //   reducer whose output schema gains a new column (a synthesized
    //   label named `label`, holding the stringified value) rather than
    //   just a single retyped aggregate column.
    Group,
    CountValues { label: String },

    // deployment-specific escape hatch (see "Design rules" above) — core
    // treats this opaquely; the owning deployment defines and interprets
    // `payload` itself, keyed by its own `ext_kind` tag
    Extension { ext_kind: String, payload: serde_json::Value },
}
```

One example source expression per variant, PromQL unless noted (`v` stands in
for any instant/range vector selector):

| `AggIntent` | Example |
|---|---|
| `Count` | `count(up)` |
| `Sum` | `sum(rate(http_requests_total[5m]))`; SQL `SUM(bytes)` |
| `Min` | `min(cpu_temp)` |
| `Max` | `max(cpu_temp)` |
| `Avg` | `avg(cpu_usage)` |
| `StdDev` | `stddev(latency_ms)`; SQL `STDDEV(col)` |
| `Variance` | `stdvar(latency_ms)`; SQL `VARIANCE(col)` |
| `Quantile` | `quantile(0.99, latency_ms)`; SQL `approx_percentile_cont(col, 0.99)` |
| `TopK` | `topk(5, http_requests_total)` |
| `Cardinality` | SQL `COUNT(DISTINCT user_id)`; the PromQL analogue is `count(...)` over a metric whose underlying storage is a cardinality sketch, not a literal PromQL function call |
| `Rate` | `rate(http_requests_total[5m])` |
| `Increase` | `increase(http_requests_total[5m])` |
| `Changes` | `changes(v[5m])` |
| `Delta` | `delta(v[5m])` |
| `IDelta` | `idelta(v[5m])` |
| `Deriv` | `deriv(v[5m])` |
| `Resets` | `resets(v[5m])` |
| `PredictLinear` | `predict_linear(v[5m], 3600)` |
| `DoubleExpSmoothing` | `double_exponential_smoothing(v[5m], 0.5, 0.5)` |
| `HistogramCount` | `histogram_count(v)` |
| `HistogramSum` | `histogram_sum(v)` |
| `HistogramAvg` | `histogram_avg(v)` |
| `HistogramStdDev` | `histogram_stddev(v)` |
| `HistogramStdVar` | `histogram_stdvar(v)` |
| `HistogramFraction` | `histogram_fraction(0.1, 0.5, v)` |
| `HistogramQuantile` | `histogram_quantile(0.99, v)` |
| `Math` | `abs(v)`, `sqrt(v)`, `ceil(v)`, … (one `MathFunc` per PromQL math/trig builtin) |
| `TimeFn` | `hour()`, `day_of_week(v)`, … (one `TimeFunc` per PromQL calendar builtin) |
| `Absent` | `absent(v)` |
| `AbsentOverTime` | `absent_over_time(v[5m])` |
| `PresentOverTime` | `present_over_time(v[5m])` |
| `Group` | `group(v)` |
| `CountValues` | `count_values("version", v)` |
| `LastOverTime` | `last_over_time(v[5m])` |
| `FirstOverTime` | `first_over_time(v[5m])` |
| `MadOverTime` | `mad_over_time(v[5m])` |
| `TsOfMinOverTime` | `ts_of_min_over_time(v[5m])` |
| `TsOfMaxOverTime` | `ts_of_max_over_time(v[5m])` |
| `TsOfFirstOverTime` | `ts_of_first_over_time(v[5m])` |
| `TsOfLastOverTime` | `ts_of_last_over_time(v[5m])` |
| `Extension` | deployment-defined — e.g. a deployment-specific membership-test function no core language has a builtin for |

**Why the `*OverTime` reducers (`LastOverTime` … `TsOfLastOverTime`) each need
their own intent, rather than composing from `Sort`/`Limit` or another generic
node.** They all reduce a single series' *raw sample sequence inside a range
window* to one value — but nothing else in this vocabulary exposes that raw
sequence as rows a generic operator could sort or limit. `Sort`/`Limit`
operate on the relation `Aggregate` already produced (one row per series,
post-reduction); a `TimeRange` window's underlying samples are never
materialized as a queryable row set at this layer, only fed directly into
whichever `AggIntent` sits above it. So there is no composition available to
express "the timestamp of this window's minimum sample" from generic parts —
each of these is the only path that can reach into the window's raw stream
for its particular statistic, which is exactly the design rule 1 exception
(a genuinely different computational access pattern, not an ordinary
composition of existing operators).

And the schema every edge in the tree carries:

```rust
pub struct Schema {
    pub columns: Vec<Column>,
    pub time_index: Option<ColumnId>,
    pub unique_keys: Vec<Vec<ColumnId>>,
    pub closed: bool,
}
```

For example, `sum by (job) (http_requests_total)` binds to an `Aggregate`
whose input schema is `{ columns: [ts, value, job], time_index: Some(0),
unique_keys: [], closed: false }` (PromQL — open, since a metric's label
set is a superset the runtime may exceed) and whose *output* schema —
after `Reduction::by([job])` collapses every other row — is:

```rust
Schema {
    columns: vec![Column::new("job", DataType::Utf8, true), Column::new("sum", DataType::Float64, false)],
    time_index: None,       // a cross-series reduction collapses the time axis
    unique_keys: vec![vec![0]],  // grouping by job makes job unique in the output
    closed: true,           // by(...) enumerates its columns exactly — this is
                             // where an open input schema freezes to closed
}
```
