# Planner pipeline

ASAPPlanner accepts a planning workload and produces the selected Post-ASAP
plan through the planning stages (#509).

    SQL / PromQL / MetricsQL queries
            | Stage 0, query-language frontends: parse and canonicalize
            v
    Pre-ASAP IR: exact, language-independent query intent
            | Stage 1, logical ASAP-aware optimization: summary-aware
            |   alternatives per computation (Pass 1), composed per query
            v
    Stage 2, physical ASAP-aware optimization: physical candidates
            v
    Stage 3, plan selection: accuracy checks, pricing, selection
            v
    Selected plan + selection report

Stage 2 does not yet decide materialization: every sub-DAG runs at query time.
Pass 2 (ASAP-aware CSE) is not implemented yet. All physical binding,
deployment, and execution remain downstream responsibilities.

The [library API](../../develop_docs/library-api.md) shows the calls, and
[input, output, and workflows](../architecture/input-output-workflow.md)
defines the workload inputs.

The [Pre-ASAP IR](pre-asap-ir.md) captures what a query means without any summary implementation. The [Post-ASAP IR](post-asap-ir.md) represents the same intent using possible ASAP primitives. See the [architecture overview](../architecture/README.md) for the full component flow and boundary details.
