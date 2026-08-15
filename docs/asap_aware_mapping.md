# ASAP-aware mapping

ASAP-aware mapping decides **whether and how an intent can be answered by a summary** rather than by scanning raw data.

The input is a pre-ASAP plan and the output is a set of candidate post-ASAP plans. This layer decides what kind of sketches can be used to satisfy a query.
Based on any given accuracy targets, it can also assign parameters to those sketches. (TODO: is this implemented?)
This layer **does not** assign physical resources like CPU and memory to nodes in the plan.

## Key concepts (not yet implemented)

- TargetSubDAG: Pre-ASAP sub-DAG that is a candidate to be replaced by a post-ASAP sub-DAG
- ReplacementSubDAG: Candidate post-ASAP sub-DAG to replace a pre-ASAP sub-DAG
- ReplacementStrategy: Each stratey defines one TargetSubDAG and one or more ReplacementSubDAGs
- CostModel: Estimates the cost of a post-ASAP plan. Can be based on heuristics or empirical estimates

## Pseudocode (not yet implemented)

```
input_replacement_strategies: List[ReplacementStrategy]
input_plan: pre-ASAP plan (DAG)

candidate_plans = [input_plan]
new_plans = []
do
    for candidate_plan in candidate_plans:
        for strategy in input_replacement_strategies:
            if candidate_plan has input_strategy.TargetSubDAG:
                candidate_plan_replacements = replace(candidate_plan, strategy)
                new_plans = candidate_plane_replacement - candidate_plans
    new_plans = deduplicate(new_plans)
    candidate_plans = candidate_plans UNION new_plans
while new_plans != []

sort candidate_plans based on CostModel

output: candidate_plans

```


## Summary mapping

Representative mappings include:

```text
Count
    -> exact counter / count summary

DistinctCount(field)
    -> HyperLogLog-family summary

Quantile(field, q)
    -> quantile sketch family such as KLL / DDSketch

TopK(key, Count, k)
    -> heavy-hitter family such as SpaceSaving
```

The mapping is not necessarily one-to-one. Multiple summary candidates may satisfy one
intent, and a cost model may rank those candidates.

For example:

```text
Quantile(latency, 0.99)
        ↓
Candidate summaries:
    KLL
    DDSketch
    exact accumulator
        ↓
Cost / accuracy / latency constraints
        ↓
chosen implementation
```

## Degrees of freedom we should explore (not yet implemented)

Selecting a sketch:
- Different sketches (KLL vs DDSketch)
- Different parameters for same sketch
- Hydra vs sketch-per-subpopulation (exact treatment of subpopulations)
- Single-level Hydra (what the paper talks about) or multi-level Hydra (e.g. CMS on top of CMS on top of CMS)
- Sliding window vs tumbling window computation (this is specific to ASAPCollector and ASAPQuery's precompute engine)
- Sliding window sketches (e.g. Promsketch) vs exact treatment of time
- AHA vs treating hierarchical subpopulations independently

## Summary properties to model

- Mergeable?
- Subpopulation aware?
- Subtractable?
- Able to delete an item?
