# ASAPPlanner Supporting Different End-to-End Use Cases

We design ASAPPlanner system architecture as in [planner-layering.md](planner-layering.md).
Next, in this doc, we introduce how to make ASAPPlanner support a new use case, from the perspectives of 
* How to define the query workloads, and data workloads, based on the use case/application pattern.
* Based on the application pattern, how to determine the suitable ASAP Primitives to use.
  * If the capabilities of ASAP Primitives are missing in current ASAPPlanner, how to add them.
* How to integrate ASAPPlanner to a deployment runtime for that use case, so the system shows an end-to-end benefits.

## Contents

[TODO: just fill]

## Given a use case, how to define the query workloads and data workloads

- We need to run the use case and record the traces of the queries and data, and then
 - We can analyze the data shape (data cardinality, data distribution, dimensions, structured data or unstructured data but we can abstract the structured part out of it).
 - We can analyze the pattern of the queries, such as what parts of the queries have overlapping, redundant computation / data scanning, and what is the query intent, mapping to a potential ASAP Primitive family (see the ASAP Glossary).

## List of (Examples) Query Patterns Mapping to ASAP Primitives

## Process of Adding a New ASAP Primitive to ASAPPlanner 

## Integrating ASAPPlanner to the Use Case Deployment

### Workflow of ASAPPlanner and Deployment

### Key Interfaces between ASAPPlanner and Use Case Deployment


