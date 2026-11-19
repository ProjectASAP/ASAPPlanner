//! The pluggable optimization pass (issue #430).
//!
//! An [`OptimizationPass`] is the whole optimization stage behind one
//! signature: pre-ASAP IR in, post-ASAP DAG out. The trait deliberately names
//! none of the stages' candidate types, so an algorithm with no
//! candidate-generation phase at all (a greedy MQO loop, say) can implement it
//! without pretending to have phases it does not have. The shipped algorithm is
//! one implementation, [`Stage1ToStage3Pass`](crate::Stage1ToStage3Pass): the planning stages in order
//! ([`run_planning`](crate::run_planning)).
//!
//! Call [`optimize`] rather than [`OptimizationPass::optimize`] directly: it
//! validates the input once for every pass and checks the output contract that
//! downstream consumers rely on.

use asap_types::ir::schema::series_identity::SeriesIdentityError;
use asap_types::workload::parsed_workload::ParsedWorkload;
use asap_types::workload::WorkloadError;

use crate::models::PlanningModels;
use crate::run::SelectionError;
use asap_stage1_pass1_per_computation_candidates::PerComputationCandidateError;
use asap_types::plan::physical_asap_dag::{PhysicalASAPDAG, PhysicalASAPDAGValidationError};

// ── Input ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct OptimizationInput<'a> {
    pub workload: &'a ParsedWorkload,
    pub models: PlanningModels<'a>,
}

impl<'a> OptimizationInput<'a> {
    pub fn new(workload: &'a ParsedWorkload, models: PlanningModels<'a>) -> Self {
        Self { workload, models }
    }

    pub fn validate(&self) -> Result<(), OptimizationInputError> {
        self.workload
            .validate()
            .map_err(OptimizationInputError::Workload)
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OptimizationInputError {
    #[error("workload: {0}")]
    Workload(WorkloadError),
}

// ── Output ───────────────────────────────────────────────────────────────

/// The selected plan: one `PhysicalASAPDAG`, the only plan structure, in
/// memory and, serialized, in the plan file installed on the executor. Its
/// `roots` answer the operator queries and its `scalar_roots` the standalone
/// scalar queries, in entry order; `check_contract` enforces that.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PlanOutput {
    pub plan: PhysicalASAPDAG,
}

impl PlanOutput {
    pub fn new(plan: PhysicalASAPDAG) -> Self {
        Self { plan }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OptimizeError {
    #[error("optimization input: {0}")]
    Input(#[from] OptimizationInputError),
    #[error("logical ASAP-aware optimization: {0}")]
    PerComputationCandidates(#[from] PerComputationCandidateError),
    #[error("plan selection: {0}")]
    Selection(#[from] SelectionError),
    /// Series identity realization would make an operator read a column it
    /// does not keep, or rebuilt an operator whose schema no longer derives:
    /// a defect in the realization, not in the query.
    #[error("PromQL series identity realization: {0}")]
    SeriesIdentity(SeriesIdentityError),
    /// The pass returned something the downstream contract forbids. This is a
    /// defect in the pass, not in its input.
    #[error("pass `{pass}` violated the output contract: {detail}")]
    ContractViolation { pass: &'static str, detail: String },
    /// The pass returned an invalid DAG: a defect in the pass.
    #[error("pass `{pass}` returned an invalid plan: {error}")]
    InvalidPlan {
        pass: &'static str,
        error: PhysicalASAPDAGValidationError,
    },
}

// ── The pass ─────────────────────────────────────────────────────────────

/// The optimization stage, end to end.
///
/// Implementors are free to ignore any part of [`OptimizationInput`] they do
/// not use, but must uphold the contract `check_contract` enforces.
pub trait OptimizationPass {
    /// Registry key, also used in diagnostics.
    fn name(&self) -> &'static str;

    fn optimize(&self, input: OptimizationInput<'_>) -> Result<PlanOutput, OptimizeError>;
}

/// Run `pass` over `input`: validate once, then check the output contract.
///
/// Prefer this over calling [`OptimizationPass::optimize`] directly. A pass is
/// third-party code, but every deployment runtime reads every pass's output
/// against the same contract; checking here means a defective pass fails at
/// the boundary instead of corrupting a deployment.
pub fn optimize(
    pass: &dyn OptimizationPass,
    input: OptimizationInput<'_>,
) -> Result<PlanOutput, OptimizeError> {
    input.validate()?;
    let expected_len = input.workload.len();

    let output = pass.optimize(input)?;
    check_contract(&output, pass.name(), expected_len)?;
    Ok(output)
}

/// Structural checks only — that every query is answered once, by an
/// operator root or the scalar root keyed with its entry index, and that
/// the DAG is valid. Whether the pass chose *well* is not checked.
fn check_contract(
    output: &PlanOutput,
    pass: &'static str,
    expected_len: usize,
) -> Result<(), OptimizeError> {
    let plan = &output.plan;
    let answered = plan.roots.len() + plan.scalar_roots.len();
    if answered != expected_len {
        return Err(OptimizeError::ContractViolation {
            pass,
            detail: format!("{answered} plan root(s) for {expected_len} workload entry/entries"),
        });
    }
    // An empty workload's plan has no roots, which `validate` rejects.
    if expected_len > 0 {
        plan.validate()
            .map_err(|error| OptimizeError::InvalidPlan { pass, error })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_plan() -> PlanOutput {
        PlanOutput::new(PhysicalASAPDAG {
            nodes: vec![],
            edges: vec![],
            roots: vec![],
            scalar_roots: vec![],
        })
    }

    /// Dropping a query is rejected: the plan's roots answer the workload's
    /// entries in order, so a plan with fewer roots is not a partial result.
    #[test]
    fn contract_rejects_a_plan_count_that_does_not_cover_every_entry() {
        let err = check_contract(&empty_plan(), "stub", 2).unwrap_err();
        assert!(matches!(
            err,
            OptimizeError::ContractViolation { pass: "stub", .. }
        ));
    }

    /// A scalar root keyed by an entry index the workload does not have is
    /// rejected as an invalid plan.
    #[test]
    fn contract_rejects_a_scalar_root_keyed_beyond_the_entries() {
        use asap_types::ir::scalar::ScalarValue;
        use asap_types::ir::ScalarExpr;
        let mut output = empty_plan();
        output
            .plan
            .scalar_roots
            .push((1, ScalarExpr::Literal(ScalarValue::Float64(1.0))));
        let err = check_contract(&output, "stub", 1).unwrap_err();
        assert!(matches!(
            err,
            OptimizeError::InvalidPlan {
                pass: "stub",
                error: PhysicalASAPDAGValidationError::ScalarRootPosition(1)
            }
        ));
    }

    /// The matching count passes.
    #[test]
    fn contract_accepts_an_empty_workload() {
        assert!(check_contract(&empty_plan(), "stub", 0).is_ok());
    }
}
