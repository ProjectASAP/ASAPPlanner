# Local logical alternatives (Pass 1)

`asap_logical_optimizer::pass1::logical_candidates` enumerates local realization choices over
unified `OperatorNode` and `QueryRoot` inputs. It is the first part of logical
ASAP optimization in [planner layering](../design_docs/proposals/planner-layering.md).

`enumerate_local_logical_candidates(roots)` returns a `LocalLogicalCandidates`
inventory containing the original named roots and one `LocalLogicalTarget` per
reachable single-measure aggregate. Discovery includes operator producers read by
scalar roots and expressions. Pointer identity prevents repeated discovery of one
shared producer. Inputs with assigned execution timing are rejected.

Each target retains its original operator, including grouping, filter and input
context, and has an unranked list of existing `Realization` descriptors:

- Exact execution of the original sub-DAG is always retained as `PassThrough`.
- Mergeable exact intents also offer their exact accumulator kind and parameters.
- Approximate-capable intents offer all declared specialized/universal sketch
  algorithms with nominal dimensions from the built-in sizing contracts.
- Exact accuracy requests do not acquire approximate alternatives. Distinct-tuple
  counts do not acquire single-value UnivMon alternatives.

For example, an approximate single-column distinct count offers exact execution,
HLL, Theta, KMV and UnivMon. These are candidate choices, not assessed accuracy
certificates. Catalog order is stable and has no cost/preference meaning.

The API accepts no empirical cost or accuracy model, runtime capabilities, storage
policy or materialization assignment. It does not rank, select, construct runtime
state or claim physical feasibility. Pass 2 must compose and structurally validate
replacement sub-DAGs and retain independent/shared alternatives before physical
planning and complete workload selection. The descriptors are not executable
plans, and callers must not execute the first choice as a selection policy.

Multi-measure aggregates remain intact in the roots until an explicit semantic
split is supported. Opaque deployment extensions retain exact execution here;
additional local alternatives require an explicit logical rule rather than a cost
model making a generation decision. This module is the stage pipeline's Stage 1
entry point; the legacy ranked search it replaced was removed.
