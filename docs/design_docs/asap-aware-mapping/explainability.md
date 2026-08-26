# Explaining a Replacement

ASAP-aware mapping should make its discovered opportunities visible to users and other components.

For each relevant region of the plan, the planner should be able to explain:

- which replacements exist,
- what alternatives each replacement introduces,
- what conditions make those alternatives legal,
- and which parts of the plan they affect.

For example:

```text
Aggregation: Quantile(latency, 0.99)

Available alternatives:
    - exact quantile
    - KLL
    - DDSketch

Additional opportunities:
    - share source filtering with Query B
    - reuse finer-grained aggregation through roll-up
```

Implemented as `asap-aware-mapping`'s `explanation` module (`explain_replacements`/`explain_replacements_with`, issue #257): a **view of the candidate search space**, not a second optimization engine — each explanation's reason is the matching candidate's own rationale, not new prose invented by the reporting layer.

This keeps explanations consistent with the plans the optimizer can actually produce.

---
