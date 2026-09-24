# DataFusion versus native ASAP execution

## Decision for #462

Implement and maintain all ASAP physical operators, summary operators and shared
DAG execution locally, following DataFusion's separation of plan contracts,
runtime, expressions and concrete operators. A DataFusion backend or hybrid
runtime is outside this PR's chosen direction.

This gives ASAP direct ownership of native summary-state edges and shared
execution across precompute and query engines. It also makes ASAP responsible
for operator correctness, resource control and future parallelism/spill.
It is an architectural choice, not a measured performance advantage.

## Key differences

| Aspect | Reuse DataFusion execution | Implement in ASAP |
| --- | --- | --- |
| Runtime overhead | Partition streams, dynamic dispatch and optional exchange/buffering; ordinary operators do not each spawn a task | Worker-local streams, producer queues, reader tracking, row/value dispatch and copies |
| Data representation | Standard physical edges carry Arrow RecordBatches; accumulators can hold native Rust sketches internally | Typed native batches can carry summary-state objects directly |
| Shared producers | Sharing a plan pointer does not ensure one execution; requires fusion, materialization or custom coordination | One producer per node per run, with independent consumer cursors |
| Summary extensions | Logical nodes, extension planners and physical/UDAF APIs exist; a core fork is not inherently required | ASAP owns interfaces, lowering and execution directly |
| Optimizations and algorithms | Existing relational operators and partition/spill infrastructure, subject to valid properties and state contracts | Local implementations; equivalent capabilities must be developed and maintained |
| Maintenance | Adaptation, semantic integration and upstream version changes | Algorithms, runtime contracts, regressions and feature development |

Neither runtime is inherently cheaper. Arrow batch clones share buffers, while
conversion from rows and sketch encoding may allocate. Native state edges avoid
encoding but retain queue, allocation and row-cloning costs. Specialized kernels
and algorithms can dominate framework overhead. No comparative benchmark has run.

## What DataFusion integration would require

**State transport.** Native sketches can live inside a UDAF accumulator.
`update_batch` need not serialize them; `state`/`merge_batch` export and consume
partial state. Across standard physical edges, use Arrow-compatible binary or
structured state, or keep sketches inside a fused operator until scalar readout.
Run-local handles require explicit lifetime, memory and transport restrictions.
Arrow extension metadata alone does not make arbitrary Rust objects portable.

**Sharing.** Expression CSE, shared plan identity and shared execution are distinct.
Multiple compatible quantiles can be fused into one KLL build and multi-readout;
independent downstream branches may need true fan-out. Custom shared execution
must handle per-run identity, slow/dropped consumers, errors, cancellation and
buffering. Asymmetric consumer polling can deadlock bounded broadcast queues;
ASAP's own runtime has the same scheduling obligation.

**Optimization and partitions.** Summary nodes must preserve population, grouping,
window coverage, parameters and approximation guarantees. A legal partial/final
merge requires compatible states and nonduplicated input coverage. Logical and
physical extension APIs provide hooks; they do not establish these semantics.
Reusing physical operators alone also does not automatically reuse logical
optimization over ASAP IR.

**Lifecycle.** Custom state must participate in memory accounting and cooperative
cancellation. Hybrid execution would additionally coordinate budgets, workers
and state ownership across runtimes. These are adapter responsibilities, not
proof that DataFusion core must change.

## What to borrow now

Borrow module boundaries and explicit contracts for schemas, boundedness,
emission, memory and cancellation. Keep ASAP's `SummaryExpr`, typed summary
states and shared DAG. #462 establishes those boundaries; partition parallelism,
spill and richer physical properties remain future native work.

Copying an algorithm is a larger commitment than following organization:
DataFusion joins, sorts and aggregates depend on Arrow kernels, expressions,
partition properties, memory reservations and spill infrastructure. Any port
requires adaptation, tests and ongoing maintenance; upstream fixes do not arrive
automatically.

Future performance comparisons should use identical sketch kernels, parameters,
inputs and worker counts. Measure setup separately from execution, including
latency, CPU, allocations, peak memory and encoded bytes. Useful workloads are
KLL multi-readout, asymmetric fan-out, grouped scans and memory-constrained
joins/sorts. Such measurements are not a prerequisite for the current decision.

## Source scope

Reviewed upstream at
[`e2ca7f3`](https://github.com/apache/datafusion/tree/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38)
on 2026-09-24. The SQL frontend uses DataFusion 43; upstream-main APIs below are
not a claim about that release. This PR adds no DataFusion execution dependency.

- [ExecutionPlan and physical properties](https://github.com/apache/datafusion/blob/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38/datafusion/physical-plan/src/execution_plan.rs)
- [Accumulator state/update/merge interface](https://github.com/apache/datafusion/blob/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38/datafusion/expr-common/src/accumulator.rs)
- [Logical extension contracts](https://github.com/apache/datafusion/blob/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38/datafusion/expr/src/logical_plan/extension.rs) and [physical extension planning](https://github.com/apache/datafusion/blob/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38/datafusion/core/src/physical_planner.rs)
- [Expression CSE](https://github.com/apache/datafusion/blob/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38/datafusion/optimizer/src/common_subexpr_eliminate.rs) and [specialized scalar-subquery sharing](https://github.com/apache/datafusion/blob/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38/datafusion/physical-plan/src/scalar_subquery.rs)
- [Arrow RecordBatch ownership](https://arrow.apache.org/rust/arrow/array/struct.RecordBatch.html) and [extension types](https://arrow.apache.org/docs/format/Intro.html#extension-types)
