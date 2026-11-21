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
use asap_types::ir::schema::series_identity::{with_promql_series_identity, SeriesIdentityError};
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::plan::physical_asap_dag::PhysicalASAPDAG;
use asap_types::stages::{LogicalASAPMemoCandidates, Memo, PhysicalASAPMemoCandidates};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::models::PlanningModels;
use crate::pass::{OptimizationInput, OptimizationPass, OptimizeError, PlanOutput};

/// How the facade searches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanSearchSettings {
    /// Keep Stage 0's and Pass 1's output for inspection (`asap_planner`,
    /// the DAG viewer): [`PlanningRun::stage0_memo`] and
    /// [`PlanningRun::pass1_memo`]. Default off:
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
    /// The run stopped before plan selection ([`run_planning_until`]).
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

/// The last stage a run runs ([`run_planning_until`]); the run's fields
/// for later stages are left empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RunUntil {
    Stage0,
    Pass1,
    Pass2,
    Stage2,
    Stage3,
}

impl RunUntil {
    const ALL: [Self; 5] = [
        Self::Stage0,
        Self::Pass1,
        Self::Pass2,
        Self::Stage2,
        Self::Stage3,
    ];

    /// The stage's name: `asap_planner --run-until`'s value, also written
    /// as the JSON `run_until`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Stage0 => "stage0",
            Self::Pass1 => "stage1_pass1",
            Self::Pass2 => "stage1_pass2",
            Self::Stage2 => "stage2",
            Self::Stage3 => "stage3",
        }
    }
}

impl std::str::FromStr for RunUntil {
    type Err = String;
    fn from_str(stage: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|until| until.name() == stage)
            .ok_or_else(|| format!("unknown stage {stage}"))
    }
}

/// Every stage's output for one workload ([`run_planning`]).
#[derive(Debug)]
pub struct PlanningRun {
    /// Stage 0's output: the memo groups (the roots hash-consed), without
    /// candidates. `None` without [`PlanSearchSettings::inspect`].
    pub stage0_memo: Option<Memo>,
    /// Stage 1, Pass 1's output: the memo groups with their per-computation
    /// candidates and the pruned candidates.
    /// `None` when Pass 1 did not run, when the deployment's capabilities
    /// contradict the data workload, or without
    /// [`PlanSearchSettings::inspect`].
    pub pass1_memo: Option<Memo>,
    /// Pass 2's output, when it ran.
    pub logical_asap_candidates: Option<LogicalASAPMemoCandidates>,
    /// Stage 2's output, when it ran: the catalog `selection`'s candidate
    /// ids index.
    pub physical_asap_candidates: Option<PhysicalASAPMemoCandidates>,
    /// Stage 3's plan and its report of the selection
    /// ([`SelectionError::NotRun`] when the run stopped before Stage 3).
    pub selection: Result<(PhysicalASAPDAG, Selection), SelectionError>,
    pub report: PlanningReport,
}

/// Run the planning stages over the frontends' `roots` (query `i` is
/// `roots[i]`, with workload entry `entries[i]`). Only a Stage 0 or Pass 1
/// failure is an error; a failed selection is reported in the run.
pub fn run_planning(
    roots: Vec<QueryRoot>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    settings: PlanSearchSettings,
) -> Result<PlanningRun, PerComputationCandidateError> {
    run_planning_until(&roots, entries, data, models, settings, RunUntil::Stage3)
}

/// [`run_planning`], stopped after the stage `until`: the stages after it
/// do not run.
pub fn run_planning_until(
    roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    settings: PlanSearchSettings,
    until: RunUntil,
) -> Result<PlanningRun, PerComputationCandidateError> {
    let started = Instant::now();
    for root in roots {
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
    let initial = memo_of_queries(roots, &accuracy_targets, data)?;
    let mut run = PlanningRun {
        stage0_memo: settings.inspect.then(|| initial.clone()),
        pass1_memo: None,
        logical_asap_candidates: None,
        physical_asap_candidates: None,
        selection: Err(SelectionError::NotRun),
        report: PlanningReport::default(),
    };
    if until == RunUntil::Stage0 {
        return Ok(run);
    }
    // The deployment's capabilities as exclusions, expanded once for Pass 1
    // and Stage 2.
    let exclusions = models.deployment_capabilities.exclusions();
    // Pass 1: the initial candidates, without the summaries the deployment
    // excludes outright (Stage 2 prunes the rest it cannot run). A query
    // Pass 1 cannot plan is an input error.
    let pass1 = pass1_memo(initial, &exclusions)?;
    run.report.pass1_ms = elapsed_ms(started);
    let report = &mut run.report;
    let kept_pass1_memo = &mut run.pass1_memo;
    let logical_asap_candidates = &mut run.logical_asap_candidates;
    let physical_asap_candidates = &mut run.physical_asap_candidates;
    run.selection = (|| {
        // A query Pass 1 cannot plan is reported before contradictory
        // deployment capabilities.
        models
            .deployment_capabilities
            .validate(data)
            .map_err(SelectionError::InvalidDeploymentCapabilities)?;
        if settings.inspect {
            *kept_pass1_memo = Some(pass1.clone());
        }
        if until == RunUntil::Pass1 {
            return Err(SelectionError::NotRun);
        }
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
        if until == RunUntil::Pass2 {
            return Err(SelectionError::NotRun);
        }
        // Stage 2.
        let started = Instant::now();
        let physical = generate_physical_asap_memo_candidates(logical, data, &exclusions);
        report.stage2_ms = elapsed_ms(started);
        let physical = physical_asap_candidates.insert(physical);
        report.physical_candidates = physical.physical_candidates.len();
        report.stage2_pruned_candidates = physical.pruned_candidates.len();
        if until == RunUntil::Stage2 {
            return Err(SelectionError::NotRun);
        }
        // Stage 3: the selection and the plan it builds.
        let started = Instant::now();
        let selection = select_plan(
            physical,
            entries,
            data,
            models.cost_inputs,
            models.deployment_capabilities.final_results_cache(),
        );
        report.stage3_ms = elapsed_ms(started);
        selection.map_err(SelectionError::Selection)
    })();
    Ok(run)
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// The planner pipeline behind the optimization-pass interface: the
/// workload's roots in entry order, those of a time-series language
/// ([`asap_types::workload::QueryLanguage::reads_time_series`]: PromQL and
/// MetricsQL) with each series' full identity as a column (the row
/// representation per-series state needs at runtime; a root with no such
/// representation is planned over its labels alone), through
/// [`run_planning`] with every entry's accuracy target. The output is the
/// plan Stage 3 built; its `Selection` report is [`run_planning`]'s.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stage1ToStage3Pass;

impl OptimizationPass for Stage1ToStage3Pass {
    fn name(&self) -> &'static str {
        "stage1-to-stage3"
    }

    fn optimize(&self, input: OptimizationInput<'_>) -> Result<PlanOutput, OptimizeError> {
        let workload = input.workload;
        // Time-series rows are keyed by each series' full label set: the
        // `PROMQL_SERIES_IDENTITY` column, added to every root before Stage
        // 0. `rate(requests_total[5m])` keeps one rate per series, each
        // series `{"__name__":"requests_total","instance":"a","job":"j1"}`
        // one identity. Table rows have no such identity.
        let series_identity = workload.query_workload().language.reads_time_series();
        let mut roots: Vec<(usize, QueryRoot)> = workload
            .operator_indices()
            .iter()
            .zip(workload.exprs())
            .map(|(&index, node)| {
                let node = match series_identity {
                    true => promql_root(node, with_promql_series_identity(node))?,
                    false => Rc::clone(node),
                };
                Ok((index, QueryRoot::Operator(node)))
            })
            .collect::<Result<_, OptimizeError>>()?;
        roots.extend(
            workload
                .scalar_roots()
                .iter()
                .map(|(index, expr)| (*index, QueryRoot::Scalar(expr.clone()))),
        );
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

/// The PromQL or MetricsQL root `node` planned: `realized`, its series identity
/// realization (`with_promql_series_identity`), or `node` itself, over its
/// label columns, when the realization does not apply to one of its
/// operators (`info(m)`'s `PromqlInfoEnrich`). A realization defect is a
/// planning error, since falling back would hide it: one that drops a
/// column an operator reads (`DroppedColumn`) or rebuilds an operator whose
/// schema no longer derives (`InvalidRealizedOperator`).
fn promql_root(
    node: &Rc<OperatorNode>,
    realized: Result<Rc<OperatorNode>, SeriesIdentityError>,
) -> Result<Rc<OperatorNode>, OptimizeError> {
    match realized {
        Ok(realized) => Ok(realized),
        Err(SeriesIdentityError::NotApplicable(_)) => Ok(Rc::clone(node)),
        Err(error) => Err(OptimizeError::SeriesIdentity(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::scalar::ScalarExpr;
    use asap_types::ir::{NonASAPOp, Operator};

    fn vector_one() -> Rc<OperatorNode> {
        OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::PromqlVectorFromScalar(
            ScalarExpr::Literal(asap_types::ir::scalar::ScalarValue::Float64(1.0)),
        )))
        .unwrap()
    }

    /// A realization that does not apply plans the root as it is.
    #[test]
    fn a_promql_root_realization_does_not_apply_to_is_planned_as_is() {
        let node = vector_one();
        let realized = Err(SeriesIdentityError::NotApplicable(
            "PromqlSeriesSample has no dynamic series-identity realization".into(),
        ));
        assert!(Rc::ptr_eq(&promql_root(&node, realized).unwrap(), &node));
    }

    /// A realization that drops a column an operator reads fails planning
    /// rather than plan the unrealized root.
    #[test]
    fn a_realization_that_drops_a_read_column_fails_planning() {
        let error = SeriesIdentityError::DroppedColumn {
            operator: "Sort",
            column: 1,
        };
        assert!(matches!(
            promql_root(&vector_one(), Err(error.clone())),
            Err(OptimizeError::SeriesIdentity(dropped)) if dropped == error
        ));
    }

    /// A realization that rebuilds an operator whose schema no longer
    /// derives fails planning rather than plan the unrealized root.
    #[test]
    fn a_realization_that_builds_an_invalid_operator_fails_planning() {
        let error = SeriesIdentityError::InvalidRealizedOperator(
            asap_types::ir::schema::error::SchemaDerivationError::EmptyConcat,
        );
        assert!(matches!(
            promql_root(&vector_one(), Err(error.clone())),
            Err(OptimizeError::SeriesIdentity(invalid)) if invalid == error
        ));
    }
}
