# Planning a workload

*This chapter is an outline; its content follows.*

This chapter plans one PromQL query over a data workload from start to
finish and reads what the planner returns. It is for users who call the
planner as a library and want the selected plan, its cost and the accuracy
each query gets.

- The query workload: a [`PlanningWorkload`](crate::PlanningWorkload) holding
  a `QueryWorkload` of `QueryWorkloadEntry` values, each a query with its
  `QueryRequirements`, predictability and recurrence.
- The data workload: a `DataWorkload` describing the series the queries
  read, their rate and their labels.
- The call: [`UserInput::new`](crate::UserInput::new) with
  [`FrontendInput::Promql`](crate::FrontendInput::Promql) and
  `PlanningModels::builtin()`, then
  [`plan_installable`](crate::plan_installable).
- Reading the result: [`InstallablePlan`](crate::InstallablePlan) holds the
  selected [`PhysicalASAPDAG`](crate::PhysicalASAPDAG) (`dag`, whose
  `roots[i]` answers query `i`) and the `query_texts`;
  [`SelectedPlanReport`](crate::SelectedPlanReport) holds its
  `cost_per_second` and `predicted_usage`.
- The full selection report: [`run_planning`](crate::run_planning) returns a
  [`PlanningRun`](crate::PlanningRun) whose `selection` holds the plan and its
  [`Selection`](crate::Selection): `selected_plan_cost_per_second` and
  `accepted_accuracy_per_query` ([`AcceptedAccuracy`](crate::AcceptedAccuracy)).
- Seeing the same plan from the command line: `asap-planner --output text`.
