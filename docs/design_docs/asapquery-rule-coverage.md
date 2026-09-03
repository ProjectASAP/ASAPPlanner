# ASAPQuery rule coverage

This document compares ASAPPlanner's rule system with the Rust planner in
ASAPQuery at upstream commit `2586400b3b0436a5414c901ebce07065d20b5223`.
The comparison is by semantic capability, not source-file or function-name
parity: ASAPPlanner operates on a front-end-independent `QueryExpr` DAG, while
ASAPQuery recognizes a smaller set of PromQL expression shapes.

The audit covers ASAPQuery's `planner/patterns.rs`, `planner/window.rs`,
`planner/labels.rs`, `planner/cleanup.rs`, `planner/agg_config.rs`, and
`planner/sketch.rs`, together with the candidate generation, sketch-property,
cost, and selection rules under `optimizer/`. The reviewed source is
<https://github.com/ProjectASAP/ASAPQuery/tree/2586400b3b0436a5414c901ebce07065d20b5223/asap-planner-rs/src>.

## Coverage map

| ASAPQuery rule family | ASAPPlanner coverage | General form in ASAPPlanner |
|---|---|---|
| Temporal aggregate functions | Lowering covered; realization varies | `Aggregate(PerEntity)` over `TimeRange` represents the full family. Sum, count, min, max, quantile, rate, and increase have summary realizations; `avg_over_time` is currently exact `PassThrough`, matching ASAPQuery's exact-only multi-stat fallback rather than claiming a maintained summary. |
| Spatial aggregate functions | Lowering covered; realization varies | `Aggregate(Reduce(GroupKeys))` is shared by SQL and PromQL. Supported single accumulators and ordinary `by(...)` avg rewrites generate candidates; shapes such as `avg without(...)` retain the same exact raw fallback that ASAPQuery uses for multi-stat AQEs. |
| Collapsible temporal + spatial aggregates | Semantic-equivalent rewriting | The existing rewrite strategy uses accumulator algebra: sum∘sum, sum∘count, min∘min, and max∘max. It rejects all other pairs and requires identical output schemas. |
| Sketch alternatives and exact fallback | Covered more generally | `SketchAlgorithmStrategy` enumerates legal summary realizations. The enclosing memo group always retains the original raw expression as the exact fallback; the strategy does not falsely label an approximate sketch as exact. |
| Subpopulation label placement | Covered more generally | `HydraGroupingStrategy` and `GroupingStrategy` express per-subpopulation and shared multi-subpopulation realizations. |
| Shared computation | Covered more generally | workload-wide CSE and `SharedSubtreeStrategy` operate on physical DAG identity rather than AQE names. |
| Average decomposition | Semantic-equivalent rewriting | The same rewrite strategy exposes independently optimizable sum/count accumulators when null semantics and schema permit it. |
| Merge/delete legality | Covered | Summary-family capabilities and lifecycle validation determine which maintenance operations are legal. |
| Window-framework selection | Separate physical-planning work | Window selection must compare an extensible set of implementations, including tumbling, sliding, PromSketch-style exponential-histogram windows, and other window frameworks. This audit does not introduce a closed window enum or choose among them. |
| Retention/cleanup scheduling | Outside planner scope | The audit deliberately does not import ASAPQuery's Arroyo-specific cleanup thresholds, timers, or failure workarounds. The planner may declare a selected summary's required retention horizon and cost it, but the runtime/storage layer owns when and how expired physical state is reclaimed. |
| Empirical per-sketch atomic costs | Covered through evidence | Analytical statistics and deployment profiles provide cost evidence; benchmark tables should be ingested as calibrated evidence rather than compiled into matching rules. |
| Greedy cross-query selection | Superseded | ASAPPlanner searches a workload plan space and accounts for shared sub-DAGs instead of reproducing ASAPQuery's AQE-local greedy restriction. |

## Placement in existing strategies

An imported rule is assigned by the decision it changes; a new syntax pattern
does not create a new strategy category.

| Decision | Existing owner |
|---|---|
| Which summary algorithm can implement one aggregate intent | `SketchAlgorithmStrategy` |
| How grouping/subpopulation state is laid out | `HydraGroupingStrategy` |
| Whether an equivalent logical expression exposes better accumulators | `SemanticEquivalentRewriteStrategy` (the broadened existing avg rewrite; `AvgToSumOverCountStrategy` remains a compatibility name) |
| Whether identical physical work is shared | `SharedSubtreeStrategy` |
| Whether a finer grouping can answer a coarser grouping | `RollupStrategy` |
| Whether tighter accuracy can answer a looser request | `AccuracyReconciliationStrategy` |
| Whether a larger Top-K result can answer a smaller limit | `TopKLimitReuseStrategy` |
| Which maintenance lifecycle is legal | the summary-maintenance lifecycle planner |
| Which window framework implements a range | the physical deployment/window-selection planner |
| How expired physical state is cleaned up | runtime/storage lifecycle management, not a planner strategy |

Accordingly, ASAPQuery's four collapsible temporal/spatial patterns extend the
existing semantic-rewrite owner. Temporal and spatial function recognition is
already front-end lowering into `AggIntent`; sketch compatibility remains in
`SketchAlgorithmStrategy`; labels remain in `HydraGroupingStrategy`; and
maintenance lifecycle legality remains in the lifecycle planner. Window
framework selection is separate physical-planning work. None of these become a
parallel syntax-oriented `PatternStrategy`.

Retention requirements and cleanup policy are separate. A plan can require
state to remain available for a duration, and the planner can include that
duration in legality and cost comparisons. Choosing cleanup timers, eviction
thresholds, garbage-collection mechanics, or failure-recovery workarounds does
not change the logical or physical query plan and therefore stays outside this
planner.

## Rule design principles

Rules match typed operators and declared capabilities, never parser spellings.
A rule that composes operators states the algebraic law it relies on and
preserves the original output schema. Unknown pairs, missing statistics, or
unsupported lifecycle operations produce no candidate; they never silently
fall back to an optimistic estimate.

Window choices should follow the same principle without assuming that every
implementation is tumbling or sliding. A physical window alternative declares
its own capabilities, state shape, alignment constraints, maintenance method,
and cost evidence. That interface can admit exponential-histogram windows and
future frameworks without extending a closed core enum. Missing or conflicting
evidence fails closed. Runtime limitations belong in deployment evidence, so
adding another query language, sketch, or execution engine does not require
another set of syntax-specific rules.
