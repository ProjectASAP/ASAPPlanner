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
//! (`asap_types::ir::materialized_view`): the views to maintain, with what
//! each is refreshed from and when ([`InstallablePlan::materialized_views`]),
//! and how far back each raw-data source is kept for query-time scans
//! ([`InstallablePlan::raw_data_retention`]). A runtime reads the file with
//! [`InstallablePlan::from_json`]. Raw data is read only at the DAG's
//! `Scan`s, by their `Source`, and a view by its `MaterializedView`: the
//! runtime implements one `asap_executor::sources::DataStore` over its raw
//! data (a time series is a table `(ts, value, *labels)` over the samples it
//! buffers; a SQL table is read as is) and its stored views, and the
//! DAG's own `TimeRange`s apply each window, an instant selector's lookback
//! and Prometheus stale markers. How the Planner represents the plan
//! internally (e.g. a tumbling-pane chain, which the stage export keeps for
//! costing and display) is not the runtime's concern: the installable DAG
//! has every chain expanded into the panes the executor runs.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use asap_cost::{NodePriceSource, OperatorKind};
use asap_logical_asap_optimization::DeploymentCapabilities;
use asap_plan_selection::{SearchReport, Selection};
use asap_types::ir::materialized_view::{InstallableDagError, MaterializedView, RawDataRetention};
use asap_types::ir::operator::pane_chain::expand_pane_chains_in_workload;
use asap_types::ir::physical_export::{
    PhysicalASAPDAG, PhysicalASAPDAGEdge, PhysicalASAPDAGNode, PhysicalASAPDAGValidationError,
    PhysicalASAPNodeId, ReferencePlanError,
};
use asap_types::ir::properties::{ExecutionDataStateError, ExecutionTiming};
use asap_types::ir::SchemaDerivationError;
use asap_types::ir::{
    apply_materialization_timings_to_roots, ASAPOp, MaterializationAssignment, OperatorNode,
    QueryRoot,
};
use asap_types::workload::QueryWorkloadEntry;
use serde::{Deserialize, Serialize};

use crate::{e2e_plan, PlanError, UserInput};

/// The version of the plan-file format [`InstallablePlan`] serializes to:
/// the DAG, its nodes' fields and the query texts.
pub const INSTALLABLE_PLAN_WIRE_VERSION: u32 = 11;

/// The selected plan, ready to install and execute: what
/// [`plan_installable`] returns and a runtime writes as its plan file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallablePlan {
    /// [`INSTALLABLE_PLAN_WIRE_VERSION`] of the planner that wrote it.
    pub wire_version: u32,
    /// The selected physical ASAP DAG with Stage 3's timings and
    /// materialization, every pane chain expanded into its panes, each
    /// materialized view and query-time `Scan` carrying its `retention_ms`;
    /// the executor compiles it as is.
    pub dag: PhysicalASAPDAG,
    /// One per workload query, in `QueryWorkload::entries()` order, as the
    /// workload gave it: query `i` is answered by `dag.roots[i]`.
    pub query_texts: Vec<String>,
}

/// What Stage 3 recorded about the plan it selected, for reporting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedPlanReport {
    /// Its cost per second; `None` under cost model 3, which has no
    /// prices.
    pub cost_per_second: Option<f64>,
    /// The resources Stage 3 predicts it uses, for a runtime to report
    /// against what it measures.
    pub predicted_usage: PredictedUsage,
    /// How the search behind the selection ended (`Selection::search_report`).
    pub search_report: SearchReport,
    /// The workload queries (entry indices) whose output Stage 3 accepted
    /// only because the accuracy model accepts it regardless of its
    /// guarantee (e.g. `PlanningModels::ignore_accuracy_guarantees`):
    /// the guarantee check would have rejected it. Empty when every
    /// output's guarantee meets its target; omitted from JSON then.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queries_accepted_without_guarantee: Vec<usize>,
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
    /// Bytes kept on disk, raw data query-time scans read included.
    pub disk_bytes: u64,
    /// CPU cores to provision for the peak.
    pub peak_cpu_cores: u32,
    /// One per operator node of the plan Stage 3 priced, in node order.
    pub per_node: Vec<PredictedNodeUsage>,
    /// One per workload query, in workload order.
    pub per_query: Vec<PredictedQueryUsage>,
}

/// One operator node's predicted usage. The node is one of the physical
/// DAG Stage 3 priced, where a tumbling-pane chain is one node; `dag` has
/// it expanded into its panes, so `installed_node_ids` says which of
/// `dag`'s nodes run it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictedNodeUsage {
    /// Its id in Stage 3's physical DAG of the selected candidate.
    pub stage3_node_id: PhysicalASAPNodeId,
    /// The ids in `dag` that run it: a tumbling-pane chain's panes, newest
    /// first; for every other node, the one node it is installed as. Empty
    /// for a node `dag` has no copy of: one the expansion rebuilds once per
    /// pane below a chain (e.g. the chain's input window).
    pub installed_node_ids: Vec<PhysicalASAPNodeId>,
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
    pub price_source: NodePriceSource,
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
    /// Internal bug: the selected plan's timings do not hold, or it does
    /// not export; or, for the query-time reference, bad input: the plan
    /// cannot run at query time.
    #[error("timing: {0}")]
    Timing(#[from] ExecutionDataStateError),
    /// Internal bug: a pane chain of the selected plan does not expand.
    #[error("pane chain expansion: {0}")]
    PaneChainExpansion(#[from] SchemaDerivationError),
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
    /// Internal bug: Stage 3 records a pane retention for node `node_id`
    /// of the selected plan, which is not one of its pane chains.
    #[error("pane retention for node {node_id}, which is not a pane chain of the selected plan")]
    PaneRetentionOfUnknownChain { node_id: PhysicalASAPNodeId },
    /// Unsatisfiable request: the executor cannot refresh materialized view
    /// `node_id` from its inputs (`reason`).
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
}

impl From<InstallableDagError> for InstallablePlanError {
    fn from(error: InstallableDagError) -> Self {
        match error {
            InstallableDagError::InvalidDag(e) => Self::InvalidDag(e),
            error => Self::Uninstallable(error),
        }
    }
}

impl From<ReferencePlanError> for InstallablePlanError {
    fn from(error: ReferencePlanError) -> Self {
        match error {
            ReferencePlanError::InvalidDag(e) => Self::InvalidDag(e),
            ReferencePlanError::Timing(e) => Self::Timing(e),
        }
    }
}

/// Plan `input` for a deployment runtime: the query-language frontends and
/// Stages 1-3 as [`e2e_plan`] runs them, returning the single selected DAG
/// ready to install, and beside it Stage 3's report of the selection
/// (`None` for a pass that selects without pricing), which is not part of
/// the plan file. Supported summaries the caller leaves unrestricted
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
    let output = e2e_plan(input).await?;
    installable_plan_of(
        &output.roots(),
        output.selection.as_ref(),
        &output.pane_retention_ms,
        &entries,
    )
    .map_err(PlanError::Installable)
}

/// The installable plan of `roots`, the selected roots of a workload whose
/// entries are `entries` (in `QueryWorkload::entries()` order), with
/// Stage 3's report of its `selection` of them when there is one, and the
/// pane retention of the selected plan's kept chains
/// (`RealizedPlan::pane_retention_ms`, by node id in the DAG `roots`
/// export): what [`plan_installable`] returns after planning. A caller that
/// runs the planning stages itself (e.g. a developer tool) uses it to see
/// what a runtime would install.
pub fn installable_plan_of(
    roots: &[QueryRoot],
    selection: Option<&Selection>,
    pane_retention_ms: &BTreeMap<PhysicalASAPNodeId, u64>,
    entries: &[QueryWorkloadEntry],
) -> Result<(InstallablePlan, Option<SelectedPlanReport>), InstallablePlanError> {
    // Each query's longest range-evaluation span, which the views it
    // reads must stay available for.
    let range_spans_ms: Vec<u64> = entries
        .iter()
        .map(|entry| entry.range_evaluation.map_or(0, |range| range.span_ms))
        .collect();
    let query_texts: Vec<String> = entries.iter().map(|entry| entry.query.0.clone()).collect();
    // Every node keeps the timing Stage 3 gave it.
    let timed =
        apply_materialization_timings_to_roots(roots, &MaterializationAssignment::default())?;
    let expanded = pane_retention_of_expanded_panes(&timed, pane_retention_ms)?;
    let report = selection.map(|selection| report_of(selection, &expanded.installed_node_ids));
    let reader_span_ms_by_node = reader_span_ms_by_node(&expanded.dag, &range_spans_ms);
    let plan = InstallablePlan::of_dag(
        expanded.dag,
        query_texts,
        &expanded.retention_ms_by_node,
        &reader_span_ms_by_node,
    )?;
    Ok((plan, report))
}

impl InstallablePlan {
    /// Every materialized view of the DAG, in dependency order (each after
    /// the views it reads): its node, refresh mode, coverage, inputs and
    /// retention ([`PhysicalASAPDAG::materialized_views`]).
    pub fn materialized_views(&self) -> Result<Vec<MaterializedView>, InstallablePlanError> {
        Ok(self.dag.materialized_views()?)
    }

    /// How far back each raw-data source is kept for query-time scans
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
    /// it: each view's refresh over the views it reads and the `Scan`s it
    /// reads (from the runtime's data store), then the query run
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
    /// set ([`PhysicalASAPDAG::set_retention`]), each view's refresh and the
    /// query run compiled ([`executor_compiles`]): a plan the executor
    /// cannot run is refused here, not installed.
    pub(crate) fn of_dag(
        mut dag: PhysicalASAPDAG,
        query_texts: Vec<String>,
        pane_retention_ms_by_node: &HashMap<PhysicalASAPNodeId, u64>,
        reader_span_ms_by_node: &HashMap<PhysicalASAPNodeId, u64>,
    ) -> Result<Self, InstallablePlanError> {
        let views = dag.set_retention(pane_retention_ms_by_node, reader_span_ms_by_node, None)?;
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

    /// The sub-DAG of `root` and every node below it, with their edges, in
    /// the DAG's node and edge order, rooted at `root`: all a refresh of
    /// `root` reads, at the cost of its size rather than the plan's.
    fn dag_below(&self, root: PhysicalASAPNodeId) -> PhysicalASAPDAG {
        let mut below = HashSet::new();
        let mut pending = vec![root];
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
            roots: vec![root],
            scalar_roots: Vec::new(),
        }
    }
}

/// For every node of `dag`, the longest range-evaluation span among the
/// queries that read it (`range_spans_ms[i]` for root `i`); a node no
/// range-evaluated query reads is absent. One walk per root:
/// O(roots · (nodes + edges)).
fn reader_span_ms_by_node(
    dag: &PhysicalASAPDAG,
    range_spans_ms: &[u64],
) -> HashMap<PhysicalASAPNodeId, u64> {
    let mut inputs: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = HashMap::new();
    for edge in &dag.edges {
        inputs.entry(edge.consumer).or_default().push(edge.producer);
    }
    let mut spans = HashMap::new();
    for (&root, &span_ms) in dag.roots.iter().zip(range_spans_ms) {
        if span_ms == 0 {
            continue;
        }
        let mut seen = HashSet::new();
        let mut pending = vec![root];
        while let Some(id) = pending.pop() {
            if seen.insert(id) {
                let span = spans.entry(id).or_insert(0);
                *span = (*span).max(span_ms);
                pending.extend(inputs.get(&id).into_iter().flatten().copied());
            }
        }
    }
    spans
}

/// Whether the executor compiles `dag` as a runtime runs it: each of
/// `views`' refreshes ([`executor_refreshes`]), the first in the whole plan,
/// which runs the executor's checks of the whole plan once, every other over
/// the nodes below it; then the query run ([`executor_runs_queries`]).
fn executor_compiles(
    dag: &PhysicalASAPDAG,
    views: &[MaterializedView],
) -> Result<(), InstallablePlanError> {
    let indexed = IndexedDag::new(dag);
    let views_by_node: HashMap<PhysicalASAPNodeId, &MaterializedView> =
        views.iter().map(|view| (view.node_id, view)).collect();
    for (position, view) in views.iter().enumerate() {
        executor_refreshes(view, &views_by_node, &indexed, position == 0)?;
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

/// Whether the executor compiles `view`'s refresh with the views it reads
/// (among `views_by_node`) as materialized view slots, as a runtime runs it;
/// the executor's reason when it does not.
fn executor_refreshes(
    view: &MaterializedView,
    views_by_node: &HashMap<PhysicalASAPNodeId, &MaterializedView>,
    indexed: &IndexedDag<'_>,
    in_whole_plan: bool,
) -> Result<(), InstallablePlanError> {
    use asap_executor::physical_planner::compile;
    // The refresh reads only the nodes below it: compile them, or the whole
    // plan once, for the executor's checks of the whole plan.
    let sub_dag;
    let below = match in_whole_plan {
        true => indexed.dag,
        false => {
            sub_dag = indexed.dag_below(view.node_id);
            &sub_dag
        }
    };
    #[cfg(test)]
    tests::COMPILED_NODES.with(|count| count.set(count.get() + below.nodes.len()));
    // Raw data is read at each `Scan`, a raw data slot `compile` makes.
    let read: Vec<MaterializedView> = view
        .inputs
        .materialized_views
        .iter()
        .filter_map(|id| views_by_node.get(id).map(|read| (*read).clone()))
        .collect();
    compile(below, &read, &[view.node_id as u64])
        .map(|_| ())
        .map_err(
            |error| InstallablePlanError::MaterializedViewNotExecutable {
                node_id: view.node_id,
                reason: error.to_string(),
            },
        )
}

/// The installable DAG of the timed selected plan, with each expanded
/// pane's retention by node id.
struct ExpandedWithRetention {
    dag: PhysicalASAPDAG,
    retention_ms_by_node: HashMap<PhysicalASAPNodeId, u64>,
    /// Each node id of the selected candidate's physical DAG (`timed`
    /// exported, Stage 3's ids) with the ids in `dag` that run it
    /// ([`PredictedNodeUsage::installed_node_ids`]).
    installed_node_ids: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
}

/// Expand the pane chains of `timed` and export it; each pane takes the
/// retention Stage 2 gives its chain (`RealizedPlan::pane_retention_ms`, by
/// node id in the selected candidate's physical DAG, which is `timed`
/// exported).
fn pane_retention_of_expanded_panes(
    timed: &[QueryRoot],
    pane_retention_ms: &BTreeMap<PhysicalASAPNodeId, u64>,
) -> Result<ExpandedWithRetention, InstallablePlanError> {
    use asap_types::ir::physical_export::compile_physical_asap_workload_with_node_ids;
    let selected = compile_physical_asap_workload_with_node_ids(timed)?;
    // Reject unsupported raw-source shapes before expanding possibly thousands
    // of panes. The complete expanded-plan validation remains authoritative.
    selected.dag.check_raw_data_windows()?;
    let mut retention_by_chain: Vec<(Rc<OperatorNode>, u64)> = Vec::new();
    for (&node_id, &retention_ms) in pane_retention_ms {
        let chain = selected
            .node_ids
            .operator_node(node_id)
            .filter(|node| matches!(node.asap(), Some(ASAPOp::PaneChain { .. })))
            .ok_or(InstallablePlanError::PaneRetentionOfUnknownChain { node_id })?;
        retention_by_chain.push((Rc::clone(chain), retention_ms));
    }
    let expansion =
        expand_pane_chains_in_workload(timed).map_err(InstallablePlanError::PaneChainExpansion)?;
    let compiled = compile_physical_asap_workload_with_node_ids(&expansion.roots)?;
    // Index physical identity once. Looking up every expanded pane through
    // the identity map's linear search makes this phase quadratic in panes.
    let mut installed_ids_by_pointer = HashMap::with_capacity(compiled.dag.nodes.len());
    for node in &compiled.dag.nodes {
        if let Some(operator) = compiled.node_ids.operator_node(node.id) {
            installed_ids_by_pointer.insert(Rc::as_ptr(operator), node.id);
        }
    }
    let installed_id =
        |node: &Rc<OperatorNode>| installed_ids_by_pointer.get(&Rc::as_ptr(node)).copied();
    let mut retention_ms_by_node = HashMap::new();
    for expanded in &expansion.chains {
        let Some((_, retention_ms)) = retention_by_chain
            .iter()
            .find(|(chain, _)| Rc::ptr_eq(chain, &expanded.chain))
        else {
            continue;
        };
        for pane in &expanded.panes {
            if let Some(node_id) = installed_id(pane) {
                retention_ms_by_node.insert(node_id, *retention_ms);
            }
        }
    }
    // A chain runs as its panes; every other node as the node replacing it
    // (itself when nothing below it changed).
    let mut installed_node_ids = HashMap::new();
    for node in &selected.dag.nodes {
        let Some(input) = selected.node_ids.operator_node(node.id) else {
            continue;
        };
        let ids: Vec<PhysicalASAPNodeId> = match expansion
            .chains
            .iter()
            .find(|expanded| Rc::ptr_eq(&expanded.chain, input))
        {
            Some(expanded) => {
                let mut ids = Vec::with_capacity(expanded.panes.len());
                for pane in &expanded.panes {
                    if let Some(id) = installed_id(pane) {
                        ids.push(id);
                    }
                }
                ids
            }
            None => {
                let replacement = expansion
                    .expanded_nodes
                    .iter()
                    .find(|(original, _)| Rc::ptr_eq(original, input))
                    .map_or(input, |(_, replacement)| replacement);
                installed_id(replacement).into_iter().collect()
            }
        };
        installed_node_ids.insert(node.id, ids);
    }
    Ok(ExpandedWithRetention {
        dag: compiled.dag,
        retention_ms_by_node,
        installed_node_ids,
    })
}

/// Stage 3's record of its selected plan.
fn report_of(
    selection: &Selection,
    installed_node_ids: &HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
) -> SelectedPlanReport {
    let cost = &selection.cost;
    let queries_accepted_without_guarantee = selection
        .accepted_accuracy_per_query
        .iter()
        .enumerate()
        .filter(|(_, accepted)| accepted.guarantee_check_rejection.is_some())
        .map(|(query, _)| query)
        .collect();
    SelectedPlanReport {
        queries_accepted_without_guarantee,
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
                    stage3_node_id: id,
                    installed_node_ids: installed_node_ids.get(&id).cloned().unwrap_or_default(),
                    operator: node.operator,
                    timing: node.timing,
                    cpu_core_seconds_per_second: node.cpu_core_seconds_per_second,
                    cost_per_second: node.cost_per_second,
                    price_source: node.price_source,
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
        search_report: selection.search_report.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::schema::{SketchAlgorithm, StatisticKind, SupportedSummary};
    use asap_types::ir::{NonASAPOp, Operator};
    use asap_types::physical::Materialization;

    thread_local! {
        /// The plan nodes handed to the executor's compiler by the views'
        /// refreshes in [`InstallablePlan::of_dag`] and
        /// [`InstallablePlan::check_compiles`], summed over their compiles.
        pub(super) static COMPILED_NODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// A p99 panel over a 300 s window every second, with no raw data kept:
    /// one DDSketch per series in each of 300 one-second panes built at
    /// ingestion, about 300 materialized views in a DAG of about 900 nodes.
    /// Each view's refresh is compiled over the nodes below it, and the
    /// whole plan's checks run once, so the nodes handed to the executor
    /// grow with the plan, not with the plan times its views (before, every
    /// refresh compiled the whole plan). The count
    /// covers both `plan_installable`'s build and `check_compiles`.
    #[tokio::test]
    async fn view_refreshes_compile_only_the_nodes_below_them() {
        let workload: crate::PlanningWorkload = serde_json::from_str(
            r#"{"query_workload": {"language": "promql", "repeating_queries": [{
                  "query": "avg by (job) (quantile_over_time(0.99, data[300s]))",
                  "demand": {"fixed_interval": 1000},
                  "requirements": {"accuracy": {"max_error": 0.05, "failure_probability": 0.3,
                      "accuracy_scope": "whole_result"}},
                  "predictability": {"predictable": {"known_at": null}}}]},
                "data_workload": {"arrival": {"ingesting": {"rows_per_second": 10000.0}},
                  "sources": {"data": {"series_count": 1000, "metric_type": "gauge",
                      "sample_interval": 100, "values_non_negative": true}}}}"#,
        )
        .unwrap();
        let capabilities = DeploymentCapabilities {
            raw_data_retained: false,
            ..DeploymentCapabilities::default()
        };
        COMPILED_NODES.with(|count| count.set(0));
        let (plan, _) = plan_installable(crate::UserInput::new(
            &workload,
            crate::FrontendInput::Promql { histograms: None },
            crate::PlanningModels::builtin().with_deployment_capabilities(&capabilities),
        ))
        .await
        .expect("a plan");
        let nodes = plan.dag.nodes.len();
        let views = plan.materialized_views().unwrap().len();
        assert!(views >= 300, "{views} materialized views");
        // Measure one installation and one `check_compiles`, independently of
        // how many candidates the optimizer explored before selecting this DAG.
        COMPILED_NODES.with(|count| count.set(0));
        let rebuilt = InstallablePlan::of_dag(
            plan.dag.clone(),
            plan.query_texts.clone(),
            &HashMap::new(),
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(rebuilt.materialized_views().unwrap().len(), views);
        plan.check_compiles().unwrap();
        let compiled = COMPILED_NODES.with(std::cell::Cell::get);
        assert!(
            compiled <= 6 * nodes,
            "{compiled} nodes compiled for a plan of {nodes} nodes and {views} views"
        );
    }

    /// `TimeShift(10 min) <- TimeRange(5 min) <- Scan`, all at query time:
    /// the shift above the range moves its window, which a window of a
    /// fixed offset below the range cannot say, so it is refused; without
    /// the shift the window is listed.
    #[test]
    fn a_time_shift_above_the_range_is_refused() {
        use asap_types::ir::operator::operator_properties::TimeShift;
        use asap_types::ir::operator::Source;
        use asap_types::ir::physical_export::compile_physical_asap_dag;
        use asap_types::ir::schema::{DataType, Field, Schema};
        use asap_types::ir::{
            apply_materialization_timings, NodeToTimedNodeCache, OperatorNode, TimeRangeKind,
        };
        use std::time::Duration;
        let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries {
                metric: "latency_ms".into(),
            },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("value", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        }))
        .unwrap();
        let range = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(300),
            kind: TimeRangeKind::Range,
            child: scan,
        }))
        .unwrap();
        let shifted = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
            shift: TimeShift {
                offset_ms: 600_000,
                at: None,
            },
            child: std::rc::Rc::clone(&range),
        }))
        .unwrap();
        let export = |root| {
            let timed = apply_materialization_timings(
                &root,
                &MaterializationAssignment::default(),
                &mut NodeToTimedNodeCache::new(),
            )
            .unwrap();
            compile_physical_asap_dag(&timed).unwrap()
        };
        let install = |dag| {
            InstallablePlan::of_dag(dag, vec![String::new()], &HashMap::new(), &HashMap::new())
        };
        match install(export(shifted)) {
            Err(InstallablePlanError::Uninstallable(
                InstallableDagError::UnsupportedRawDataWindow { reason, .. },
            )) => {
                assert_eq!(reason, "a subquery or a TimeShift above the range")
            }
            other => panic!("{other:?}"),
        }
        let plan = install(export(range)).unwrap();
        assert!(plan.materialized_views().unwrap().is_empty());
        assert_eq!(
            plan.raw_data_retention().unwrap(),
            [RawDataRetention::new(
                Source::TimeSeries {
                    metric: "latency_ms".into()
                },
                Some(300_000)
            )]
        );
    }

    // A summary stored at ingestion time over a window ending at the
    // evaluation and one ending a minute before it cannot be built in one
    // ingestion run, so the plan is refused, not built wrong.
    #[test]
    fn an_ingestion_build_of_windows_at_different_offsets_is_refused() {
        use asap_types::ir::operator::operator_properties::TimeShift;
        use asap_types::ir::operator::{Reduction, Source};
        use asap_types::ir::physical_export::compile_physical_asap_dag;
        use asap_types::ir::scalar::ColumnRef;
        use asap_types::ir::schema::{
            DataType, ExactKind, ExactParams, Field, FieldDataType, Schema, SummaryUpdate,
        };
        use asap_types::ir::{
            apply_materialization_timings, NodeToTimedNodeCache, OperatorNode, TimeRangeKind,
        };
        use asap_types::physical::Medium;
        use std::time::Duration;
        let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries {
                metric: "latency_ms".into(),
            },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("value", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        }))
        .unwrap();
        // A one-minute window ending `offset_ms` before the evaluation.
        let window = |offset_ms: i64| {
            let shifted = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeShift {
                shift: TimeShift {
                    offset_ms,
                    at: None,
                },
                child: Rc::clone(&scan),
            }))
            .unwrap();
            OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
                range: Duration::from_secs(60),
                kind: TimeRangeKind::Range,
                child: shifted,
            }))
            .unwrap()
        };
        // One sum over both windows' rows.
        let both = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Concat {
            children: vec![window(0), window(60_000)],
            discriminator_unique_key: None,
        }))
        .unwrap();
        let merged = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
            child: both,
            family: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            input: SummaryUpdate::column(ColumnRef::Named("value".into())),
            reduction: Reduction::by(vec![]),
            grouping: Default::default(),
            filter: None,
        }))
        .unwrap();
        let mut assignment = MaterializationAssignment::default();
        assignment.set(&merged, Materialization::MatAtIngestion(Medium::Memory));
        let timed =
            apply_materialization_timings(&merged, &assignment, &mut NodeToTimedNodeCache::new())
                .unwrap();
        let dag = compile_physical_asap_dag(&timed).unwrap();
        match InstallablePlan::of_dag(dag, vec![String::new()], &HashMap::new(), &HashMap::new()) {
            Err(InstallablePlanError::Uninstallable(
                InstallableDagError::IngestionBuildReadsWindowsOfDifferentOffsets {
                    end_offsets_ms,
                    ..
                },
            )) => assert_eq!(end_offsets_ms, [0, 60_000]),
            other => panic!("{other:?}"),
        }
        // A merge stored at ingestion time over two panes stored at
        // ingestion time, one a minute older: an ingestion run would read
        // both panes for the window ending at its end, so it is refused too.
        let pane = |offset_ms: i64| {
            OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
                child: window(offset_ms),
                family: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
                input: SummaryUpdate::column(ColumnRef::Named("value".into())),
                reduction: Reduction::by(vec![]),
                grouping: Default::default(),
                filter: None,
            }))
            .unwrap()
        };
        let panes = [pane(0), pane(60_000)];
        let merge = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
            children: panes.to_vec(),
            newest_panes: None,
            group_by: None,
        }))
        .unwrap();
        let mut assignment = MaterializationAssignment::default();
        for node in panes.iter().chain([&merge]) {
            assignment.set(node, Materialization::MatAtIngestion(Medium::Memory));
        }
        let timed =
            apply_materialization_timings(&merge, &assignment, &mut NodeToTimedNodeCache::new())
                .unwrap();
        let dag = compile_physical_asap_dag(&timed).unwrap();
        match InstallablePlan::of_dag(dag, vec![String::new()], &HashMap::new(), &HashMap::new()) {
            Err(InstallablePlanError::Uninstallable(
                InstallableDagError::IngestionBuildReadsWindowsOfDifferentOffsets {
                    end_offsets_ms,
                    ..
                },
            )) => assert_eq!(end_offsets_ms, [0, 60_000]),
            other => panic!("{other:?}"),
        }
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
                family: SketchAlgorithm::Kll,
                readouts: vec![StatisticKind::Quantile],
            }]),
            ..DeploymentCapabilities::default()
        };
        assert_eq!(executor_summaries_when_unrestricted(&kll_only), kll_only);
    }
}
