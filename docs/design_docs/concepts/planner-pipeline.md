# Planner pipeline

ASAPPlanner accepts a planning workload and produces `PlanSpace`, a compact
representation of candidate Post-ASAP DAGs. Ranking and selection are operations
over that output, not mandatory stages of candidate search.

    SQL / PromQL / MetricsQL queries
            | parse and canonicalize
            v
    Pre-ASAP IR: exact, language-independent query intent
            | enumerate legal summary-aware alternatives
            v
    PlanSpace: candidate Post-ASAP DAGs
            |
            +--> inspect candidates, optionally using cost_sorted
            +--> select and assemble logical DAGs
            +--> select and assemble with summary-maintenance lifecycle decisions

The last two branches are alternatives: use the summary-maintenance-lifecycle-aware
workflow when Planner owns maintenance-versus-recomputation decisions; otherwise
the backend owns them. All physical binding, deployment, and execution remain
downstream responsibilities.

The [input, output, and workflows](../architecture/input-output-workflow.md)
document defines the public boundary and helper call order.

The [Pre-ASAP IR](pre-asap-ir.md) captures what a query means without any summary implementation. The [Post-ASAP IR](post-asap-ir.md) represents the same intent using possible ASAP primitives. See the [architecture overview](../architecture/README.md) for the full component flow and boundary details.
