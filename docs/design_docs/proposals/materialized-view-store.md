# Materialized view store

> **Status:** proposal. Today a deployment binds each materialized view slot
> through `BindingDataSources.materialized_views: BTreeMap<NodeId, Arc<dyn
> TableProvider>>`. This document proposes one store interface keyed by the
> view instead. The interface is Planner and executor work. How a store finds
> instances (§4) is a reference for deployments; ASAPQuery's SummaryStore
> design (`summary-catalog-sds-architecture.md` in ASAPQuery-backend) owns the
> actual implementation.

**Audience:** designers of the executor ↔ deployment boundary, and backend
developers implementing a summary store.

## 1. Problem

KLL p50/p99 over five 1-minute panes kept as materialized views (the running
example of [Physical planning and deployment](../physical-planning-and-deployment.md)).
The installed DAG expands the pane chain into five pane nodes, `pane_0` …
`pane_4`. Each is a materialized view, and all five have the same
`coverage.definition_id`.

Today, before every query run, a deployment turns those node ids into
providers:

```rust
let mut materialized_views = BTreeMap::new();
for view in plan.materialized_views()? {
    let states = store.states_for(&view.coverage);
    materialized_views.insert(view.node_id, Arc::new(StoreProvider::new(states)) as Arc<dyn TableProvider>);
}
query.instantiate(&BindingDataSources { tables, materialized_views })?;
```

The store's real question is "read the stored states of definition D for time
range R". The current shape has three problems:

- **Node ids leak into the deployment.** The store is keyed by
  definition and interval, so node ids are only routing.
- **`TableProvider` does not fit a view.** A view's rows are not re-filtered,
  its `projection` and `predicates` are always empty, and its states are not
  charged to the run's memory. Implementing a table trait invites a provider
  to ignore the time range, which the contract then forbids in prose.
- **The map is rebuilt every run,** although node → view is fixed once a plan
  is installed.

## 2. Components and ownership

| Component | Holds | Owner |
| --- | --- | --- |
| Plan file | `InstallablePlan`: the DAG, query texts, and from them `materialized_views()` and `raw_data_retention()` | Planner |
| Executor | compiled DAGs; one materialized view slot per view a query DAG reads, each knowing its `MaterializedView` | `asap-executor` |
| Materialized view store | resolves a read of a view for a time range to stored states | deployment |
| Summary catalog and instances | definitions (descriptors) and instances (interval, group values, completeness, placement, state reference, status) | deployment |

Only definitions cross the interface. Instances never do:

| SDS part | Crosses the interface? | As |
| --- | --- | --- |
| Summary Descriptor (operator, parameters, fidelity, state schema) | yes, as the Planner states it | `MaterializedView.coverage.definition_id` and the slot's schema |
| Data Descriptor (source, population) | yes, as the Planner states it | `coverage.selection`; raw sources in `inputs.raw_data_windows` |
| Summary Instance (interval, group values, completeness, placement, `state_reference`, status, lifecycle) | **no** | resolved inside the store |

The time range in a read is a predicate the store evaluates against instance
intervals. No instance identity, location or status reaches the executor or
the Planner.

## 3. Interface

```rust
/// The deployment's stored materialized views, read by the executor.
pub trait MaterializedViewStore {
    /// The stored states of `view` for this run: the instances of
    /// `view.coverage` whose pane ends in `time_range_ms` (`(start, end]`).
    /// Rows are in the view's schema: group columns and one state column.
    fn read(
        &self,
        view: &MaterializedView,
        time_range_ms: (i64, i64),
        context: RunContext,
    ) -> Result<OutputStream<'_, Batch>, Error>;
}

pub struct BindingDataSources {
    pub tables: DataSources,                                // Source → TableProvider (unchanged)
    pub materialized_views: Arc<dyn MaterializedViewStore>, // was BTreeMap<NodeId, Arc<dyn TableProvider>>
}
```

- **Executor side.** `compile` records, for each materialized view slot, the
  `MaterializedView` it stands for. At run time the slot computes
  `time_range_ms`. For a pane view (one range `TimeRange` under it), that is
  `RawDataWindow::bounds_ms(T)` of its window. For any other view, it is
  `(T - lookback, T]`, where `lookback` is the largest `lookback_ms` over the
  view's raw-data windows and the views it reads. A time range is therefore
  always given; there is no `None`. The slot then calls `read`, checks the
  schema, and marks the states as held outside the run's memory limit.
- **Contract.** `read` returns exactly the instances that cover
  `time_range_ms` for the view's definition. The executor does not re-filter
  them, so the store must:
  - never count an observation twice;
  - never return an instance of another definition;
  - never treat missing state as empty.

  If it cannot prove that, it returns an error. The deployment decides the
  fallback, for example exact execution.
- **Snapshot.** All reads of one run see the same store revision. `RunContext`
  carries it (`Scope::Query { revision, .. }`).

## 4. How a store resolves a read (reference algorithm)

### 4.1 At install

For each `view` in `plan.materialized_views()`:

1. **Definition.** Map `view.coverage.definition_id` to the store's summary
   definition. The Summary Descriptor comes from the view node's operator and
   state schema. The Data Descriptor comes from `view.inputs` (sources) and
   the non-time part of `view.coverage.selection` (value sets and ranges).
   Equal `definition_id` and equal non-time selection mean the same
   definition. That is how panes of one chain, and identical views of
   different plans, share instances.
2. **Lifecycle.** Keep each instance for `view.retention_ms` behind the newest
   evaluation time.
3. **Raw data.** Keep raw data per `plan.raw_data_retention()`.
4. **Refresh schedule.** For an `AtIngestion` view, schedule a refresh per
   pane: `compile(dag, {}, [view.node_id])`, run in an ingestion run over
   `(end - width, end]` of its raw-data window. Publish the result as an
   instance with interval `(end - width, end]`.

### 4.2 Per query run at evaluation time `T`

1. **Ensure on-first-read views.** For every `OnFirstRead` view the query DAG
   reads, compute its `time_range_ms` at `T` as the executor will (§3). If the
   store has no complete instance for it, run the view's refresh in a query
   run over the raw data, then publish it. Views are refreshed in
   `materialized_views()` order, so a view's inputs are refreshed first.
2. **Fix the snapshot.** Take one store revision for the whole run, and put it
   in `RunContext`.
3. **Instantiate and run** the query DAG with
   `BindingDataSources { tables, materialized_views: store }`. Each slot calls
   `read(view, time_range_ms, ctx)`.

### 4.3 `read(view, (start, end], ctx)`

1. **Resolve the definition** from `view.coverage` (§4.1, step 1) in the
   catalog generation the plan was installed with. If it is unknown, return an
   error.
2. **Select candidate instances** of that definition at the snapshot revision
   whose pane end is in `(start, end]`, across all group values. Exclude
   instances of an older catalog generation, retired instances and expired
   ones.
3. **Check coverage.** For each group value, the selected intervals must tile
   `(start, end]` exactly: no gap and no overlap.
   - A gap is acceptable only when the store can prove the group had no
     samples there (closed-input proof). Otherwise return an error.
   - An overlap, for example a replaced snapshot next to the original, keeps
     only the current instance.
4. **Check readiness.** Every selected instance is published and not fenced
   by admitted-but-unpublished input overlapping the range. Otherwise return
   an error, or wait, as the deployment chooses.
5. **Decode** each instance's state into rows of the view's schema (group
   columns and the state column), and stream them.

Cost: one catalog lookup plus a range scan over the definition's instances by
pane end. With instances indexed by `(definition, pane end)`, this is
O(log n + k) for k returned instances.

### 4.4 Running example

At `T = 12:05`, the query DAG reads `pane_0` … `pane_4`. Pane `i` sits under
`TimeShift(i·1m)`.

```text
pane_0.read((12:04, 12:05]) → instance kll-200-latency / (12:04,12:05]
pane_1.read((12:03, 12:04]) → instance kll-200-latency / (12:03,12:04]
…
pane_4.read((12:00, 12:01]) → instance kll-200-latency / (12:00,12:01]
NativeKllMerge merges the five states once → p50, p99
```

All five reads resolve to the same definition. Only the interval differs.

## 5. Alternatives considered

| Alternative | Why not |
| --- | --- |
| Keep `NodeId → TableProvider` (today) | Leaks node ids, rebuilds the map per run, and uses a table contract that does not fit (§1) |
| Key the store by `definition_id` only | Two views can share a definition but differ in value selection, and panes differ in time. The read needs the full `MaterializedView` and a time range |
| Let the executor call SDS directly | Puts storage identity and instance lifecycle in the executor; SDS must stay in the deployment |
| Pass instances in the plan file | Instances exist only at run time; a plan file cannot name them |

## 6. Open points

- **Boundaries that are not views.** `precompute::compile` composes fragments
  whose `Scan` frontiers are bound by node id, and some tests cut at a
  `TimeRange`. Either keep a crate-internal or `test-support` node-id binding
  for them, or give each such boundary a `MaterializedView`.
- **Interval convention.** The executor's windows are `(start, end]`, and
  ASAPQuery's instances record `[start, end)`. An instance's contents follow
  the refresh DAG that built it. So the store must match by pane end and
  width, and record intervals with the executor's convention or convert them
  exactly.
- **Group completeness.** §4.3 step 3 needs the store to know which groups
  exist. For a closed population this is exact. For an open one, the store
  needs a closed-input proof per group, or returns an error.

## 7. Migration

This is a public API change in `asap_executor`: the type of
`BindingDataSources.materialized_views` changes, and the materialized view
slot's request always carries a time range. ASAPQuery-backend does not depend
on `asap_executor` yet. Its integration implements `MaterializedViewStore`
against its SummaryStore directly.
