//! The facade's orchestration of the planning stages (#1047): Stage 0
//! (the memo of logical groups), Pass 1, Pass 2, Stage 2, Stage 3, and
//! finally the selected plan built through Stage 2's realizer. No stage
//! calls another, and none has a deadline: each runs to completion.

use asap_logical_asap_optimization::generate_dag_of_logical_memo_groups;
use asap_types::stages::PerComputationLogicalMemoCandidates;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Instant;

use asap_logical_asap_optimization::generate_logical_asap_memo_candidates;
use asap_logical_asap_optimization::ChosenCandidatePerMemoGroup;
use asap_logical_asap_optimization::DeploymentCapabilitiesError;
use asap_logical_asap_optimization::{
    generate_per_computation_logical_memo_candidates, PerComputationCandidateError,
    PerComputationLogicalMemoCandidatesError,
};
use asap_physical_asap_optimization::{
    generate_physical_asap_memo_candidates, Stage2AssignmentRealizer,
};
use asap_plan_selection::{select_plan, ErrorSelectingPlan, SearchReport, Selection};
use asap_types::ir::schema::schema_support::with_promql_series_identity;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::stages::{
    AssignmentRealizer, CandidateForMemoGroupId, DagOfSelectedCandidates,
    LogicalASAPMemoCandidates, Memo, MemoGroupId, PhysicalASAPMemoCandidates,
    PhysicalCandidateError, RealizedPlan,
};
use asap_types::workload::bounds::with_confirmed_weights_at;
use asap_types::workload::{DataWorkload, QueryLanguage, QueryWorkloadEntry};

use crate::pass::{
    OptimizationInput, OptimizationPass, OptimizeError, PlanOutput, PlanningModels, QueryPlan,
};

/// How the facade searches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanSearchSettings {
    /// Keep Pass 1's output for inspection (`stage_pipeline`, the DAG
    /// viewer): [`PlanningRun::per_computation_candidates`]. Default off:
    /// production planning copies no memo.
    pub inspect: bool,
}

/// Why the facade selected no plan.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SelectionError {
    /// Bad input: the deployment's capabilities contradict the data
    /// workload.
    #[error("deployment capabilities: {0}")]
    InvalidDeploymentCapabilities(DeploymentCapabilitiesError),
    /// Pass 1's memo candidates or Pass 2 failed.
    #[error("logical ASAP-aware optimization: {0}")]
    LogicalASAPCandidates(String),
    /// Stage 2 failed.
    #[error("physical ASAP-aware optimization: {0}")]
    PhysicalASAPCandidates(String),
    /// Stage 3 selected no plan.
    #[error("plan selection: {0}")]
    Selection(ErrorSelectingPlan),
    /// Stage 2 could not build the plan Stage 3 selected.
    #[error("building the selected plan: {0}")]
    Realization(PhysicalCandidateError),
}

/// What the facade did: each stage's counts and time.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct PlanningReport {
    /// Stage 0 and Pass 1, in milliseconds.
    pub pass1_ms: f64,
    /// Pass 2's output: memo groups and their candidates.
    pub memo_groups: usize,
    pub logical_candidates: usize,
    /// Stage 2's output: operator nodes, physical candidates and pruned
    /// candidates.
    pub operator_nodes: usize,
    pub physical_candidates: usize,
    /// Stage 2's producer links: over every physical candidate and input,
    /// the candidates that may produce it.
    pub physical_producer_links: usize,
    pub stage2_pruned_candidates: usize,
    pub pass2_ms: f64,
    pub stage2_ms: f64,
    pub stage3_ms: f64,
    /// Stage 3's search report, when it selected a plan.
    pub search_report: Option<SearchReport>,
}

/// Every stage's output for one workload ([`run_planning`]).
#[derive(Debug)]
pub struct PlanningRun {
    /// Stage 1, Pass 1's output: the memo groups with their per-computation
    /// candidates and the pruned candidates.
    /// `None` when Pass 1 failed or without [`PlanSearchSettings::inspect`].
    pub per_computation_candidates: Option<PerComputationLogicalMemoCandidates>,
    /// Pass 2's output, when it ran.
    pub logical_asap_candidates: Option<LogicalASAPMemoCandidates>,
    /// Stage 2's output, when it ran: the catalog `selection`'s candidate
    /// ids index.
    pub physical_asap_candidates: Option<PhysicalASAPMemoCandidates>,
    /// Stage 3's selection.
    pub selection: Result<Selection, SelectionError>,
    /// The selected plan as Stage 2 built it, with its tumbling-window retention.
    pub selected_physical_plan: Option<RealizedPlan>,
    /// The plan to run: the selected plan's roots with the weight proofs
    /// Stage 3 made applied.
    pub selected_plan: Option<Vec<QueryRoot>>,
    pub report: PlanningReport,
}

/// Run the planning stages over the frontends' `roots` (query `i` is
/// `roots[i]`, with workload entry `entries[i]`). Only a Pass 1 failure is
/// an error; a failed selection is reported in the run.
pub fn run_planning(
    roots: Vec<QueryRoot>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    settings: PlanSearchSettings,
) -> Result<PlanningRun, PerComputationCandidateError> {
    let started = Instant::now();
    for root in &roots {
        root.validate_structure()?;
    }
    // Stage 0: the memo of logical groups.
    let initial = generate_dag_of_logical_memo_groups(&roots);
    // Pass 1: the per-computation candidates the deployment can run and
    // some query can use.
    let accuracy_targets: Vec<_> = entries
        .iter()
        .map(|entry| entry.requirements.accuracy.clone())
        .collect();
    let accuracy = models.accuracy_check_inputs();
    let pass1 = generate_per_computation_logical_memo_candidates(
        initial,
        &accuracy_targets,
        accuracy.model,
        accuracy.evidence,
        data,
        models
            .deployment_capabilities
            .supported_summaries
            .as_deref(),
    );
    // A query Pass 1 cannot plan is an input error.
    let pass1 = match pass1 {
        Err(PerComputationLogicalMemoCandidatesError::PerComputation(error)) => return Err(error),
        pass1 => pass1,
    };
    let mut report = PlanningReport {
        pass1_ms: elapsed_ms(started),
        ..PlanningReport::default()
    };
    let per_computation_candidates = pass1.as_ref().ok().filter(|_| settings.inspect).cloned();
    let realizer = Stage2AssignmentRealizer {
        original_roots: &roots,
        entries,
        data,
        query_time_results_retained: models.deployment_capabilities.query_time_results_retained,
    };
    let mut logical_asap_candidates = None;
    let mut physical_asap_candidates = None;
    let mut selected_physical_plan = None;
    let mut selected_plan = None;
    let selection = (|| {
        models
            .deployment_capabilities
            .validate(data)
            .map_err(SelectionError::InvalidDeploymentCapabilities)?;
        let pass1 =
            pass1.map_err(|error| SelectionError::LogicalASAPCandidates(error.to_string()))?;
        // Pass 2: local window composition over every legal
        // tumbling-window width, then the sharing rules.
        let started = Instant::now();
        let logical = generate_logical_asap_memo_candidates(pass1, entries);
        report.pass2_ms = elapsed_ms(started);
        let logical = logical_asap_candidates.insert(
            logical.map_err(|error| SelectionError::LogicalASAPCandidates(error.to_string()))?,
        );
        report.memo_groups = logical.memo.groups.len();
        report.logical_candidates = logical.memo.candidates.iter().map(Vec::len).sum();
        // Stage 2.
        let started = Instant::now();
        let physical = generate_physical_asap_memo_candidates(
            logical,
            entries,
            data,
            models.deployment_capabilities.query_time_results_retained,
            models.deployment_capabilities.raw_data_retained,
        );
        report.stage2_ms = elapsed_ms(started);
        let physical = physical_asap_candidates.insert(
            physical.map_err(|error| SelectionError::PhysicalASAPCandidates(error.to_string()))?,
        );
        report.operator_nodes = physical.operator_nodes_of_candidates_for_memo_groups.len();
        report.physical_candidates = physical.candidates.len();
        report.physical_producer_links = physical
            .candidates
            .iter()
            .flat_map(|candidate| &candidate.producer_candidates_per_input)
            .map(|producers| producers.len())
            .sum();
        report.stage2_pruned_candidates = physical.pruned_candidates.len();
        // Stage 3.
        let started = Instant::now();
        let selection = select_plan(
            physical,
            entries,
            data,
            &models.accuracy_check_inputs(),
            models.cost_inputs,
            &realizer,
        );
        report.stage3_ms = elapsed_ms(started);
        let selection = selection.map_err(SelectionError::Selection)?;
        report.search_report = Some(selection.search_report.clone());
        // The selected plan, as Stage 2 builds it.
        let realized = realizer
            .realize(physical, &selection.selected_dag)
            .map_err(SelectionError::Realization)?;
        let proven: HashSet<*const OperatorNode> = selection
            .nodes_with_proven_non_negative_weights
            .iter()
            .filter_map(|node| realized.operator_node_of_selected_node.get(node.0))
            .map(Rc::as_ptr)
            .collect();
        selected_plan = Some(with_confirmed_weights_at(
            &realized.plan.roots,
            data,
            &|node| proven.contains(&Rc::as_ptr(node)),
        ));
        selected_physical_plan = Some(realized);
        Ok(selection)
    })();
    Ok(PlanningRun {
        per_computation_candidates,
        logical_asap_candidates,
        physical_asap_candidates,
        selection,
        selected_physical_plan,
        selected_plan,
        report,
    })
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// The selected plan's memo candidate for each group it uses: the memo
/// candidate whose root each selected node's candidate is. Factoring may give several
/// memo candidates of a group one root (identical terminal readers of
/// different inputs); the one reading the selected producers' groups is taken.
/// `memo` and `physical` are the run's
/// ([`PlanningRun::logical_asap_candidates`]'s memo and
/// [`PlanningRun::physical_asap_candidates`]); `selected` is the selection's
/// `selected_dag`. An error says why the plan is not one candidate per group.
pub fn selected_chosen_candidates(
    memo: &Memo,
    physical: &PhysicalASAPMemoCandidates,
    selected: &DagOfSelectedCandidates,
) -> Result<ChosenCandidatePerMemoGroup, String> {
    use std::collections::HashMap;
    let mut producer_groups = HashMap::<_, Vec<MemoGroupId>>::new();
    for edge in &selected.edges {
        producer_groups
            .entry(edge.consumer)
            .or_default()
            .push(physical.memo_group_of(selected.candidate_of_node[edge.producer.0]));
    }
    let mut chosen_candidates = ChosenCandidatePerMemoGroup::new();
    for (node_id, &id) in selected.candidate_of_node.iter().enumerate() {
        let node_id = asap_types::stages::SelectedNodeId(node_id);
        let node = &physical.operator_nodes_of_candidates_for_memo_groups
            [physical.candidates[id.0].operator_node.0];
        if !node.is_implementation_root {
            continue; // a candidate's private node
        }
        let options = &node.candidates_for_memo_group;
        let reads = |&&CandidateForMemoGroupId { group, index }: &&CandidateForMemoGroupId| {
            producer_groups.get(&node_id).is_none_or(|read| {
                read.iter().all(|&child| {
                    child == group
                        || memo.candidates[group.0 as usize][index]
                            .input_groups
                            .contains(&child)
                })
            })
        };
        let &CandidateForMemoGroupId { group, index } =
            options.iter().find(reads).ok_or_else(|| {
                format!(
                    "selected candidate {} reads groups none of its memo candidates read",
                    id.0
                )
            })?;
        if let Some(previous) = chosen_candidates.insert(group, index) {
            if previous != index {
                return Err(format!(
                    "the selected plan implements group {} with two candidates, {previous} and {index}",
                    group.0
                ));
            }
        }
    }
    // Every group the chosen candidates need, from the query roots, is chosen.
    let mut pending: Vec<_> = asap_types::stages::root_groups(&memo.query_roots)
        .into_iter()
        .collect();
    let mut seen = HashSet::new();
    while let Some(group) = pending.pop() {
        if !seen.insert(group) {
            continue;
        }
        let index = chosen_candidates.get(&group).ok_or_else(|| {
            format!(
                "the selected plan uses group {} but no candidate of it is selected",
                group.0
            )
        })?;
        pending.extend(&memo.candidates[group.0 as usize][*index].input_groups);
    }
    Ok(chosen_candidates)
}

/// The planner pipeline behind the optimization-pass interface: the
/// workload's roots in entry order, PromQL roots with each series' full
/// identity as a column (the row representation per-series state needs at
/// runtime; a root with no such representation is planned over its labels
/// alone), through [`run_planning`] with every entry's accuracy target. The
/// output is the selected plan's roots, with the weight proofs plan
/// selection made applied, and its tumbling-window retention.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stage1ToStage3Pass;

impl OptimizationPass for Stage1ToStage3Pass {
    fn name(&self) -> &'static str {
        "stage1-to-stage3"
    }

    fn optimize(&self, input: OptimizationInput<'_>) -> Result<PlanOutput, OptimizeError> {
        let workload = input.workload;
        let promql = workload.query_workload().language == QueryLanguage::PromQL;
        let mut roots: Vec<(usize, QueryRoot)> = workload
            .operator_indices()
            .iter()
            .zip(workload.exprs())
            .map(|(&index, node)| {
                let node = match promql {
                    true => with_promql_series_identity(node).unwrap_or_else(|_| Rc::clone(node)),
                    false => Rc::clone(node),
                };
                (index, QueryRoot::Operator(node))
            })
            .chain(
                workload
                    .scalar_roots()
                    .iter()
                    .map(|(index, expr)| (*index, QueryRoot::Scalar(expr.clone()))),
            )
            .collect();
        roots.sort_by_key(|(index, _)| *index);
        let roots = roots.into_iter().map(|(_, root)| root).collect();
        let entries: Vec<_> = workload.query_workload().entries().collect();
        let data = workload.data_workload().cloned().unwrap_or_default();
        let run = run_planning(
            roots,
            &entries,
            &data,
            input.models,
            PlanSearchSettings::default(),
        )?;
        let selection = run.selection?;
        let roots = run.selected_plan.expect("a selection has a plan");
        let mut output = PlanOutput::new(Vec::new());
        for (entry_index, root) in roots.iter().enumerate() {
            match root {
                QueryRoot::Operator(node) => output.plans.push(QueryPlan {
                    entry_index,
                    root: Rc::clone(node),
                }),
                QueryRoot::Scalar(expr) => {
                    // One plan per distinct operator the scalar reads.
                    let mut refs: Vec<&Rc<OperatorNode>> = Vec::new();
                    for node in expr.operator_refs() {
                        if !refs.iter().any(|r| Rc::ptr_eq(r, node)) {
                            refs.push(node);
                        }
                    }
                    output
                        .scalar_plans
                        .extend(refs.into_iter().map(|node| QueryPlan {
                            entry_index,
                            root: Rc::clone(node),
                        }));
                    output.scalar_roots.push((entry_index, expr.clone()));
                }
            }
        }
        output.tumbling_window_retention_ms = run
            .selected_physical_plan
            .map(|realized| realized.tumbling_window_retention_ms)
            .unwrap_or_default();
        output.selection = Some(selection);
        Ok(output)
    }
}
