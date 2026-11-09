//! The facade's orchestration of the planning stages (#1047): Stage 0
//! (the memo of logical groups), Pass 1, Pass 2, Stage 2 and Stage 3, which
//! builds the selected plan. No stage calls another, and none has a
//! deadline: each runs to completion.

use asap_stage0_memo::memo_of_queries;

use std::rc::Rc;
use std::time::Instant;

use asap_stage1_pass1_per_computation_candidates::{pass1_memo, PerComputationCandidateError};
use asap_stage1_pass2_reuse_candidates::generate_logical_asap_memo_candidates;
use asap_stage2_physical_optimization::generate_physical_asap_memo_candidates;
use asap_stage3_plan_selection::{select_plan, ErrorSelectingPlan, Selection};
use asap_types::deployment_capabilities::DeploymentCapabilitiesError;
use asap_types::ir::schema::series_identity::with_promql_series_identity;
use asap_types::ir::QueryRoot;
use asap_types::plan::physical_asap_dag::PhysicalASAPDAG;
use asap_types::stages::{LogicalASAPMemoCandidates, Memo, PhysicalASAPMemoCandidates};
use asap_types::workload::{DataWorkload, QueryLanguage, QueryWorkloadEntry};

use crate::models::PlanningModels;
use crate::pass::{OptimizationInput, OptimizationPass, OptimizeError, PlanOutput};

/// How the facade searches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanSearchSettings {
    /// Keep Pass 1's output for inspection (`asap_planner`, the DAG
    /// viewer): [`PlanningRun::pass1_memo`]. Default off:
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
    /// Stage 3 selected no plan.
    #[error("plan selection: {0}")]
    Selection(ErrorSelectingPlan),
    /// The run stopped before plan selection (`asap_planner --run-until`).
    #[error("plan selection did not run")]
    NotRun,
}

/// What the facade did: each stage's counts and time.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct PlanningReport {
    /// Stage 0 and Pass 1, in milliseconds.
    pub pass1_ms: f64,
    /// Pass 2's output: memo groups and their candidates.
    pub memo_groups: usize,
    pub logical_candidates: usize,
    /// Stage 2's output: physical candidates and pruned candidates.
    pub physical_candidates: usize,
    pub stage2_pruned_candidates: usize,
    pub pass2_ms: f64,
    pub stage2_ms: f64,
    pub stage3_ms: f64,
}

/// Every stage's output for one workload ([`run_planning`]).
#[derive(Debug)]
pub struct PlanningRun {
    /// Stage 1, Pass 1's output: the memo groups with their per-computation
    /// candidates and the pruned candidates.
    /// `None` when Pass 1 failed or without [`PlanSearchSettings::inspect`].
    pub pass1_memo: Option<Memo>,
    /// Pass 2's output, when it ran.
    pub logical_asap_candidates: Option<LogicalASAPMemoCandidates>,
    /// Stage 2's output, when it ran: the catalog `selection`'s candidate
    /// ids index.
    pub physical_asap_candidates: Option<PhysicalASAPMemoCandidates>,
    /// Stage 3's plan and its report of the selection.
    pub selection: Result<(PhysicalASAPDAG, Selection), SelectionError>,
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
    // Stage 0: the memo of logical groups, each query with its accuracy
    // target, or none when accuracy is not enforced.
    let accuracy_targets: Vec<_> = entries
        .iter()
        .map(|entry| {
            (!models.ignore_accuracy_guarantees).then(|| entry.requirements.accuracy.clone())
        })
        .collect();
    let initial = memo_of_queries(&roots, &accuracy_targets, data)?;
    // Pass 1: the initial candidates the deployment can run. A query Pass 1
    // cannot plan is an input error.
    let pass1 = pass1_memo(
        initial,
        models
            .deployment_capabilities
            .supported_summaries
            .as_deref(),
    )?;
    let mut report = PlanningReport {
        pass1_ms: elapsed_ms(started),
        ..PlanningReport::default()
    };
    let pass1_memo = settings.inspect.then(|| pass1.clone());
    let mut logical_asap_candidates = None;
    let mut physical_asap_candidates = None;
    let selection = (|| {
        models
            .deployment_capabilities
            .validate(data)
            .map_err(SelectionError::InvalidDeploymentCapabilities)?;
        // Pass 2: the reuse rules, readout requirements and logical edges.
        let started = Instant::now();
        let logical = generate_logical_asap_memo_candidates(pass1, entries, models.evidence);
        report.pass2_ms = elapsed_ms(started);
        let logical = logical_asap_candidates.insert(
            logical.map_err(|error| SelectionError::LogicalASAPCandidates(error.to_string()))?,
        );
        report.memo_groups = logical.memo.groups().len();
        report.logical_candidates = logical
            .memo
            .groups()
            .iter()
            .map(|group| group.candidates.len())
            .sum();
        // Stage 2.
        let started = Instant::now();
        let physical =
            generate_physical_asap_memo_candidates(logical, data, models.deployment_capabilities);
        report.stage2_ms = elapsed_ms(started);
        let physical = physical_asap_candidates.insert(physical);
        report.physical_candidates = physical.physical_candidates.len();
        report.stage2_pruned_candidates = physical.pruned_candidates.len();
        // Stage 3: the selection and the plan it builds.
        let started = Instant::now();
        let selection = select_plan(physical, entries, data, models.cost_inputs);
        report.stage3_ms = elapsed_ms(started);
        selection.map_err(SelectionError::Selection)
    })();
    Ok(PlanningRun {
        pass1_memo,
        logical_asap_candidates,
        physical_asap_candidates,
        selection,
        report,
    })
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// The planner pipeline behind the optimization-pass interface: the
/// workload's roots in entry order, PromQL roots with each series' full
/// identity as a column (the row representation per-series state needs at
/// runtime; a root with no such representation is planned over its labels
/// alone), through [`run_planning`] with every entry's accuracy target. The
/// output is the plan Stage 3 built; its `Selection` report is
/// [`run_planning`]'s.
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
        let (plan, _selection) = run.selection?;
        let output = PlanOutput::new(plan);
        Ok(output)
    }
}
