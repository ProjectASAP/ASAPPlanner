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

`SummaryAgg` describes the state-producing aggregate. `SummaryEstimate` turns
state into query values. Merge, join, subtract and delete nodes express summary
operations whose use depends on family capabilities and compatible inputs.
Exact accumulators can expose their result without a separate sketch readout.

`KeepPreAsap` retains exact source-language intent when work is not rewritten.
`ValueOperation`, `BinaryOp` and `RelationalJoin` compose row-producing work with
summary readouts. `CandidateTopK` separates membership proposals from the exact
values used to rank them; its completeness contract matters to correctness.

A `SummaryNode` carries schema and guarantee metadata alongside its expression.
State and query values have different contracts, and execution-data-state checks
constrain where their consumers can run. Representability alone does not promise
that a given runtime or physical cost model supports every node.

For node definitions and implementation obligations, read the
[Post-ASAP developer reference](../../develop_docs/post-asap-ir.md). The
[physical-plan boundary](../architecture/physical-plan-integration.md) explains
how a selected semantic DAG is bound to physical operators and evidence.
