# ASAP-aware mapping code architecture

Audience: contributors changing search, candidate construction, or ranking.
The [system overview](../design_docs/architecture/README.md) defines component
ownership. The [library guide](library-api.md) provides runnable integration
examples; [mapping contracts](asap-aware-mapping-contracts.md) define the rules
shared by all strategies.

## Search and candidate construction

`search_workload` uses the default strategies. `search_workload_with` accepts
an explicit strategy set. Both canonicalize sharing once, discover targets
throughout each root DAG, and run strategy application to a bounded fixpoint.
A target retains its shared `Rc<QueryExpr>` identity and structural consumer
count. This count is not a runtime invocation frequency.

The default context-free registry contains:

| Strategy | Candidate source |
| --- | --- |
| `SketchAlgorithmStrategy` | Supported summary realizations and composed binary/temporal-average shapes |
| `HydraGroupingStrategy` | Eligible shared multi-subpopulation summary layouts |
| `SharedSubtreeStrategy` | Share versus independently recompute |
| `AvgToSumOverCountStrategy` | Supported average rewrites |
| `ExactCompositionStrategy` | Exact operations composed with child summary alternatives |

`default_strategies_with` uses `SemanticEquivalentRewriteStrategy` for its
rewrite slot. The evidence-aware registry supplies the accuracy evidence
provider to summary and Hydra construction. Workload search also derives
`RollupStrategy` after CSE, using the actual sibling set. Additional strategies
can be supplied explicitly; the registry is not the list of every supported
extension.

A strategy returns proposals for its target. Summary construction derives
schemas, realizes inputs and readouts, and checks accuracy from committed
parameters. `propose` can retain structured accuracy rejections alongside
accepted replacements. Search deduplicates candidates into memo groups and
prepares compatible compositions. `search_workload_with_targets` additionally
checks the supplied per-root accuracy requirements before ranking.

## Representation and selection

`Replacement::Summary` carries a constructed summary DAG; `Rewrite` carries a
logical alternative. `ExactComposition` refers to a child target without
prematurely choosing its implementation. That dependency must survive search
so selection can choose compatible parent and child alternatives.

`PlanSpace::cost_sorted` returns each group's candidates with index-aligned
costs. It preserves the candidates available at the ranking boundary; legality
checks may already have rejected proposals. Unknown numeric cost remains
explicit and is not a zero-cost plan.

For callers needing a selected semantic DAG, `global_selection` coordinates
cross-group choices, including sharing and composition dependencies.
`GlobalSelection::materialize` constructs that DAG. Neither operation deploys
state or establishes physical feasibility. Recurrence and lifecycle-aware
entry points require their corresponding workload and evidence inputs; see the
[library workflows](library-api.md#optional-whole-plan-selection-and-materialization).

`TargetSubDAG::new` and taking the first replacement are useful for isolated
inspection. They do not discover workload sharing or replace compatible
whole-workload selection.

## Code map

| Concern | Source |
| --- | --- |
| Registry, discovery, memo, ranking and selection | [replacement.rs](../../crates/asap-aware-mapping/src/replacement.rs) |
| Parent/child composition | [exact_composition.rs](../../crates/asap-aware-mapping/src/exact_composition.rs) |
| Accuracy and evidence | [accuracy.rs](../../crates/asap-aware-mapping/src/accuracy.rs) |
| Cost and capability hooks | [cost_model.rs](../../crates/asap-aware-mapping/src/cost_model.rs) |
| Lifecycle planning | [summary_maintenance_lifecycle.rs](../../crates/asap-aware-mapping/src/summary_maintenance_lifecycle.rs) |
| Reporting | [explanation.rs](../../crates/asap-aware-mapping/src/explanation.rs) |

Use the [extension task index](extend-asap-aware-mapping.md) for changes to these
components. Reporting consumes candidate information; it must not implement a
second applicability or optimization rule.
