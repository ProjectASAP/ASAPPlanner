# Deployment capabilities and cost

*This chapter is an outline; its content follows.*

This chapter shows that the plan depends on the deployment as well as the
queries: what it can store and run, and what resources cost. It is for
users integrating the planner into a deployment and for anyone asking why
the planner chose one plan over another.

- [`DeploymentCapabilities`](crate::DeploymentCapabilities): its
  `supported_summaries` ([`SupportedSummary`](crate::SupportedSummary)),
  `raw_data_retained` and `query_time_results_retained`, set through
  `PlanningModels::with_deployment_capabilities`.
- [`CostInputs`](crate::CostInputs): its [`CostModel`](crate::CostModel),
  [`ResourceSpeed`](crate::ResourceSpeed) and measured operation costs, set
  through `PlanningModels::with_cost_inputs`. Plan selection supports cost
  model 1 (`CostModel::UsageOverTime` with its
  [`ResourceUnitCost`](crate::ResourceUnitCost)); cost models 2 and 3 return
  `ErrorSelectingPlan::UnsupportedCostModel`.
- One query, several plans: the same query planned with and without raw
  data retained, and under different `ResourceUnitCost` and `ResourceSpeed`
  values, with each selected plan and its cost.
- Candidates Stage 3 ruled out before selection, each with its reason:
  [`Selection::pruned_candidates`](crate::Selection::pruned_candidates).
- When no plan is selected:
  [`ErrorSelectingPlan::NoValidCandidate`](crate::ErrorSelectingPlan::NoValidCandidate),
  with one `Rejection` (`id`, [`RejectionCause`](crate::RejectionCause),
  `reason`) per candidate.
