# Post-ASAP IR

The goal of the post-ASAP IR is to represent operations using ASAP primitives
such as sketches, exact summaries, summary samples and wavelets, while retaining the
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

Every variant of [`ASAPOp`](../../../crates/types/src/ir/operator/asap.rs) operates
over summary state rather than raw data. The summary family, kind/algorithm and
parameters are committed in the node; the state itself is typed by the
`FieldDataType` of the output field that carries it (`ExactAggregate`,
`Sketch`, `SummarySample`, `Wavelet`, `StatModel`).

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
- `SummaryMerge { children, newest_tumbling_windows, group_by }`: merge summary states with identical
  schemas (same family, parameters and grouping) into the state of their
  union; its coverage check rejects inputs whose selections overlap. With
  `group_by` (a rollup) the output is keyed by those input group columns, and
  each output state merges the input states of its key; `newest_tumbling_windows`
  merges only the newest tumbling windows of one tumbling-window chain, never with `group_by`. The timing pass does
  not place it yet (`ExecutionDataStateError::UnimplementedOperator`).

Reserved (migrated but unimplemented; schema derivation rejects them with
`UNIMPLEMENTED_ASAP_OP` and the timing pass with `UnimplementedOperator`):

- `SummarySubtract`: subtract one summary state from another when supported by
  the selected representation.
- `SummaryDelete`: delete a key from summary state when the representation supports
  deletion.
- `SummaryJoin`: combine summary states for join estimation; this is distinct
  from joining ordinary rows.
- `Extension`: a deployment-defined operator.

The earlier draft listed `SummaryCreate` and `SummaryInsert`. These are not
separate variants. `SummaryAgg` describes the state-producing
computation and its update input. Stage 2 (physical ASAP-aware optimization) decides
whether and when that state is maintained. Physical binding and runtime
execution implement the actual build and update operations. Not every summary family supports incremental maintenance.

## Exact work and composition

Exact work is represented by the ordinary operators, unchanged:

- A sub-DAG the planner does not rewrite keeps its `NonASAPOp` nodes. Plan
  assembly marks such a sub-DAG with an exact `AccuracyGuarantee`
  (`asap_aware_mapping::replacement::retain_exact`); a sub-DAG with no ASAP
  operator and no guarantee is a logical rewrite candidate that has not been
  assessed yet (`is_logical_rewrite`).
- `BinaryOp` combines independently planned operands. Summary planning may set
  its typed division guards (`checked_finite_division`,
  `checked_relative_division`); the operator's timing comes from the
  physical properties assignment, not from the operator.
- Aggregate, projection, filter, sort and limit over an evaluation are the ordinary
  `Aggregate`, `Project`, `Filter`, `Sort` and `Limit` operators reading an ASAP
  node. Exact-accumulator state may pass through the projection-like
  operators unchanged; a value consumer needs a `FinalizeExactAccumulator`
  boundary first.
- Candidate pruning uses `Join` with `JoinKind::Semi` and an explicit
  equality predicate on key columns. The left input supplies authoritative
  values; the right input supplies keys. Grouped `Sort` followed by grouped
  `Limit` (both with the same `partition_by`) ranks and selects the joined
  rows. Completeness evidence belongs to pruning, not ranking.

Every `OperatorNode` carries its schema and an optional `AccuracyGuarantee`.
State and query values have different contracts; see
[Schema and physical data for ASAP primitives](../proposals/asap-primitive-schema.md). Exact operations over
approximate evaluations still require composed accuracy guarantees. See the
[accuracy implementation companion](../../develop_docs/end-to-end-accuracy-guarantees.md)
and [physical-plan integration](../architecture/physical-plan-integration.md)
for the corresponding correctness and realization requirements.

## Execution timing

An operator defines what computation happens. The plan decides when it
happens: **ingestion time** or **query time**. Operator identity must not imply
one of these phases. Backend capability restrictions are implementation gaps,
not definitions of the operator.

Operator nodes carry no physical properties: the logical DAG has no timing
or lifetime.
Materialization ([#509 §Materialization](../proposals/planner-layering.md#materialization))
decides, for each sub-DAG, whether its output is materialized, and if so with which
[`PhysicalProperties`](../../../crates/types/src/physical/properties.rs): a
timing and a `Lifetime` (#1200). The lifetime is `None` (discarded after its
consumer), `OneEvaluation(medium)` (materialized until every consumer of one evaluation
finishes) or `AcrossEvaluations(medium)` (materialized for later evaluations), with
`StorageMedium::Memory` or `StorageMedium::Disk`; an ingestion-time output cannot be materialized
for `OneEvaluation`. A
[`PhysicalPropertiesAssignment`](../../../crates/types/src/ir/properties/timing.rs) records,
per sub-DAG, the properties of its materialized output, named by the node whose output
would be materialized. Any node may be named: a summary state, a population, or, as in
#509 Example 4, the tumbling windows a query reads. `set(node, properties)`
lists one; an entry whose lifetime is `None` fixes only the node's timing, and every
sub-DAG the assignment does not list is not materialized. Stage 2 chooses the
assignment. Operator nodes carry no timing or lifetime (#1200): physical
stages read them from the assignment.
`apply_physical_properties(root, &assignment)` then completes it into a timing and a
lifetime for every node, top-down, and returns the completed assignment:

- `SummaryEstimate` and `EvaluatePopulation` run at query time;
- a node the assignment lists runs at its entry's timing; a `SummaryAgg`
  or `MaintainPopulation` that is not materialized runs at query time. Every node
  upstream of an ingestion-time node also runs at ingestion time, so a materialized
  ingestion-time output over something that can only exist at query time (an
  evaluation, or a state not materialized at ingestion time) fails with
  `ExecutionDataStateError::UpstreamNotAtIngestion` ("upstream of
  ingestion-time node not at ingestion time");
- every other node is not materialized and runs when its consumer runs: everything
  that feeds an ingestion-time node runs at ingestion time, everything above
  an evaluation at query time. A query-time consumer reads the stored output
  of a node materialized at ingestion time (`AcrossEvaluations`).

Applying a completed assignment again gives every node the same properties: the
facade re-applies Stage 3's selection this way for export
(`PlanOutput::physical_properties`). A root may be materialized
at ingestion time, and the query reads its stored output; a root of rows that
runs at ingestion time without being stored there fails with
`ExecutionDataStateError::UnstoredIngestionRoot`.

The pass then validates every edge (rows or exact-accumulator state into a
`SummaryAgg`, state into an evaluation, a `MaintainPopulation` under
an `EvaluatePopulation`, no ingestion work reading a query-time value) and rejects a
node reached from two consumers that need different timings;
`split_shared_by_timing` copies such a sub-DAG for one side before the
assignment is applied. How much a materialized output retains (retention) is decided
later and is not in the assignment.

A node the assignment does not set is not materialized. The test helper
`PhysicalPropertiesAssignment::materialized_at_ingestion_where_allowed(roots)`
(feature `test-support`) sets every `SummaryAgg`, `SummaryTumblingWindowChain`
and `MaintainPopulation` materialized at ingestion time where the DAG allows it (not
filtered, nothing below it that can only run at query time). The test
helpers `validate_at_ingestion` and `planned_data_state` use it to check, at
planning time and without materializing anything, that a candidate stays valid
when every sub-DAG is materialized at ingestion time.

## Exported DAG

The pre-ASAP DAG and the post-ASAP DAG are both logical: they describe what is
computed, not which physical operators execute it. Planning builds and shares
`OperatorNode` trees;
[`asap_types::ir::physical_export::compile_physical_asap_dag`](../../../crates/types/src/ir/physical_export.rs)
converts a selected, timed tree into a `PhysicalASAPDAG` with stable node IDs
and typed edges (`compile_physical_asap_workload` does the same for a batch,
with one root per query). The plan file a runtime installs,
`asap_planner::InstallablePlan`, carries the selected DAG with the query
texts, versioned as a whole by `wire_version` (`INSTALLABLE_PLAN_WIRE_VERSION`).
Physical compilation consumes `PhysicalASAPDAG` and produces a separate
physical DAG.

The DAG has **one node per operator** — relational operators included — with
children as edges and no embedded sub-DAGs:

- A node's payload is its `Operator<NodeId>`: the same `NonASAPOp` or `ASAPOp`
  variant as in the IR, with each child replaced by its node ID.
- Edges carry a role: `Input`, `Left`/`Right` for the two sides of a `Join`,
  `SetOp`, `BinaryOp`, `SummarySubtract` or `SummaryJoin`, and `ScalarRef`
  when the consumer reads the producer from inside one of its scalar
  expressions (`scalar(v)`). Every edge records the intermediate schema, the
  producer's data state and grouping/window compatibility.
- Each node records `output_state` (timing plus `Raw` or `SummaryState`),
  `output_schema` and `guarantee`. Export reads each node's timing and lifetime
  from the assignment `apply_physical_properties` completed and rejects a node
  it does not list
  (`ExecutionDataStateError::UntimedNode`); it does not re-run data-state
  validation.

Each node also records its `lifetime`, including the medium, so the
deployment knows which outputs to materialize, for how long and where; validation
rejects a timing and lifetime that cannot go together (an ingestion-time
output materialized for `OneEvaluation`) and a `SummaryAgg` or `MaintainPopulation`
that is not materialized outside query time
(`PhysicalASAPDAGValidationError::PhysicalProperties`). Ingestion
work cannot depend on a future query result. The
deployment executes the plan as given and does not change its lifetime
or timing ([#509 Assumption 8](../proposals/planner-layering.md#assumptions)); it must
separately check that it has an implementation and a valid data source for
each placement.

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

The physical properties assignment decides whether these summaries run at
ingestion time or at query time, and the exported DAG carries that placement.
Either placement must give each evaluation a complete
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
