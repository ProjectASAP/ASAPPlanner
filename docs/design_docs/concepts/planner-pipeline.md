# Planner pipeline

ASAPPlanner accepts a planning workload and the deployment's inputs, and
produces one selected physical plan for the whole workload. The
[planning stages design](../proposals/planner-layering.md) defines each
stage.

    SQL / PromQL / MetricsQL queries + data workload
            | Stage 0: query-language frontends
            v
    Logical DAG (Pre-ASAP IR): exact, language-independent query intent
            | Stage 1: logical ASAP-aware optimization
            |   Pass 1: exact or a summary per computation
            |   Pass 2: share, split into panes, or reuse across computations
            v
    Logical ASAP candidates (Post-ASAP IR, untimed)
            | Stage 2: physical ASAP-aware optimization
            |   materialization: not materialized, at ingestion time,
            |   or at query time (shared, or kept across evaluations)
            v
    Physical ASAP candidates (timed)
            | Stage 3: plan selection
            |   accuracy, latency and deployment checks; cost model 1, 2 or 3
            v
    Selected PhysicalASAPDAG, with every candidate's cost or rejection reason

Every stage keeps all its candidates; only Stage 3 compares them. The
deployment supplies its cost model, accuracy model and capabilities, and runs
the selected plan; physical deployment and execution stay downstream.

The [input, output, and workflows](../architecture/input-output-workflow.md)
document defines the public boundary and helper call order.
