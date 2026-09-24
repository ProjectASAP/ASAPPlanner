# DataFusion and native ASAP execution: architecture comparison

Audience: designers and maintainers of the Planner and execution engines.

This survey evaluates the execution choice in PR #462. It does not replace
ASAP's summary planning semantics or introduce a DataFusion dependency. Sources
were inspected on 2026-09-24 at DataFusion commit
[`e2ca7f3`](https://github.com/apache/datafusion/tree/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38).
The native baseline is PR #462 at `d782c4e`; its subsequent module/resource
refactor improves contracts but does not add partitioned execution or spill.
Performance statements below are hypotheses unless described as implementation
facts. No comparative benchmark has been run.

The [SQL frontend](../../crates/frontend-sql/Cargo.toml) already depends on
DataFusion 43 for parsing/planning; the physical-operator crate does not depend
on DataFusion. Reusing the existing frontend dependency and selecting a newer
execution backend are different choices. This survey follows the requested
upstream main, so an implementation must select a supported release and verify
its exact APIs rather than assume main's interfaces exist in version 43.
Build/binary-size effects also depend on which crates a deployment already links;
they should be measured separately from per-query runtime costs.

## Findings that affect the decision

1. DataFusion's physical operator boundary uses Arrow RecordBatch streams.
   Internal sketch state need not be an Arrow array or be serialized per update.
2. DataFusion does not provide arbitrary common-subplan fan-out merely by sharing
   an `Arc<ExecutionPlan>`. That is different from being unable to implement it:
   custom physical operators and explicit shared execution state are available.
3. Both logical and physical extension points are supported. A basic summary
   operator need not require a DataFusion fork. Integration with optimization,
   state transport and execution lifecycle is the substantial work.
4. A smaller native runtime is not evidence of lower end-to-end overhead.
   Representation, batch size, state crossings and algorithm choice may dominate.
5. Borrowing module boundaries is inexpensive. Porting DataFusion algorithms to
   another batch/runtime contract creates an ongoing integration and maintenance
   obligation; it does not retain upstream improvements automatically.

## Runtime costs: compare the actual execution paths

DataFusion is an embedded Rust library. Its normal physical interface starts a
stream for an output partition; an ordinary projection starts its child stream
and wraps it. It does not create a network boundary or a separately scheduled
worker for every operator. Repartition and explicit buffering introduce tasks,
channels and buffering where the plan calls for them. See
[ProjectionExec](https://github.com/apache/datafusion/blob/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38/datafusion/physical-plan/src/projection.rs),
[RepartitionExec](https://github.com/apache/datafusion/blob/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38/datafusion/physical-plan/src/repartition/mod.rs),
and [BufferExec](https://github.com/apache/datafusion/blob/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38/datafusion/physical-plan/src/buffer.rs).

A useful decomposition, rather than a fixed “DataFusion overhead” percentage, is:

```text
elapsed work ≈ planning + binding + execution setup
             + batch/stream bookkeeping + representation conversion
             + scalar/aggregate kernels + exchange + spill/I/O
```

| Cost | DataFusion implementation | Native #462 implementation |
| --- | --- | --- |
| Plan setup | Schema/property derivation, optimizer passes, physical construction | Graph/binding validation, per-node stream and consumer setup |
| Stream dispatch | Boxed streams and dynamic plan/expression interfaces | Local boxed streams and dynamic PhysicalOperator calls |
| Sharing | Arc ownership; explicit shared state where an operator implements it | Rc/RefCell producer state, reader maps, queues, wakers and output reservations |
| Ordinary data | Column arrays, null bitmaps, batched kernels; some operations allocate new arrays | Vec<Vec<Value>>, per-value enum dispatch, row vectors and clones; repeated row/schema checks in some paths |
| Summary data | Native accumulator while computing; Arrow-compatible state at standard operator boundaries | Native accumulator/state objects can cross edges behind Arc without encoding |
| Parallelism | Partition/exchange tasks, synchronization and data movement when selected | Worker-local execution; comparable partition parallelism is not implemented |
| Large blocking work | Specialized algorithms and spill-capable operators | In-memory joins/sorts/reductions with budget failure rather than spill |

A RecordBatch clone shares its array references; it does not inherently copy all
column buffers. Creating arrays from row-oriented input, filtering/taking values,
and serializing opaque state can still allocate or copy. Conversely, native
fan-out shares an output Batch, but downstream operators can clone its row
vectors. Neither representation makes every operation zero-copy. See the
[Arrow RecordBatch contract](https://arrow.apache.org/rust/arrow/array/struct.RecordBatch.html)
and [array/buffer model](https://arrow.apache.org/rust/arrow_array/index.html).

For a tiny precomputed-state readout, binding, allocation and decoding might
exceed kernel time; native execution could have an advantage. For large scans,
joins or high-cardinality grouping, vectorized kernels and a better algorithm
can outweigh framework bookkeeping. These are workload hypotheses, not measured
results. Compare equal thread counts first, then compare each engine's usable
parallelism separately. More CPU consumption from parallel execution can coexist
with lower latency.

Both runtimes need cooperative cancellation: a long synchronous kernel can
prevent its worker from observing cancellation. DataFusion explicitly documents
this and supplies cooperative wrappers/optimizer support; embedding it does not
make a custom KLL operator preemptible. ASAP's new checkpoints solve the same
class of problem. See [DataFusion cooperative scheduling](https://github.com/apache/datafusion/blob/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38/datafusion/physical-plan/src/coop.rs).

## Arrow is a boundary contract, not a required sketch implementation

If ASAP reuses DataFusion's standard ExecutionPlan and existing operators, their
interchange remains `SendableRecordBatchStream`. Replacing its item with an
arbitrary ASAP Batch would require adapters or a separate/forked execution
contract. Merely implementing a logical extension does not change that boundary.
See [ExecutionPlan](https://github.com/apache/datafusion/blob/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38/datafusion/physical-plan/src/execution_plan.rs).

There are several practical state representations:

| Representation | Where the native sketch lives | Consequence |
| --- | --- | --- |
| Custom UDAF accumulator | Rust accumulator object, updated from Arrow arrays | Native update algorithm; encode partial state only when exporting it for merge/spill or final state output |
| Binary/LargeBinary state column | Serialized sketch payload in a standard Arrow column | Portable through generic batch transport; encode/decode cost at state-consuming boundaries |
| Struct/List state columns | Sketch components represented by supported Arrow types | May expose buffers without a monolithic encoding; requires a stable representation and reconstruction rules |
| Run-local handle column | Native state in a registry, integer/binary handle in the batch | Avoids payload serialization locally; registry lifetime, memory, retries and transport require custom handling |
| Fused custom physical operator | Native state remains internal until ordinary outputs are produced | Avoids intermediate state transport; generic optimizers cannot operate inside the fused region |

DataFusion's `Accumulator` has `update_batch`, `state`, `merge_batch`, `evaluate`
and `size`. Its partial state can differ from its final value and contain several
fields. This is a natural fit for build/merge/finalize sketches, provided ASAP
implements parameter compatibility and the appropriate state schema. It does
not imply that an accumulator serializes itself for every input update. See
[Accumulator](https://github.com/apache/datafusion/blob/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38/datafusion/expr-common/src/accumulator.rs).

An Arrow extension type annotates a supported storage type with additional
semantics; it is not a universal container for a Rust trait object. Implementing
a custom Array trait object likewise does not ensure that generic take, filter,
IPC or spill code understands it. A binary extension type for “KLL version X,
parameters Y” is more interoperable than disguising an in-process pointer as a
portable value. Handles may work inside a controlled execution island, but must
not silently escape through spill, distributed exchange or persisted results.
See [Arrow extension types](https://arrow.apache.org/docs/format/Intro.html#extension-types).

Family, parameters, encoding version, grouping layout, source/window coverage
and ownership must be validated whichever representation is selected. Ordinary
transport of bytes does not prove that two sketches can legally merge.

## One producer and multiple consumers

There are three separate mechanisms:

- Detect equivalent expressions or subplans.
- Represent shared identity in the plan.
- Execute a producer once and deliver its results to independent consumers.

DataFusion's expression CSE addresses repeated expressions; it is not a general
shared-subplan execution guarantee. Holding the same Arc from two parents is
also insufficient: ordinary parent execution can start its child independently.
See [expression CSE](https://github.com/apache/datafusion/blob/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38/datafusion/optimizer/src/common_subexpr_eliminate.rs).

However, “DataFusion cannot represent one producer, multiple consumers” is too
strong. Its custom ExecutionPlan implementations can coordinate shared state.
Upstream already has specialized sharing, for example one-shot scalar-subquery
execution and shared results. Repartition also coordinates producer/output
partition state, although partition distribution is not general broadcast.
These are evidence that custom coordination is possible, not an off-the-shelf
replacement for ASAP's DAG runtime. See
[ScalarSubqueryExec](https://github.com/apache/datafusion/blob/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38/datafusion/physical-plan/src/scalar_subquery.rs).

The motivating KLL case has a simpler alternative when the consumers are
compatible readouts of the same population/window:

```text
raw rows → build/merge one KLL → estimate_many([0.5, 0.9, 0.99])
                             → ordinary columns q50, q90, q99
```

This can be a custom aggregate returning a Struct, or a build-state aggregate
followed by a multi-readout operator. It can avoid both general DAG broadcast and
repeated decoding. Three independent quantile aggregate calls do not by
themselves guarantee one KLL: an ASAP rule must explicitly select the common
state. Different filters, windows or downstream pipelines may prevent this
fusion and still require true fan-out.

A general DataFusion integration would need an explicit run-scoped shared
producer/subscription operator or materialization service. Its contract must
cover producer identity per partition/run, consumer registration, bounded queues
or replay, slow/dropped consumers, terminal errors, cancellation, memory and plan
reuse. A new run must not accidentally reuse stale mutable state. Cross-query
reuse additionally requires cache coverage/revision invalidation; it is a
separate feature in both engines.

A particularly important acceptance test is a diamond with asymmetric polling.
If one branch waits for the other to finish before polling, a shared producer
can fill that dormant branch's bounded queue and deadlock. The design must poll
branches appropriately, materialize/spill, or otherwise resolve the dependency.
This obligation applies to an ASAP adapter too; bounded broadcast alone does
not solve it. BufferExec's background queue is not automatically multicast.

## Logical versus physical extension effort

The difference is semantic scope, not simply adding an enum case.

| Layer | Extension work | What is not automatic |
| --- | --- | --- |
| Logical node | UserDefinedLogicalNode, schema, children, expressions, reconstruction, equality/hash and explain | Correct approximation semantics, coverage, sharing identity and rewrite legality |
| Logical-to-physical lowering | Register an ExtensionPlanner mapping the node to existing or custom physical operators | Choosing a legal summary implementation and preserving ASAP guarantees |
| Physical operator | ExecutionPlan properties, child replacement, partition execution, stream, metrics and memory behavior | Efficient merging, spillable state, consumer sharing and durable maintenance |
| UDAF route | Native accumulator, state fields, update/merge/finalize, memory size; optionally specialized grouped accumulation | Arbitrary summary subtraction/join or a shared multi-consumer DAG |

Logical extensions default to conservative predicate pushdown and expose hooks
for required columns and reconstruction. Approximation-sensitive rules must be
explicit: filtering before a summary can change its population, and removing a
grouping/identity column can invalidate it. See
[UserDefinedLogicalNode](https://github.com/apache/datafusion/blob/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38/datafusion/expr/src/logical_plan/extension.rs).

The physical planner already invokes registered extension planners and checks
their output schema. A conventional SummaryBuild or SummaryReadout can therefore
be implemented outside upstream DataFusion. Basic physical extension is feasible;
claiming transparent partitioning, spill, shared execution and maintenance
semantics is the larger engineering task. See
[extension planning](https://github.com/apache/datafusion/blob/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38/datafusion/core/src/physical_planner.rs).

For aggregate-shaped work, an AggregateUDF can reuse the existing aggregate
operator rather than introducing a custom physical node. High group cardinality
may justify implementing GroupsAccumulator to avoid a generic per-group adapter.
Reusing a partial/final aggregate pipeline still requires testing the sketch's
merge guarantees, memory reporting and exported state. See
[aggregate expression support](https://github.com/apache/datafusion/blob/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38/datafusion/physical-expr/src/aggregate.rs).

ASAP would retain its accuracy/cost model, state-family rules, coverage and
revision metadata, and deployment publication/window lifecycle. DataFusion does
not infer these from an opaque extension. Optimizations must also preserve
correlations introduced when several estimates use the same randomized state;
shared estimates must not silently acquire independence assumptions.

## How much optimization is actually reused?

Reusing standard relational nodes gives the widest access to existing expression,
projection/filter, join, aggregation, ordering and partition optimizations. A
large opaque extension hides internal opportunities unless it supplies properties
or is lowered into supported nodes. False ordering/distribution declarations
risk correctness, while conservative declarations can reduce optimization.

Keeping ASAP's IR and lowering directly to DataFusion physical operators is a
valid alternative to replacing the logical IR. It can reuse kernels and selected
physical optimization, but does not automatically run DataFusion's logical
optimizations over ASAP-specific nodes. Likewise, reusing logical IR alone does
not supply DataFusion physical execution to a separate native runtime.

| Option | Reused capability | Main obligation |
| --- | --- | --- |
| Native ASAP execution | Existing summary model and explicit DAG fan-out | Own relational kernels, partitioning, spill, metrics and optimizer/runtime contracts |
| DataFusion backend with extensions | Standard relational operators, physical infrastructure; logical optimizations where mapped | Arrow boundaries, custom summary semantics and shared execution adapter |
| ASAP scheduler around coarse DataFusion subplans | DataFusion relational execution inside islands; native summary edges outside | Control conversion boundaries, resource budgets, cancellation and parallelism across both layers |
| DataFusion outer plan with fused native summary regions | DataFusion surrounding relational computation; native state inside regions | Keep opaque regions coarse enough to avoid repeated conversion but expose useful properties |

A hybrid is an option to benchmark, not automatically the best of both worlds.
Wrapping every tiny native operation in a separate DataFusion execution adds
repeated setup and conversions. Coarse subplans amortize those boundaries, but
two independent budgets or schedulers must not oversubscribe memory or threads.

## Borrowing organization versus maintaining copied algorithms

Adopting modules such as plan, runtime, expressions, joins, aggregate, sources
and spill is a useful ownership decision independent of execution framework.
It does not require adopting DataFusion IR or Arrow, and #462 does this now.

Porting a hash join or external sort is much more than copying its main loop.
Those implementations rely on array kernels, expressions, row encodings,
distribution/ordering facts, memory reservations, async streams, spill formats
and tests. Retaining Arrow can reduce the port surface; replacing the data model
increases it. A maintained fork must also track upstream correctness fixes and
behavioral changes. See the concrete dependency surface in
[grouped aggregation](https://github.com/apache/datafusion/blob/e2ca7f38051744b2010cf09b80db8e9dfa4b5d38/datafusion/physical-plan/src/aggregates/grouped_hash_stream.rs).

A native engine can intentionally support less. That can be a sound decision
when workloads remain bounded and summary-heavy, but missing large-query
capabilities are a scope tradeoff, not evidence that their overhead has been
eliminated at equivalent functionality.

## Experiments needed before making a performance claim

Use the same sketch implementation, parameters, input values, grouping, windows
and accuracy contract. First measure already-bound execution; measure planning
and cold setup separately. Report both single-worker and independently tuned
parallel results. Do not compare a parallel hash join with a single-worker nested
loop and label the difference “runtime overhead.”

| Workload | Question isolated |
| --- | --- |
| Existing KLL → 1/3/10 quantiles | Readout setup, state cloning, decoding, fusion and consumer overhead |
| Raw values → KLL → several quantiles | Build cost versus state export/transport; verify one build |
| One producer → two asymmetric branches | Buffer growth, progress, dropped consumer and cancellation behavior |
| Many tiny panes and high group cardinality | Allocation, per-group adapters, state bytes and setup amortization |
| Raw Scan → Filter/Project → grouped aggregate | Native rows versus Arrow conversion and columnar computation |
| Join and Sort/Limit under memory pressure | Algorithm choice, workspace, graceful failure versus spill |

Record wall latency (including p50/p95 for repeated small queries), CPU time,
allocations, peak RSS, charged memory, serialized bytes, encode/decode counts,
producer starts, task/partition counts and cancellation latency. Distinguish
first-run initialization from warm execution. Verify results and summary
compatibility before interpreting timing. Acceptance also includes repeated
executions, multiple roots, reordered consumers and shared-error propagation.

The KLL prototype should compare native fan-out, a DataFusion UDAF producing
multiple estimates, DataFusion state output plus readout, and only then a custom
shared-producer adapter if independent branches are necessary. This distinguishes
an unavoidable domain cost from a cost introduced by a particular adapter.

## Position for PR #462

Proceed with the native module/runtime cleanup as scoped, while keeping the
execution-backend decision evidence-based. The current justification is direct
ownership of native summary-state edges and shared execution across both engines,
with explicitly limited generic execution capabilities. It is not established
that DataFusion is slower, cannot carry sketches, or cannot support fan-out.

If broad relational workloads, partition scaling and spill become near-term
requirements, a DataFusion execution backend or coarse hybrid deserves a serious
prototype before porting those subsystems. If bounded summary DAGs dominate and
measured conversion/state-transport costs are material, the native path has a
stronger workload-specific case. The proposed experiments are the decision gate;
this survey alone does not establish a performance winner.
