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
- [`CostInputs`](crate::CostInputs): its [`CostModel`](crate::CostModel)
  (cost model 1, 2 or 3, with [`ResourceUnitCost`](crate::ResourceUnitCost)),
  [`ResourceSpeed`](crate::ResourceSpeed) and measured operation costs, set
  through `PlanningModels::with_cost_inputs`.
- One query, several plans: the same query planned with and without raw
  data retained, and under two cost models, with each selected plan and its
  cost.
- Why a candidate lost: reading [`Selection`](crate::Selection) and
  [`RejectionCause`](crate::RejectionCause).
