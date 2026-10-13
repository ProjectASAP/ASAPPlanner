# ASAP-Aware Mapping architecture

This document explains how the planner turns logical query operations into
alternative realizations built from ASAP primitives, such as exact accumulators
and approximate sketches, and selects among them. A **realization** is one
candidate form of one logical operation, not a selected workload plan or a
deployed executable.

The design is the #509 planning stages
([planner layering](../design_docs/proposals/planner-layering.md)); the
[library API](library-api.md) shows how to call them.

> **Historical:** the legacy replacement search (`ReplacementStrategy`,
> `search_workload`, `CostModel`) that this document used to describe was
> removed (#630, #635); the [contracts](asap-aware-mapping-contracts.md) and
> [extension guide](extend-asap-aware-mapping.md) are kept as a record of it.

## Terms

- A **workload** is the set of named queries planned together. A **query root**
  is the top-level `QueryRoot` (an `Rc<OperatorNode>` operator DAG or a scalar
  expression over them) of one query. **Pre-ASAP** means a DAG of ordinary
  `NonASAPOp` operators; **post-ASAP** means the same IR after some nodes became
  `ASAPOp` summary operators.
- A **computation** (`ComputationTarget`) is one measure of an aggregate whose
  intent has a realization besides plain exact execution. A **candidate** is
  one `ComputationCandidate` for it: `Exact` (the original sub-DAG),
  `ExactAccumulator` (a mergeable exact state), `AvgAsSumCount` or
  `Summary` (an approximate summary family, sized for the accuracy target when
  a candidate is composed).
- An **accuracy target** states the allowed error and failure probability.

## The stages

```text
pre-ASAP roots (Stage 0, query-language frontends)
  -> Stage 1, logical ASAP-aware optimization, Pass 1: the candidates of every
     computation (per_computation_candidates, realization); then one composed
     whole-workload candidate per choice of a candidate for every computation
  -> Stage 2, physical ASAP-aware optimization: one physical candidate per
     composed logical candidate (stage2_physical_asap_optimization); every
     sub-DAG is NotMaterialized for now
  -> Stage 3, plan selection: reject candidates whose query output is not shown
     to meet its accuracy target; price the rest and select the cheapest
     (stage3_plan_selection)
```

Pass 2 (ASAP-aware CSE) is not implemented yet.

| Concern | Location |
| --- | --- |
| Which realizations an intent has, sizing, summary input rules | `asap_aware_mapping::realization` |
| Pass 1 candidates and composition of one choice per computation | `asap_aware_mapping::per_computation_candidates` |
| Analytical error bounds of each summary family | `asap_aware_mapping::accuracy` |
| Physical candidates | `asap_aware_mapping::stage2_physical_asap_optimization` |
| Accuracy check, pricing and selection | `asap_aware_mapping::stage3_plan_selection` |
| All stages for one workload; the default optimization pass | `asap_aware_mapping::pass` (`run_stage1_to_stage3`, `Stage1ToStage3Pass`, `optimize`) |
| Facade | `asap_planner::e2e_plan` |

Stage 1 never prices: candidate generation is independent of cost. It does not
rank candidates either; the catalog order of `summary_candidates` has no
preference meaning. Stage 3 is the only stage that computes cost. It checks
each query output's guarantee with the `AccuracyModel` in `PlanningModels`, or
accepts the deployment's evidence (`AccuracyEvidenceProvider::output_guarantee`).

## Adding a realization

A new summary algorithm for an intent is added to `summary_candidates` and
sized in `accuracy::estimators` (through `realization::default_size_params`);
its guarantee goes in the accuracy model's `local_guarantee`, and Stage 3
rejects it for an accuracy-targeted query until one exists. How each input row
updates the summary is decided in `per_computation_candidates`
(`summary_update`). Stage 2 must also be able to implement it as a physical
operator.
