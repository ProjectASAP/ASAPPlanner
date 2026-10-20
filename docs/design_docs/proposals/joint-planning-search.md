# Joint Planning Search: Pruning the Whole-Workload Candidate Space

This document designs how ASAPPlanner selects one plan for a whole workload without building every whole-workload candidate. It builds on the planning stages in [planner-layering.md](planner-layering.md) and changes how Stage 1 to Stage 3 search, not what they decide.

## Contents

- [Problem](#problem)
- [Goals and non-goals](#goals-and-non-goals)
- [Where the candidates come from](#where-the-candidates-come-from)
- [Design](#design)
  - [1. Independent components](#1-independent-components)
  - [2. Per-query validity before composition](#2-per-query-validity-before-composition)
  - [3. Lower-bound dominance](#3-lower-bound-dominance)
  - [4. Local materialization choice](#4-local-materialization-choice)
  - [5. Sharing that never costs more](#5-sharing-that-never-costs-more)
  - [6. Streaming selection and a search budget](#6-streaming-selection-and-a-search-budget)
- [Cost model 3](#cost-model-3)
- [What the selection report shows](#what-the-selection-report-shows)
- [Validation](#validation)
- [Relation to other search approaches](#relation-to-other-search-approaches)

## Problem

Planning a workload jointly lets queries share computation: one summary answers several queries, identical sub-expressions run once, panes are reused across evaluations. Today the planner finds the best joint plan by enumerating every whole-workload candidate and pricing each one, and it keeps all of them in memory.

That does not scale past a handful of queries. Planning a growing prefix of one 10-query workload of ASAPQuery's random mixed workloads:

| Queries | Time | Peak memory |
| ---: | ---: | ---: |
| 3 | 0.4 s | 0.14 GB |
| 4 | 5 s | 1.5 GB |
| 5 | 129 s | 24.7 GB |
| 6 | stopped at ~228 s | ~100 GB |

Each added query multiplies the work by roughly 20. None of the 30 workloads of that set plan jointly, and neither does the Kubernetes recording-rule set. Planned one query at a time, every query plans in milliseconds, but sharing across queries is lost.

## Goals and non-goals

**Goals**

- Joint planning of realistic workloads (tens of queries) in seconds and under a few GB.
- **The same answer as exhaustive search.** Every pruning step below is exact under an additive cost: it only discards candidates that cannot be selected. The selected plan's cost equals the exhaustive search's.
- When a search must stop early, it says so. Truncation is never silent.

**Non-goals**

- Changing what the stages decide: candidate generation rules, sharing rules, materializations, accuracy and cost checks stay as they are.
- A solver-based formulation (MILP). That remains an option; see the last section.
- Re-planning or incremental planning when the workload changes.

## Where the candidates come from

The number of whole-workload candidates is the product of three factors:

| Stage | Factor | Grows as |
| --- | --- | --- |
| Stage 1, Pass 1 | One per-computation candidate per target, combined across targets | product of the targets' candidate counts |
| Stage 1, Pass 2 | Sharing alternatives of each composed candidate: identical expressions, summary capability, window composition, rollup, top-k reuse | subsets of the sharing opportunities |
| Stage 2 | One materialization per summary build: at ingestion, at query time, at query time kept across evaluations | three per build |

Most of these candidates can never win. Either they lose to another candidate of the same query regardless of sharing, or their choices in one part of the workload do not affect any other part.

## Design

Six steps, applied in this order. Steps 1 to 5 are exact. Step 6 bounds the work and reports when it does.

### 1. Independent components

Two queries interact only through sharing. A Pass 2 rule can link them only if they read the same source and have a sharing opportunity between them: an identical sub-expression, a summary able to answer both, a rollup, or a top-k reuse.

- Build a graph with one vertex per query and an edge for each possible sharing opportunity.
- Plan each connected component on its own.
- Because cost is additive across queries that share nothing, the workload's best plan is the union of each component's best plan.

A workload over many metrics, such as a set of recording rules, splits into many small components. ASAPQuery's mixed workloads read two metrics, so they split into at most two components per workload.

### 2. Per-query validity before composition

Some checks depend on one query's candidate only:

- whether the deployment supports the candidate's summaries;
- whether the executor can run it;
- whether its accuracy guarantee, or the deployment's evidence, meets the query's target.

These run on each query's candidates before they are combined. Today they run in Stage 3, after composition, so invalid candidates still multiply the product.

### 3. Lower-bound dominance

For each candidate *c* of query *q*:

- **alone(c):** the cost of *c* planned by itself, with its best materialization.
- **shareable(c):** an upper bound on what sharing can save *c*. This is the cost of the parts of *c* that another query in the component could share.

No joint plan containing *c* costs *q* less than **alone(c) − shareable(c)**. So *c* is discarded when

> alone(c) − shareable(c) > min over the other candidates c′ of q of alone(c′).

This also decides which sharing candidates are kept: a candidate whose sharing could still make it win stays, and every other candidate goes. A candidate with nothing to share has shareable(c) = 0, so only the query's cheapest candidates survive.

### 4. Local materialization choice

A summary build that no other query reads affects only its own query's cost. Its materialization is chosen locally: the cheapest of the three. Only builds read by several queries are chosen jointly. The Stage 2 factor shrinks from three per build to three per shared build.

### 5. Sharing that never costs more

Running an identical sub-expression once is never more expensive than running it twice. So the identical-expression rule is always applied, and its subsets are not enumerated.

Two kinds of sharing can cost more, and keep both alternatives:

- **Summary capability.** A shared summary is sized for its strictest reader, which can make it larger than either reader's own summary.
- **Rollup.** Answering a coarse query from a finer summary can cost more than a summary of its own.

### 6. Streaming selection and a search budget

- **Streaming.** Candidates are priced as they are generated, and only the best so far is kept. For cost model 3, the Pareto front is kept instead. The full candidate lists are kept only when a developer tool asks for them.
- **Budget.** A budget on candidates priced, or on time, guards against a component that is still too large after steps 1 to 5.
- **When the budget runs out:** selection returns the best plan found so far and marks the result as truncated, with how much of the space was searched. The default budget is large, so workloads that fit are never truncated.

## Cost model 3

Cost model 3 (the deployment's own cluster) is not additive: memory, disk and cores are hard limits, and the objective chooses among the candidates that fit. Steps 1 to 6 apply with these changes:

- **Step 1:** each component keeps its Pareto front over memory, disk and latency, not a single best plan. The workload's plans are the combinations of component fronts that fit the limits.
- **Step 3:** a candidate is dominated only if another candidate of the same query is at least as good on every dimension, after the sharing bound.

## What the selection report shows

- The components found.
- For each query: how many candidates steps 2 and 3 discarded, and why.
- For each component: how many candidates were priced, and whether the search was truncated.

## Validation

- **Equivalence.** On the planner-layering examples, and on randomly generated workloads small enough to search exhaustively, the selected plan's cost equals the exhaustive search's. Each step is tested on its own.
- **Scale.** Every workload of ASAPQuery's random mixed workloads, and the Kubernetes recording-rule set, plan jointly. The target is seconds and under 1 GB each. The report records time and peak memory.

## Relation to other search approaches

A solver-based selection (MILP over candidate, sharing and materialization choices) could replace steps 3 to 6 later. Steps 1 and 2 would still apply, because they shrink the problem any solver receives. This design keeps the enumeration's exact semantics and makes it scale first.
