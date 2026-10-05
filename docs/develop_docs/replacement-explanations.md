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

The legacy replacement search implemented this (`pass1::explanation`,
`explain_replacements`, issue #257); it was removed with that search. Today the
`sketch_coverage` devtool lists, per query, the sketch alternatives Stage 1's
Pass 1 offers.
