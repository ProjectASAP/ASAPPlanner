# Candidate Plan Search

The central design principle is that ASAP-aware mapping should **preserve alternatives long enough to reason about their interactions**.

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

Choosing KLL immediately because it appears locally cheapest may be wrong if another summary enables more efficient sharing elsewhere in the plan.

Similarly, deciding independently whether to share two computations may miss a better plan produced after semantic rewriting.

The planner should therefore construct a search space of local alternatives and evaluate the complete candidate plans formed from their compatible combinations.

---

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

The planner should represent common structure once and attach alternatives only at the decision points where plans differ.

Conceptually, each decision point forms an **alternative group** containing its local choices:

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

A candidate plan is formed by selecting one compatible alternative from each relevant group.

This avoids representing every full plan independently when most of their structure is identical.

---

## Global Rather Than Local Decisions

The planner should evaluate alternatives at the plan level because several dimensions interact:

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

A locally optimal choice may prevent a globally better combination.

The planner should therefore retain local alternatives until enough context exists to compare the complete candidate plans they produce.

The design may prune clearly dominated alternatives, but pruning should preserve choices that could become useful because of interactions elsewhere in the plan.
