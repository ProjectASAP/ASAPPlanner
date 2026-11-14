# Executing a plan

*This chapter is an outline; its content follows.*

This chapter takes the selected plan from the planner and runs it with
`asap-executor`, at ingestion time and at query time. It is for developers
building a deployment runtime on top of the planner.

- The input: an [`InstallablePlan`](crate::InstallablePlan) from
  [`plan_installable`](crate::plan_installable), its `dag` and the
  materialized views it installs (`InstallablePlan::materialized_views`).
- Compile: `asap_executor::physical_planner::compile` turns the DAG, its views
  and its roots into a `CompiledPhysicalDAG`, opening no reader.
- Instantiate: `CompiledPhysicalDAG::instantiate` binds every raw data and
  materialized view slot to a `DataStore`, here an `InMemoryDataStore`.
- Execute: `PhysicalDAG::execute` with a `RunContext` whose `Scope` is
  `Ingestion` (to build views) or `Query` (to answer a query at an
  evaluation time).
- Reading the results: the returned batches, one per root, and checking them
  against the exact answer.
