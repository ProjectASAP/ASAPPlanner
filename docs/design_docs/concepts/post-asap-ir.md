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
- `MembershipFilter`: semijoin value rows against membership identities without
  sorting, limiting or replacing their values. The completeness contract belongs
  to pruning, not ranking. A candidate-based TopK optimization expands to this
  filter followed by an ordinary `ValueOperation::Exact(Aggregate::TopK)` node.
  The filter has no `k` or grouping parameter. Certified and explicitly
  best-effort membership remain distinct; exact requests cannot use an
  uncertified pruning rewrite.

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

SummaryMerge supports both phases in the executable contract. A query-time merge
can combine stored ingestion results and query-produced states; an ingestion-time
merge cannot depend on a future query result. Other operators still have current
placement restrictions that require further implementation before this general
contract is fully supported.
