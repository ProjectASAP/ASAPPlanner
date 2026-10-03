# Post-ASAP IR

The goal of the post-ASAP IR is to represent operations using ASAP primitives
such as sketches, exact summaries, samples and wavelets, while retaining the
exact query operators that no summary replaces, and supporting operations over
summary evaluations.

ASAPPlanner has one operator IR before and after ASAP optimization
([`crates/types/src/ir/`](../../../crates/types/src/ir/)). A post-ASAP plan is
the same `Rc<OperatorNode>` DAG a front end produced, in which some nodes now
carry `Operator::ASAP(ASAPOp)` instead of `Operator::NonASAP(NonASAPOp)`. There
is no wrapper around retained exact work: an unreplaced `Filter`, `Join` or
`Aggregate` is the same node it was before, and either category can consume
the other's output. The node structure, the `Schema`, scalar expressions and
the catalog of non-ASAP operators are described once in the
[Pre-ASAP IR reference](../../develop_docs/pre-asap-ir.md); this document covers
what optimization adds: the ASAP operators, the accuracy guarantee, execution
timing, and the exported DAG.

A node's presence in the IR does not imply that every summary family, cost
model or downstream runtime supports it.

## ASAP operators

Every variant of [`ASAPOp`](../../../crates/types/src/ir/asap.rs) operates
over summary state rather than raw data. The summary family, kind/algorithm and
parameters are committed in the node; the state itself is typed by the
`FieldDataType` of the output field that carries it (`ExactAggregate`,
`Sketch`, `Sample`, `Wavelet`, `StatModel`).

Implemented:

- `SummaryAgg { child, family, input, reduction, grouping }`: produce summary
  state from input rows using the selected family, parameters, update input,
  reduction and grouping layout. Output: the grouping columns plus one `state`
  field typed `family`; result kind `State`.
- `SummaryEstimate { summary_input, query }`: read the requested statistic
  (`SketchStatistic`) from summary state and return query values in a row-shaped
  schema.
- `FinalizeExactAccumulator { child }`: read an exact accumulator's state as
  its finalized value — the maintenance-to-read boundary before query-time
  operators consume it.
- `MaintainPopulation { child, population }`: maintain the full declared
  population, including membership changes.
- `EvaluatePopulation { child, evaluation }`: read an aggregate or TopK prefix from a
  maintained population.

Reserved (migrated but unimplemented; schema derivation, timing and export
reject them with `UNIMPLEMENTED_ASAP_OP`):

- `SummaryMerge`: merge compatible summary states when the family supports merging.
- `SummarySubtract`: subtract one summary state from another when supported by
  the selected representation.
- `SummaryDelete`: delete a key from summary state when the representation supports
  deletion.
- `SummaryJoin`: combine summary states for join estimation; this is distinct
  from joining ordinary rows.
- `Extension`: a deployment-defined operator.

The earlier draft listed `SummaryCreate` and `SummaryInsert`. These are not
separate variants. `SummaryAgg` describes the state-producing
computation and its update input. Stage 2 materialization (#509) will decide
whether and when that state is maintained. Physical binding and runtime
execution implement the actual build and update operations. Not every summary family supports incremental maintenance.

## Exact work and composition

Exact work is represented by the ordinary operators, unchanged:

- A sub-DAG the planner does not rewrite keeps its `NonASAPOp` nodes. Plan
  assembly marks such a sub-DAG with an exact `ResultGuarantee`
  (`asap_aware_mapping::replacement::retain_exact`); a sub-DAG with no ASAP
  operator and no guarantee is a logical rewrite candidate that has not been
  assessed yet (`is_logical_rewrite`).
- `BinaryOp` combines independently planned operands. Summary planning may set
  its typed division guards (`checked_finite_division`,
  `checked_relative_division`); the operator's timing comes from the
  materialization assignment, not from the operator.
- Aggregate, projection, filter, sort and limit over a evaluation are the ordinary
  `Aggregate`, `Project`, `Filter`, `Sort` and `Limit` operators reading an ASAP
  node. Exact-accumulator state may pass through the projection-like
  operators unchanged; a value consumer needs a `FinalizeExactAccumulator`
  boundary first.
- Candidate pruning uses `Join` with `JoinKind::Semi` and an explicit
  equality predicate on key columns. The left input supplies authoritative
  values; the right input supplies keys. Grouped `Sort` followed by grouped
  `Limit` (both with the same `partition_by`) ranks and selects the joined
  rows. Completeness evidence belongs to pruning, not ranking.

Every `OperatorNode` carries its schema and an optional `ResultGuarantee`.
State and query values have different contracts. Exact operations over
approximate evaluations still require composed accuracy guarantees. See the
[accuracy implementation companion](../../develop_docs/end-to-end-accuracy-guarantees.md)
and [physical-plan integration](../architecture/physical-plan-integration.md)
for the corresponding correctness and realization requirements.

## Execution timing

An operator defines what computation happens. The plan decides when it
happens: **ingestion time** or **query time**. Operator identity must not imply
one of these phases. Backend capability restrictions are implementation gaps,
not definitions of the operator.

The logical DAG carries no timing: `OperatorNode::timing` is `None` on every
front-end node and every candidate, and `map_children` clears it. Summary
materialization chooses a timing per summary state and records it in a
[`MaterializationAssignment`](../../../crates/types/src/ir/timing.rs) (ingestion-time
maintenance or query-time computation per `SummaryAgg`). The default is
`all_query_time()`; until Stage 2 materialization (#509) decides otherwise, the
planner times every `SummaryAgg` at query time.
`apply_materialization_timings(root, &assignment, &mut TimingMemo)` then writes a
timing into every node, top-down:

- a node of fixed kind takes its kind's timing — `SummaryEstimate` and
  `EvaluatePopulation` run at query time, `MaintainPopulation` at ingestion time;
- a `SummaryAgg` takes the assignment's timing, unless something below it can
  only exist at query time (a evaluation);
- every other node runs when its consumer runs: everything that feeds a
  maintained state runs at ingestion time, everything above a evaluation at
  query time.

The pass then validates every edge (rows or exact-accumulator state into a
`SummaryAgg`, state into a evaluation, an ingestion-time `MaintainPopulation` under
a `EvaluatePopulation`, no ingestion work reading a query-time value) and rejects a
node reached from two consumers that need different timings;
`split_shared_by_phase` copies such a sub-DAG for one side before the
assignment is applied. `validate_maintained` and `planned_data_state` answer the
same questions for a candidate at planning time, assuming every summary is
maintained at ingestion time, without keeping anything.

## Exported DAG

The pre-ASAP DAG and the post-ASAP DAG are both logical: they describe what is
computed, not which physical operators execute it. Planning builds and shares
`OperatorNode` trees;
[`asap_types::ir::export::compile_post_asap_dag`](../../../crates/types/src/ir/export.rs)
converts a selected, timed tree into a `PostAsapDAG` with stable node IDs and
typed edges, and `PostAsapDAGDocument` is its versioned wire envelope
(`schema_version` = `POST_ASAP_DAG_WIRE_VERSION`, currently 6). Physical
compilation consumes `PostAsapDAG` and produces a separate physical DAG.

Wire version 7 emits **one node per operator** — relational operators
included — with children as edges and no embedded sub-DAGs:

- A non-ASAP node is a `Relational { operator: NonASAPOpKind }` payload:
  the operator's own fields with scalar expressions mirrored as
  `WireScalarExpr`, children removed. An ASAP node's payload is its variant
  (`SummaryAgg`, `SummaryEstimate`, `FinalizeExactAccumulator`,
  `MaintainPopulation`, `EvaluatePopulation`, …).
- Edges carry a role: `Input`, `Left`/`Right` for the two sides of a `Join`,
  `SetOp`, `BinaryOp`, `SummarySubtract` or `SummaryJoin`, and `ScalarRef`
  when the consumer reads the producer from inside one of its scalar
  expressions (`scalar(v)`). Every edge records the intermediate schema, the
  producer's data state and grouping/window compatibility.
- Each node records `output_state` (timing plus `Raw` or `SummaryState`),
  `output_schema` and `guarantee`. Export reads the timing written by
  `apply_materialization_timings` and rejects an untimed node
  (`ExecutionDataStateError::UntimedNode`); it does not re-run data-state
  validation.

Phase is stored on the `PostAsapDAG` node, independently of its payload.
`PostAsapDAG::with_execution_phases` reassigns a phase to every node and
updates its edges; ingestion work cannot depend on a future query result.
Deployments must separately check that they have an implementation and a
valid data source for the chosen placement.

## Weighted grouped TopK

For `topk by(job)(2, sum by(service, job)(rate(m[1m])))`, the summary
realization consumes the complete per-series rate results. Each job owns a
separate CMS and candidate heap. Inside that partition, the item is service and
the update weight is the series rate. Summing updates for one item implements
the logical grouped sum without first constructing all exact grouped sums.

The DAG is per-series rate → finalized values → partitioned summary construction
→ typed candidate/score evaluation → output projection → grouped Sort → grouped
Limit. The output count is two per job. The candidate capacity is a separate
parameter, provisionally `max(k, ceil(1 / epsilon))`; this sizing choice is not a
membership theorem. Missing evidence retains a logical candidate with symbolic unknown guarantees;
default selection does not certify or choose it.
The row evaluation restores job and service identities and returns estimated sums.
There is no mandatory exact scoring branch or candidate semi-join in this path.
The old raw counter-delta update expression is removed rather than retained as a
compatibility option: counter increments are not complete windowed rate results.

The direct evaluation represents both score error and membership. A source provider
supplies an enforced upper bound on distinct partition/item identities for the
complete evaluation. Planner uses this bound to size confidence and union-bound
score errors over adaptively selected items. Membership evidence is evaluated
for the query's output count, not the candidate capacity. Score and membership
failure probabilities are combined, and the score guarantee remains in the
membership guarantee's child provenance. An exact request does not accept this
approximate output path merely because its selected identities are certified.

Deployment chooses ingestion time or query time for these operators. The
materialization assignment writes the placement; `with_execution_phases` can
reassign it on the exported DAG. Either deployment must give each evaluation a complete
rate window and an isolated summary state, or maintain an equivalent replacement
strategy. Appending successive rate snapshots to one cumulative state is invalid.
An ingestion execution can compute a window before the query and store its state;
a query execution can construct the same state on demand. These are placements
of the same computation, not separate summary semantics.

This follows the evidence-dependent candidate contract from #455. Missing
population or margin evidence is exported as symbolic unknown terms, rather than
erasing a constructible summary. Backend/runtime/deployment inspects these
requirements and supplies applicable evidence before selection and installation.
Re-running planning with that provider resolves guarantees and may resize the
candidate. Known-invalid evidence or known bounds that already miss the target
are rejected; an optimistic floor is never exported as a certificate.
