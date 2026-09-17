# Planner pipeline

ASAPPlanner accepts a workload of source-language queries and produces a set of legal, ranked logical alternatives. It does not select, deploy, or execute the final physical plan.

    SQL / PromQL / MetricsQL queries
            | parse and canonicalize
            v
    Pre-ASAP IR: exact, language-independent query intent
            | enumerate legal summary-aware alternatives
            v
    Post-ASAP IR: logical candidates using ASAP primitives
            | rank with available cost and accuracy evidence
            v
    Downstream physical planner: choose, deploy, and execute

The [Pre-ASAP IR](pre-asap-ir.md) captures what a query means without any summary implementation. The [Post-ASAP IR](post-asap-ir.md) represents the same intent using possible ASAP primitives. See the [architecture overview](../architecture/overview.md) for the full component flow and boundary details.
