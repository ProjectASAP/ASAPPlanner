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

One legal Post-ASAP alternative for answering an intent. Planner preserves alternatives until its selection, which uses the deployment's cost model, rather than committing to one early.

## Summary

Maintained state, exact or approximate, that can answer some query intent more efficiently than raw data. A sketch is one kind of approximate summary.

## Physical plan

A precompute DAG and a query DAG of physical operators, with the lifecycle, window framework and retention of every stored output. Planner compiles and selects it; the deployment binds its inputs, places, stores and executes it.
