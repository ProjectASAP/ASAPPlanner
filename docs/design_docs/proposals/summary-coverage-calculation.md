# How Summary Coverage Is Computed

> Status: proposal, not implemented. Audience: planner designers.
> Builds on [#567](https://github.com/ProjectASAP/ASAPPlanner/pull/567) (coverage
> type), [#560](https://github.com/ProjectASAP/ASAPPlanner/pull/560) (merge) and
> [#646](https://github.com/ProjectASAP/ASAPPlanner/pull/646) (first, narrow derivation).

## Problem

A summary state's **coverage** says which observations it summarizes: from which
source, for which population (label values), over which time range. The planner
needs it to decide whether two states can be merged or shared without counting an
observation twice or mixing different data.

Today coverage is only computed in two narrow places:

| Where | How | PR |
|---|---|---|
| `SummaryAgg` | read from its subtree, but only through `Filter` and `TimeRange` down to one `Scan`; anything else gives "population unknown" | #646 |
| `SummaryMerge` | disjoint union of the inputs' coverage | #646 |
| every other operator | none (`coverage: None`) | — |

But a summary's input is an arbitrary sub-DAG: a `Project`, an `Aggregate` (rate),
a join, a union, a PromQL relabel. There is no single rule that says, for each
operator, what coverage its output has. #646 is one special case of that rule.

## What coverage is (and is not)

Coverage says which **observations** reach a summary state, and which of their
columns the state reads:

- **source**: which `Scan` source the observations come from;
- **population**: `field = 'text'` restrictions on those observations (PromQL label
  matchers, SQL text equality);
- **time**: which time range of the source;
- **columns** (#560): the update expression fed into the state (`input`) and its
  grouping (`group_by`), so two states over the same rows but different columns
  are not merged.

It is not the schema. The schema says what kind of state a node produces
(family, parameters, grouping). Two states worth merging have equal schemas and
different coverage, for example the panes `[0,1)` and `[1,2)`.

It is also not SDS. SDS (Self-Describing Summary, in ASAPQuery-backend) describes
stored results at runtime:

| | Planner coverage | Backend SDS |
|---|---|---|
| When | plan time, before data exists | runtime, one stored record |
| Source and filters | `source`, `population` | part of `SummaryDefinition` (semantic identity) |
| Time | the range a planned state covers | `StoredSummary.window`, per record |
| "Coverage" | which observations a state summarizes | `coverage: complete`: whether a record has all data for its window |
| Merge legality | decided here | not decided; runtime only checks committed, non-overlapping, complete panes |

So planner coverage feeds SDS's `SummaryDefinition`; record windows and
completeness stay in the backend.

## Proposal: one rule per operator, computed bottom-up

Every node gets a coverage, computed at construction like its schema, from its
inputs' coverage and its own operator. A coverage is either **known**
(one source, a set of regions) or **unknown** (can't tell, or several sources).
Unknown is safe: it only blocks merges and sharing decisions that need proof.

| Operator | Output coverage |
|---|---|
| `Scan` | its source; population from its `field = 'text'` predicates; time unbounded |
| `Values`, `PromqlVectorFromScalar` | no source: unknown |
| `Filter` | input, narrowed by `field = 'text'` conjuncts; any other predicate makes the population unknown |
| `Project`, `SQLWindowFunc`, `Sort`, `Dedup` | input; population keys follow column renames; a key whose column is dropped or computed becomes unknown |
| `Aggregate` | input (the result summarizes the same observations; `HAVING` drops groups, not observations) |
| `Limit`, `PromqlSeriesSample` | unknown (which observations survive depends on the data) |
| `TimeRange`, `TimeShift`, `PromqlSubquery` | input population; time is relative to evaluation time (see Time below) |
| `PromqlRelabel` | input, with the relabeled label unknown |
| `PromqlInfoEnrich` | input (added labels come from another source; the observations are the input's) |
| `Join`, `SetOp`, `BinaryOp` | same source on both sides: union for a union `SetOp`, otherwise unknown; different sources: unknown |
| `Concat` | union of the inputs if they share a source, otherwise unknown |
| summary operators | see [Summary operators](#summary-operators) |

Population keys are named from the `Scan`'s own schema and carried through
operators by column, so a field renamed by `with_schema` or a `Project` alias
cannot stand for a different source column (the bug fixed in #646).

With an explicit **unknown**, #646's special rule ("an unreadable population may
only merge with the same input") becomes ordinary: a merge needs known,
disjoint coverage on every input, and time panes of one computation carry known
coverage that differs only in time.

## Summary operators

A `SummaryCoverage` describes a summary state, so only nodes that build a
mergeable state carry one: rows (`source`, `regions`, each region a
`time_range` × `population`) and columns (`input`, the update expression fed
into the state, and `group_by`). `SummaryCoverage::derive` is the only function
that computes it. The table below says what `derive` returns for each `ASAPOp`.

| Operator | `derive(node, time_range)` | In |
|---|---|---|
| `SummaryAgg` | **source**: the one `Scan` source reachable from `child` (else `NoSingleSource`). **population**: the `field = 'text'` conjuncts of `filter`, every `Filter` and `Scan.predicates` on the path to the `Scan` (else `UnprovenPopulation`). **time**: `time_range` from the caller. **columns**: `input` copied; `reduction` keys named from `child.schema` (else `UnknownColumn`). One region. | #646 |
| `SummaryMerge` | `time_range` must be `None` (else `TimeRangeOnMerge`). Every child must carry coverage (else the child's own `derive` error, or `UnknownInput`). Result: `merge_disjoint` of the children's coverage: same source (`SourceMismatch`) and columns (`ColumnMismatch`), pairwise disjoint regions (`PossibleOverlap`), adjacent time ranges with equal population coalesced. Nested merges work because a child merge already carries its union. | #646 |
| `SummaryEstimate`, `FinalizeExactAccumulator` | none (`NotSummary`). They read out a state and are never merged; a caller that needs the observations behind a result reads `summary_input.coverage` / `child.coverage`. | #646 |
| `MaintainPopulation`, `EvaluatePopulation` | none (`NotSummary`). A maintained population tracks current membership (series appear and expire), so an observation does not contribute exactly once and two such states cannot be merged by disjoint union. | #646 |
| `SummarySubtract`, `SummaryDelete`, `SummaryJoin`, `Extension` | none (`NotSummary`); reserved operators fail schema derivation today. When implemented: `SummarySubtract` = `left` minus `right`'s regions, requiring `right` ⊆ `left`; `SummaryDelete` makes the population unknown; `SummaryJoin` reads two states, so no single source. | later |

### How a `SummaryAgg`'s source and population change in the follow-up

#646 reads source and population by walking `child` through `Filter` and
`TimeRange` only. The follow-up keeps the `SummaryAgg` rule above but takes the
source and population from `child`'s observation coverage, computed bottom-up by
the operator table in [Proposal](#proposal-one-rule-per-operator-computed-bottom-up),
and then narrows the population by `SummaryAgg.filter`. A `SummaryAgg` over a
`Project` or a `rate` `Aggregate` then gets a known population instead of
`UnprovenPopulation`. Nothing else in `derive` changes.

## Code interface

The public interface is the one in #646. The follow-up only changes private
helpers.

```rust
// crates/types/src/ir/summary_coverage.rs
impl SummaryCoverage {
    /// Coverage of the state `node` builds (table above).
    /// `time_range` is the absolute range of a `SummaryAgg`'s state (an
    /// ingested pane); `None` in a logical plan and for a `SummaryMerge`.
    pub fn derive(
        node: &OperatorNode,
        time_range: Option<Range<i64>>,
    ) -> Result<Self, CoverageError>;

    /// Disjoint union; used by `derive` for `SummaryMerge`.
    pub fn merge_disjoint(inputs: &[Self]) -> Result<Self, CoverageError>;

    /// Intervals non-empty, population keys non-empty, regions pairwise disjoint.
    pub fn validate(&self) -> Result<(), CoverageError>;
}

// crates/types/src/ir/node.rs
impl OperatorNode {
    /// Producers (Stage 2 panes) set a `SummaryAgg`'s time range here;
    /// coverage is derived, never written by hand.
    pub fn with_time_range(
        self,
        time_range: Option<Range<i64>>,
    ) -> Result<Self, SchemaDerivationError>;
}
```

Where `derive` is called, so a node's `coverage` always equals what its subtree
gives:

| Call site | What it does |
|---|---|
| `OperatorNode::new` | `SummaryMerge`: derives, and fails if the merge is not provably disjoint. `SummaryAgg`: derives with `time_range = None`, and leaves `coverage = None` if source or population cannot be read (the state is valid but cannot be merged). |
| `OperatorNode::with_time_range` | derives with the producer's time range; fails instead of leaving `None`. |
| `OperatorNode::with_new_children` | rebuilds, then derives again from the new children, keeping a `SummaryAgg`'s time range. |
| `OperatorNode::validate_structure` | derives every node again and compares: `Missing` if a derivable coverage is absent, `DerivedMismatch` if the stored one differs (e.g. edited JSON). |

Follow-up, private to `summary_coverage.rs`: one recursive function replaces
`scanned_source` and `population`:

```rust
/// Source and population of the observations reaching `node`'s output, by
/// the per-operator table. `Err` when unknown (no single source, or a
/// population the rules cannot read).
fn observations(node: &OperatorNode) -> Result<(Source, BTreeMap<String, String>), CoverageError>;
```

Whether its result is also stored on every node is open question 1; either way
`derive` calls it on a `SummaryAgg`'s `child`.

## Time

Most time in a plan is relative: `rate(x[5m])` covers "the 5 minutes before
evaluation time t". Absolute bounds exist only once a state is materialized, for
example an ingested pane `[12:00, 12:01)`.

#567 stores absolute `time_range` (named `time_ms` before #646) on a materialized
state. Given SDS, a cleaner split
may be: the planner records the **shape** of the time range (for example
"tumbling 1-minute panes aligned to the minute", or "the 5 minutes before t"), and
the backend owns the concrete windows of stored records. Open question 3.

## Uses

- `SummaryMerge` (#560): inputs need known, disjoint coverage.
- CSE (#645): two states with different coverage are never shared.
- Later: deciding whether a stored state can answer a query (its coverage must
  contain the query's population and time), and building SDS `SummaryDefinition`s.

## Open questions

1. **Which nodes carry coverage?** Every node (proposed: computed once at
   construction, inspectable anywhere), or only summary states, with the per-operator
   rules applied during one traversal.
2. **How to represent unknown?** An explicit `Known(..) | Unknown` (proposed), or
   `Option`, or a per-source map so a join can record both of its sources (no user
   yet).
3. **Absolute or relative time in the planner?** Keep absolute `time_range` on
   materialized states, or record only the time shape and leave windows to SDS.
4. **Confirm the operator table**, in particular `Aggregate` (pass through), `Join`
   (unknown) and `PromqlInfoEnrich` (pass through).

## Plan

#646 stays the narrow first version (Filter/TimeRange path, checked declarations,
the review fixes). A follow-up PR implements the table above once the open
questions are settled, and replaces #646's path walk.
