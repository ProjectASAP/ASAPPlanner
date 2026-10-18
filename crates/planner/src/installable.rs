//! The plan a deployment runtime installs: one call from a workload and the
//! deployment's inputs to the single selected physical ASAP DAG, ready to
//! execute ([`plan_installable`]).
//!
//! The runtime writes the returned [`InstallablePlan`] as its plan file and
//! executes its `dag` with the executor, as given (#509 Assumption 8). How
//! the Planner represents the plan internally (e.g. a tumbling-pane chain,
//! which the stage export keeps for costing and display) is not the
//! runtime's concern: the installable DAG has every chain expanded into the
//! panes the executor runs.

use asap_logical_asap_optimization::DeploymentCapabilities;
use asap_plan_selection::{ResourceUsage, Selection};
use asap_types::ir::operator::pane_chain::expand_pane_chains_in_workload;
use std::collections::HashMap;

use asap_types::ir::operator::Source;
use asap_types::ir::physical_export::{
    compile_physical_asap_workload, PhysicalASAPDAG, PhysicalASAPDAGNode,
    PhysicalASAPDAGValidationError, PhysicalASAPNodeId, ReferencePlanError,
};
use asap_types::ir::properties::ExecutionDataStateError;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::SchemaDerivationError;
use asap_types::ir::{apply_materialization_timings_to_roots, MaterializationAssignment};
use asap_types::ir::{NonASAPOp, Operator};
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
    /// materialization, every pane chain expanded into its panes; the
    /// executor compiles it as is.
    pub dag: PhysicalASAPDAG,
    /// One per workload query, in `QueryWorkload::entries()` order: its
    /// text and the root of `dag` that answers it.
    pub queries: Vec<InstalledQuery>,
    /// Every node whose output the runtime stores across evaluations
    /// (`MatAtIngestion`, or `MatAtQueryKeptAcrossEvaluations`), in node
    /// order, with the pane of raw samples each one reads.
    pub stored_outputs: Vec<StoredOutput>,
    /// Every `TimeRange` computed at query time over raw samples, in node
    /// order: the runtime feeds each the samples in its window when a query
    /// evaluates.
    pub query_time_raw_inputs: Vec<RawSampleInput>,
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
    /// The node whose output is stored.
    pub node_id: PhysicalASAPNodeId,
    /// Where and when: `MatAtIngestion(medium)` (built at ingestion time,
    /// read at query time) or `MatAtQueryKeptAcrossEvaluations(medium)`
    /// (built at query time, kept for later evaluations).
    pub materialization: Materialization,
    /// Hex SHA-256 of what the output computes, independent of node ids
    /// and of which pane it reads: stored outputs that differ only in
    /// `pane.window_end_offset_ms` share it, so a runtime builds each pane once;
    /// equal across plans for equal computations.
    pub computation_id: String,
    /// The pane of raw samples it is built from; `None` when it does not
    /// read one window of one `Scan` (`TimeRange(width) <-
    /// TimeShift(offset)* <- Scan` below single-input operators).
    pub pane: Option<RawSampleInput>,
}

/// A window of raw samples the runtime feeds to the DAG: at evaluation time
/// `T`, the samples in `(T - window_end_offset_ms - window_width_ms,
/// T - window_end_offset_ms]`, as input rows of `time_range_node_id`
/// ([`asap_executor::input_rows_from_samples`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSampleInput {
    /// The `TimeRange` node the runtime binds the window's input rows to.
    pub time_range_node_id: PhysicalASAPNodeId,
    /// Width of the window, from the `TimeRange`.
    pub window_width_ms: i64,
    /// How long before the evaluation time the window ends: the sum of the
    /// offset `TimeShift`s between the `TimeRange` and the `Scan`.
    pub window_end_offset_ms: i64,
    /// The `Scan` below, whose source names the samples.
    pub scan_node_id: PhysicalASAPNodeId,
    /// The samples' source, e.g. `TimeSeries { metric: "latency_ms" }`.
    pub scan_source: Source,
}

/// What Stage 3 recorded about the plan it selected, for reporting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedPlanReport {
    /// The selected physical candidate's id, e.g. `P3-m1`.
    pub selected_candidate_id: String,
    /// What the selected plan uses per second (CPU, scans, memory, disk,
    /// cores, per-query latency).
    pub resource_usage: ResourceUsage,
    /// Its price per second under cost model 1 or 2; `None` under cost
    /// model 3, which has no prices.
    pub cost_per_second: Option<f64>,
    /// Per query, the accuracy Stage 3 accepted, e.g. `metric rank;
    /// accepted rank (Kll)`.
    pub accepted_accuracy_per_query: Vec<String>,
}

/// Why an installable plan could not be built or is not consistent.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InstallablePlanError {
    /// Internal bug: the selected plan does not export.
    #[error("the selected plan does not export: {0}")]
    Export(#[from] ExecutionDataStateError),
    /// Internal bug: a pane chain of the selected plan does not expand.
    #[error("pane chain expansion: {0}")]
    PaneChainExpansion(#[from] SchemaDerivationError),
    /// Bad input (a plan file): `dag` is not a valid physical ASAP DAG.
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
    /// Bad input (a plan file): `stored_outputs` or `query_time_raw_inputs`
    /// is not what `dag` stores or reads.
    #[error("stored_outputs or query_time_raw_inputs do not match the DAG")]
    InputsOrStoredOutputsDoNotMatchDag,
}

/// Plan `input` for a deployment runtime: the query-language frontends and
/// Stages 1-3 as [`e2e_plan`] runs them, returning the single selected DAG
/// ready to install. Supported summaries the caller leaves unrestricted
/// (`DeploymentCapabilities::supported_summaries` `None`) default to the
/// families the Planner's executor runs
/// ([`asap_executor::supported_summary_families`]); raw-data and
/// query-time-result retention stay the caller's.
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
        let plan = InstallablePlan {
            queries: queries_bound_to_roots(&dag, query_texts)?,
            stored_outputs: stored_outputs_of(&dag),
            query_time_raw_inputs: query_time_raw_inputs_of(&dag),
            dag,
            selection,
        };
        plan.validate()?;
        Ok(plan)
    };
    installable().map_err(PlanError::Installable)
}

impl InstallablePlan {
    /// Checks a plan, e.g. one read back from a plan file: `dag` is valid
    /// and `queries` binds each query, in order, to its roots.
    pub fn validate(&self) -> Result<(), InstallablePlanError> {
        self.dag.validate()?;
        let texts = self.queries.iter().map(|q| q.query.clone()).collect();
        if queries_bound_to_roots(&self.dag, texts)? != self.queries {
            return Err(self.mismatch());
        }
        if stored_outputs_of(&self.dag) != self.stored_outputs
            || query_time_raw_inputs_of(&self.dag) != self.query_time_raw_inputs
        {
            return Err(InstallablePlanError::InputsOrStoredOutputsDoNotMatchDag);
        }
        Ok(())
    }

    /// For correctness tests only, never for installing: this plan with
    /// nothing materialized, the same operators, summaries and parameters
    /// with every node at query time, and the same queries. A runtime's
    /// test compares its installed plan's answers with the executor running
    /// this one over raw data. Its `selection` is `None`.
    pub fn with_every_node_at_query_time(&self) -> Result<InstallablePlan, ReferencePlanError> {
        let dag = self.dag.with_every_node_at_query_time()?;
        let texts = self.queries.iter().map(|q| q.query.clone()).collect();
        let queries = queries_bound_to_roots(&dag, texts)
            .expect("re-timing keeps the roots and their positions");
        Ok(InstallablePlan {
            stored_outputs: stored_outputs_of(&dag),
            query_time_raw_inputs: query_time_raw_inputs_of(&dag),
            dag,
            queries,
            selection: None,
        })
    }

    fn mismatch(&self) -> InstallablePlanError {
        InstallablePlanError::QueriesDoNotMatchRoots {
            operator_roots: self.dag.roots.len(),
            scalar_roots: self.dag.scalar_roots.len(),
        }
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

/// Every node of `dag` stored across evaluations, in node order.
fn stored_outputs_of(dag: &PhysicalASAPDAG) -> Vec<StoredOutput> {
    let (nodes, inputs) = indexed(dag);
    let mut computation_hashes = HashMap::new();
    dag.nodes
        .iter()
        .filter(|n| {
            matches!(
                n.materialization,
                Materialization::MatAtIngestion(_)
                    | Materialization::MatAtQueryKeptAcrossEvaluations(_)
            )
        })
        .map(|n| StoredOutput {
            node_id: n.id,
            materialization: n.materialization,
            computation_id: computation_hash(n.id, &nodes, &inputs, &mut computation_hashes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            pane: pane_of(n.id, &nodes, &inputs),
        })
        .collect()
}

/// Every query-time `TimeRange` of `dag` over raw samples, in node order.
fn query_time_raw_inputs_of(dag: &PhysicalASAPDAG) -> Vec<RawSampleInput> {
    let (nodes, inputs) = indexed(dag);
    dag.nodes
        .iter()
        .filter(|n| {
            n.output_state.timing == ExecutionTiming::QueryTime
                && matches!(n.payload, Operator::NonASAP(NonASAPOp::TimeRange { .. }))
        })
        .filter_map(|n| raw_input_at(n.id, &nodes, &inputs))
        .collect()
}

/// The nodes of `dag` by id, and each node's inputs in edge order.
fn indexed(dag: &PhysicalASAPDAG) -> (Nodes<'_>, Inputs) {
    let nodes = dag.nodes.iter().map(|n| (n.id, n)).collect();
    let mut inputs: Inputs = HashMap::new();
    for edge in &dag.edges {
        inputs.entry(edge.consumer).or_default().push(edge.producer);
    }
    (nodes, inputs)
}

type Nodes<'a> = HashMap<PhysicalASAPNodeId, &'a PhysicalASAPDAGNode>;
type Inputs = HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>;

fn only_input(inputs: &Inputs, id: PhysicalASAPNodeId) -> Option<PhysicalASAPNodeId> {
    match inputs.get(&id).map(Vec::as_slice) {
        Some([input]) => Some(*input),
        _ => None,
    }
}

/// The window `node` is built from: follow single inputs down to a
/// `TimeRange`, then [`raw_input_at`] it.
fn pane_of(node: PhysicalASAPNodeId, nodes: &Nodes, inputs: &Inputs) -> Option<RawSampleInput> {
    let mut current = node;
    loop {
        match &nodes.get(&current)?.payload {
            Operator::NonASAP(NonASAPOp::TimeRange { .. }) => {
                return raw_input_at(current, nodes, inputs)
            }
            Operator::NonASAP(NonASAPOp::TimeShift { .. } | NonASAPOp::Scan { .. }) => return None,
            _ => current = only_input(inputs, current)?,
        }
    }
}

/// The `TimeRange` `time_range` as a raw input: its width, and the offset
/// `TimeShift`s down to its `Scan` added up; `None` for any other shape.
fn raw_input_at(
    time_range: PhysicalASAPNodeId,
    nodes: &Nodes,
    inputs: &Inputs,
) -> Option<RawSampleInput> {
    let Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) = &nodes.get(&time_range)?.payload
    else {
        return None;
    };
    let window_width_ms = i64::try_from(range.as_millis()).ok()?;
    let mut window_end_offset_ms = 0i64;
    let mut current = only_input(inputs, time_range)?;
    loop {
        match &nodes.get(&current)?.payload {
            Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) if shift.at.is_none() => {
                window_end_offset_ms = window_end_offset_ms.checked_add(shift.offset_ms)?;
                current = only_input(inputs, current)?;
            }
            Operator::NonASAP(NonASAPOp::Scan { source, .. }) => {
                return Some(RawSampleInput {
                    time_range_node_id: time_range,
                    window_width_ms,
                    window_end_offset_ms,
                    scan_node_id: current,
                    scan_source: source.clone(),
                })
            }
            _ => return None,
        }
    }
}

/// SHA-256 over a node's payload (input ids zeroed) and output schema, then
/// its inputs' hashes in edge order. An offset `TimeShift` with one input
/// takes its input's hash: the offset says which pane an output reads, not
/// what it computes.
fn computation_hash(
    node: PhysicalASAPNodeId,
    nodes: &Nodes,
    inputs: &Inputs,
    done: &mut HashMap<PhysicalASAPNodeId, [u8; 32]>,
) -> [u8; 32] {
    if let Some(hash) = done.get(&node) {
        return *hash;
    }
    let dag_node = nodes[&node];
    let node_inputs = inputs.get(&node).map(Vec::as_slice).unwrap_or_default();
    let hash = match (&dag_node.payload, node_inputs) {
        (Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }), [input]) if shift.at.is_none() => {
            computation_hash(*input, nodes, inputs, done)
        }
        _ => {
            let payload = dag_node.payload.map_children(|_| 0);
            let mut hasher = Sha256::new();
            hasher.update(
                serde_json::to_vec(&(&payload, &dag_node.output_schema)).expect("serializes"),
            );
            for &input in node_inputs {
                hasher.update(computation_hash(input, nodes, inputs, done));
            }
            hasher.finalize().into()
        }
    };
    done.insert(node, hash);
    hash
}

/// Stage 3's record of its selected candidate.
fn report_of(selection: &Selection) -> SelectedPlanReport {
    let cost = selection.costs.get(&selection.selected);
    SelectedPlanReport {
        selected_candidate_id: selection.selected.clone(),
        resource_usage: cost.map(|c| c.usage.clone()).unwrap_or_default(),
        cost_per_second: cost.and_then(|c| c.cost_per_second),
        accepted_accuracy_per_query: selection
            .accepted
            .get(&selection.selected)
            .map(|accepted| accepted.iter().map(ToString::to_string).collect())
            .unwrap_or_default(),
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
