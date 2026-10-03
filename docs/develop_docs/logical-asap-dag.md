# Logical ASAP DAG transport

This interface exports the unified operator/scalar IR at the logical stage of
[planner layering](../design_docs/proposals/planner-layering.md). It describes
what to compute, including committed summary families, before physical planning
chooses implementations and materialization.

## Interface

`asap_types::ir::export` exposes:

- `compile_logical_asap_dag(&Rc<OperatorNode>)` for a flat `LogicalASAPDAG`.
- `compile_logical_asap_query(&QueryRoot)` for operator or standalone scalar roots.
- `compile_logical_asap_dag_with_node_ids(...)` for that DAG and a compiler-local
  `LogicalASAPNodeIdentityMap` with `node_id` and `operator_node` lookups.
- `LogicalASAPDAGDocument::new(dag)` and `validate()` for the versioned transport
  envelope. Logical wire version 1 is distinct from the older phase-assigned
  post-ASAP format.

The compiler first checks the in-memory DAG's structural contracts. Untimed
ordinary plans and summary plans are valid inputs. Export does not assess
accuracy against request requirements or select a physical plan.

Each `LogicalASAPDAGNode` contains an ID, operator payload, result kind, output
schema and optional accuracy guarantee. Each `LogicalASAPDAGEdge` contains
producer/consumer IDs, input role, intermediate schema and grouping compatibility.
The DAG has one semantic operator or scalar root. A standalone scalar constant
needs no synthetic operator node. IDs are local to one export.

Scalar expressions remain owned by their operators. Their wire representations
replace explicit operator references with IDs. `ScalarRef` edges record those
producer dependencies. A shared operator is exported once even when ordinary
inputs and scalar expressions both reference it.

## Physical boundary

Logical nodes and edges contain no execution state, assigned timing, storage tier,
retention or pane-alignment assertion. Physical planning chooses, for each eligible
sub-DAG, no materialization, query-time materialization, or ingestion-time
materialization. Execution timing follows that choice and its dependencies.

The optional `OperatorNode.timing` field belongs to the common IR and may later
record a physical assignment; it is not part of logical transport. There is no
intermediate timed-DAG stage. Physical planning owns phase validation and any
splitting needed when shared consumers require incompatible execution contexts.

Transport validation checks graph identity, connectivity, acyclicity, edge schemas
and declared summary family/grouping metadata. Full scalar/operator typing remains
an in-memory structural validation responsibility. Neither check proves runtime
capability, cost, response latency or accuracy feasibility.
