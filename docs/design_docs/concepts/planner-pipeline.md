# Planner pipeline

ASAPPlanner accepts a planning workload and produces the selected logical
Post-ASAP plan, through the #509 stage pipeline.

    SQL / PromQL / MetricsQL queries
            | parse and canonicalize
            v
    Pre-ASAP IR: exact, language-independent query intent
            | Stage 1: list summary-aware alternatives per target (Pass 1)
            |          and sharing variants across queries (Pass 2)
            v
    Stage 2: physical candidates (materialization)
            v
    Stage 3: accuracy and capability checks, pricing, selection
            v
    Selected Post-ASAP plan + selection report

All physical binding, deployment, and execution remain downstream
responsibilities.

The [library API](../../develop_docs/library-api.md) shows the calls, and
[input, output, and workflows](../architecture/input-output-workflow.md)
defines the workload inputs.

The [Pre-ASAP IR](pre-asap-ir.md) captures what a query means without any summary implementation. The [Post-ASAP IR](post-asap-ir.md) represents the same intent using possible ASAP primitives. See the [architecture overview](../architecture/README.md) for the full component flow and boundary details.
