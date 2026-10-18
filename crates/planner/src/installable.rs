//! The plan a deployment runtime installs: one call from a workload and the
//! deployment's inputs to the single selected physical ASAP DAG, ready to
//! execute ([`plan_installable`]).
//!
//! The runtime writes the returned [`InstallablePlan`] as its plan file and
//! executes its DAG with the executor, as given (#509 Assumption 8). It
//! needs three things from it: the DAG to install (`dag`), the outputs to
//! store (`stored_outputs`), and where raw samples go (each stored output's
//! `pane` and `query_time_raw_windows`, fed with
//! `asap_executor::input_rows_from_samples`). How the Planner represents the
//! plan internally (e.g. a tumbling-pane chain, which the stage export keeps
//! for costing and display) is not the runtime's concern: the installable
//! DAG has every chain expanded into the panes the executor runs.

use std::collections::{HashMap, HashSet};

use asap_logical_asap_optimization::DeploymentCapabilities;
use asap_plan_selection::Selection;
use asap_types::ir::operator::pane_chain::expand_pane_chains_in_workload;
use asap_types::ir::physical_export::{
    compile_physical_asap_workload, PhysicalASAPDAG, PhysicalASAPDAGDocument, PhysicalASAPDAGNode,
    PhysicalASAPDAGValidationError, PhysicalASAPNodeId, RawSampleWindow, ReferencePlanError,
};
use asap_types::ir::properties::{ExecutionDataStateError, ExecutionTiming};
use asap_types::ir::SchemaDerivationError;
use asap_types::ir::{
    apply_materialization_timings_to_roots, MaterializationAssignment, NonASAPOp, Operator,
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
    /// Every window of raw samples read at query time, in node order: the
    /// runtime feeds each its samples when a query evaluates.
    pub query_time_raw_windows: Vec<RawSampleWindow>,
    /// Stage 3's record of the selected plan, for reporting only; `None` for
    /// [`InstallablePlan::with_every_node_at_query_time`] and for a pass
    /// that selects without pricing.
    pub selection: Option<SelectedPlanReport>,
}

/// One workload query and the root of the installable DAG that answers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledQuery {
    /// The query text, as the workload gave it.
    pub query: String,
    /// Where in the DAG its answer comes from.
    pub root: InstalledQueryRoot,
}

/// Where a query's answer comes from in the installable DAG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstalledQueryRoot {
    /// An operator query: the output of node `node_id`, one of `dag.roots`.
    Operator { node_id: PhysicalASAPNodeId },
    /// A standalone scalar query: the expression `dag.scalar_roots[scalar_root_index]`.
    Scalar { scalar_root_index: usize },
}

/// One node of the installable DAG whose output the runtime stores, and the
/// pane of raw samples it is built from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredOutput {
    /// The node whose output is stored; its `materialization` says where
    /// and when.
    pub node_id: PhysicalASAPNodeId,
    /// Hex SHA-256 of what the output computes, independent of node ids
    /// and of which pane it reads: stored outputs that differ only in
    /// `pane.window_end_offset_ms` share it, so a runtime builds each pane
    /// once; equal across plans of one DAG wire version.
    pub computation_id: String,
    /// The pane of raw samples it is built from.
    pub pane: RawSampleWindow,
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
    #[error(
        "queries do not match the DAG's {operator_roots} operator and {scalar_roots} scalar roots"
    )]
    QueriesDoNotMatchRoots {
        operator_roots: usize,
        scalar_roots: usize,
    },
    /// Bad input (a plan file): `stored_outputs` or `query_time_raw_windows`
    /// is not what the DAG stores or reads.
    #[error("stored_outputs or query_time_raw_windows do not match the DAG")]
    WindowsOrStoredOutputsDoNotMatchDag,
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
        let dag = compile_physical_asap_workload(&expand_pane_chains_in_workload(&timed)?)?;
        InstallablePlan::of_dag(dag, query_texts, selection)
    };
    installable().map_err(PlanError::Installable)
}

impl InstallablePlan {
    /// The plan of `dag`, with what the runtime reads from it derived.
    fn of_dag(
        dag: PhysicalASAPDAG,
        query_texts: Vec<String>,
        selection: Option<SelectedPlanReport>,
    ) -> Result<Self, InstallablePlanError> {
        let (stored_outputs, query_time_raw_windows) = runtime_inputs_of(&dag)?;
        let plan = InstallablePlan {
            queries: queries_bound_to_roots(&dag, query_texts)?,
            stored_outputs,
            query_time_raw_windows,
            dag: PhysicalASAPDAGDocument::new(dag),
            selection,
        };
        plan.validate()?;
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
                operator_roots: dag.roots.len(),
                scalar_roots: dag.scalar_roots.len(),
            });
        }
        if runtime_inputs_of(dag)?
            != (
                self.stored_outputs.clone(),
                self.query_time_raw_windows.clone(),
            )
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
    pub fn with_every_node_at_query_time(&self) -> Result<InstallablePlan, InstallablePlanError> {
        self.validate()?;
        let dag = self.dag.dag.with_every_node_at_query_time()?;
        InstallablePlan::of_dag(dag, self.query_texts(), None)
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

/// Query `i` of `query_texts` bound to the scalar root keyed `i` when there
/// is one, and otherwise to the next of `dag.roots`.
fn queries_bound_to_roots(
    dag: &PhysicalASAPDAG,
    query_texts: Vec<String>,
) -> Result<Vec<InstalledQuery>, InstallablePlanError> {
    let mismatch = || InstallablePlanError::QueriesDoNotMatchRoots {
        operator_roots: dag.roots.len(),
        scalar_roots: dag.scalar_roots.len(),
    };
    if query_texts.len() != dag.roots.len() + dag.scalar_roots.len() {
        return Err(mismatch());
    }
    let mut operator_roots = dag.roots.iter();
    let mut scalar_root_index = 0;
    query_texts
        .into_iter()
        .enumerate()
        .map(|(position, query)| {
            let root = match dag.scalar_roots.get(scalar_root_index) {
                Some((at, _)) if *at == position => {
                    scalar_root_index += 1;
                    InstalledQueryRoot::Scalar {
                        scalar_root_index: scalar_root_index - 1,
                    }
                }
                _ => InstalledQueryRoot::Operator {
                    node_id: *operator_roots.next().ok_or_else(mismatch)?,
                },
            };
            Ok(InstalledQuery { query, root })
        })
        .collect()
}

/// The nodes of a DAG by id, each node's inputs in edge order, and each
/// node's consumers.
struct IndexedDag<'a> {
    nodes: HashMap<PhysicalASAPNodeId, &'a PhysicalASAPDAGNode>,
    inputs: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
    consumers: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
}

impl<'a> IndexedDag<'a> {
    fn new(dag: &'a PhysicalASAPDAG) -> Self {
        let mut indexed = IndexedDag {
            nodes: dag.nodes.iter().map(|n| (n.id, n)).collect(),
            inputs: HashMap::new(),
            consumers: HashMap::new(),
        };
        for edge in &dag.edges {
            indexed
                .inputs
                .entry(edge.consumer)
                .or_default()
                .push(edge.producer);
            indexed
                .consumers
                .entry(edge.producer)
                .or_default()
                .push(edge.consumer);
        }
        indexed
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

    /// Whether a `Scan` is reachable below `id` without passing another
    /// `TimeRange`: `id`'s rows come from raw samples.
    fn reads_raw_samples(&self, id: PhysicalASAPNodeId) -> bool {
        self.inputs
            .get(&id)
            .into_iter()
            .flatten()
            .any(|&input| match self.relational(input) {
                Some(NonASAPOp::Scan { .. }) => true,
                Some(NonASAPOp::TimeRange { .. }) => false,
                _ => self.reads_raw_samples(input),
            })
    }

    /// Whether some node above `id` is a `TimeShift`, a `TimeRange` or a
    /// `PromqlSubquery`, which evaluates `id` at other times than the
    /// evaluation time.
    fn re_timed_above(&self, id: PhysicalASAPNodeId) -> bool {
        let mut pending = vec![id];
        let mut seen = HashSet::new();
        while let Some(current) = pending.pop() {
            for &consumer in self.consumers.get(&current).into_iter().flatten() {
                if !seen.insert(consumer) {
                    continue;
                }
                if matches!(
                    self.relational(consumer),
                    Some(
                        NonASAPOp::TimeShift { .. }
                            | NonASAPOp::TimeRange { .. }
                            | NonASAPOp::PromqlSubquery { .. }
                    )
                ) {
                    return true;
                }
                pending.push(consumer);
            }
        }
        false
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
        if self.re_timed_above(time_range) {
            return Err(unsupported("a subquery or a TimeShift above the range"));
        }
        let window_width_ms =
            i64::try_from(range.as_millis()).map_err(|_| unsupported("window width overflows"))?;
        let mut window_end_offset_ms = 0i64;
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
                    window_end_offset_ms = window_end_offset_ms
                        .checked_add(shift.offset_ms)
                        .ok_or(unsupported("window offset overflows"))?;
                }
                Some(NonASAPOp::Scan { .. }) => {
                    return Ok(RawSampleWindow {
                        time_range_node_id: time_range,
                        window_width_ms,
                        window_end_offset_ms,
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
) -> Result<(Vec<StoredOutput>, Vec<RawSampleWindow>), InstallablePlanError> {
    let indexed = IndexedDag::new(dag);
    let mut computation_hashes = HashMap::new();
    let mut stored_outputs = Vec::new();
    let mut query_time_raw_windows = Vec::new();
    for node in &dag.nodes {
        if matches!(
            node.materialization,
            Materialization::MatAtIngestion(_)
                | Materialization::MatAtQueryKeptAcrossEvaluations(_)
        ) {
            stored_outputs.push(StoredOutput {
                node_id: node.id,
                computation_id: computation_hash(node.id, &indexed, &mut computation_hashes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
                pane: indexed.pane_of(node.id)?,
            });
        }
        if matches!(node.payload, Operator::NonASAP(NonASAPOp::TimeRange { .. }))
            && indexed.reads_raw_samples(node.id)
        {
            // Every raw-sample window must be one the runtime can feed; an
            // ingestion-time one is fed through its stored output's pane.
            let window = indexed.window_at(node.id)?;
            if node.output_state.timing == ExecutionTiming::QueryTime {
                query_time_raw_windows.push(window);
            }
        }
    }
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
