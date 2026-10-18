//! The plan a deployment runtime installs: one call from a workload and the
//! deployment's inputs to the single selected physical ASAP DAG, ready to
//! execute ([`plan_installable`]).
//!
//! The runtime writes the returned [`InstallablePlan`] as its plan file and
//! executes its DAG with the executor, as given (#509 Assumption 8). It
//! needs three things from it: the DAG to install (`dag`), the outputs to
//! store (`stored_outputs`), and where raw samples go (each stored output's
//! `pane` and `query_time_raw_windows`, fed with
//! `asap_executor::RawSampleSelector`). How the Planner represents the
//! plan internally (e.g. a tumbling-pane chain, which the stage export keeps
//! for costing and display) is not the runtime's concern: the installable
//! DAG has every chain expanded into the panes the executor runs.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_logical_asap_optimization::DeploymentCapabilities;
use asap_plan_selection::Selection;
use asap_types::ir::operator::pane_chain::expand_pane_chains_in_workload;
use asap_types::ir::physical_export::{
    compile_physical_asap_workload_with_node_ids, PhysicalASAPDAG, PhysicalASAPDAGDocument,
    PhysicalASAPDAGNode, PhysicalASAPDAGValidationError, PhysicalASAPNodeId, RawSampleWindow,
    ReferencePlanError,
};
use asap_types::ir::properties::{ExecutionDataStateError, ExecutionTiming};
use asap_types::ir::SchemaDerivationError;
use asap_types::ir::{
    apply_materialization_timings_to_roots, ASAPOp, MaterializationAssignment, NonASAPOp, Operator,
    OperatorNode, QueryRoot,
};
use asap_types::physical::Materialization;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{e2e_plan, PlanError, UserInput};

/// The selected plan, ready to install and execute: what
/// [`plan_installable`] returns and a runtime writes as its plan file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallablePlan {
    /// The selected physical ASAP DAG with Stage 3's timings and
    /// materialization, every pane chain expanded into its panes, in its
    /// versioned envelope; the executor compiles `dag.dag` as is.
    pub dag: PhysicalASAPDAGDocument,
    /// One per workload query, in `QueryWorkload::entries()` order: its
    /// text and the root of the DAG that answers it.
    pub queries: Vec<InstalledQuery>,
    /// Every node whose output the runtime stores across evaluations (its
    /// node's materialization is `MatAtIngestion` or
    /// `MatAtQueryKeptAcrossEvaluations`), in node order, with the pane of
    /// raw samples each one is built from.
    pub stored_outputs: Vec<StoredOutput>,
    /// Every window of raw samples read at query time other than a stored
    /// output's pane, in node order: the runtime feeds each its samples
    /// when a query evaluates.
    pub query_time_raw_windows: Vec<RawSampleWindow>,
    /// Stage 3's record of the selected plan, for reporting only; `None` for
    /// [`InstallablePlan::query_time_reference`] and for a pass
    /// that selects without pricing.
    pub selection: Option<SelectedPlanReport>,
}

/// One workload query and the root of the installable DAG that answers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledQuery {
    /// The query text, as the workload gave it.
    pub query: String,
    /// The node whose output answers it, one of `dag.dag.roots`.
    pub root_node_id: PhysicalASAPNodeId,
}

/// One node of the installable DAG whose output the runtime stores, and the
/// pane of raw samples it is built from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredOutput {
    /// The node whose output is stored.
    pub node_id: PhysicalASAPNodeId,
    /// Which run builds it: an ingestion run over its pane's window, or
    /// the query run of the evaluation that first reads it.
    pub built_at: StoredOutputBuild,
    /// Hex SHA-256 of what the output computes, independent of node ids
    /// and of which pane it reads: stored outputs that differ only in
    /// `pane.end_offset_ms` share it, so a runtime builds each pane
    /// once; equal across plans of one DAG wire version.
    pub computation_id: String,
    /// The pane of raw samples it is built from.
    pub pane: RawSampleWindow,
    /// How far back from the newest evaluation time the runtime keeps this
    /// pane once built (a pane whose window starts earlier is dropped):
    /// Stage 3's retention of its chain (`Selection::pane_retention_ms`:
    /// its input's offset plus its window plus the longest
    /// range-evaluation span of its readers); or, for an output Stage 3
    /// records none for, how far back its pane reaches
    /// (`pane.end_offset_ms + pane.width_ms`).
    pub retention_ms: u64,
}

/// Which run builds a stored output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredOutputBuild {
    /// Built at ingestion time (`MatAtIngestion`): the runtime runs the
    /// node in an ingestion run over its pane's window once the pane's
    /// samples have arrived, and stores the result.
    IngestionRun,
    /// Built at query time and kept across evaluations
    /// (`MatAtQueryKeptAcrossEvaluations`): when an evaluation needs the
    /// pane and the store lacks it, the runtime runs the node in a query
    /// run over its pane's samples, stores the result, and binds it like
    /// a stored pane. Its `TimeRange` is fed only through this build, so
    /// it is not listed in `query_time_raw_windows`.
    QueryRun,
}

/// What Stage 3 recorded about the plan it selected, for reporting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedPlanReport {
    /// The selected physical candidate's id, e.g. `P3-m1`.
    pub selected_candidate_id: String,
    /// Its price per second; `None` under cost model 3, which has no
    /// prices, or when Stage 3 recorded no cost for it.
    pub cost_per_second: Option<f64>,
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
    /// Bad input (a plan file): the DAG is not a valid physical ASAP DAG
    /// of this wire version.
    #[error("invalid physical ASAP DAG: {0}")]
    InvalidDag(#[from] PhysicalASAPDAGValidationError),
    /// Bad input (a plan file): `queries` does not bind each query, in
    /// order, to the DAG's roots.
    #[error("queries do not match the DAG's {roots} roots")]
    QueriesDoNotMatchRoots { roots: usize },
    /// Unsatisfiable request: query `entry_index` is a standalone scalar
    /// query (e.g. `scalar(sum(m))`), which no node of the DAG answers, so
    /// a runtime cannot answer it through the executor's public API.
    #[error("query {entry_index} is a standalone scalar query, which is not supported yet")]
    UnsupportedQueryRoot { entry_index: usize },
    /// Bad input (a plan file): `stored_outputs` or `query_time_raw_windows`
    /// is not what the DAG stores or reads.
    #[error("stored_outputs or query_time_raw_windows do not match the DAG")]
    WindowsOrStoredOutputsDoNotMatchDag,
    /// Bad input (a plan file): stored output `node_id` is kept for less
    /// than its pane reaches back, so a later evaluation would miss it.
    #[error("stored output {node_id} is kept for less than its pane reaches back")]
    StoredOutputRetentionTooShort { node_id: PhysicalASAPNodeId },
    /// Internal bug: Stage 3 records a pane retention for node `node_id`
    /// of the selected plan, which is not one of its pane chains.
    #[error("pane retention for node {node_id}, which is not a pane chain of the selected plan")]
    PaneRetentionOfUnknownChain { node_id: PhysicalASAPNodeId },
    /// Unsatisfiable request: the plan stores node `node_id`, which is not
    /// built from one pane of raw samples (`TimeRange <- TimeShift* <-
    /// Scan` below single-input operators), so a runtime cannot build it.
    #[error("stored output {node_id} is not built from one pane of raw samples")]
    StoredOutputWithoutPane { node_id: PhysicalASAPNodeId },
    /// Unsatisfiable request: the plan reads raw samples through
    /// `TimeRange` `time_range_node_id` in a shape whose window is not a
    /// fixed offset from the evaluation time (`reason`: a subquery or a
    /// `TimeShift` above it, or an `@` anchor below it). Not supported yet.
    #[error("raw-sample window of node {time_range_node_id} is unsupported: {reason}")]
    UnsupportedRawSampleWindow {
        time_range_node_id: PhysicalASAPNodeId,
        reason: &'static str,
    },
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
/// ready to install. Supported summaries the caller leaves unrestricted
/// (`DeploymentCapabilities::supported_summaries` `None`) default to the
/// families the Planner's executor runs; raw-data and query-time-result
/// retention stay the caller's.
pub async fn plan_installable(input: UserInput<'_>) -> Result<InstallablePlan, PlanError> {
    let capabilities = executor_summaries_when_unrestricted(input.models.deployment_capabilities);
    let query_texts: Vec<String> = input
        .workload
        .query_workload
        .entries()
        .map(|entry| entry.query.0)
        .collect();
    let input = UserInput {
        workload: input.workload,
        frontend_specific: input.frontend_specific,
        models: input.models.with_deployment_capabilities(&capabilities),
        pass: input.pass,
    };
    let output = e2e_plan(input).await?;
    let selection = output.selection.as_ref().map(report_of);
    let installable = || -> Result<InstallablePlan, InstallablePlanError> {
        // Every node keeps the timing Stage 3 gave it.
        let timed = apply_materialization_timings_to_roots(
            &output.roots(),
            &MaterializationAssignment::default(),
        )?;
        let retention_ms = pane_retention_of_expanded_panes(&timed, output.selection.as_ref())?;
        InstallablePlan::of_dag(
            retention_ms.dag,
            query_texts,
            selection,
            &retention_ms.by_node,
        )
    };
    installable().map_err(PlanError::Installable)
}

impl InstallablePlan {
    /// The plan of `dag`, with what the runtime reads from it derived.
    fn of_dag(
        dag: PhysicalASAPDAG,
        query_texts: Vec<String>,
        selection: Option<SelectedPlanReport>,
        retention_ms_by_node: &HashMap<PhysicalASAPNodeId, u64>,
    ) -> Result<Self, InstallablePlanError> {
        let (stored_outputs, query_time_raw_windows) =
            runtime_inputs_of(&dag, retention_ms_by_node)?;
        let plan = InstallablePlan {
            queries: queries_bound_to_roots(&dag, query_texts)?,
            stored_outputs,
            query_time_raw_windows,
            dag: PhysicalASAPDAGDocument::new(dag),
            selection,
        };
        // Derived from the exported (validated) DAG, so it validates.
        debug_assert!(plan.validate().is_ok());
        Ok(plan)
    }

    /// Checks a plan, e.g. one read from a plan file: the DAG is valid and
    /// of this wire version, and `queries`, `stored_outputs` and
    /// `query_time_raw_windows` are what it binds, stores and reads.
    pub fn validate(&self) -> Result<(), InstallablePlanError> {
        self.dag.validate()?;
        let dag = &self.dag.dag;
        if queries_bound_to_roots(dag, self.query_texts())? != self.queries {
            return Err(InstallablePlanError::QueriesDoNotMatchRoots {
                roots: dag.roots.len(),
            });
        }
        // Retention comes from Stage 3, not from the DAG: take the plan's
        // own, and check only that each pane is kept while it is read.
        let retention_ms_by_node = self
            .stored_outputs
            .iter()
            .map(|output| (output.node_id, output.retention_ms))
            .collect();
        if let Some(short) = self
            .stored_outputs
            .iter()
            .find(|output| output.retention_ms < output.pane.reach_ms())
        {
            return Err(InstallablePlanError::StoredOutputRetentionTooShort {
                node_id: short.node_id,
            });
        }
        let (stored_outputs, query_time_raw_windows) =
            runtime_inputs_of(dag, &retention_ms_by_node)?;
        if stored_outputs != self.stored_outputs
            || query_time_raw_windows != self.query_time_raw_windows
        {
            return Err(InstallablePlanError::WindowsOrStoredOutputsDoNotMatchDag);
        }
        Ok(())
    }

    /// For correctness tests only, never for installing: this plan with
    /// nothing materialized, the same operators, summaries and parameters
    /// with every node at query time, and the same queries. A runtime's
    /// test compares its installed plan's answers with the executor running
    /// this one over raw samples. Its `selection` is `None`. Validates this
    /// plan first.
    pub fn query_time_reference(&self) -> Result<InstallablePlan, InstallablePlanError> {
        self.validate()?;
        let dag = self.dag.dag.with_every_node_at_query_time()?;
        InstallablePlan::of_dag(dag, self.query_texts(), None, &HashMap::new())
    }

    fn query_texts(&self) -> Vec<String> {
        self.queries.iter().map(|q| q.query.clone()).collect()
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

/// Query `i` of `query_texts` bound to root `i` of `dag`; a standalone
/// scalar query, which no node answers, is refused.
fn queries_bound_to_roots(
    dag: &PhysicalASAPDAG,
    query_texts: Vec<String>,
) -> Result<Vec<InstalledQuery>, InstallablePlanError> {
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
    Ok(query_texts
        .into_iter()
        .zip(&dag.roots)
        .map(|(query, &root_node_id)| InstalledQuery {
            query,
            root_node_id,
        })
        .collect())
}

/// The nodes of a validated (acyclic) DAG by id, each node's inputs in edge
/// order, and two facts every raw-sample window check reads, computed in
/// one pass each over the DAG (O(nodes + edges)).
struct IndexedDag<'a> {
    nodes: HashMap<PhysicalASAPNodeId, &'a PhysicalASAPDAGNode>,
    inputs: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
    /// The nodes whose rows come from raw samples: a `Scan` is reachable
    /// below them without passing a `TimeRange`.
    reading_raw_samples: HashSet<PhysicalASAPNodeId>,
    /// The nodes with a `TimeShift`, `TimeRange` or `PromqlSubquery` above
    /// them, which evaluates them at other times than the evaluation time.
    re_timed_from_above: HashSet<PhysicalASAPNodeId>,
}

/// Whether `operator` evaluates its input at other times than its own.
fn re_times_its_input(operator: &Operator<PhysicalASAPNodeId>) -> bool {
    matches!(
        operator,
        Operator::NonASAP(
            NonASAPOp::TimeShift { .. }
                | NonASAPOp::TimeRange { .. }
                | NonASAPOp::PromqlSubquery { .. }
        )
    )
}

impl<'a> IndexedDag<'a> {
    fn new(dag: &'a PhysicalASAPDAG) -> Self {
        let nodes: HashMap<_, _> = dag.nodes.iter().map(|n| (n.id, n)).collect();
        let mut inputs: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = HashMap::new();
        for edge in &dag.edges {
            inputs.entry(edge.consumer).or_default().push(edge.producer);
        }
        // Every node after its inputs: an iterative depth-first postorder.
        let mut children_first = Vec::with_capacity(dag.nodes.len());
        let mut visited = HashSet::new();
        for node in &dag.nodes {
            let mut pending = vec![(node.id, false)];
            while let Some((id, inputs_done)) = pending.pop() {
                if inputs_done {
                    children_first.push(id);
                } else if visited.insert(id) {
                    pending.push((id, true));
                    for &input in inputs.get(&id).into_iter().flatten() {
                        pending.push((input, false));
                    }
                }
            }
        }
        let payload = |id: &PhysicalASAPNodeId| &nodes[id].payload;
        let mut reading_raw_samples = HashSet::new();
        for id in &children_first {
            let reads = inputs
                .get(id)
                .into_iter()
                .flatten()
                .any(|input| match payload(input) {
                    Operator::NonASAP(NonASAPOp::Scan { .. }) => true,
                    Operator::NonASAP(NonASAPOp::TimeRange { .. }) => false,
                    _ => reading_raw_samples.contains(input),
                });
            if reads {
                reading_raw_samples.insert(*id);
            }
        }
        let mut re_timed_from_above = HashSet::new();
        for id in children_first.iter().rev() {
            if re_timed_from_above.contains(id) || re_times_its_input(payload(id)) {
                re_timed_from_above.extend(inputs.get(id).into_iter().flatten().copied());
            }
        }
        IndexedDag {
            nodes,
            inputs,
            reading_raw_samples,
            re_timed_from_above,
        }
    }

    fn relational(&self, id: PhysicalASAPNodeId) -> Option<&'a NonASAPOp<PhysicalASAPNodeId>> {
        match &self.nodes.get(&id)?.payload {
            Operator::NonASAP(op) => Some(op),
            Operator::ASAP(_) => None,
        }
    }

    fn only_input(&self, id: PhysicalASAPNodeId) -> Option<PhysicalASAPNodeId> {
        match self.inputs.get(&id).map(Vec::as_slice) {
            Some([input]) => Some(*input),
            _ => None,
        }
    }

    /// The raw-sample window read through the `TimeRange` `time_range`, or
    /// why its shape is unsupported.
    fn window_at(
        &self,
        time_range: PhysicalASAPNodeId,
    ) -> Result<RawSampleWindow, InstallablePlanError> {
        let unsupported = |reason| InstallablePlanError::UnsupportedRawSampleWindow {
            time_range_node_id: time_range,
            reason,
        };
        let Some(NonASAPOp::TimeRange { range, .. }) = self.relational(time_range) else {
            return Err(unsupported("not a TimeRange"));
        };
        if self.re_timed_from_above.contains(&time_range) {
            return Err(unsupported("a subquery or a TimeShift above the range"));
        }
        let width_ms =
            i64::try_from(range.as_millis()).map_err(|_| unsupported("window width overflows"))?;
        let mut end_offset_ms = 0i64;
        let mut current = time_range;
        loop {
            current = self.only_input(current).ok_or(unsupported(
                "an operator with several inputs below the range",
            ))?;
            match self.relational(current) {
                Some(NonASAPOp::TimeShift { shift, .. }) if shift.at.is_some() => {
                    return Err(unsupported("an @ anchor below the range"))
                }
                Some(NonASAPOp::TimeShift { shift, .. }) => {
                    end_offset_ms = end_offset_ms
                        .checked_add(shift.offset_ms)
                        .ok_or(unsupported("window offset overflows"))?;
                }
                Some(NonASAPOp::Scan { .. }) => {
                    return Ok(RawSampleWindow {
                        time_range_node_id: time_range,
                        width_ms,
                        end_offset_ms,
                        scan_node_id: current,
                    })
                }
                _ => {
                    return Err(unsupported(
                        "an operator other than TimeShift below the range",
                    ))
                }
            }
        }
    }

    /// The pane stored output `node` is built from: follow single inputs
    /// down to a `TimeRange`, then [`Self::window_at`] it.
    fn pane_of(&self, node: PhysicalASAPNodeId) -> Result<RawSampleWindow, InstallablePlanError> {
        let without_pane = || InstallablePlanError::StoredOutputWithoutPane { node_id: node };
        let mut current = node;
        loop {
            match self.relational(current) {
                Some(NonASAPOp::TimeRange { .. }) => return self.window_at(current),
                Some(NonASAPOp::TimeShift { .. } | NonASAPOp::Scan { .. }) => {
                    return Err(without_pane())
                }
                _ => current = self.only_input(current).ok_or_else(without_pane)?,
            }
        }
    }
}

/// Every node of `dag` stored across evaluations with its pane, in node
/// order, and every window of raw samples read at query time, in node
/// order; an error for a stored output without a pane or a raw-sample
/// window of an unsupported shape.
fn runtime_inputs_of(
    dag: &PhysicalASAPDAG,
    retention_ms_by_node: &HashMap<PhysicalASAPNodeId, u64>,
) -> Result<(Vec<StoredOutput>, Vec<RawSampleWindow>), InstallablePlanError> {
    let indexed = IndexedDag::new(dag);
    let mut computation_hashes = HashMap::new();
    let mut stored_outputs = Vec::new();
    // Every raw-sample window with its timing, and the ranges of stored
    // outputs' panes, which are fed through those outputs' builds.
    let mut raw_windows = Vec::new();
    let mut stored_pane_ranges = HashSet::new();
    for node in &dag.nodes {
        if matches!(
            node.materialization,
            Materialization::MatAtIngestion(_)
                | Materialization::MatAtQueryKeptAcrossEvaluations(_)
        ) {
            let pane = indexed.pane_of(node.id)?;
            stored_pane_ranges.insert(pane.time_range_node_id);
            stored_outputs.push(StoredOutput {
                node_id: node.id,
                built_at: match node.materialization {
                    Materialization::MatAtIngestion(_) => StoredOutputBuild::IngestionRun,
                    _ => StoredOutputBuild::QueryRun,
                },
                retention_ms: retention_ms_by_node
                    .get(&node.id)
                    .copied()
                    .unwrap_or_else(|| pane.reach_ms()),
                computation_id: computation_hash(node.id, &indexed, &mut computation_hashes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
                pane,
            });
        }
        if matches!(node.payload, Operator::NonASAP(NonASAPOp::TimeRange { .. }))
            && indexed.reading_raw_samples.contains(&node.id)
        {
            // Every raw-sample window must be one the runtime can feed; a
            // stored output's pane is fed through that output's build.
            raw_windows.push((node.output_state.timing, indexed.window_at(node.id)?));
        }
    }
    let query_time_raw_windows = raw_windows
        .into_iter()
        .filter(|(timing, window)| {
            *timing == ExecutionTiming::QueryTime
                && !stored_pane_ranges.contains(&window.time_range_node_id)
        })
        .map(|(_, window)| window)
        .collect();
    Ok((stored_outputs, query_time_raw_windows))
}

/// SHA-256 over a node's payload (input ids zeroed) and output schema, then
/// its inputs' hashes in edge order. An offset `TimeShift` with one input
/// takes its input's hash: the offset says which pane an output reads, not
/// what it computes.
fn computation_hash(
    node: PhysicalASAPNodeId,
    dag: &IndexedDag,
    done: &mut HashMap<PhysicalASAPNodeId, [u8; 32]>,
) -> [u8; 32] {
    if let Some(hash) = done.get(&node) {
        return *hash;
    }
    let dag_node = dag.nodes[&node];
    let node_inputs = dag.inputs.get(&node).map(Vec::as_slice).unwrap_or_default();
    let hash = match (&dag_node.payload, node_inputs) {
        (Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }), [input]) if shift.at.is_none() => {
            computation_hash(*input, dag, done)
        }
        _ => {
            let payload = dag_node.payload.map_children(|_| 0);
            let mut hasher = Sha256::new();
            hasher.update(
                serde_json::to_vec(&(&payload, &dag_node.output_schema)).expect("serializes"),
            );
            for &input in node_inputs {
                hasher.update(computation_hash(input, dag, done));
            }
            hasher.finalize().into()
        }
    };
    done.insert(node, hash);
    hash
}

/// The installable DAG of the timed selected plan, with each expanded
/// pane's retention by node id.
struct ExpandedWithRetention {
    dag: PhysicalASAPDAG,
    by_node: HashMap<PhysicalASAPNodeId, u64>,
}

/// Expand the pane chains of `timed` and export it; each pane takes the
/// retention Stage 3 gives its chain (`Selection::pane_retention_ms`, by
/// node id in the selected candidate's physical DAG, which is `timed`
/// exported).
fn pane_retention_of_expanded_panes(
    timed: &[QueryRoot],
    selection: Option<&Selection>,
) -> Result<ExpandedWithRetention, InstallablePlanError> {
    let selected = compile_physical_asap_workload_with_node_ids(timed)?;
    let mut retention_by_chain: Vec<(Rc<OperatorNode>, u64)> = Vec::new();
    for (&node_id, &retention_ms) in selection.iter().flat_map(|s| &s.pane_retention_ms) {
        let chain = selected
            .node_ids
            .operator_node(node_id)
            .filter(|node| matches!(node.asap(), Some(ASAPOp::PaneChain { .. })))
            .ok_or(InstallablePlanError::PaneRetentionOfUnknownChain { node_id })?;
        retention_by_chain.push((Rc::clone(chain), retention_ms));
    }
    let expansion = expand_pane_chains_in_workload(timed)?;
    let compiled = compile_physical_asap_workload_with_node_ids(&expansion.roots)?;
    let mut by_node = HashMap::new();
    for expanded in &expansion.chains {
        let Some((_, retention_ms)) = retention_by_chain
            .iter()
            .find(|(chain, _)| Rc::ptr_eq(chain, &expanded.chain))
        else {
            continue;
        };
        for pane in &expanded.panes {
            if let Some(node_id) = compiled.node_ids.node_id(pane) {
                by_node.insert(node_id, *retention_ms);
            }
        }
    }
    Ok(ExpandedWithRetention {
        dag: compiled.dag,
        by_node,
    })
}

/// Stage 3's record of its selected candidate.
fn report_of(selection: &Selection) -> SelectedPlanReport {
    SelectedPlanReport {
        selected_candidate_id: selection.selected.clone(),
        cost_per_second: selection
            .costs
            .get(&selection.selected)
            .and_then(|cost| cost.cost_per_second),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::schema::{SketchAlgorithm, StatisticKind, SupportedSummary};

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
            apply_materialization_timings, OperatorNode, TimeRangeKind, TimingMemo,
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
                &mut TimingMemo::new(),
            )
            .unwrap();
            compile_physical_asap_dag(&timed).unwrap()
        };
        match runtime_inputs_of(&export(shifted), &HashMap::new()) {
            Err(InstallablePlanError::UnsupportedRawSampleWindow { reason, .. }) => {
                assert_eq!(reason, "a subquery or a TimeShift above the range")
            }
            other => panic!("{other:?}"),
        }
        let (stored, windows) = runtime_inputs_of(&export(range), &HashMap::new()).unwrap();
        assert!(stored.is_empty());
        assert_eq!(
            windows
                .iter()
                .map(|w| (w.width_ms, w.end_offset_ms))
                .collect::<Vec<_>>(),
            [(300_000, 0)]
        );
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
        assert!(!defaulted.supports(&SketchAlgorithm::Theta, StatisticKind::Distinct));
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
