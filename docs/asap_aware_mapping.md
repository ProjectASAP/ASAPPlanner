# ASAP-aware mapping

ASAP-aware mapping decides **whether and how an intent can be answered by a summary** rather than by scanning raw data.

The input is a pre-ASAP plan and the output is a set of candidate post-ASAP plans. This layer decides what kind of sketches can be used to satisfy a query.
Based on any given accuracy targets, it can also assign parameters to those sketches (implemented today — `boundary::implementation_for`'s `bind_summary_with` sizes each candidate via `CostModel::size_params`; see `default_size_params`'s per-family formulas).
This layer **does not** assign physical resources like CPU and memory to nodes in the plan.

This document tracks issue #33 ("add logic to detect which optimizations/summaries are applicable to a query workload"). The sub-issues cited below are where each piece below is actually being implemented.

## Key concepts

- TargetSubDAG: Pre-ASAP sub-DAG that is a candidate to be replaced by a post-ASAP sub-DAG
- ReplacementSubDAG: Candidate post-ASAP sub-DAG to replace a pre-ASAP sub-DAG
- ReplacementStrategy: Each strategy defines one TargetSubDAG match and every valid ReplacementSubDAG for it. ReplacementStratey only suggests strategies, it does not rank or filter them or decide which is best.
- CostModel: Estimates the cost of a post-ASAP plan. Can be based on heuristics or empirical estimates

Currently, this crate has logic for two decisions: (a) which summary family/kind realizes an `AggIntent` (`boundary::implementation_for`), and (b) which subtrees can be shared (`cse::share_common_subtrees`). These are implemented as separate code. #251 generalizes both of these to use the `ReplacementStrategy` trait.

## Replacement plan search

Tracked as **#252**.

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

Two things #252 has to get right that this pseudocode leaves open:

- **Dedup** must be structural-hash-based, reusing `pre_asap::cse`'s existing `InternTable`/`structural_hash` machinery (hash as a filter, `PartialEq` as the real decision) — not `Vec` containment over whole serialized plans.
- **MEMO-style sharing across candidate plans, not a flat plan list** — a naive flat list of whole candidate plans duplicates every untouched subtree across every candidate (a workload with N independently-choosable sites produces up to 2^N flat plans). Each distinct `TargetSubDAG` becomes a MEMO group holding its alternative `ReplacementSubDAG`s, so two candidate plans differing at one site still share every other node by `Rc`, the same way `share_common_subtrees` already shares identical subtrees today.

This is a genuine change of scope from `docs/cse-cost-model-decision.md` (issue #237), which deliberately chose a direct cost comparison over "full Volcano/Cascades-scale infrastructure" for *one* binary, single-candidate-pair decision (share a CSE'd subtree or don't) — reasoning at the time that "this repo has no plan-enumeration/DP-search engine anywhere" and didn't need one for that narrow question. #252 is where that stops being true, for the reason #237 itself named: once multiple interacting axes exist at once (sketch family/kind, roll-up vs. recompute, Hydra vs. per-subpopulation, semantic rewrite), a per-decision-point heuristic can't see interactions across sites the way a real candidate-plan search can. `CostModel::cse_share_decision`'s existing recompute-vs-maintenance comparison isn't thrown away — it becomes the cost function backing the CSE `ReplacementStrategy`'s two candidates (share vs. recompute-independently) inside this engine, so the final `sorted_by(cost_model)` step reuses it rather than re-solving the same comparison a second way.

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

## Degrees of freedom we should explore

Selecting a sketch:
- Different sketches (KLL vs DDSketch) — today only *ranked* (`CostModel::rank_candidates` takes the head); exposed as real alternative candidates once #251 lands
- Different parameters for same sketch
- Hydra vs sketch-per-subpopulation (exact treatment of subpopulations) — tracked as **#256**: a new axis orthogonal to `SketchKind`/`SamplingKind`/…, `GroupingStrategy::{PerSubpopulationInstance, SharedMultiSubpopulation}` (named for sharing across a query's own subpopulations), with `HydraKind`/`HydraParams` mirroring the existing per-family `(Kind, Params)` pattern
- Single-level Hydra (what the paper talks about) or multi-level Hydra (e.g. CMS on top of CMS on top of CMS) — not yet tracked; a refinement of #256's `HydraParams` once single-level lands
- Sliding window vs tumbling window computation (this is specific to ASAPCollector and ASAPQuery's precompute engine) — not yet tracked here
- Sliding window sketches (e.g. Promsketch) vs exact treatment of time — not yet tracked here
- AHA vs treating hierarchical subpopulations independently — tracked as **#254**: two `Aggregate` nodes over the same shared source with mergeable intents and one's `by` a superset of the other's can share via roll-up instead of two independent passes; must consult #256's grouping-strategy legality before assuming a `SharedMultiSubpopulation` structure composes with a roll-up
- Combining computation between part from the sketch, and part from the raw data to meet an accuracy target — not yet tracked

Cross-query, not just per-node:
- Semantic-equivalent rewriting (e.g. `avg` → `sum`/`count`) to increase how often the optimizations above apply — tracked as **#253**, landing as a `ReplacementStrategy` rather than a bespoke before/after-CSE heuristic: once #252's search exists, both the original and rewritten forms are just two competing candidates, and whichever lets more sharing happen elsewhere wins on cost
- CSE across aggregations, and group-by key management — #251 turns `share_common_subtrees`'s binary share/don't-share into a real candidate pair; #254 extends the same idea to *non-identical* (subset) group-by keys, which today's structural-equality-only CSE cannot see at all

## Applicability reporting

`crates/asap-aware-mapping/src/applicability.rs` (issue #247) reports "is optimization X applicable, and where" by re-walking the tree itself for each of its two rules. Tracked as **#257**: once #251/#252 exist, rebuild it as a read-only view over the search's resulting candidate-plan space instead — "which optimizations/summaries are applicable" and "what does this site's candidate list contain" become the same question, so there is exactly one place (the search) that computes it.

## Summary properties to model

- Mergeable?
- Subpopulation aware?
- Subtractable?
- Able to delete an item?
- Time aware?
- Linearability?
- Does accuracy drop when more items are inserted?
- Can a summary work as an inner operator for other operators?
