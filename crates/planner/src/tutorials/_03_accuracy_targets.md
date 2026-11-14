# Accuracy targets

*This chapter is an outline; its content follows.*

This chapter shows how a query states how accurate its answer must be, and
how that choice decides between an exact computation and a sketch. It is for
users who want cheaper plans in exchange for a bounded error.

- Where a target lives: `QueryRequirements::accuracy` on each
  `QueryWorkloadEntry`; the default is exact.
- The builder: `AccuracyTarget::exact()`, or
  `AccuracyTarget::max_error(ε)` refined with `.failure_probability(δ)`,
  `.accuracy_metric(AccuracyMetric)` and `.accuracy_scope(AccuracyScope)`;
  the same target in JSON.
- The three metrics in `AccuracyMetric` and what ε means in each.
- Exact versus sketch: one quantile query planned with
  `AccuracyTarget::exact()` and again with a loose target, and the plans and
  costs side by side.
- Reading what was accepted: [`AcceptedAccuracy`](crate::AcceptedAccuracy) in
  [`Selection`](crate::Selection), and why a target no candidate meets makes
  selection fail with
  [`ErrorSelectingPlan::NoValidCandidate`](crate::ErrorSelectingPlan::NoValidCandidate).
