# Local logical alternatives (Pass 1)

`asap_aware_mapping::logical_candidates` enumerates local realization choices over
unified `OperatorNode` and `QueryRoot` inputs. It is the first part of logical
ASAP optimization in [planner layering](../design_docs/proposals/planner-layering.md).

`enumerate_local_logical_candidates(roots)` returns a `LocalLogicalCandidates`
inventory containing the original named roots and one `LocalLogicalTarget` per
eligible aggregate measure. Discovery includes operator producers read by
scalar roots and expressions. Pointer identity prevents repeated discovery of one
shared producer. Inputs with assigned execution timing or materialization, or
with ASAP operators, are rejected: the input `LogicalDAG` contains no ASAP
summary choices or physical annotations. An approximate accuracy requirement
needs epsilon and delta in (0, 1); otherwise enumeration fails with
`InvalidAccuracy`, naming the root and measure.

## Eligibility

A target is one measure of an `Aggregate` whose intent has a realization besides
exact execution: an exact accumulator state, a summary family or a rewrite rule.
Each measure of a multi-measure aggregate is its own target, identified by the
aggregate node and the measure index. A measure with no such realization (for
example `stddev`) is not a target and keeps exact execution.

Each target retains its original operator, including grouping, filter and input
context, and has an unranked list of `LocalCandidate` descriptors:

- Each target has exactly one exact option, listed first. A mergeable exact
  intent (`count`, `sum`, `min`, `max`, `rate`, `irate`, `increase`) offers
  its exact accumulator kind and parameters (`ExactAccumulator`); `avg` offers
  `AvgAsSumCount` (below). Plain exact execution of the original sub-DAG
  (`Exact`) is offered only when the intent has no exact accumulator form or
  that form is pruned. The plain form would duplicate "accumulator, not
  materialized", which is the raw plan for such targets; Stage 2/3 decide
  materialization and sharing.
- Approximate-capable intents offer all declared specialized/universal summary
  algorithms. Summary candidates are unsized: Pass 2 may share a summary and
  tighten it to the strictest consumer, so it is sized for the accuracy
  requirement later.
- `avg` is rewritten as exact `sum` and `count` accumulator states
  (`AvgAsSumCount`). `avg` carries no approximate accuracy requirement, so no
  summary family applies to its components.

A measure filter or `HAVING` does not limit a target to its exact option;
the target keeps every candidate and records how the predicate is applied:

- A measure's own filter is recorded as `LocalLogicalTarget.filter`; only
  rows that pass it update the state (`SummaryAgg.filter`).
- `HAVING` is recorded as `LocalLogicalTarget.having`, one `HavingEntry`
  (consumer `Filter`, predicate, placement) each. Only the adjacent form is
  recorded for push-down: the aggregate's own `having` (an entry with no
  `Filter`) and a `Filter` directly above the aggregate, which is how the SQL
  frontend lowers `HAVING`. A predicate that reads only `by(...)` group keys
  is `HavingPlacement::BelowAggregate`: it is pushed below the aggregate as a
  row predicate, like a measure filter. A consumer `Filter`'s predicate is
  pushed below only when every consumer of the aggregate applies the same
  predicate; otherwise another consumer would lose rows. Any other `HAVING`,
  and every `HAVING` of a global aggregate (no group keys, so its one output
  row exists even with no input rows), is `HavingPlacement::AboveOutput`: it
  stays a `Filter` above the finalize/estimate output, and approximate
  candidates of the target carry `guarantee = None`, since membership near
  the threshold may change. A value predicate anywhere above an approximate
  output, adjacent or not, gets `guarantee = None` through guarantee
  propagation (#575); multi-level push-down is tracked in #664.

For example, an approximate single-column distinct count offers exact execution,
HLL, Theta, KMV and UnivMon; an approximate `count` offers its exact `Count`
accumulator, CMS, Count Sketch and UnivMon, but not plain `Exact`. These are candidate choices, not assessed accuracy
certificates. Catalog order is stable and has no cost/preference meaning.

## Pruning

Every pruned candidate is recorded on its target with a `PruneReason`:

- `RequiresExact`: an exact accuracy request does not acquire approximate
  summaries.
- `NullableAvgInput`: `count` counts rows, so `avg` over a possibly-null input
  is not rewritten.

When the exact accumulator form (`ExactAccumulator` or `AvgAsSumCount`) is
pruned, it stays in the pruned list with its reason and plain `Exact` becomes
the target's one exact option.

Distinct-tuple counts do not acquire single-value UnivMon alternatives; that
family is not declared for them, so it is not a pruned candidate.

TopK offers its per-group heap summaries (`CmsWithHeap`,
`CountSketchWithHeap`). The grouping-level Hydra candidate ("over all groups")
is not generated in Pass 1 here; it is added in #605.

## Scope

The API accepts no empirical cost or accuracy model, runtime capabilities, storage
policy or materialization assignment. It does not rank, select, construct runtime
state or claim physical feasibility. Composing each candidate into a replacement
sub-DAG, including applying measure filters and `HAVING` as recorded on the
target, and validating its structure is still Pass 1 work, done later when
candidates are realized (#575); Pass 2 then retains independent and shared
alternatives before physical planning and complete workload selection. The
descriptors are not
executable plans, and callers must not execute the first choice as a selection
policy.

Opaque deployment extensions retain exact execution here; additional local
alternatives require an explicit logical rule rather than a cost model making a
generation decision. The legacy ranked search remains available for the existing
pipeline until its later cutover; this module supplies the new logical-only entry
point without changing production selection prematurely.
