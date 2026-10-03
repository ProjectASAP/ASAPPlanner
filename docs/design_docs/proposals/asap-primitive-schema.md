# Schema and Physical Data for ASAP Primitives

This document is the single source of truth for the schema, and column design for ASAP Primitives. This is used in the logical stage (LogicalASAPDAG), and physical stage (PhysicalASAPDAG). 

## 1. Goal, problem, and requirements

Unlike existing Database engines, which work on raw data or explicitly defined materialized tables with schema and column names provided by the users, ASAPPlanner is designed for querying and execution over the mix of raw data and ASAP Primitives. ASAP primitives are usually compact summaries over raw data. Therefore, it introduces new requirement when we design the schema and node definitions for LogicalASAPDAG and PhysicalASAPDAG.

Assuming we have the Logical DAG defined for a canonicalized representation for a batch of queries. [TODO: add links for this here. ]
The LogicalASAPDAG will share/reuse the NonASAP operator and ScalarExpr nodes in LogicalDAG [TODO: link PR 511's doc here], but replacing some operators in LogicalDAG with the operators operated with ASAP Primitives: SummaryCreation?, SummaryUpdate, SummaryMerge, SummaryDelete, SummarySubtraction, SummaryEstimate [TODO: check what is the complete list or discuss with others about the list]. 
Each of the Summary operators also require the ASAP primitive information above to inter-operate correctly, preserving semantic correctness. 

Basically, the following information should be represented to preserve the equivalent query semantics when we introduce ASAP Primitives to logical query representation, and following physical one. 

- What type of the ASAP Primitive is
- What is the ASAP Primitive parameters
- What data sources a ASAP primitive summarizes
- What query intent the summarized ASAP Primitive can support, e.g., statistical aggregation intents, time window aggregation intents



And these information will be combined with relational or time series query operator information, such as group by/reduction, filtering, projection, join, time series selection, together. 

Therefore, these requirements drive the following schema and metadata, node information, and column design. 



## 2. Existing database terminology for schema, table, column, and physical data layout


## 3. Proposed schema design 
Schema represents the **metadata** of information flow along an **edge** between two nodes in a logical or physical DAG. The schema field is associated with the node in the DAG. The consumer of the node in the DAG takes the schema from the producer node as input. 

Schema definition here is shared between LogicalDAG, LogicalASAPDAG, and PhysicalASAPDAG. The schema contain fields, and each field is mapping to a column in the physical data representation. 
Based on our requirement, each field should contain the following information.
1. **What type of the ASAP Primitive is** A state column can be a raw data type (e.g., numerical number, string). It can also be a [summary type](TODO: add link), e.g., the summary family is sketch, and the sketch type is quantile KLL sketch algorithm, and KLL sketch has K  as parameter as the schema. (TODO: confirm the terminology with corresponding code/doc)  It has a family, an algorithm and parameters. 
2. **What query intent the summarized ASAP Primitive can support, e.g., statistical aggregation intents, time window aggregation intents** This information is being mapped based on the primitive type. 


## 4. Proposed Node field design 

A node in the physical data will represent the data or summary instance, so a node has a field for **What data sources a ASAP primitive summarizes**.

Based on the above the proposed OperatorNode interface is as below:
```rust
```

## 5. Examples on how OperatorNode, schema, and physical data information are being used with Summary operators

Given that these information requirements are introduced by summary operators to work correctly semantically, we show the examples of how the defined OperatorNode, schema, and physical data information work with each kind of summary operators. 

