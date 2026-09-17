# Post-ASAP IR

Post-ASAP IR represents a query using exact work and eligible ASAP summaries.
It preserves the required result semantics while recording summary algorithms,
parameters, grouping and guarantees. It is not a deployed physical plan.

## State production, readout and composition

A sketch-backed aggregate typically has this shape:

```text
exact source/filter work
    -> SummaryAgg: build the chosen summary state
    -> SummaryEstimate: read the requested statistic
    -> exact operations on the returned values, when needed
```

Exact accumulators can expose their result without a separate sketch readout.

## Complete node catalog

The current [SummaryExpr definition](../../../crates/types/src/post_asap/expr.rs)
contains the following nodes. This is the conceptual catalog; the
[developer reference](../../develop_docs/post-asap-ir.md) covers construction,
validation and export obligations.

### Summary state and readout

| Node | Meaning |
| --- | --- |
| `SummaryAgg` | Produce summary state from an input using the selected family, parameters, update semantics, reduction and grouping layout. |
| `SummaryEstimate` | Read a requested statistic from summary state and return query values. |
| `SummaryMerge` | Merge compatible summary states, such as partial states from different partitions; the family must support merging. |
| `SummarySubtract` | Subtract one summary state from another when the family supports that operation. |
| `SummaryDelete` | Delete a key from summary state when the representation supports deletion. |
| `SummaryJoin` | Combine summary states for join estimation; this differs from joining ordinary rows. |

### Exact work and composition

| Node | Meaning |
| --- | --- |
| `KeepPreAsap` | Retain an exact Pre-ASAP subtree when work is not rewritten. |
| `BinaryOp` | Combine independently planned operands while preserving the binary operation and its execution timing. |
| `ValueOperation` | Apply an operation to values, including aggregation, exact functions, population maintenance/readout, projection, filtering, sorting, limits or extensions, with explicit execution timing. |
| `RelationalJoin` | Join row-producing children using the specified join kind and predicate. |
| `CandidateTopK` | Propose candidate members and rank them by authoritative exact values; the completeness contract determines whether the membership result is certified or best effort. |

## Why there are no separate create and insert nodes

The earlier design sketch listed `SummaryCreate` and `SummaryInsert`. Neither
is a variant of the current `SummaryExpr`. `SummaryAgg` describes the
state-producing computation and its update input; it does not split state
initialization and each incoming-record update into separate logical nodes.

The [summary-maintenance lifecycle design](../proposals/asap-aware-mapping/workload-demand-and-summary-lifecycle.md)
describes when state is created, retained, shared, updated and retired. Direct
build versus incremental maintenance is a separate deployment choice, subject
to family capabilities and evidence. Physical binding and runtime execution
implement the actual build and update operations.

Creation and insertion behavior therefore remains part of the summary and
maintenance contracts. This is not a one-to-one rename of the old node names,
and does not imply that every family supports incremental updates. The other
summary operations listed above remain explicit IR nodes.

## Guarantees and implementation boundary

A `SummaryNode` carries schema and guarantee metadata alongside its expression.
State and query values have different contracts, and execution-data-state checks
constrain where their consumers can run. Representability alone does not promise
that a given runtime or physical cost model supports every node.

For node definitions and implementation obligations, read the
[Post-ASAP developer reference](../../develop_docs/post-asap-ir.md). The
[physical-plan boundary](../architecture/physical-plan-integration.md) explains
how a selected semantic DAG is bound to physical operators and evidence.
