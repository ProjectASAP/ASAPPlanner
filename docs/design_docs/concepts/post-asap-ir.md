# Post-ASAP IR

The goal of the post-ASAP IR is to represent operations using ASAP primitives
such as sketches, exact summaries, samples and wavelets. Post-ASAP IR also
retains exact Pre-ASAP subtrees and supports operations over summary readouts,
since only some query operations can be satisfied using summaries.

The lists below cover every current variant of
[`SummaryExpr`](../../../crates/types/src/post_asap/expr.rs). A node's presence
in the IR does not imply that every summary family, cost model or downstream
runtime supports it.

## ASAP-specific nodes operated over a summary structure, not raw data

- `SummaryAgg`: produce summary state from input data using the selected family,
  parameters, update input, reduction and grouping layout.
- `SummaryEstimate`: read the requested statistic from summary state and return
  query values. Exact accumulators can expose results without a separate sketch
  readout.
- `SummaryMerge`: merge compatible summary states when the family supports merging.
- `SummarySubtract`: subtract one summary state from another when supported by
  the selected representation.
- `SummaryDelete`: delete a key from summary state when the representation supports
  deletion.
- `SummaryJoin`: combine summary states for join estimation; this is distinct
  from joining ordinary rows.

The earlier draft listed `SummaryCreate` and `SummaryInsert`. These are not
separate variants in the current IR. `SummaryAgg` describes the state-producing
computation and its update input. The
[summary-maintenance lifecycle](../proposals/asap-aware-mapping/workload-demand-and-summary-lifecycle.md)
separately describes when state is created, retained, shared, updated and retired.
Physical binding and runtime execution implement the actual build and update
operations. This is not a one-to-one rename of the old nodes, and not every
summary family supports incremental maintenance.

## Exact work and composition nodes

- `KeepPreAsap`: retain an exact Pre-ASAP subtree when it is not rewritten.
- `BinaryOp`: combine independently planned operands with the specified binary
  semantics and execution timing.
- `ValueOperation`: apply aggregate, exact-function, population, projection,
  filter, sort, limit or extension semantics with explicit execution timing.
- `RelationalJoin`: join row-producing children using the specified join kind
  and predicate.
- Candidate pruning uses `RelationalJoin` with `JoinKind::Semi` and an explicit
  equality predicate on key columns. The left input supplies authoritative
  values; the right input supplies keys. Grouped Sort followed by grouped Limit ranks
  and selects the joined rows. Completeness evidence belongs to pruning, not ranking.

A `SummaryNode` carries its expression, schema and optional result guarantee.
State and query values have different contracts. Exact operations over
approximate readouts still require composed accuracy guarantees. See the
[accuracy implementation companion](../../develop_docs/end-to-end-accuracy-guarantees.md)
and [physical-plan integration](../architecture/physical-plan-integration.md)
for the corresponding correctness and realization requirements.

## Execution phase

A physical operator defines what computation happens. The plan decides when it
happens: **ingestion time** or **query time**. Operator identity must not imply
one of these phases. Backend capability restrictions are implementation gaps,
not definitions of the operator.

Every executable physical payload supports both phase assignments. Phase is
stored on the physical node, independently of its operator payload.
`ExecutableDag::with_execution_phases` assigns a phase to every node and updates
its edges. Ingestion work cannot depend on a future query result. Default
semantic realization still proposes an initial layout; it does not restrict
which phase a physical operator may use. Deployments must separately check that
they have an implementation and a valid data source for the chosen placement.

## Weighted grouped TopK

For `topk by(job)(2, sum by(service, job)(rate(m[1m])))`, the summary
realization consumes the complete per-series rate results. Each job owns a
separate CMS and candidate heap. Inside that partition, the item is service and
the update weight is the series rate. Summing updates for one item implements
the logical grouped sum without first constructing all exact grouped sums.

The DAG is per-series rate → finalized values → partitioned summary construction
→ typed candidate/score readout → output projection → grouped Sort → grouped
Limit. The output count is two per job. The candidate capacity is a separate
parameter, provisionally `max(k, ceil(1 / epsilon))`; this sizing choice is not a
membership theorem. Missing membership evidence still prevents realization.
The row readout restores job and service identities and returns estimated sums.
There is no mandatory exact scoring branch or candidate semi-join in this path.
The old raw counter-delta update expression is removed rather than retained as a
compatibility option: counter increments are not complete windowed rate results.

The direct readout checks both score error and membership. A source provider
supplies an enforced upper bound on distinct partition/item identities for the
complete readout. Planner uses this bound to size confidence and union-bound
score errors over adaptively selected items. Membership evidence is evaluated
for the query's output count, not the candidate capacity. Score and membership
failure probabilities are combined, and the score guarantee remains in the
membership guarantee's child provenance. An exact request does not accept this
approximate output path merely because its selected identities are certified.

Deployment chooses ingestion time or query time for these operators. The
semantic constructor proposes a layout; `with_execution_phases` assigns the
executable placement. Either deployment must give each evaluation a complete
rate window and an isolated summary state, or maintain an equivalent replacement
strategy. Appending successive rate snapshots to one cumulative state is invalid.
An ingestion execution can compute a window before the query and store its state;
a query execution can construct the same state on demand. These are placements
of the same computation, not separate summary semantics.
