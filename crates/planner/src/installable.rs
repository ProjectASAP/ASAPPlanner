//! The plan a deployment runtime installs: one call from a workload and the
//! deployment's inputs to the single selected physical ASAP DAG, ready to
//! execute ([`plan_installable`]).
//!
//! The runtime writes the returned [`InstallablePlan`] as its plan file and
//! executes its DAG with the executor, as given (#509 Assumption 8). The
//! plan file holds only what the DAG cannot derive: the DAG, with the
//! `retention_ms` of each materialized view and each query-time `Scan` on
//! its node, and the query texts.
//! Everything else a runtime needs is derived from the DAG
//! (`asap_types::plan::materialized_view`): the views to maintain, with what
//! each is refreshed from and when ([`InstallablePlan::materialized_views`]),
//! and how far back each raw-data source is retained for query-time scans
//! ([`InstallablePlan::raw_data_retention`]). A runtime reads the file with
//! [`InstallablePlan::from_json`]. Raw data is read only at the DAG's
//! `Scan`s, by their `Source`, and a view by its `MaterializedView`: the
//! runtime implements one `asap_executor::sources::DataStore` over its raw
//! data (a time series is a table `(ts, value, *labels)` over the samples it
//! buffers; a SQL table is read as is) and its stored views, and the
//! DAG's own `TimeRange`s apply each window, an instant selector's lookback
//! and Prometheus stale markers. A tumbling-window chain runs as it is: each
//! ingestion run builds its newest window, the runtime appends it to its
//! store, and the `TimeRange`s above the chain select the windows a query
//! merges.

use std::collections::{HashMap, HashSet};

use asap_cost::{NodeCostSource, OperatorKind, PlanCost};
use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::plan::materialized_view::{
    IngestionRefreshGroup, InstallableDagError, MaterializedView, RawDataRetention,
};

use asap_types::ir::properties::ExecutionTiming;
use asap_types::plan::physical_asap_dag::{
    PhysicalASAPDAG, PhysicalASAPDAGEdge, PhysicalASAPDAGNode, PhysicalASAPDAGValidationError,
    PhysicalASAPNodeId,
};

use asap_types::workload::QueryWorkloadEntry;
use serde::{Deserialize, Serialize};

use crate::{e2e_plan, PlanError, UserInput};

/// The version of the plan-file format [`InstallablePlan`] serializes to:
/// the DAG, its nodes' fields and the query texts.
pub const INSTALLABLE_PLAN_WIRE_VERSION: u32 = 15;

/// The selected plan, ready to install and execute: what
/// [`plan_installable`] returns and a runtime writes as its plan file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallablePlan {
    /// [`INSTALLABLE_PLAN_WIRE_VERSION`] of the planner that wrote it.
    pub wire_version: u32,
    /// The selected physical ASAP DAG with Stage 3's timings and
    /// materialization, each materialized view and query-time `Scan`
    /// carrying its `retention_ms`; the executor compiles it as is.
    pub dag: PhysicalASAPDAG,
    /// One per workload query, in `QueryWorkload::entries()` order, as the
    /// workload gave it: query `i` is answered by `dag.roots[i]`.
    pub query_texts: Vec<String>,
}

/// The selected plan's cost (`asap_cost::plan_cost` of the plan), for
/// reporting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedPlanReport {
    /// Its cost per second; `None` under cost model 3, which has no
    /// prices.
    pub cost_per_second: Option<f64>,
    /// The resources Stage 3 predicts it uses, for a runtime to report
    /// against what it measures.
    pub predicted_usage: PredictedUsage,
}

/// What Stage 3 predicts the selected plan uses, per second of wall time
/// unless the name says otherwise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictedUsage {
    /// Core-seconds of CPU per second, ingestion and query time: the sum
    /// of `per_node`'s.
    pub cpu_core_seconds_per_second: f64,
    /// Bytes scanned per second.
    pub scanned_bytes_per_second: f64,
    /// Memory held between computations, averaged over time.
    pub average_memory_bytes: f64,
    /// The most memory live at once.
    pub peak_memory_bytes: u64,
    /// Bytes retained on disk, raw data query-time scans read included.
    pub disk_bytes: u64,
    /// CPU cores to provision for the peak.
    pub peak_cpu_cores: u32,
    /// One per operator node of the plan Stage 3 priced, in node order.
    pub per_node: Vec<PredictedNodeUsage>,
    /// One per workload query, in workload order.
    pub per_query: Vec<PredictedQueryUsage>,
}

/// One operator node's predicted usage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictedNodeUsage {
    /// Its id in the installed DAG.
    pub node_id: PhysicalASAPNodeId,
    /// Its operator, as the measured cost table names it.
    pub operator: OperatorKind,
    /// When it runs.
    pub timing: ExecutionTiming,
    /// Core-seconds of CPU per second.
    pub cpu_core_seconds_per_second: f64,
    /// Its price per second under cost model 1; `None` under cost models
    /// 2 and 3.
    pub cost_per_second: Option<f64>,
    /// Whether its CPU came from the measured cost table or the formulas.
    pub cost_source: NodeCostSource,
}

/// One query's predicted latency per evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictedQueryUsage {
    /// Milliseconds compacting the newest window of the ingestion-time
    /// state it reads.
    pub compaction_ms: f64,
    /// Milliseconds of its query-time work, its per-request cost included.
    pub query_ms: f64,
}

/// Why an installable plan could not be built or is not consistent.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InstallablePlanError {
    /// Bad input (a plan file): written for another plan-file format; `None`
    /// for a file without `wire_version`, written before version 11.
    #[error("unsupported installable plan wire version {0:?}")]
    UnsupportedWireVersion(Option<u32>),
    /// Bad input (a plan file): not JSON of an [`InstallablePlan`].
    #[error("plan file: {0}")]
    Json(#[from] serde_json::Error),
    /// Bad input (a plan file): the DAG is not a valid physical ASAP DAG.
    #[error("invalid physical ASAP DAG: {0}")]
    InvalidDag(#[from] PhysicalASAPDAGValidationError),
    /// Bad input (a plan file): `query_texts` has not one text per root of
    /// the DAG.
    #[error("query texts do not match the DAG's {roots} roots")]
    QueriesDoNotMatchRoots { roots: usize },
    /// Unsatisfiable request: query `entry_index` is a standalone scalar
    /// query (e.g. `scalar(sum(m))`), which no node of the DAG answers, so
    /// a runtime cannot answer it through the executor's public API.
    #[error("query {entry_index} is a standalone scalar query, which is not supported yet")]
    UnsupportedQueryRoot { entry_index: usize },
    /// Unsatisfiable request, or bad input (a plan file): a runtime cannot
    /// derive the DAG's materialized views or raw-data windows (see
    /// [`InstallableDagError`]).
    #[error("{0}")]
    Uninstallable(InstallableDagError),
    /// Unsatisfiable request: the executor cannot refresh materialized view
    /// `node_id` from its inputs (`reason`); for a refresh group, `node_id`
    /// is its first view and `reason` names every view compiled with it.
    #[error("the executor cannot refresh materialized view {node_id} from its inputs: {reason}")]
    MaterializedViewNotExecutable {
        node_id: PhysicalASAPNodeId,
        reason: String,
    },
    /// Unsatisfiable request: the executor cannot compile the query run,
    /// the roots over the views they read and the raw data they scan
    /// (`reason`).
    #[error("the executor cannot compile the plan's query run: {reason}")]
    QueryRunNotExecutable { reason: String },
    /// Internal bug: the cost of the selected plan cannot be computed at
    /// node `node_id` (`reason`).
    #[error("the selected plan's cost at node {node_id}: {reason}")]
    PlanCostFailed {
        node_id: PhysicalASAPNodeId,
        reason: String,
    },
}

impl From<InstallableDagError> for InstallablePlanError {
    fn from(error: InstallableDagError) -> Self {
        match error {
            InstallableDagError::InvalidDag(e) => Self::InvalidDag(e),
            error => Self::Uninstallable(error),
        }
    }
}

/// Plan `input` for a deployment runtime: the query-language frontends and
/// Stages 1-3 as [`e2e_plan`] runs them, returning the single selected DAG
/// ready to install, and beside it the plan's cost report, which is not
/// part of the plan file. Supported summaries the caller leaves unrestricted
/// (`DeploymentCapabilities::supported_summaries` `None`) default to the
/// families the Planner's executor runs; raw-data and query-time-result
/// retention stay the caller's. Stage 3 does not compile candidates;
/// installation compiles the selected plan, and one the executor cannot run
/// returns `PlanError::Installable` with
/// [`InstallablePlanError::MaterializedViewNotExecutable`] or
/// [`InstallablePlanError::QueryRunNotExecutable`].
pub async fn plan_installable(
    input: UserInput<'_>,
) -> Result<(InstallablePlan, Option<SelectedPlanReport>), PlanError> {
    let capabilities = executor_summaries_when_unrestricted(input.models.deployment_capabilities);
    let entries: Vec<_> = input.workload.query_workload.entries().collect();
    let models = input.models.with_deployment_capabilities(&capabilities);
    let input = UserInput {
        workload: input.workload,
        frontend_specific: input.frontend_specific,
        models,
        pass: input.pass,
    };
    let data = input.workload.data_workload.clone().unwrap_or_default();
    let cost_inputs = input.models.cost_inputs;
    let output = e2e_plan(input).await?;
    let (cost, _) = asap_cost::plan_cost(&output.plan, &entries, &data, cost_inputs).map_err(
        |(node_id, error)| {
            PlanError::Installable(InstallablePlanError::PlanCostFailed {
                node_id,
                reason: error.to_string(),
            })
        },
    )?;
    installable_plan_of(&output.plan, Some(&cost), &entries).map_err(PlanError::Installable)
}

/// The installable plan of `plan`, the plan Stage 3 built for a workload
/// whose entries are `entries` (in `QueryWorkload::entries()` order), with
/// the report of its `cost` (`asap_cost::plan_cost` of `plan`) when there is
/// one: what [`plan_installable`] returns after planning. A caller that runs
/// the planning stages itself (e.g. a developer tool) uses it to see what a
/// runtime would install.
pub fn installable_plan_of(
    plan: &PhysicalASAPDAG,
    cost: Option<&PlanCost>,
    entries: &[QueryWorkloadEntry],
) -> Result<(InstallablePlan, Option<SelectedPlanReport>), InstallablePlanError> {
    let query_texts: Vec<String> = entries.iter().map(|entry| entry.query.0.clone()).collect();
    let installable = InstallablePlan::of_dag(plan.clone(), query_texts)?;
    Ok((installable, cost.map(report_of)))
}

impl InstallablePlan {
    /// Every materialized view of the DAG, in dependency order (each after
    /// the views it reads): its node, refresh mode, coverage, inputs and
    /// retention ([`PhysicalASAPDAG::materialized_views`]).
    pub fn materialized_views(&self) -> Result<Vec<MaterializedView>, InstallablePlanError> {
        Ok(self.dag.materialized_views()?)
    }

    /// The views refreshed at ingestion, by refresh group, each group after
    /// the groups it reads ([`PhysicalASAPDAG::ingestion_refresh_groups`]):
    /// a runtime refreshes each group in one ingestion run per window.
    pub fn ingestion_refresh_groups(
        &self,
    ) -> Result<Vec<IngestionRefreshGroup>, InstallablePlanError> {
        Ok(self.dag.ingestion_refresh_groups()?)
    }

    /// How far back each raw-data source is retained for query-time scans
    /// ([`PhysicalASAPDAG::raw_data_retention`]): the raw data a runtime
    /// keeps, the raw-data counterpart of each view's retention.
    pub fn raw_data_retention(&self) -> Result<Vec<RawDataRetention>, InstallablePlanError> {
        Ok(self.dag.raw_data_retention()?)
    }

    /// Reads a plan file: its `wire_version` first, so a file of another
    /// format is refused as [`InstallablePlanError::UnsupportedWireVersion`]
    /// rather than as a field it does not have, then the plan. Does not
    /// [`validate`](Self::validate) it.
    pub fn from_json(text: &str) -> Result<Self, InstallablePlanError> {
        /// Only the version of any plan file.
        #[derive(Deserialize)]
        struct WireVersion {
            #[serde(default)]
            wire_version: Option<u32>,
        }
        let WireVersion { wire_version } = serde_json::from_str(text)?;
        if wire_version != Some(INSTALLABLE_PLAN_WIRE_VERSION) {
            return Err(InstallablePlanError::UnsupportedWireVersion(wire_version));
        }
        Ok(serde_json::from_str(text)?)
    }

    /// Whether the Planner's executor compiles this plan as a runtime runs
    /// it: each refresh group's views compiled together (every root its
    /// ingestion runs may have) and each other view's refresh,
    /// over the views it reads and the `Scan`s it reads (from the runtime's
    /// data store), then the query run
    /// over the views it reads and the `Scan`s it reads outside them. The
    /// DAG's own `TimeRange`s are compiled, so a window the executor cannot
    /// run fails here. `Err` holds the executor's error, or why the views
    /// cannot be derived. [`plan_installable`] runs the same compiles before
    /// it returns a plan; this checks a plan read from a plan file.
    pub fn check_compiles(&self) -> Result<(), String> {
        let views = self
            .materialized_views()
            .map_err(|error| error.to_string())?;
        executor_compiles(&self.dag, &views).map_err(|error| error.to_string())
    }

    /// The plan of `dag`, with each view's and query-time scan's retention
    /// (which Stage 3 set) checked ([`PhysicalASAPDAG::materialized_views`],
    /// [`PhysicalASAPDAG::raw_data_retention`]), and each view's refresh
    /// and the query run compiled ([`executor_compiles`]): a plan the
    /// executor cannot run is refused here, not installed.
    pub(crate) fn of_dag(
        dag: PhysicalASAPDAG,
        query_texts: Vec<String>,
    ) -> Result<Self, InstallablePlanError> {
        let views = dag.materialized_views()?;
        dag.raw_data_retention()?;
        check_queries(&dag, &query_texts)?;
        executor_compiles(&dag, &views)?;
        let plan = InstallablePlan {
            wire_version: INSTALLABLE_PLAN_WIRE_VERSION,
            dag,
            query_texts,
        };
        debug_assert!(plan.validate().is_ok());
        Ok(plan)
    }

    /// Checks a plan, e.g. one read from a plan file, for what is not
    /// derived from its DAG: its wire version, the DAG's validity, one query
    /// text per root and no standalone scalar query, and each view's and
    /// query-time scan's retention covering how far back it is read (which
    /// derives the views, [`Self::materialized_views`], validating the DAG).
    pub fn validate(&self) -> Result<(), InstallablePlanError> {
        if self.wire_version != INSTALLABLE_PLAN_WIRE_VERSION {
            return Err(InstallablePlanError::UnsupportedWireVersion(Some(
                self.wire_version,
            )));
        }
        check_queries(&self.dag, &self.query_texts)?;
        self.materialized_views()?;
        self.raw_data_retention()?;
        Ok(())
    }
}

/// `declared` with `supported_summaries` set to the executor's families
/// when the caller left them unrestricted (`None`).
fn executor_summaries_when_unrestricted(
    declared: &DeploymentCapabilities,
) -> DeploymentCapabilities {
    let mut capabilities = declared.clone();
    if capabilities.supported_summaries.is_none() {
        capabilities.supported_summaries = Some(asap_executor::supported_summary_families());
    }
    capabilities
}

/// Whether query `i` of `query_texts` can be answered by root `i` of
/// `dag`: one text per root, and no standalone scalar query, which no node
/// answers.
fn check_queries(
    dag: &PhysicalASAPDAG,
    query_texts: &[String],
) -> Result<(), InstallablePlanError> {
    if let Some((entry_index, _)) = dag.scalar_roots.first() {
        return Err(InstallablePlanError::UnsupportedQueryRoot {
            entry_index: *entry_index,
        });
    }
    if query_texts.len() != dag.roots.len() {
        return Err(InstallablePlanError::QueriesDoNotMatchRoots {
            roots: dag.roots.len(),
        });
    }
    Ok(())
}

/// The nodes of a validated (acyclic) DAG by id with each node's inputs
/// and input edges, to compile a refresh over the nodes below its view.
struct IndexedDag<'a> {
    dag: &'a PhysicalASAPDAG,
    nodes: HashMap<PhysicalASAPNodeId, &'a PhysicalASAPDAGNode>,
    /// Each node's position in the DAG's node list.
    position: HashMap<PhysicalASAPNodeId, usize>,
    /// Each consumer's input edges, in the DAG's edge order.
    edges_into: HashMap<PhysicalASAPNodeId, Vec<&'a PhysicalASAPDAGEdge>>,
    inputs: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
}

impl<'a> IndexedDag<'a> {
    fn new(dag: &'a PhysicalASAPDAG) -> Self {
        let mut inputs: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = HashMap::new();
        let mut edges_into: HashMap<PhysicalASAPNodeId, Vec<&PhysicalASAPDAGEdge>> = HashMap::new();
        for edge in &dag.edges {
            inputs.entry(edge.consumer).or_default().push(edge.producer);
            edges_into.entry(edge.consumer).or_default().push(edge);
        }
        IndexedDag {
            dag,
            nodes: dag.nodes.iter().map(|n| (n.id, n)).collect(),
            position: dag
                .nodes
                .iter()
                .enumerate()
                .map(|(i, node)| (node.id, i))
                .collect(),
            edges_into,
            inputs,
        }
    }

    /// The sub-DAG of `roots` and every node below them, with their edges,
    /// in the DAG's node and edge order, rooted at `roots`: all a refresh of
    /// `roots` reads, at the cost of its size rather than the plan's.
    fn dag_below(&self, roots: &[PhysicalASAPNodeId]) -> PhysicalASAPDAG {
        let mut below = HashSet::new();
        let mut pending = roots.to_vec();
        while let Some(id) = pending.pop() {
            if below.insert(id) {
                pending.extend(self.inputs.get(&id).into_iter().flatten().copied());
            }
        }
        let mut ids: Vec<PhysicalASAPNodeId> = below.into_iter().collect();
        ids.sort_unstable_by_key(|id| self.position[id]);
        let edges = ids
            .iter()
            .flat_map(|id| self.edges_into.get(id).into_iter().flatten())
            .map(|edge| (*edge).clone())
            .collect();
        PhysicalASAPDAG {
            nodes: ids.iter().map(|id| self.nodes[id].clone()).collect(),
            edges,
            roots: roots.to_vec(),
            scalar_roots: Vec::new(),
        }
    }
}

/// Whether the executor compiles `dag` as a runtime runs it: each refresh
/// ([`executor_refreshes`]): one per refresh group of the views refreshed
/// at ingestion ([`PhysicalASAPDAG::ingestion_refresh_groups_of`] `views`)
/// and one per other view of `views` (refreshed on first read), the first
/// in the whole plan, which runs the executor's checks of the whole plan
/// once, every other over the nodes below it; then the query run
/// ([`executor_runs_queries`]). A group is compiled with all its views as
/// roots: a compile check covering every run's roots (the views whose
/// state for the run's window is not stored yet, a subset), not the root
/// set of any one run.
fn executor_compiles(
    dag: &PhysicalASAPDAG,
    views: &[MaterializedView],
) -> Result<(), InstallablePlanError> {
    let indexed = IndexedDag::new(dag);
    let views_by_node: HashMap<PhysicalASAPNodeId, &MaterializedView> =
        views.iter().map(|view| (view.node_id, view)).collect();
    let groups = dag.ingestion_refresh_groups_of(views);
    let refreshed_at_ingestion: HashSet<PhysicalASAPNodeId> = groups
        .iter()
        .flat_map(|group| group.views.iter().map(|view| view.node_id))
        .collect();
    let refreshes = groups
        .iter()
        .map(|group| group.views.iter().collect::<Vec<_>>())
        .chain(
            views
                .iter()
                .filter(|view| !refreshed_at_ingestion.contains(&view.node_id))
                .map(|view| vec![view]),
        );
    for (position, refreshed) in refreshes.enumerate() {
        executor_refreshes(&refreshed, &views_by_node, &indexed, position == 0)?;
    }
    executor_runs_queries(&indexed, &views_by_node)
}

/// Whether the executor compiles the query run: the roots over the views
/// they reach without passing another (among `views_by_node`) and the
/// `Scan`s they reach outside them, raw data slots the runtime's data store
/// reads (a time series, or a SQL table read whole or through a window).
fn executor_runs_queries(
    indexed: &IndexedDag<'_>,
    views_by_node: &HashMap<PhysicalASAPNodeId, &MaterializedView>,
) -> Result<(), InstallablePlanError> {
    use asap_executor::physical_planner::compile;
    let dag = indexed.dag;
    let mut read = Vec::new();
    let mut pending = dag.roots.clone();
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        match views_by_node.get(&id) {
            Some(view) => read.push((*view).clone()),
            None => pending.extend(indexed.inputs.get(&id).into_iter().flatten().copied()),
        }
    }
    let roots: Vec<u64> = dag.roots.iter().map(|&id| id as u64).collect();
    compile(dag, &read, &roots).map(|_| ()).map_err(|error| {
        InstallablePlanError::QueryRunNotExecutable {
            reason: error.to_string(),
        }
    })
}

/// Whether the executor compiles the refresh of `refreshed` (one view, or
/// every view of one refresh group) as roots, with the views they read
/// (among `views_by_node`) that are not among them as materialized view
/// slots; the executor's reason, naming the roots, when it does not.
fn executor_refreshes(
    refreshed: &[&MaterializedView],
    views_by_node: &HashMap<PhysicalASAPNodeId, &MaterializedView>,
    indexed: &IndexedDag<'_>,
    in_whole_plan: bool,
) -> Result<(), InstallablePlanError> {
    use asap_executor::physical_planner::compile;
    let roots: Vec<PhysicalASAPNodeId> = refreshed.iter().map(|view| view.node_id).collect();
    // The refresh reads only the nodes below it: compile them, or the whole
    // plan once, for the executor's checks of the whole plan.
    let sub_dag;
    let below = match in_whole_plan {
        true => indexed.dag,
        false => {
            sub_dag = indexed.dag_below(&roots);
            &sub_dag
        }
    };
    #[cfg(test)]
    tests::COMPILED_NODES.with(|count| count.set(count.get() + below.nodes.len()));
    // Raw data is read at each `Scan`, a raw data slot `compile` makes.
    let root_set: HashSet<PhysicalASAPNodeId> = roots.iter().copied().collect();
    let mut read_ids = HashSet::new();
    let read: Vec<MaterializedView> = refreshed
        .iter()
        .flat_map(|view| &view.inputs.materialized_views)
        .filter(|id| !root_set.contains(id) && read_ids.insert(**id))
        .filter_map(|id| views_by_node.get(id).map(|read| (*read).clone()))
        .collect();
    let root_ids: Vec<u64> = roots.iter().map(|&id| id as u64).collect();
    compile(below, &read, &root_ids)
        .map(|_| ())
        .map_err(
            |error| InstallablePlanError::MaterializedViewNotExecutable {
                node_id: roots[0],
                reason: match roots.as_slice() {
                    [_] => error.to_string(),
                    _ => format!("refreshed with views {roots:?}: {error}"),
                },
            },
        )
}

/// The report of the selected plan's `cost`.
fn report_of(cost: &PlanCost) -> SelectedPlanReport {
    SelectedPlanReport {
        cost_per_second: cost.cost_per_second,
        predicted_usage: PredictedUsage {
            cpu_core_seconds_per_second: cost
                .per_node
                .values()
                .map(|node| node.cpu_core_seconds_per_second)
                .sum(),
            scanned_bytes_per_second: cost.usage.scanned_bytes_per_second,
            average_memory_bytes: cost.usage.average_memory_bytes,
            peak_memory_bytes: cost.usage.peak_memory_bytes,
            disk_bytes: cost.usage.disk_bytes,
            peak_cpu_cores: cost.usage.peak_cpu_cores,
            per_node: cost
                .per_node
                .iter()
                .map(|(&id, node)| PredictedNodeUsage {
                    node_id: id,
                    operator: node.operator,
                    timing: node.timing,
                    cpu_core_seconds_per_second: node.cpu_core_seconds_per_second,
                    cost_per_second: node.cost_per_second,
                    cost_source: node.cost_source,
                })
                .collect(),
            per_query: cost
                .usage
                .latency_per_query
                .iter()
                .map(|latency| PredictedQueryUsage {
                    compaction_ms: latency.compaction_ms,
                    query_ms: latency.query_ms,
                })
                .collect(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::summary_type::{SketchAlgorithm, StatisticKind, SupportedSummary};
    use std::rc::Rc;

    thread_local! {
        /// The plan nodes handed to the executor's compiler by the views'
        /// refreshes in [`InstallablePlan::of_dag`] and
        /// [`InstallablePlan::check_compiles`], summed over their compiles.
        pub(super) static COMPILED_NODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// A p99 panel over a 300 s window every second, with no raw data
    /// retained: one DDSketch per series in a chain of one-second windows
    /// built at ingestion. Each refresh (here one refresh group) is compiled
    /// over the nodes below its views, and the whole plan's checks run once,
    /// so the nodes handed to the executor grow with the plan, not with the
    /// plan times its views. The count covers both an installation and
    /// `check_compiles`.
    #[tokio::test]
    async fn view_refreshes_compile_only_the_nodes_below_them() {
        let workload: crate::PlanningWorkload = serde_json::from_str(
            r#"{"query_workload": {"language": "promql", "repeating_queries": [{
                  "query": "avg by (job) (quantile_over_time(0.99, data[300s]))",
                  "demand": {"fixed_interval_at": {"interval": 1000, "evaluation_phase": 0}},
                  "requirements": {"accuracy": {"max_error": 0.05, "failure_probability": 0.3,
                      "accuracy_scope": "whole_result"}},
                  "predictability": {"predictable": {"known_at": null}}}]},
                "data_workload": {"arrival": {"ingesting": {"rows_per_second": 10000.0}},
                  "sources": {"data": {"series_count": 1000, "metric_type": "gauge",
                      "sample_interval": 100, "column_signs": {"value": "non_negative"}}}}}"#,
        )
        .unwrap();
        let capabilities = DeploymentCapabilities {
            raw_data_retained: false,
            ..DeploymentCapabilities::default()
        };
        COMPILED_NODES.with(|count| count.set(0));
        let (plan, _) = plan_installable(crate::UserInput::new(
            &workload,
            crate::FrontendInput::Promql,
            crate::PlanningModels::builtin().with_deployment_capabilities(&capabilities),
        ))
        .await
        .expect("a plan");
        let nodes = plan.dag.nodes.len();
        let views = plan.materialized_views().unwrap().len();
        // Measure one installation and one `check_compiles`, independently of
        // how many candidates the optimizer explored before selecting this DAG.
        COMPILED_NODES.with(|count| count.set(0));
        let rebuilt = InstallablePlan::of_dag(plan.dag.clone(), plan.query_texts.clone()).unwrap();
        assert_eq!(rebuilt.materialized_views().unwrap().len(), views);
        plan.check_compiles().unwrap();
        let compiled = COMPILED_NODES.with(std::cell::Cell::get);
        assert!(
            compiled <= 6 * nodes,
            "{compiled} nodes compiled for a plan of {nodes} nodes and {views} views"
        );
    }

    /// The plan of the `sum_over_time(latency[1h])` panel as Stage 3 builds
    /// it: a chain of 10-minute windows of exact sums per `job` over
    /// `latency`, built at ingestion and retained for `chain_retention_ms`,
    /// read through `TimeRange(1h)` by a merge and its finalization at query
    /// time; and the chain's node id.
    fn chain_plan(chain_retention_ms: Option<u64>) -> (PhysicalASAPDAG, PhysicalASAPNodeId) {
        use asap_types::ir::operator::{Reduction, Source};
        use asap_types::ir::scalar::ColumnRef;
        use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
        use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot, TimeRangeKind};
        use asap_types::plan::physical_asap_dag::ViewCoverage;
        use asap_types::stages::{Lifetime, StorageMedium};
        use asap_types::summary_type::{ExactKind, ExactParams, SummaryUpdate};
        let node = |operator| OperatorNode::new_shared(operator).unwrap();
        let scan = node(Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries {
                metric: "latency".into(),
            },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("job", DataType::Utf8, false),
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("value", DataType::Float64, false),
                ],
                1,
                vec![],
            ),
        }));
        let chain = node(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: scan,
            summary_type: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::by(vec![0]),
            filter: None,
            tumbling_window_width_ms: 600_000,
        }));
        let windows = node(Operator::NonASAP(NonASAPOp::TimeRange {
            range: std::time::Duration::from_secs(3_600),
            kind: TimeRangeKind::Range,
            child: chain.clone(),
        }));
        let merged = node(Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![windows],
            group_by: None,
        }));
        let root = node(Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
            child: merged,
        }));
        let (mut dag, nodes) =
            asap_types::test_support::forwarders::physical_asap_dag_at_query_time_with_node_ids(&[
                QueryRoot::Operator(root),
            ])
            .unwrap();
        let chain_id = nodes.iter().position(|n| Rc::ptr_eq(n, &chain)).unwrap();
        let scan_id = producer_of(&dag, chain_id);
        for node in &mut dag.nodes {
            if node.id == chain_id {
                node.lifetime = Lifetime::AcrossEvaluations(StorageMedium::Memory);
                node.retention_ms = chain_retention_ms;
                node.view_coverage =
                    Some(ViewCoverage::new("sum of latency by job".into(), vec![]));
            }
            if node.id == chain_id || node.id == scan_id {
                node.output_state.timing = ExecutionTiming::IngestionTime;
            }
        }
        for edge in &mut dag.edges {
            if edge.producer == chain_id || edge.producer == scan_id {
                edge.data_state_mut().timing = ExecutionTiming::IngestionTime;
            }
        }
        (dag, chain_id)
    }

    /// One exact query's entry.
    fn entry() -> QueryWorkloadEntry {
        QueryWorkloadEntry {
            query: Default::default(),
            requirements: asap_types::workload::QueryRequirements {
                accuracy: asap_types::accuracy::AccuracyTarget::exact(),
                max_latency_ms: None,
            },
            predictability: Default::default(),
            recurrence: Default::default(),
            range_evaluation: None,
        }
    }

    /// The input of `consumer`, which reads one.
    fn producer_of(dag: &PhysicalASAPDAG, consumer: PhysicalASAPNodeId) -> PhysicalASAPNodeId {
        dag.edges
            .iter()
            .find(|edge| edge.consumer == consumer)
            .unwrap()
            .producer
    }

    /// The installable plan of a DAG with a chain is the DAG as Stage 3
    /// built it: the chain is one materialized view, refreshed at ingestion
    /// one 10-minute window per run, and no raw data is kept for queries.
    #[test]
    fn a_chain_is_installed_as_one_view() {
        let (dag, chain) = chain_plan(Some(3_600_000));
        let (plan, report) = installable_plan_of(&dag, None, &[entry()]).unwrap();
        assert_eq!(plan.dag, dag);
        assert!(report.is_none());
        let views = plan.materialized_views().unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].node_id, chain);
        assert_eq!(views[0].retention_ms(), 3_600_000);
        let groups = plan.ingestion_refresh_groups().unwrap();
        assert_eq!(
            groups[0].refresh_window,
            Some(asap_types::plan::materialized_view::RefreshWindow {
                width_ms: 600_000,
                phase_ms: 0
            })
        );
        assert!(plan.raw_data_retention().unwrap().is_empty());
        plan.check_compiles().unwrap();
    }

    /// Stage 3 sets each view's retention; installation checks it: a view
    /// without one, or retained for less than the hour its query reads back,
    /// is refused.
    #[test]
    fn installation_checks_the_retention_stage3_set() {
        let entries = [entry()];
        assert!(matches!(
            installable_plan_of(&chain_plan(None).0, None, &entries),
            Err(InstallablePlanError::Uninstallable(
                InstallableDagError::ViewWithoutRetention { .. }
            ))
        ));
        assert!(matches!(
            installable_plan_of(&chain_plan(Some(3_000_000)).0, None, &entries),
            Err(InstallablePlanError::Uninstallable(
                InstallableDagError::ViewRetentionTooShort { .. }
            ))
        ));
    }

    /// A query-time `Scan` read through a window carries its retention;
    /// one without is refused, and one with it is the raw data kept for its
    /// source.
    #[test]
    fn a_query_time_scan_carries_its_retention() {
        let (mut dag, chain) = chain_plan(Some(3_600_000));
        let scan = producer_of(&dag, chain);
        for node in &mut dag.nodes {
            if node.id == chain || node.id == scan {
                node.output_state.timing = ExecutionTiming::QueryTime;
            }
        }
        for edge in &mut dag.edges {
            if edge.producer == chain || edge.producer == scan {
                edge.data_state_mut().timing = ExecutionTiming::QueryTime;
            }
        }
        assert!(matches!(
            dag.raw_data_retention(),
            Err(InstallableDagError::ScanWithoutRetention { .. })
        ));
        dag.nodes
            .iter_mut()
            .find(|n| n.id == scan)
            .unwrap()
            .retention_ms = Some(600_000);
        let retention = dag.raw_data_retention().unwrap();
        assert_eq!(retention.len(), 1);
        assert_eq!(retention[0].lookback_ms(), Some(600_000));
    }

    /// Unrestricted summaries become the executor's families; a caller's
    /// own list and its retention flags are kept.
    #[test]
    fn unrestricted_summaries_default_to_the_executors_families() {
        let unrestricted = DeploymentCapabilities {
            raw_data_retained: false,
            ..DeploymentCapabilities::default()
        };
        let defaulted = executor_summaries_when_unrestricted(&unrestricted);
        assert_eq!(
            defaulted.supported_summaries,
            Some(asap_executor::supported_summary_families())
        );
        assert!(!defaulted.raw_data_retained);
        assert!(!defaulted.supports(&SketchAlgorithm::CountSketch, StatisticKind::ItemFrequency));
        let kll_only = DeploymentCapabilities {
            supported_summaries: Some(vec![SupportedSummary {
                summary_type: SketchAlgorithm::Kll,
                readouts: vec![StatisticKind::Quantile],
            }]),
            ..DeploymentCapabilities::default()
        };
        assert_eq!(executor_summaries_when_unrestricted(&kll_only), kll_only);
    }
}
