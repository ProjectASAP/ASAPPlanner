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
| `SummaryMerge` | disjoint union of the inputs' coverage | #560 |
| every other operator | none (`coverage: None`) | — |

But a summary's input is an arbitrary sub-DAG: a `Project`, an `Aggregate` (rate),
a join, a union, a PromQL relabel. There is no single rule that says, for each
operator, what coverage its output has. #646 is one special case of that rule.

## What coverage is (and is not)

Coverage is a property of the **observations** that reach a node's output, not of
its rows or columns:

- **source**: which `Scan` source the observations come from;
- **population**: `field = 'text'` restrictions on those observations (PromQL label
  matchers, SQL text equality);
- **time**: which time range of the source.

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
| `SummaryAgg` | input, narrowed by its `filter`; time from the producer (Time below) |
| `SummaryMerge` | disjoint union of the inputs; fails if they may overlap |
| `SummaryEstimate`, `FinalizeExactAccumulator`, `MaintainPopulation`, `EvaluatePopulation` | the input state's coverage |

Population keys are named from the `Scan`'s own schema and carried through
operators by column, so a field renamed by `with_schema` or a `Project` alias
cannot stand for a different source column (the bug fixed in #646).

With an explicit **unknown**, #646's special rule ("an unreadable population may
only merge with the same input") becomes ordinary: a merge needs known,
disjoint coverage on every input, and time panes of one computation carry known
coverage that differs only in time.

## Time

Most time in a plan is relative: `rate(x[5m])` covers "the 5 minutes before
evaluation time t". Absolute bounds exist only once a state is materialized, for
example an ingested pane `[12:00, 12:01)`.

#567 stores absolute `time_ms` on a materialized state. Given SDS, a cleaner split
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
3. **Absolute or relative time in the planner?** Keep absolute `time_ms` on
   materialized states, or record only the time shape and leave windows to SDS.
4. **Confirm the operator table**, in particular `Aggregate` (pass through), `Join`
   (unknown) and `PromqlInfoEnrich` (pass through).

## Plan

#646 stays the narrow first version (Filter/TimeRange path, checked declarations,
the review fixes). A follow-up PR implements the table above once the open
questions are settled, and replaces #646's path walk.
