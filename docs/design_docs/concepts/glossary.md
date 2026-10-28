# Glossary

## Query language

A source syntax such as SQL, PromQL, or MetricsQL.

## Query intent

The semantics of what a query requests, independent of the source language. Different query strings can express the same intent.

## Query workload

A set of queries considered together. It can describe a batch of queries or queries that recur while data arrives.

## Pre-ASAP IR

The common, exact representation of query intent. It contains no ASAP primitive or summary choice.

## Post-ASAP IR

A logical plan representation that can contain ASAP primitives, alongside relational and time-series operations.

## Candidate

One legal Post-ASAP alternative for answering an intent. Planner preserves alternatives for downstream selection rather than committing to one.

## Memo group

One distinct sub-expression of the workload, shared by every query that contains it, as in the Cascades/Volcano memo. Not a GROUP BY group. Code: `MemoGroup`, in `LogicalMemo` and `CandidateMemo`. See the [name decoder](../architecture/system-architecture.md#3-name-decoder) for the related candidate structures.

## Implementation candidate

One way to implement a memo group over its child groups, produced by Pass 1 or a Pass 2 rule (Cascades calls it a group expression). Code: `ImplementationCandidate`.

## Physical candidate

In Stage 3's search, one executable way to implement a memo group, with its own cost and input slots (`PhysicalCandidate`). A whole-workload physical plan is a `PhysicalWorkloadPlan`.

## Summary

Maintained state, exact or approximate, that can answer some query intent more efficiently than raw data. A sketch is one kind of approximate summary.

## Physical plan

A concrete execution and deployment choice: runtime topology of data lifecycle stages, placement of computation, transmission, storage. Physical plans are owned by downstream systems.

## Materialized view

A node of the physical DAG whose output is kept across evaluations (`MatAtIngestion` or `MatAtQueryKeptAcrossEvaluations`), with how long it is kept (`retention_ms` on the node). A `MatAtQuery` node is not a view: its output is discarded after one evaluation. Code: `MaterializedView`, `PhysicalASAPDAG::materialized_views`.

## Raw data sample

One timestamped value of a time series as it arrives, such as one Prometheus Remote Write sample. Raw data samples are the rows a time-series scan reads. Code names them with `raw_data_sample`, e.g. `WeightDomain::RawDataSamplesOfMetric`, `raw_data_samples_of_metric`.

## Summary sample

The subset of input rows a sampling summary family, such as a reservoir, keeps as its state. Code names it `SummarySample` / `summary_sample`, e.g. `FieldDataType::SummarySample(SamplingKind, SamplingParams)`. A summary sample is not a raw data sample, and the rows it is drawn from need not be raw data samples.
