# ASAPPlanner Layering Design

Status: proposal. Audience: designers and developers of ASAPPlanner and of deployments such
as ASAPQuery-backend.

## Goal

ASAPPlanner takes a [query workload](https://github.com/ProjectASAP/ASAPPlanner/blob/main/crates/types/src/workload.rs), a [data workload](https://github.com/ProjectASAP/ASAPPlanner/blob/main/crates/types/src/workload.rs#L531) and the [deployment's
inputs](TODO: A data structure should be explicitly defined in another PR), and returns one optimal physical plan. It decides what is computed, how
it is computed, and which plan is best. The deployment only supplies inputs and
executes the plan: it supplies its own empirical cost estimation, empirical accuracy estimation and capabilities of deployment but never does the query planning or plan selection.

## Layers

```text
 Query workload
   (PromQL / SQL / MetricsQL,
    query recurrence,
    accuracy requirements,
    latency requirements)

 + Data workload
   (streaming vs. data at rest,
    data distribution,
    cardinality)

 + Deployment inputs
   (empirical cost model,
    empirical accuracy model,
    deployment capabilities)
                         │
                         ▼
┌────────────────────────────── ASAPPlanner ──────────────────────────────┐
│                                                                        │
│ 0. Language-specific frontends                                                           │
│    Parse and convert source-language queries into a common logical       │
│    representation. Reject unsupported query expressions.               │
│                                                                        │
│    Output: CandidateLogicalDAGs                                        │
│                                                                        │
│                         │                                              │
│                         ▼                                              │
│ Logical planning — what to compute                                     │
│                                                                        │
│ 1. Logical ASAP-aware optimization                                     │
│    Explore semantically equivalent and legal logical candidates:       │
│                                                                        │
│      summary families                                                  │
│      × query rewrites                                                  │
│      × sharing one summary across multiple computations                │
│                                                                        │
│    Output: CandidateLogicalASAPDAGs                                    │
│                                                                        │
│                         │                                              │
│                         ▼                                              │
│ Physical planning — how to compute                                     │
│                                                                        │
│ 2. Physical ASAP-aware optimization                                    │
│    Explore executable implementations of each logical candidate:       │
│                                                                        │
│      materialization decisions                                         │
│      × physical operator implementations                               │
│      × parallelism and partitioning                                    │
│      × resource management                                             │
│                                                                        │
│    Output: CandidatePhysicalASAPDAGs                                   │
│                                                                        │
│                         │                                              │
│                         ▼                                              │
│ 3. Plan selection                                                      │
│    Evaluate complete physical candidates using the deployment's        │
│    empirical cost and accuracy models. Reject candidates that violate  │
│    accuracy, latency, or capability constraints.                       │
│                                                                        │
│    Choose the cheapest valid plan for the whole workload.         │
│                                                                        │
└────────────────────────────────┬───────────────────────────────────────┘
                                 │
                                 ▼
                    one selected PhysicalASAPDAG
                                 │
                                 ▼
┌────────────────────────────── Deployment ───────────────────────────────┐
│                                                                        │
│ 4. Execution                                                           │
│   deployment executes the selected DAG (plan).                         │
│                                                                        │
└────────────────────────────────────────────────────────────────────────┘
```


## Layers/DAGs and what decisions each layer makes


Stages 0 to 2 each output a candidate set holding every semantically equivalent and legal candidate DAG of that stage; stage 3 is the only step that chooses one candidate DAG as output. Candidate sets are internal to ASAPPlanner and may be shared or enumerated lazily. A stage may prune a candidate early only when it is provably inadmissible (for example, a summary family that cannot meet the query's accuracy target), and every rejected candidate carries a reason.

| Stage | Input | Decides | Output |
|---|---|---|---|
| 0. Frontends | Each `QueryWorkloadEntry.query` and `QueryWorkload.language` | Language semantics and converting source-language queries into a common logical representation. Rejects constructs it cannot represent faithfully. | `CandidateLogicalDAGs`; nodes are logical operations with no summary operations |
| 1. Logical ASAP-aware optimization | Logical query DAGs, each query's accuracy requirements across the workload | **Pass 1, per subDAG:** replace subDAGs with summary families, apply query rewriting rules and generate candidates. **Pass 2, across subDAGs:** detect Common Subexpression with ASAP awareness, meaning CSE with exactly the same subexpression, one summary type can support multiple computation nodes and replace the per-node summary with one shared summary (e.g., UnivMon supporting distinct counting, entropy, L2 norm node), window-overlap patterns (sliding windows, sub-interval windows) and replace the per-node window with one shared window primitive (sliding window, tumbling window, Exponential Histogram). | `CandidateLogicalASAPDAGs`; nodes include summary and window-primitive operations |
| 2. Physical ASAP-aware optimization | Logical ASAP DAGs, each entry's `recurrence` and `predictability`, the `DataWorkload` | **Materialization:** for subDAGs, whether it is materialized and therefore computed at ingestion time or query time, and how long it is retained. **Lowering:** physical operators for every node, and the cut into a precompute DAG and a query DAG. **Parallelism, partitioning, resources:** TODO. | `CandidatePhysicalASAPDAGs` |
| 3. Plan selection | Physical ASAP DAG candidates, `requirements`, the deployment's cost model, accuracy model and capabilities | Rejects candidates that miss an accuracy target, a latency bound or a capability; picks the cheapest admissible plan for the whole workload. A shared state is costed once with all its consumers' demand. Unknown cost is never selected. | One `PhysicalASAPDAG` |
| 4. Execution (deployment) | The selected `PhysicalASAPDAG` | Binds raw samples and stored states to the plan's inputs, runs ingestion, storage, precomputation and query-time computation. | Query results |

## End-to-End Example for Layers and Decision Space for Logical and Physical ASAP-aware Optimization
Every example below is a PlanningWorkload in the serialized form of [workload.rs](https://github.com/ProjectASAP/ASAPPlanner/blob/main/crates/types/src/workload.rs). Times and durations are milliseconds.

TODO: write concrete query expressions, workload pattern based on the workload data structure, data workload based on the data structure definition per subsection. 
### Aggregation over dimensions and motivation for summary replacement decisions in logical planning

sum by (job) (rate(data[1m])) as an example 

logical asap-aware  planning can represent sum with exact aggregation SUM.

topk by (job) (sum_over_time(data[1m])), logical planning asap-aware can represent this as Count-Min Sketch with heap per job row, or Hydra group-by for the entire job column. 



### Aggregation over windows and motivation for summary replacement and sharing one summary for multiple computation nodes in logical planning

query can be quantile aggregation of past 5 year data, past 1 year data, past year between 1 - 2 years,  2 to 3, 2 to 5 etc. And during logical asap-aware planning stage, these aggregation over time windows can be mapped to KLL for quantile over these time windows. query can also be, querying data over every 5min windows, and repeating the query every 1min, as the repeatedness of the query workload. 

In the logical asap-aware planning stage, a second pass will detect that the window overlapping patten, e.g., it appears as sliding windows, or sub-interval window queries, and then the logical asap-aware planning stage, can map these batch queries' window overlapping pattern to a shared ASAP window primitive node, e.g., sliding window, Exponential Histogram for sub-interval window queries. 

### Aggregation over windows and motivation for materialization decisions in physical planning

query can be quantile aggregation of past 5 year data, past 1 year data, past year between 1 - 2 years,  2 to 3, 2 to 5 etc. And during logical asap-aware planning stage, these aggregation over time windows can be mapped to KLL for quantile over these time windows. query can also be, querying data over every 5min windows, and repeating the query every 1min, as the repeatedness of the query workload. 

In the logical asap-aware planning stage, a second pass will detect that the window overlapping patten, e.g., it appears as sliding windows, or sub-interval window queries, and then the logical asap-aware planning stage, can map these batch queries' window overlapping pattern to a shared ASAP window primitive node, e.g., sliding window, Exponential Histogram for sub-interval window queries. 

Note that the ASAP primitives with windows (e.g., sliding window, tumbling window, Exponential Histogram window frameworks) replacement to a logical query subDAG happens in the logical asap-aware planning stage, the decision of whether materializing the window summary nodes or not happens in the physical planning stage and thus decoupled from the window primitive candidate enumeration. 


