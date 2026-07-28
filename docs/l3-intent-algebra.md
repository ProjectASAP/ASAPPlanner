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
vocabulary — a representative slice, not the full list:

```rust
pub enum QueryExpr {
    Scan { source: Source, predicates: Vec<Predicate>, schema: Schema },
    Filter { pred: Predicate, child: Box<QueryExpr> },
    Project { cols: Vec<ProjectItem>, qualifier: Option<String>, child: Box<QueryExpr> },
    Aggregate {
        reduction: Reduction,
        aggs: Vec<AggIntent>,
        output_names: Vec<String>,
        having: Option<Predicate>,
        child: Box<QueryExpr>,
    },
    Window { .. }, Distinct { .. }, Merge { .. }, Join { .. }, SetOp { .. },
    Sort { .. }, Limit { .. }, LetBinding { .. }, Ref { .. },
    Subquery { .. }, TimeRange { .. }, TimeShift { .. }, WindowFunc { .. },
    BinaryOp { .. },
    // .. plus scalar/vector bridges and a small number of
    // language-specific extension points with no general equivalent yet
}
```

The two small types this doc's design principles turn on, in full:

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

`AggIntent` — again a representative slice:

```rust
pub enum AggIntent {
    Count { accuracy: AccuracyTarget },
    Sum { col: Option<ColumnId> },
    Quantile { col: Option<ColumnId>, q: f64, accuracy: AccuracyTarget },
    TopK { k: usize, accuracy: AccuracyTarget },
    Cardinality { col: Option<ColumnId>, accuracy: AccuracyTarget },
    Rate, Increase, Changes, Delta, IDelta, Deriv, Resets,  // counter-derivative family
    // .. native-histogram accessors, per-sample transforms, Extension
}
```

And the schema every edge in the tree carries:

```rust
pub struct Schema {
    pub columns: Vec<Column>,
    pub time_index: Option<ColumnId>,
    pub unique_keys: Vec<Vec<ColumnId>>,
    pub closed: bool,
}
```
