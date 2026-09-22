# Candidate Plan Search

ASAP-aware mapping should consider all alternatives holistically rather than optimize prematurely.

Suppose a plan contains several independent-looking decision points:

```text
                 Query workload
                       |
          +------------+------------+
          |                         |
      Quantile                  Group-by
          |                         |
    KLL / DDSketch          independent / roll-up
```

Choosing KLL because it appears locally cheapest is wrong if another summary enables more efficient sharing elsewhere in the plan. Similarly, deciding independently whether to share two computations may miss a better plan produced after semantic rewriting. The planner should therefore construct a search space of local alternatives and evaluate the complete candidate plans formed from their compatible combinations.

Several optimization dimensions may interact and the planner should consider all alternatives holistically:

```text
summary family
    ×
summary parameters
    ×
subpopulation organization
    ×
roll-up structure
    ×
computation sharing
    ×
semantic rewrites
    ×
time representation
```

## Shared Representation of Alternatives

Candidate plans often differ in only a small part of the overall query DAG.

For example:

```text
                  shared source
                       |
                 shared filters
                       |
                +------+------+
                |             |
              KLL         DDSketch
                |             |
                +------+------+
                       |
                shared remainder
```

The planner represents common structure once and stores a candidate set for
each target sub-DAG (`TargetSubDAGCandidates`). The dimensions below illustrate
possible local choices; they are not separate collections keyed by optimization
category:

```text
Quantile target candidates
    - KLL
    - DDSketch
    - exact quantile

Subpopulation target candidates
    - one summary per subpopulation
    - shared multi-subpopulation summary

Related-aggregation target candidates
    - compute independently
    - compute once and roll up
```

A candidate plan combines compatible choices from the relevant target candidate
sets. This avoids copying every full plan when most structure is shared.


## Current implementation boundary

`PlanSpace` stores per-target candidates rather than eagerly enumerating their
Cartesian product. `cost_sorted` returns a `RankedTargetSubDAGCandidates` view
for each target. `global_selection` coordinates supported sharing and
composition choices; `assemble_selected_dag(root)` assembles one selected DAG
per query root. This does not prove global physical optimality or select a
summary-maintenance lifecycle. The
[workflow design](input-output-workflow.md#workflows) explains when to use the
ordinary or summary-maintenance-lifecycle-aware path.

The [code architecture](../../develop_docs/asap-aware-mapping-architecture.md)
describes current discovery and registry behavior; the
[library guide](../../develop_docs/library-api.md#optional-whole-plan-selection-and-dag-assembly)
shows selection and its evidence boundaries. Broader optimization dimensions are
tracked in the [proposal](../proposals/asap-aware-mapping/optimizations.md).
