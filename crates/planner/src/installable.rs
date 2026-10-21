//! The plan a deployment runtime installs: one call from a workload and the
//! deployment's inputs to the single selected physical ASAP DAG, ready to
//! execute ([`plan_installable`]).
//!
//! The runtime writes the returned [`InstallablePlan`] as its plan file and
//! executes its DAG with the executor, as given (#509 Assumption 8). It
//! needs three things from it: the DAG to install (`dag`), the summaries to
//! store (`stored_outputs`), and where raw samples go (each stored output's
//! `inputs.raw_sample_windows` and `query_time_raw_windows`, fed with
//! `asap_executor::RawSampleSelector`). How the Planner represents the
//! plan internally (e.g. a tumbling-pane chain, which the stage export keeps
//! for costing and display) is not the runtime's concern: the installable
//! DAG has every chain expanded into the panes the executor runs.
//!
//! ## Building a stored output
//!
//! `stored_outputs` is in dependency order: every stored output a build
//! reads is listed before it. A runtime builds an output once all of its
//! inputs are complete: each raw-sample window's samples have arrived
//! (sealed) and each stored input is built. It compiles the DAG with the
//! executor for that output's node, with each input bound as a source: a
//! raw-sample window at its `time_range_node_id` (the rows
//! `RawSampleSelector::window_batch` returns) and a stored input at its own
//! node id (its stored state). It runs the build in an ingestion run
//! (`StoredOutputBuild::IngestionRun`) or in the query run of the
//! evaluation that first needs it (`StoredOutputBuild::QueryRun`), stores
//! the result, and binds it the same way wherever the DAG reads that node.
//! A query run binds the stored outputs it reaches from the queries' roots
//! without passing another stored output; an output only other stored
//! outputs read is read through them.
//!
//! Each output's `coverage` says what the stored state is: a stable id of
//! its definition (what it computes) and its selection (which rows went in,
//! with time relative to the evaluation time). Two stored outputs with the
//! same definition id and the same concrete selection (relative time made
//! absolute at an evaluation) hold the same state, so a runtime may store
//! it once.

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
use asap_types::ir::properties::summary_coverage::{SelectionBox, SummaryCoverage};
use asap_types::ir::properties::{ExecutionDataStateError, ExecutionTiming};
use asap_types::ir::SchemaDerivationError;
use asap_types::ir::{
    apply_materialization_timings_to_roots, ASAPOp, MaterializationAssignment, NonASAPOp, Operator,
    OperatorNode, QueryRoot,
};
use asap_types::physical::Materialization;
use serde::{Deserialize, Serialize};

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
    /// `MatAtQueryKeptAcrossEvaluations`), in dependency order (each after
    /// the stored outputs it is built from), with its coverage and inputs.
    pub stored_outputs: Vec<StoredOutput>,
    /// Every window of raw samples read at query time other than one a
    /// stored output's build reads, in node order: the runtime feeds each
    /// its samples when a query evaluates.
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

/// One node of the installable DAG whose output (a summary state) the
/// runtime stores: what it is, what it is built from, when and for how
/// long.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredOutput {
    /// The node whose output is stored.
    pub node_id: PhysicalASAPNodeId,
    /// Which run builds it: an ingestion run once its inputs are complete,
    /// or the query run of the evaluation that first reads it.
    pub built_at: StoredOutputBuild,
    /// What the stored state is: the node's summary coverage.
    pub coverage: StoredCoverage,
    /// What its build reads.
    pub inputs: StoredOutputInputs,
    /// How far back from the newest evaluation time this node's output
    /// must stay available once built. At least how far back the data it
    /// summarizes reaches (its raw-sample windows', and its stored inputs',
    /// `end_offset_ms + width_ms`). A pane of a tumbling-pane chain takes
    /// Stage 3's retention of its chain (`Selection::pane_retention_ms`: its
    /// offset plus its window plus the longest range-evaluation span of its
    /// readers). An output built from stored outputs adds the longest
    /// range-evaluation span of the queries that read it. An input is kept
    /// at least as long as every output built from it needs it: as long as
    /// that output's own retention when it is built in a query run (a range
    /// request may build it for its oldest step), else as far back as that
    /// output's data reaches. This is this node's own requirement: a
    /// runtime that stores identical outputs of several nodes once must keep
    /// them for the largest requirement among those nodes, which is the
    /// runtime's choice.
    pub retention_ms: u64,
}

/// A stored output's summary coverage
/// ([`SummaryCoverage`], `docs/design_docs/proposals/asap-primitive-schema.md`
/// §4.2): what it computes and which rows went in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredCoverage {
    /// [`SummaryCoverage::definition_id`]: equal on every node, plan and
    /// process that computes the same thing, whatever its selection; a
    /// rollup to a coarser grouping computes something else and has its
    /// own.
    pub definition_id: String,
    /// The rows that went in: a union of boxes of value sets, value ranges
    /// and time relative to the evaluation time. A merge's is the union of
    /// its inputs'.
    pub selection: Vec<SelectionBox>,
}

/// What a stored output's build reads, each bound as a source of the build.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredOutputInputs {
    /// The windows of raw samples it reads directly, in node order.
    pub raw_sample_windows: Vec<RawSampleWindow>,
    /// The stored outputs it reads, by node id, in node order; each is
    /// listed before it in `stored_outputs`.
    pub stored_outputs: Vec<PhysicalASAPNodeId>,
}

/// Which run builds a stored output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredOutputBuild {
    /// Built at ingestion time (`MatAtIngestion`): the runtime runs the
    /// node in an ingestion run once its inputs are complete (its windows'
    /// samples have arrived and its stored inputs are built), and stores
    /// the result.
    IngestionRun,
    /// Built at query time and kept across evaluations
    /// (`MatAtQueryKeptAcrossEvaluations`): when an evaluation needs this
    /// output for a selection it has not been built for yet, the runtime
    /// runs the node in a query run over its inputs, keeps the result, and
    /// binds it like an output built at ingestion. Its raw-sample windows
    /// are fed only through this build, so they are not listed in
    /// `query_time_raw_windows`.
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
    /// than the data it summarizes reaches back, so a later evaluation
    /// would miss it.
    #[error("stored output {node_id} is kept for less than its data reaches back")]
    StoredOutputRetentionTooShort { node_id: PhysicalASAPNodeId },
    /// Internal bug: Stage 3 records a pane retention for node `node_id`
    /// of the selected plan, which is not one of its pane chains.
    #[error("pane retention for node {node_id}, which is not a pane chain of the selected plan")]
    PaneRetentionOfUnknownChain { node_id: PhysicalASAPNodeId },
    /// Unsatisfiable request: the plan stores node `node_id`, whose build
    /// reads a source `Scan` other than through a raw-sample `TimeRange`
    /// (e.g. a whole SQL table), which a runtime cannot feed it.
    #[error("stored output {node_id} reads a source outside a raw-sample window")]
    StoredOutputReadsUnwindowedSource { node_id: PhysicalASAPNodeId },
    /// Unsatisfiable request: the plan stores node `node_id`, which is not
    /// a summary state with a coverage (`reason`), so a runtime cannot say
    /// what it stores.
    #[error("stored output {node_id} has no summary coverage: {reason}")]
    StoredOutputWithoutCoverage {
        node_id: PhysicalASAPNodeId,
        reason: String,
    },
    /// Unsatisfiable request: the executor cannot build stored output
    /// `node_id` from its inputs (`reason`).
    #[error("the executor cannot build stored output {node_id} from its inputs: {reason}")]
    StoredOutputNotExecutable {
        node_id: PhysicalASAPNodeId,
        reason: String,
    },
    /// Unsatisfiable request: the plan reads raw samples through
    /// `TimeRange` `time_range_node_id` in a shape whose window is not a
    /// fixed offset from the evaluation time (`reason`: a subquery or a
    /// `TimeShift` above it, or an `@` anchor below it), or that ends after
    /// the evaluation (a negative offset below it), which no ingestion run
    /// can build in time. Not supported yet.
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
    let entries: Vec<_> = input.workload.query_workload.entries().collect();
    // Each query's longest range-evaluation span, which the outputs it
    // reads must stay available for.
    let range_spans_ms: Vec<u64> = entries
        .iter()
        .map(|entry| entry.range_evaluation.map_or(0, |range| range.span_ms))
        .collect();
    let query_texts: Vec<String> = entries.into_iter().map(|entry| entry.query.0).collect();
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
        let expanded = pane_retention_of_expanded_panes(&timed, output.selection.as_ref())?;
        let reader_span_ms_by_node = reader_span_ms_by_node(&expanded.dag, &range_spans_ms);
        InstallablePlan::of_dag(
            expanded.dag,
            query_texts,
            selection,
            &expanded.retention_ms_by_node,
            &reader_span_ms_by_node,
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
        reader_span_ms_by_node: &HashMap<PhysicalASAPNodeId, u64>,
    ) -> Result<Self, InstallablePlanError> {
        let (stored_outputs, query_time_raw_windows) =
            runtime_inputs_of(&dag, retention_ms_by_node, reader_span_ms_by_node)?;
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
        // Retention comes from Stage 3 and the workload, not from the DAG:
        // take the plan's own, and check only that each output is kept while
        // its data is read.
        let retention_ms_by_node: HashMap<_, _> = self
            .stored_outputs
            .iter()
            .map(|output| (output.node_id, output.retention_ms))
            .collect();
        let mut reach_ms_by_node = HashMap::new();
        for output in &self.stored_outputs {
            let reach_ms = reach_ms_of(&output.inputs, &reach_ms_by_node);
            if output.retention_ms < reach_ms {
                return Err(InstallablePlanError::StoredOutputRetentionTooShort {
                    node_id: output.node_id,
                });
            }
            reach_ms_by_node.insert(output.node_id, reach_ms);
        }
        let (stored_outputs, query_time_raw_windows) =
            runtime_inputs_of(dag, &retention_ms_by_node, &HashMap::new())?;
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
        InstallablePlan::of_dag(
            dag,
            self.query_texts(),
            None,
            &HashMap::new(),
            &HashMap::new(),
        )
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
    /// Every node after its inputs, from the DAG's node order.
    children_first: Vec<PhysicalASAPNodeId>,
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
            children_first,
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
                Some(NonASAPOp::Scan { .. }) if end_offset_ms < 0 => {
                    return Err(unsupported("a negative offset below the range"))
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

    /// What stored output `node`'s build reads: below it, each stored node
    /// (`stored`) and each raw-sample `TimeRange` it reaches without
    /// passing another of them, in node order.
    fn inputs_of_stored(
        &self,
        node: PhysicalASAPNodeId,
        stored: &HashSet<PhysicalASAPNodeId>,
    ) -> Result<StoredOutputInputs, InstallablePlanError> {
        let mut raw_ranges = Vec::new();
        let mut stored_inputs = Vec::new();
        let mut seen = HashSet::new();
        let mut pending: Vec<PhysicalASAPNodeId> = self
            .inputs
            .get(&node)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            if stored.contains(&id) {
                stored_inputs.push(id);
                continue;
            }
            match self.relational(id) {
                Some(NonASAPOp::TimeRange { .. }) if self.reading_raw_samples.contains(&id) => {
                    raw_ranges.push(id);
                    continue;
                }
                Some(NonASAPOp::Scan { .. }) => {
                    return Err(InstallablePlanError::StoredOutputReadsUnwindowedSource {
                        node_id: node,
                    })
                }
                _ => pending.extend(self.inputs.get(&id).into_iter().flatten().copied()),
            }
        }
        raw_ranges.sort_unstable();
        stored_inputs.sort_unstable();
        Ok(StoredOutputInputs {
            raw_sample_windows: raw_ranges
                .into_iter()
                .map(|range| self.window_at(range))
                .collect::<Result<_, _>>()?,
            stored_outputs: stored_inputs,
        })
    }
}

/// How far back from the evaluation time the data an output built from
/// `inputs` summarizes reaches: the farthest of its raw-sample windows and
/// of its stored inputs (`reach_ms_by_node`, each already computed).
fn reach_ms_of(
    inputs: &StoredOutputInputs,
    reach_ms_by_node: &HashMap<PhysicalASAPNodeId, u64>,
) -> u64 {
    let windows = inputs
        .raw_sample_windows
        .iter()
        .map(RawSampleWindow::reach_ms);
    let stored = inputs
        .stored_outputs
        .iter()
        .map(|id| reach_ms_by_node.get(id).copied().unwrap_or(0));
    windows.chain(stored).max().unwrap_or(0)
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

/// Every node of `dag` stored across evaluations, in dependency order,
/// with its coverage, inputs and retention, and every window of raw samples
/// read at query time other than through a stored output's build, in node
/// order. `retention_ms_by_node` gives a node's retention (Stage 3's for a
/// pane, or a plan file's own); otherwise it is how far back its data
/// reaches, plus, for an output built from stored outputs, its readers'
/// longest range-evaluation span (`reader_span_ms_by_node`). Then each
/// stored input is kept as long as the outputs built from it need. An
/// error for a stored output the runtime cannot build, or a raw-sample
/// window of an unsupported shape.
fn runtime_inputs_of(
    dag: &PhysicalASAPDAG,
    retention_ms_by_node: &HashMap<PhysicalASAPNodeId, u64>,
    reader_span_ms_by_node: &HashMap<PhysicalASAPNodeId, u64>,
) -> Result<(Vec<StoredOutput>, Vec<RawSampleWindow>), InstallablePlanError> {
    let indexed = IndexedDag::new(dag);
    let stored: HashSet<PhysicalASAPNodeId> = dag
        .nodes
        .iter()
        .filter(|node| {
            matches!(
                node.materialization,
                Materialization::MatAtIngestion(_)
                    | Materialization::MatAtQueryKeptAcrossEvaluations(_)
            )
        })
        .map(|node| node.id)
        .collect();
    // The IR of each node, which coverage is derived from.
    let ir = if stored.is_empty() {
        HashMap::new()
    } else {
        dag.operator_nodes()?
    };
    let mut stored_outputs: Vec<StoredOutput> = Vec::new();
    let mut coverage_by_node: HashMap<PhysicalASAPNodeId, SummaryCoverage> = HashMap::new();
    let mut position_of = HashMap::new();
    let mut reach_ms_by_node = HashMap::new();
    // The ranges of stored outputs' raw-sample windows, which are fed
    // through those outputs' builds.
    let mut stored_window_ranges = HashSet::new();
    for &id in indexed
        .children_first
        .iter()
        .filter(|id| stored.contains(id))
    {
        let node = indexed.nodes[&id];
        let inputs = indexed.inputs_of_stored(id, &stored)?;
        let coverage = coverage_of(id, &inputs, &ir, &coverage_by_node).map_err(|error| {
            InstallablePlanError::StoredOutputWithoutCoverage {
                node_id: id,
                reason: error.to_string(),
            }
        })?;
        coverage_by_node.insert(id, coverage.clone());
        executor_builds(dag, id, &inputs, &indexed)?;
        let reach_ms = reach_ms_of(&inputs, &reach_ms_by_node);
        let span_ms = match inputs.stored_outputs.is_empty() {
            true => 0,
            false => reader_span_ms_by_node.get(&id).copied().unwrap_or(0),
        };
        stored_window_ranges.extend(
            inputs
                .raw_sample_windows
                .iter()
                .map(|window| window.time_range_node_id),
        );
        reach_ms_by_node.insert(id, reach_ms);
        position_of.insert(id, stored_outputs.len());
        stored_outputs.push(StoredOutput {
            node_id: id,
            built_at: match node.materialization {
                Materialization::MatAtIngestion(_) => StoredOutputBuild::IngestionRun,
                _ => StoredOutputBuild::QueryRun,
            },
            coverage: StoredCoverage {
                definition_id: coverage.definition_id(),
                selection: coverage.selection,
            },
            retention_ms: retention_ms_by_node
                .get(&id)
                .copied()
                .unwrap_or(reach_ms + span_ms),
            inputs,
        });
    }
    // Outputs before their inputs: an input stays while every output built
    // from it may still be built.
    for index in (0..stored_outputs.len()).rev() {
        let output = &stored_outputs[index];
        let needed_ms = match output.built_at {
            StoredOutputBuild::QueryRun => output.retention_ms,
            StoredOutputBuild::IngestionRun => reach_ms_by_node[&output.node_id],
        };
        for input in output.inputs.stored_outputs.clone() {
            let kept = &mut stored_outputs[position_of[&input]].retention_ms;
            *kept = (*kept).max(needed_ms);
        }
    }
    let mut query_time_raw_windows = Vec::new();
    for node in &dag.nodes {
        if matches!(node.payload, Operator::NonASAP(NonASAPOp::TimeRange { .. }))
            && indexed.reading_raw_samples.contains(&node.id)
        {
            // Every raw-sample window must be one the runtime can feed; a
            // stored output's window is fed through that output's build.
            let window = indexed.window_at(node.id)?;
            if node.output_state.timing == ExecutionTiming::QueryTime
                && !stored_window_ranges.contains(&window.time_range_node_id)
            {
                query_time_raw_windows.push(window);
            }
        }
    }
    Ok((stored_outputs, query_time_raw_windows))
}

/// The coverage of stored output `id` (IR `ir`), built from `inputs`.
/// A merge's is the union of its inputs' selections under their shared
/// definition ([`SummaryCoverage::derive`]). A summary built from exactly
/// one stored output and nothing else, with no selection of its own (a
/// rollup to a coarser grouping, or across series), takes that input's
/// selection: its definition is its own sub-DAG with the input replaced by
/// the input's definition, so the rollups of equal inputs share one
/// definition. Anything else keeps its own derived coverage.
fn coverage_of(
    id: PhysicalASAPNodeId,
    inputs: &StoredOutputInputs,
    ir: &HashMap<PhysicalASAPNodeId, Rc<OperatorNode>>,
    coverage_by_node: &HashMap<PhysicalASAPNodeId, SummaryCoverage>,
) -> Result<SummaryCoverage, asap_types::ir::properties::summary_coverage::CoverageError> {
    let node = &ir[&id];
    let own = SummaryCoverage::derive(node)?;
    let [input] = inputs.stored_outputs.as_slice() else {
        return Ok(own);
    };
    if !inputs.raw_sample_windows.is_empty()
        || matches!(node.asap(), Some(ASAPOp::SummaryMerge { .. }))
        || own.selection != [SelectionBox::default()]
    {
        return Ok(own);
    }
    let input_coverage = &coverage_by_node[input];
    let over_definition = with_replaced(node, &ir[input], &input_coverage.definition);
    Ok(SummaryCoverage {
        definition: SummaryCoverage::derive(&over_definition)?.definition,
        selection: input_coverage.selection.clone(),
    })
}

/// `node` with every use of `old` (by address) replaced by `new`; nodes
/// that do not reach `old` are shared, not copied.
fn with_replaced(
    node: &Rc<OperatorNode>,
    old: &Rc<OperatorNode>,
    new: &Rc<OperatorNode>,
) -> Rc<OperatorNode> {
    fn replace(
        node: &Rc<OperatorNode>,
        old: &Rc<OperatorNode>,
        new: &Rc<OperatorNode>,
        memo: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
    ) -> Rc<OperatorNode> {
        if Rc::ptr_eq(node, old) {
            return Rc::clone(new);
        }
        if let Some(done) = memo.get(&Rc::as_ptr(node)) {
            return Rc::clone(done);
        }
        let children = node.children();
        let replaced: Vec<Rc<OperatorNode>> = children
            .iter()
            .map(|child| replace(child, old, new, memo))
            .collect();
        let result = if children
            .iter()
            .zip(&replaced)
            .all(|(a, b)| Rc::ptr_eq(a, b))
        {
            Rc::clone(node)
        } else {
            let mut next = replaced.into_iter();
            let mut copy = (**node).clone();
            copy.operator = node
                .operator
                .map_children(|_| next.next().expect("one per child"));
            Rc::new(copy)
        };
        memo.insert(Rc::as_ptr(node), Rc::clone(&result));
        result
    }
    replace(node, old, new, &mut HashMap::new())
}

/// Whether the executor compiles stored output `node`'s build with its
/// `inputs` bound as sources, as a runtime runs it; the executor's reason
/// when it does not.
fn executor_builds(
    dag: &PhysicalASAPDAG,
    node: PhysicalASAPNodeId,
    inputs: &StoredOutputInputs,
    indexed: &IndexedDag<'_>,
) -> Result<(), InstallablePlanError> {
    use asap_executor::physical_planner::{compile, InputContract};
    let bound = inputs
        .raw_sample_windows
        .iter()
        .map(|window| window.time_range_node_id)
        .chain(inputs.stored_outputs.iter().copied())
        .map(|id| {
            let schema = std::sync::Arc::new(indexed.nodes[&id].output_schema.clone());
            (id as u64, InputContract::bounded(schema))
        })
        .collect();
    compile(dag, bound, &[node as u64])
        .map(|_| ())
        .map_err(|error| InstallablePlanError::StoredOutputNotExecutable {
            node_id: node,
            reason: error.to_string(),
        })
}

/// The installable DAG of the timed selected plan, with each expanded
/// pane's retention by node id.
struct ExpandedWithRetention {
    dag: PhysicalASAPDAG,
    retention_ms_by_node: HashMap<PhysicalASAPNodeId, u64>,
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
    let mut retention_ms_by_node = HashMap::new();
    for expanded in &expansion.chains {
        let Some((_, retention_ms)) = retention_by_chain
            .iter()
            .find(|(chain, _)| Rc::ptr_eq(chain, &expanded.chain))
        else {
            continue;
        };
        for pane in &expanded.panes {
            if let Some(node_id) = compiled.node_ids.node_id(pane) {
                retention_ms_by_node.insert(node_id, *retention_ms);
            }
        }
    }
    Ok(ExpandedWithRetention {
        dag: compiled.dag,
        retention_ms_by_node,
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
        match runtime_inputs_of(&export(shifted), &HashMap::new(), &HashMap::new()) {
            Err(InstallablePlanError::UnsupportedRawSampleWindow { reason, .. }) => {
                assert_eq!(reason, "a subquery or a TimeShift above the range")
            }
            other => panic!("{other:?}"),
        }
        let (stored, windows) =
            runtime_inputs_of(&export(range), &HashMap::new(), &HashMap::new()).unwrap();
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
