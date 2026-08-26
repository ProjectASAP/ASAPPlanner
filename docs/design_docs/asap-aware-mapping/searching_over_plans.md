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

The planner should represent common structure once and attach alternatives only at the decision points where plans differ. Each decision point forms an **alternative group** containing its local choices:

```text
Group A: Quantile implementation
    - KLL
    - DDSketch
    - exact quantile

Group B: Subpopulation organization
    - one summary per subpopulation
    - shared multi-subpopulation summary

Group C: Related aggregations
    - compute independently
    - compute once and roll up
```

A candidate plan is formed by selecting one compatible alternative from each relevant group. This avoids representing every full plan independently when most of their structure is identical.
