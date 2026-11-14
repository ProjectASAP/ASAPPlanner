# Background: why plan monitoring queries

*This chapter is an outline; its content follows.*

This chapter explains the problem ASAPPlanner solves and the vocabulary the
other chapters use. It is for anyone new to the planner, user or
contributor, and assumes only familiarity with a query language such as
PromQL or SQL.

- Repeated monitoring queries: dashboards and alerts run the same queries on
  a schedule over a growing data workload, so work can be shared and kept
  between runs.
- Sketches: a summary such as a Count-Min sketch or KLL answers a statistic
  approximately from a fraction of the state an exact answer needs
  (`SketchAlgorithm`, `StatisticKind`).
- Materialization: a computation may be not materialized, built at ingestion
  time, or built at query time (`ExecutionTiming`, `MaterializedView`).
- Memo group: one distinct sub-expression of the workload, shared by every
  query that contains it (`Memo`, `MemoGroup`).
- Candidate: one way to compute a memo group, exact or with a sketch, shared
  or reused (`CandidateForMemoGroup`).
- Plan: one candidate chosen per memo group, joined into the
  [`PhysicalASAPDAG`](crate::PhysicalASAPDAG) that Stage 3 selects as the
  cheapest plan meeting every query's requirements.
- The stages from workload to plan, in one picture: the
  [planner pipeline](https://github.com/ProjectASAP/ASAPPlanner/blob/main/docs/design_docs/concepts/planner-pipeline.md); every term
  is defined in the [glossary](https://github.com/ProjectASAP/ASAPPlanner/blob/main/docs/design_docs/concepts/glossary.md).
