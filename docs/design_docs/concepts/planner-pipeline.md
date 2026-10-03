# Planner pipeline

ASAPPlanner accepts a planning workload and produces `CandidateLogicalASAPDAGs`, a compact
representation of candidate Post-ASAP DAGs. Ranking and selection are operations
over that output, not mandatory stages of candidate search.

    SQL / PromQL / MetricsQL queries
            | parse and canonicalize
            v
    Pre-ASAP IR: exact, language-independent query intent
            | enumerate legal summary-aware alternatives
            v
    CandidateLogicalASAPDAGs: candidate Post-ASAP DAGs
            |
            +--> inspect candidates, optionally using cost_sorted
            +--> select and assemble logical DAGs

Stage 2 materialization (#509) will decide per sub-DAG whether to materialize
and whether at ingestion or query time; until then every summary runs at query
time. All physical binding, deployment, and execution remain downstream
responsibilities.

The [input, output, and workflows](../architecture/input-output-workflow.md)
document defines the public boundary and helper call order.

The [Pre-ASAP IR](pre-asap-ir.md) captures what a query means without any summary implementation. The [Post-ASAP IR](post-asap-ir.md) represents the same intent using possible ASAP primitives. See the [architecture overview](../architecture/README.md) for the full component flow and boundary details.
