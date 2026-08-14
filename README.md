# ASAPController — design

ASAPController is a system to map a query workload to an ASAP plan. A query workload is a batch of queries or a set of repeating queries, in any query language like PromQL or SQL. An ASAP plan is a query plan (like in databases) that uses ASAP primitives like sketches, exact summaries, wavelets, etc.

ASAPController does this mapping in 2 steps:
(1) normalizing query workloads from different query languages into a common intermediate representation (IR)
(2) mapping the IR to an ASAP plan

Separating these 2 steps is helpful to decouple concerns and for extensibility.
Step 1 **interprets** the query workload semantics and **normalizes** them into our own IR. Adding a new query language or dialect can be done by extending step 1 and not touching step 2.
Step 2 **maps** query workload semantics to ASAP primitives. Adding a new ASAP primitive can be done by extending step 2 and not touching step 1.

1. **Interpretation** — understand a language-specific query and construct semantic intent.
2. **Intent canonicalization** — normalize equivalent queries from different languages into a common IR
3. **Mapping intents to ASAP primitives** — decide whether and how each intent can be answered by a summary,
   and select/size the corresponding summary family.

## Glossary

Let us define a few terms.

### Query language

E.g. PromQL, Clickhouse dialect of SQL, Datafusion dialect of SQL, etc.

### Query intent

The semantics of what the query wants to do. Multiple different query strings (in the same language or different) can have the same query intent.

See examples below. All queries in the same example share the same query intent.
In each example, the `orders` table has columns `time`, `price`, `city`, and `category`.

#### Example 1

```SQL
SELECT SUM(price)
FROM orders
WHERE time BETWEEN NOW() and NOW() - 1m
GROUP BY city
```

and

```SQL
WITH intermediate_table AS (
    SELECT SUM(price)
    FROM orders
    WHERE time BETWEEN NOW() and NOW() - 1m
    GROUP BY city, category
)
SELECT SUM(price)
FROM intermediate_table
GROUP BY city
```

#### Example 2

```SQL
SELECT SUM(price)
FROM orders
WHERE time BETWEEN NOW() and NOW() - 1m
GROUP BY city
```

and

```promql
sum by (cpu) (sum_over_time(orders[1m]))
```

### Query workload

A set of queries that are to be executed. For now, this is either a batch of queries executed on data at rest, or a set of repeating queries executed on recently ingested data.

### Pre-ASAP IR

A common IR that different query workloads in different languages are normalized to. has **no notion** of ASAP primitives or summaries. The purpose of this IR is to simply have a common representation for diverse query workloads and languages.

### Post-ASAP IR

An IR that includes of ASAP primitives, apart from the usual relational and time-series operators.

### Plan

A DAG constructed in either the pre-ASAP IR or post-ASAP IR. Pre-ASAP plan represents the original exact intent of the input query workload. Post-ASAP plan represents the same intent using ASAP primitives.

## Scope

As of Aug 13, 2026, ASAPController will be scoped to:
- generating a set of candidate ASAP plans, not choosing the optimal one between them
- not caring about CTSA stages i.e. whether a part of a plan is executed at the collector or at the analytics stage
- not caring about assignment of physical resources, like CPU threads and memory, to nodes in the ASAP plan i.e. ASAPController is NOT doing any phyiscal query planning (ref: database term)
- // TODO: add scope on what "IR to ASAP plan logic" we support right now

---

## High-level workflow

The pipeline is:

```text
query workload
    │
    │ parse
    ▼
pre-ASAP plan
    │
    │ canonicalize
    ▼
canonical pre-ASAP plan
    │
    │ ASAP-aware mapping
    ▼
post-ASAP plan
```

# Example: Equivalent SQL and PromQL have the same pre-ASAP plan

## SQL

```sql
SELECT service, COUNT(*)
FROM metrics
WHERE region = 'us-east'
GROUP BY service
ORDER BY COUNT(*) DESC
LIMIT 10;
```

## PromQL

```promql
topk(
  10,
  count by (service) (
    {region="us-east"}
  )
)
```

## Unified intent

```text
TopK(
    k = 10,
    key = FieldRef("service"),
    measure = Count,
    input = Filter(
        predicate = region = "us-east",
        input = Scan("metrics")
    )
)
```

The two languages may use very different syntax and data models, but the summary-relevant
semantic intent is the same.

# End-to-end example

Consider:

```sql
SELECT service, COUNT(*)
FROM metrics
WHERE region = 'us-east'
GROUP BY service
ORDER BY COUNT(*) DESC
LIMIT 5;
```

## parse + canonicalize

```text
TopK(
    k = 5,
    key = FieldRef("service"),
    measure = Count,
    input = Filter(
        predicate = region = "us-east",
        input = Scan("metrics")
    )
)
```

Equivalent PromQL converges to the same structure.

## ASAP-aware mapping

```text
TopK(Count, service, 5)
        ↓
SpaceSaving(k=5)
```

The filter and input domain are also considered when deciding whether the chosen summary
can answer the request directly or whether additional filtering / partitioning information
is required.

// MS: At this point, we need a concrete example of 2 query workloads (one SQL, one PromQL), their pre-ASAP IRs, and post-ASAP IRs

// MS: After that, the goal should be to clearly write what the pre-ASAP and post-ASAP IRs are. The list of nodes

# Dev tools

`crates/lower` ships debugging binaries and examples for poking at the lowering pipeline. Binaries (`cargo run -p asap-lower --bin <name>`):

- **`show_ir`** — prints pre-ASAP IRs for ad-hoc SQL/PromQL queries from a file or stdin, `sql>`/`promql>` prefixed
- **`dag_export`** — dumps pre-ASAP IRs for given `--sql`/`--promql` queries for
  [`tools/dag-viewer`](tools/dag-viewer/index.html), an interactive DAG viewer
  (see [`tools/dag-viewer/RUNNING.md`](tools/dag-viewer/RUNNING.md) for
  end-to-end setup, including running it over a remote tunnel).
- **`variant_coverage`** — parses and canonicalizes every query corpus in the repo to pre-ASAP IR and reports which `QueryExpr` variants get exercised.

Examples (`cargo run -p asap-lower --example <name>`):

- **`topk_ir`** — prints pre-ASAP IR for a hardcoded set of topk-shaped SQL/PromQL queries
- **`canonical_examples`** — prints pre-ASAP IR for one canonical query per `QueryExpr` variant, and custom join/set-op/distinct/CTE probes, to eyeball their shape.

# Design principles

## 1. Normalize semantics, not syntax

The canonical algebra should represent query intent rather than reproduce the source
language's syntax tree. Canonicalization should make semantically equivalent computations structurally identical so
that reusable sub-computations can be recognized and shared.

## 2. Make summary-relevant intents explicit

Operations such as `Quantile`, `DistinctCount`, and `TopK` deserve semantic representation
because they have distinct summary mappings.

## 3. Avoid adding nodes prematurely

Do not add a node merely because SQL has an operator with that name. Add a node when it carries
semantic information that matters downstream.

# Open questions

1. **Time semantics:** Should `TimeWindow` be an explicit node, or should time restriction be
   represented as a specialized predicate?
2. **Grouping semantics:** Should grouping remain embedded in `Aggregate`, or should grouping
   become a reusable relational dimension node?
3. **Expression semantics:** Which arithmetic or derived expressions need dedicated semantic
   nodes because they materially affect summary selection?
4. **Approximation contracts:** Should accuracy/error requirements be fields on the intent,
   the workload, or the measure itself?
5. **Summary composability:** How should nested intents describe summaries that can be merged,
   transformed, or reused across queries?
