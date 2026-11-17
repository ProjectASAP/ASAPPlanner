//! One installed plan of the ASAP lane: its compiled runs and its store.

use std::sync::Arc;

use aqpbm_harness::engine::{
    Answers, EngineError, Footprint, InstalledArm, PlanSummary, SampleChunk, StepReport,
    WorkloadSpec,
};
use asap_executor::physical_planner::CompiledPhysicalDAG;
use asap_executor::runtime::{Limits, RunContext, Scope};
use asap_executor::sources::{DataStore, InMemoryDataStore};
use asap_executor::values::Batch;
use asap_planner::{InstallablePlan, MaterializedView, MaterializedViewRefresh, RefreshWindow};
use futures::executor::block_on;
use futures::StreamExt;

use super::time_series_batches::{answers_of, time_series_tables, SampleRouter, TimeSeriesTable};

/// The memory one run may reserve: no limit in practice, so the lane
/// measures the plan, not a limit of this runtime.
const RUN_MEMORY_LIMIT_BYTES: usize = 1 << 40;

/// An installed plan: the time series it reads, its ingestion refresh
/// groups and query run compiled once, and the store holding its raw
/// samples and stored views.
pub(super) struct InstalledPlan {
    plan: InstallablePlan,
    /// The workload query each root answers, in root order.
    query_ids: Vec<String>,
    tables: Vec<TimeSeriesTable>,
    router: SampleRouter,
    refreshes: Vec<GroupRefresh>,
    query_run: CompiledPhysicalDAG,
    /// Shared with each run while it runs, held alone between steps.
    store: Arc<InMemoryDataStore>,
    /// The end of the samples ingested so far: the data start before the
    /// first chunk (the harness's samples lie in `(start, start + duration]`).
    ingested_until_ms: i64,
}

/// One ingestion refresh group: refreshed by one ingestion run per window
/// `(e - width, e]` with `e + phase ≡ 0 (mod width)`, whose roots are its
/// views. The phase is measured back from evaluation times on the width's
/// grid: the harness evaluates at multiples of the evaluation interval,
/// which plans made for evaluation phase 0 assume.
struct GroupRefresh {
    window: RefreshWindow,
    views: Vec<MaterializedView>,
    compiled: CompiledPhysicalDAG,
}

/// Bad input: the lane cannot run this plan, though the executor may.
fn unsupported(reason: impl std::fmt::Display) -> EngineError {
    EngineError(format!("unsupported by the ASAP lane: {reason}"))
}

/// The executor cannot compile this plan.
fn not_executable(reason: impl std::fmt::Display) -> EngineError {
    EngineError(format!("not executable: {reason}"))
}

fn run_failed(step: &str, reason: impl std::fmt::Display) -> EngineError {
    EngineError(format!("{step}: {reason}"))
}

impl InstalledPlan {
    /// Compiles `plan`'s query run and each ingestion refresh group's
    /// refresh, and binds an empty store: each time series the plan reads,
    /// kept as far back as its query-time `Scan`s read and the refreshes
    /// need. `query_ids` names the query of each root.
    pub(super) fn install(
        plan: InstallablePlan,
        query_ids: Vec<String>,
        workload: &WorkloadSpec,
    ) -> Result<Self, EngineError> {
        let query_run = plan.compile_query_run().map_err(not_executable)?;
        let groups = plan.ingestion_refresh_groups().map_err(not_executable)?;
        let mut refreshes = Vec::with_capacity(groups.len());
        for group in &groups {
            let compiled = plan
                .compile_ingestion_refresh(group)
                .map_err(not_executable)?;
            let first = group.views[0].node_id;
            let window = group
                .refresh_window
                .ok_or_else(|| unsupported(format!("view {first} has no bounded window")))?;
            refreshes.push(GroupRefresh {
                window,
                views: group.views.clone(),
                compiled,
            });
        }
        let views = plan.materialized_views().map_err(not_executable)?;
        if let Some(view) = views
            .iter()
            .find(|view| view.refresh() == MaterializedViewRefresh::OnFirstRead)
        {
            return Err(unsupported(format!(
                "view {} is refreshed on first read",
                view.node_id
            )));
        }
        let tables = time_series_tables(&plan.dag).map_err(unsupported)?;
        let retention = plan.raw_data_retention().map_err(not_executable)?;
        // A refresh reads the window ending at a chunk's end or before it,
        // so the raw samples it reads reach back one window and one chunk;
        // the harness's chunks are one evaluation interval each
        // (`ArmSchedule::for_workload`).
        let chunk_ms = workload
            .queries
            .iter()
            .map(|query| query.evaluation_interval_ms)
            .max()
            .unwrap_or(0);
        let refresh_lookback_ms = refreshes
            .iter()
            .map(|refresh| refresh.window.width_ms + chunk_ms)
            .max()
            .unwrap_or(0);
        let mut store = InMemoryDataStore::default();
        for table in &tables {
            let query_lookback_ms = retention
                .iter()
                .filter(|kept| kept.source() == &table.source)
                .map(|kept| kept.lookback_ms())
                .try_fold(0, |longest, lookback| Some(longest.max(lookback?)));
            let lookback_ms = query_lookback_ms.map(|ms| ms.max(refresh_lookback_ms));
            store = store
                .with_appended_raw_data(table.source.clone(), table.schema.clone(), lookback_ms)
                .map_err(unsupported)?;
        }
        let ingested_until_ms = workload
            .data
            .iter()
            .map(|record| record.description.start_ms)
            .min()
            .unwrap_or(0);
        let mut store = Arc::new(store);
        // The windows before the data that a view keeps, refreshed empty,
        // so a query shifted back past the data start reads windows with
        // no samples rather than missing ones. Example: a 10 s chain kept
        // 5 min, with data from 0, has empty windows ending at -290 s ...
        // 0 s before the first chunk.
        for refresh in &refreshes {
            let kept_ms = refresh
                .views
                .iter()
                .map(MaterializedView::retention_ms)
                .max();
            let from_ms = ingested_until_ms - kept_ms.unwrap_or(0) as i64;
            for end_ms in window_ends(refresh.window, from_ms, ingested_until_ms) {
                refresh_window(refresh, &mut store, end_ms)?;
            }
        }
        Ok(InstalledPlan {
            plan,
            query_ids,
            tables,
            router: SampleRouter::default(),
            refreshes,
            query_run,
            store,
            ingested_until_ms,
        })
    }
}

/// The store, between runs: no run holds it then.
fn store_between_runs(store: &mut Arc<InMemoryDataStore>) -> &mut InMemoryDataStore {
    Arc::get_mut(store).expect("no run holds the store between steps")
}

/// Refreshes `refresh` for the window ending at `end_ms`: one ingestion run
/// whose roots are its views, each view's output appended to `store`.
fn refresh_window(
    refresh: &GroupRefresh,
    store: &mut Arc<InMemoryDataStore>,
    end_ms: i64,
) -> Result<(), EngineError> {
    let start_ms = end_ms - refresh.window.width_ms as i64;
    let scope = Scope::Ingestion {
        window_start_ms: start_ms,
        window_end_ms: end_ms,
    };
    let outputs = run_ingestion(&refresh.compiled, store, scope)
        .map_err(|error| run_failed(&format!("refresh of ({start_ms}, {end_ms}]"), error))?;
    let store = store_between_runs(store);
    for (view, batches) in refresh.views.iter().zip(outputs) {
        store
            .append_materialized_view(view, (start_ms, end_ms), batches)
            .map_err(|error| run_failed("storing a view", error))?;
    }
    Ok(())
}

/// `compiled` run in ingestion `scope` over `store`: each root's batches.
fn run_ingestion(
    compiled: &CompiledPhysicalDAG,
    store: &Arc<InMemoryDataStore>,
    scope: Scope,
) -> Result<Vec<Vec<Batch>>, asap_executor::Error> {
    let instance = compiled.instantiate(Arc::clone(store) as Arc<dyn DataStore>)?;
    let context = RunContext::new(scope, run_limits())?;
    let outputs = block_on(instance.execute_ingestion(compiled.roots(), context)?)?;
    Ok(outputs
        .into_iter()
        .map(|batches| batches.iter().map(|batch| (**batch).clone()).collect())
        .collect())
}

/// `compiled` run at `evaluation_time_ms` over `store`: each root's batches.
fn run_query(
    compiled: &CompiledPhysicalDAG,
    store: &Arc<InMemoryDataStore>,
    evaluation_time_ms: i64,
) -> Result<Vec<Vec<Batch>>, asap_executor::Error> {
    let instance = compiled.instantiate(Arc::clone(store) as Arc<dyn DataStore>)?;
    let context = RunContext::new(Scope::Query { evaluation_time_ms }, run_limits())?;
    let outputs = instance.execute(compiled.roots(), context)?;
    block_on(async {
        let mut per_root = Vec::with_capacity(outputs.len());
        for mut output in outputs {
            let mut batches = Vec::new();
            while let Some(batch) = output.next().await {
                batches.push((*batch?).clone());
            }
            per_root.push(batches);
        }
        Ok(per_root)
    })
}

fn run_limits() -> Limits {
    Limits {
        max_bytes: RUN_MEMORY_LIMIT_BYTES,
    }
}

/// The ends `e` in `(after_ms, until_ms]` of `window`'s windows: `e +
/// phase ≡ 0 (mod width)`, since a window ending `phase` before an
/// evaluation on the width's grid ends there. Example: width 10 s, phase 0,
/// `(0, 30 s]`: 10, 20 and 30 s.
fn window_ends(window: RefreshWindow, after_ms: i64, until_ms: i64) -> impl Iterator<Item = i64> {
    let width = window.width_ms as i64;
    let phase = window.phase_ms as i64;
    let first = after_ms + (-phase - after_ms).rem_euclid(width);
    let first = if first == after_ms {
        first + width
    } else {
        first
    };
    (0..)
        .map(move |k| first + k * width)
        .take_while(move |&end| end <= until_ms)
}

impl InstalledArm for InstalledPlan {
    /// One line per DAG node (id, operator, timing, lifetime, retention),
    /// its operators, and its stored views; the lane runs the plan as
    /// given, so no substitutions and no sharing lost.
    fn plan_summary(&self) -> PlanSummary {
        let dag = &self.plan.dag;
        let text = dag
            .nodes
            .iter()
            .map(|node| {
                format!(
                    "n{} {} {:?} {:?} retention_ms {:?}",
                    node.id,
                    node.payload.kind_name(),
                    node.output_state.timing,
                    node.lifetime,
                    node.retention_ms
                )
            })
            .chain(std::iter::once(format!("roots {:?}", dag.roots)))
            .collect::<Vec<_>>()
            .join("\n");
        let stored_outputs = self
            .plan
            .materialized_views()
            .unwrap_or_default()
            .iter()
            .map(|view| {
                format!(
                    "n{} refreshed {:?}, kept {} ms",
                    view.node_id,
                    view.refresh(),
                    view.retention_ms()
                )
            })
            .collect();
        PlanSummary {
            text,
            node_kinds: dag
                .nodes
                .iter()
                .map(|node| node.payload.kind_name().to_string())
                .collect(),
            stored_outputs,
            substitutions: Vec::new(),
            sharing_lost: Vec::new(),
        }
    }

    /// Appends the chunk's samples of each time series the plan reads as
    /// its rows for `(previous end, chunk end]` (nothing for a series with
    /// no sample in it), then refreshes each group for every window ending
    /// in that range.
    fn ingest(&mut self, chunk: &SampleChunk) -> Result<StepReport, EngineError> {
        let (after_ms, until_ms) = (self.ingested_until_ms, chunk.end_ms);
        let routed = self.router.route(chunk, &mut self.tables);
        for (table, samples) in self.tables.iter_mut().zip(routed) {
            if samples.is_empty() {
                continue;
            }
            let batch = table
                .batch(&samples)
                .map_err(|error| run_failed("raw samples", error))?;
            store_between_runs(&mut self.store)
                .append_raw_data(&table.source, (after_ms, until_ms), vec![batch])
                .map_err(|error| run_failed("raw samples", error))?;
        }
        for refresh in &self.refreshes {
            for end_ms in window_ends(refresh.window, after_ms, until_ms) {
                refresh_window(refresh, &mut self.store, end_ms)?;
            }
        }
        self.ingested_until_ms = until_ms;
        Ok(StepReport::default())
    }

    /// One query run at `at_ms`: root `i`'s rows answer query `i`.
    fn evaluate(&mut self, at_ms: i64) -> Result<(Answers, StepReport), EngineError> {
        let per_root = run_query(&self.query_run, &self.store, at_ms)
            .map_err(|error| run_failed(&format!("query run at {at_ms}"), error))?;
        let mut answers = Answers::default();
        for (query_id, batches) in self.query_ids.iter().zip(&per_root) {
            let series = answers_of(batches).map_err(|error| run_failed(query_id, error))?;
            answers.0.insert(query_id.clone(), series);
        }
        Ok((answers, StepReport::default()))
    }

    /// The bytes of the stored views and of the raw samples held, as the
    /// executor counts each batch: a dictionary of series texts shared by
    /// several batches is counted in each.
    fn footprint(&self) -> Result<Footprint, EngineError> {
        Ok(Footprint {
            view_bytes: self.store.materialized_view_bytes() as u64,
            raw_retained_bytes: self.store.raw_data_bytes() as u64,
            peak_bytes: None,
        })
    }
}
