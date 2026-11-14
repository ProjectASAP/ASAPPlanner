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

## Memo

The memo groups (the distinct logical sub-expressions of the workload), each query's root over them, and each group's candidate implementations. Stage 1 builds it and Stage 2 implements it. Code: `Memo`, `MemoGroup`, `CandidateForMemoGroup` (`asap_types::stages`).

## Summary

Maintained state, exact or approximate, that can answer some query intent more efficiently than raw data. A sketch is one kind of approximate summary.

## Physical plan

A concrete execution and deployment choice: runtime topology of data lifecycle stages, placement of computation, transmission, storage. Physical plans are owned by downstream systems.

## Materialized view

A node of the physical DAG whose output is materialized across evaluations (lifetime `AcrossEvaluations`, at ingestion time or at query time), with how long it is materialized (`retention_ms` on the node). A node materialized for `OneEvaluation` is not a view: its output is discarded after one evaluation. Code: `MaterializedView`, `PhysicalASAPDAG::materialized_views`.

## Raw data sample

One timestamped value of a time series as it arrives, such as one Prometheus Remote Write sample. Raw data samples are the rows a time-series scan reads. Code names them with `raw_data_sample`, e.g. `RawDataSample`.

## Summary sample

The subset of input rows a sampling summary family, such as a reservoir, keeps as its state. Code names it `SummarySample` / `summary_sample`, e.g. `FieldDataType::SummarySample(SamplingKind, SamplingParams)`. A summary sample is not a raw data sample, and the rows it is drawn from need not be raw data samples.
