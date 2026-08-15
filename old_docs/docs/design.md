# ASAPPlanner — design

ASAPPlanner **plans** queries — turning a query string into a plan,
not running it. Two different questions come up when describing that
pipeline, and this document answers them separately rather than
conflating them into one "layer":

- **Representation** — "what abstraction level are we at right now?"
  A representation is a concrete shape data sits in at some point —
  a real type, or (for L4's physical IR) an interface boundary owned
  by the deployment. Representations don't run; they just *are*.
- **Pass** — "what processing does the code go through next?" A pass
  is the function/algorithm that turns one representation into
  another — or, for `canonicalize`, rewrites a representation into a
  normal form of *itself* (same type in, same type out — a within-
  representation pass, not a level change).

Four representations, connected by the passes below:

```
query string
    │  pass: interpret (each language's own third-party parser, then the
    │        front end's own translation directly into the canonical
    │        shape — dedicated topk() -> AggIntent::TopK, m[5m] ->
    │        TimeRange, WHERE/matchers -> Scan.predicates, etc. — using
    │        a named column-reference state)
    ▼
per-language AST/DAG, unresolved column references ← L1
    │  pass': resolve (bind names to schema positions, substitute them
    │        throughout the already-canonical-shaped tree)
    ▼
    │  pass'': canonicalize (cross-language / cross-phrasing pattern
    │        normalization — e.g. promoting a generic
    │        Limit{Sort{Aggregate([Count])}} shape to the heavy-hitter
    │        Aggregate{aggs:[TopK]} shape)
    ▼
unified canonical intent algebra  ← L2
    │  pass: implement (bind each intent to a summary family + size its
    │        parameters, or leave it PassThrough to raw data; considering the set of summary candidates mapping to aggregation intent, CSE)
    ▼
summary-bound IR  ← L3
    │  pass: physical-lower (deployment-supplied: assign each piece to
    │        a stage + executor, given the topology + deployment
    │        constraints — typically delegating the stage-level part to
    │        ASAPPlanner's own StageAllocator internally — then emit
    │        the deployment's own output format)
    ▼
physical IR, ready for runtime/execution  ← L4
  (a deployment's own Output type)
```



## L1 — query string → canonical intent algebra

Two passes, both internal to L1: interpret, then resolve+canonicalize.

**Pass 1 — interpret.** Each query language owns its own third-party
parser (`promql-parser`, `sqlparser`), producing that parser's own AST —
a different Rust type per language. The front end then
interprets that AST **directly into the canonical shape** (a dedicated
`topk()` call becomes `Aggregate{aggs:[AggIntent::TopK]}` directly, a
range selector `m[5m]` becomes `TimeRange` directly, `WHERE`/label
matchers become `Scan.predicates` directly) — the same type regardless
of source language, but with column references left **named**
(unresolved). Every language-construct-specific
structural decision belongs here, in the front end that has full
context on what it's looking at, at parse time.

**Pass 2 — resolve, then canonicalize.** One shared step, used by every
front end regardless of language: bind names to schema positions and
substitute them throughout, then run a cross-language normalization
pass so that semantically equivalent queries — from any supported
language, or differently-phrased within the same language — converge
on the identical canonical shape. `canonicalize` catches the cases
where a language has no dedicated syntax for an intent — e.g. SQL's
`ORDER BY count DESC LIMIT k` has no `topk()`-shaped AST node; a
pattern-detection pass over the already-assembled tree recognizes it as
the same `Aggregate{aggs:[TopK]}` shape PromQL's dedicated `topk()`
produces directly in pass 1.

```mermaid
flowchart LR
    P["Query string (language A)"] --> PA["language A's parser"] --> IA["interpret directly\ninto canonical shape\n(unresolved refs)"]
    S["Query string (language B)"] --> SA["language B's parser"] --> IB["interpret directly\ninto canonical shape\n(unresolved refs)"]
    IA --> RS["resolve: bind + substitute refs"]
    IB --> RS
    RS --> CZ["canonicalize"]
    CZ --> L1T["canonical intent algebra\n(same shape for equivalent\nqueries, any source language)"]
```

- Doc: [`l1-query-language.md`](./l1-query-language.md)

## L2 — intent algebra

**Job: define the canonical intent tree's vocabulary — the shape L1's
passes produce — expressed declaratively (e.g. "a quantile to this
accuracy," "the top-k by this ranking"), with implementation
strategy left to L3.** L2 is the vocabulary/rule set
L1's output conforms to, enforced by construction: every front end's
output runs through the same `resolve`+`canonicalize`.
- The result is a language- and deployment-independent canonical
  intent tree: what to compute, without committing to how. Deployment
  here refers to a physical execution context — e.g. parallelism and
  the lifecycle stage a computation runs at — a different sense of
  "deployment" than the Glossary's "Deployment model" entry below;
  see that entry for the distinction.
- Summary type and parameters are committed later, at L3.
- Row identity (which positions uniquely identify a row — see
  `Schema::unique_keys`) and cross-query sharing of common
  sub-computations are properties of this canonical form. Both depend
  on L1's `canonicalize` pass having already converged
  semantically-equivalent queries onto the same shape: only
  structurally-identical sub-trees can be recognized as the same
  reusable computation.

```mermaid
flowchart LR
    L1T["canonical intent tree\n(from L1)"] --> V["intent vocabulary\n+ design rules"]
    V --> L2G["governs what a valid\nL1 output looks like"]
```

- Doc: [`l2-intent-algebra.md`](./l2-intent-algebra.md)

## L3 — canonical intent algebra → summary-bound IR

**Pass: implement (planning-time only).** Decide, for each intent,
whether and how it is answered by a summary rather than by scanning raw
data — symbolically picking a summary family and its parameters, based
purely on the intent's own shape.
- An "implementation" is the concrete realization chosen for one piece
  of intent — e.g. a summary family and its parameters, an exact
  accumulator, or passing through to compute directly from raw data.
- Physical execution (where something runs, how parallel it is) is
  L4's decision alone.

```mermaid
flowchart LR
    L2N["canonical intent algebra"] --> IT["implement: bind intent to summary\n+ size parameters (or none: PassThrough)"] --> L3N["summary-bound IR"]
```

**Serving-time `execute` is a separate, runtime concern.** It walks an
already-produced `L3Node` against whatever is actually materialized
right now, to answer one query, producing a value consumed immediately.
Covered in its own "Serving-time execution" section below, kept
distinct since it runs at request time rather than at planning time.

- Doc: [`l3-summary-bound-ir.md`](./l3-summary-bound-ir.md)

## L4 — summary-bound IR → physical IR (physical runtime / data plane)

**One pass: `physical-lower`** (deployment-supplied:
`PhysicalPlanner::lower`) — assign each piece of the summary-bound IR
to a stage + executor, then emit the deployment's own output format.
ASAPPlanner defines the contract; the deployment performs this pass
and owns its output type.
- ASAPPlanner ships `StageAllocator` as shared, generic code for the
  stage-level half of that decision — given the summary-bound IR, a
  topology, and deployment constraints, it decides which physical
  *stage* each piece runs at, the same way for every deployment. A
  `lower()`
  implementation typically delegates to it internally as its own first
  step, keeping only per-executor fan-out and final emission as
  genuinely deployment-specific.
- Inputs: which physical stages exist and how they connect (topology),
  and the budgets/capabilities a given deployment offers (deployment
  constraints).

```mermaid
flowchart LR
    L3N["summary-bound IR"] --> PP
    subgraph PP["deployment's own physical-lower\n(implements a shared interface)"]
        SA["stage-allocate\n(ASAPPlanner-owned,\ntypically delegated to)"] --> FO["per-executor fan-out\n+ final emission"]
    end
    PP --> ART["physical IR\n(deployment-owned type)"]
```

- Doc: [`l4-physical-plan.md`](./l4-physical-plan.md)

## Serving-time execution

Everything above (L1-L4) is the **planning** pipeline: turning a query
string into a plan, one representation at a time. This section covers
what actually **answers** a query at request time, using whatever plan
L1-L4 already decided — a runtime evaluator, kept as its own interface
on purpose: a downstream deployment consumes it independently of how
the plan was produced.

**Job: walk an already-decided summary-bound IR against whatever is
actually materialized right now, and produce an answer.** Serving has
its own error cases, separate from planning-time `implement`'s,
**because** reality can diverge from what planning assumed: data might be missing,
several instances of the same summary might need merging, instances
might disagree on parameters.
- The structural rules — which nestings of a summary-bound IR are
  valid, what must agree for a merge to be legal — are shared and
  deployment-independent. Storage, summary math, and readout are
  entirely deployment-supplied.
- Planning and serving are two separate interfaces a downstream
  deployment implements against.

```mermaid
flowchart LR
    L3N["summary-bound IR\n(L3's output)"] --> EX["deployment-supplied executor"] --> V["answer"]
```

- Doc: [`l3-summary-bound-ir.md`](./l3-summary-bound-ir.md) — the
  "Serving-time" section covers this in full.

## Glossary

Terms that are project-specific, or that get conflated across layers.
Full detail lives in the layer doc linked from each entry; this is a
quick reference.

### Architecture terms

- **Representation** / **layer** — a concrete shape data sits in at
  some point in the pipeline (a real type, or, for L4's output, an
  interface boundary owned by the deployment). Answers "what
  abstraction level are we at." See the representation/pass table at
  the top of this document.
- **Pass** — the function/algorithm that turns one representation into
  another, or (uniquely, `canonicalize`) rewrites a representation into
  a normal form of itself. Answers "what happens next." Named right
  beneath a layer's heading — a "Job:" line where a layer runs exactly
  one, a short list where it bundles more than one; the heading itself
  names what the layer produces.
- **Front end** — The per-language component that elaborates a raw
  query string directly into the canonical shape, with unresolved
  column references (L1's `interpret` pass). One per language. See
  [`l1-query-language.md`](./l1-query-language.md).
- **Bind** — Name resolution: mapping a symbolic column reference to a
  schema position. Part of L1's `resolve` pass. See
  [`l1-query-language.md`](./l1-query-language.md).
- **Summary** — Umbrella term for whatever answers a piece of intent
  without re-scanning raw samples: an approximate sketch or an exact
  accumulator. See [`l3-summary-bound-ir.md`](./l3-summary-bound-ir.md).
- **Cost model** — The extension point that ranks candidate summary
  choices for a given intent. See
  [`l3-summary-bound-ir.md`](./l3-summary-bound-ir.md).
- **Deployment model** — A concrete bundle of (L3 summary choices) +
  (L4 topology) + configuration emission, packaged for one downstream
  deployment. See [`l4-physical-plan.md`](./l4-physical-plan.md). A
  different sense of "deployment" than L2's Job description uses (a
  physical execution context — parallelism, lifecycle stage) — this
  entry is the packaged product that implements L3/L4 for one
  downstream consumer.
- **Stage** / **Executor** / **Topology** / **Deployment constraints** —
  L4 concepts: a categorical placement tier, a concrete runtime
  instance, which stages exist and how they connect, and the
  per-deployment budgets/capabilities. See
  [`l4-physical-plan.md`](./l4-physical-plan.md).

### Workload

The normalized input meant to sit in front of every query entry point.

- **Query workload** — a batch of one-shot and/or repeating queries,
  all in the same query language, each with an optional accuracy
  and/or latency requirement.
- **Data characteristics** — workload-level facts (series/row count,
  sample rate, wire size, key distribution) used to size summaries
  without running the query.

## End-to-end example

`SELECT service, COUNT(*) FROM metrics GROUP BY service ORDER BY COUNT(*) DESC LIMIT 5`

```mermaid
flowchart TB
    Q["query string"]

    subgraph L1["L1"]
        direction TB
        A1["Limit{n:5}"] --> A2["Sort{by: cnt DESC}"]
        A2 --> A3["Aggregate{Reduce([service]), aggs:[Count]}"]
        A3 --> A4["Scan{metrics}"]
    end

    subgraph L2["L2"]
        direction TB
        B1["Aggregate{Reduce([0]), aggs:[TopK k:5]}"] --> B2["Aggregate{Reduce([0]), aggs:[Count]}"]
        B2 --> B3["Scan{metrics}"]
    end

    subgraph L3["L3"]
        direction TB
        C1["SummaryAgg{SpaceSaving, k:5}"] --> C2["Logical(Aggregate[Count])"]
        C2 --> C3["Logical(Scan{metrics})"]
    end

    subgraph L4["L4"]
        direction TB
        D1["stage: backend, executor: backend-3\nSpaceSaving sketch on metrics.service"]
    end

    Q -->|interpret| A1
    A1 -.->|resolve + canonicalize| B1
    B1 -->|implement| C1
    C1 -->|physical-lower| D1
```
