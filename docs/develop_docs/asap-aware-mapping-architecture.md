# ASAP-Aware Mapping architecture

This document explains how the planner turns logical query operations into
alternative realizations built from ASAP primitives, such as exact accumulators
and approximate sketches, and selects among them. A **realization** is one
candidate form of one logical operation, not a selected workload plan or a
deployed executable.

The design is the #509 stage pipeline
([planner layering](../design_docs/proposals/planner-layering.md)); the
[library API](library-api.md) shows how to call it. The legacy replacement
search (`ReplacementStrategy`, `search_workload`, `CostModel`) that this
document used to describe was removed; the
[contracts](asap-aware-mapping-contracts.md) and
[extension guide](extend-asap-aware-mapping.md) are kept as a historical
record of it.

## Terms

- A **workload** is the set of named queries planned together. A **query root**
  is the top-level `QueryRoot` (an `Rc<OperatorNode>` operator DAG or a scalar
  expression over them) of one query. **Pre-ASAP** means a DAG of ordinary
  `NonASAPOp` operators; **post-ASAP** means the same IR after some nodes became
  `ASAPOp` summary operators.
- A **target** is one single-measure aggregate a summary can realize. An
  **alternative** is one `Realization` of it: `PassThrough` (exact execution of
  the original sub-DAG), `ExactAggregate` (a mergeable exact accumulator) or
  `Sketch` (an approximate summary sized to the query's accuracy target).
- An **accuracy target** states the allowed error and failure probability.

## The stages

```text
pre-ASAP roots
  -> Stage 1, Pass 1: one LocalLogicalTarget per target aggregate, with its
     alternatives (pass1::logical_candidates, pass1::realization)
  -> Stage 1, Pass 2: sharing variants across queries — independent,
     identical expressions merged, one summary sized for the strictest
     consumer — plus tumbling-window forms for repeating queries (pass2)
  -> Stage 2: physical candidates of each composed logical candidate, one per
     materialization choice (asap-physical-optimizer)
  -> Stage 3: reject candidates that miss an accuracy target, need a capability
     the deployment lacks or exceed its memory budget; price the rest and
     select the cheapest (asap-plan-selection)
```

| Concern | Location |
| --- | --- |
| Which realizations an intent has, sizing, summary input rules | `asap_logical_optimizer::pass1::realization` |
| Pass 1 inventory and composition of one choice per target | `asap_logical_optimizer::pass1::logical_candidates` |
| Pass 2 sharing rules | `asap_logical_optimizer::pass2` |
| Analytical error bounds of each summary family | `asap_logical_optimizer::accuracy` |
| Materialization | `asap_physical_optimizer` |
| Accuracy model, capabilities, pricing and selection | `asap_plan_selection` (`plan_stages`, `select_plan`) |
| Facade | `asap_planner::e2e_plan` |

Stage 1 never prices: candidate generation is independent of cost (#572,
decision Q36(a)). It does not rank alternatives either; the catalog order of
`summary_candidates` has no preference meaning. Stage 3 is the only stage that
computes cost, and it checks accuracy per summary estimate with the
`AccuracyModel` in `PlanningModels`.

## Adding a realization

A new summary algorithm for an intent is added to `summary_candidates` and
sized in `accuracy::estimators::size_params`; its local guarantee goes in
`accuracy::estimators::local_guarantee`, and Stage 3 rejects it for an
accuracy-targeted query until one exists. How each input row updates the
summary is decided in `logical_candidates::summary_update`. The executor must
also be able to build and read it (`DeploymentCapabilities`).
