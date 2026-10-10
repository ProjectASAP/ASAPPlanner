//! The pluggable optimization pass (issue #430).
//!
//! An [`OptimizationPass`] is the whole optimization stage behind one
//! signature: pre-ASAP IR in, post-ASAP DAG out. The trait deliberately names
//! none of the stage pipeline's vocabulary — no Stage 1 inventory, sharing
//! variants or physical candidates — so an algorithm with no
//! candidate-generation phase at all (a greedy MQO loop, say) can implement it
//! without pretending to have phases it does not have. The shipped algorithm is
//! one implementation, [`StagePipeline`].
//!
//! Call [`optimize`] rather than [`OptimizationPass::optimize`] directly: it
//! validates the input once for every pass and checks the output contract that
//! downstream consumers rely on.

mod stage_pipeline;

use std::collections::BTreeMap;
use std::rc::Rc;

use asap_types::ir::physical_export::compile_physical_asap_workload;
use asap_types::ir::properties::ExecutionDataStateError;
use asap_types::ir::OperatorNode;
use asap_types::ir::{apply_materialization_timings, MaterializationAssignment, TimingMemo};
use asap_types::workload::parsed_workload::ParsedWorkload;
use asap_types::workload::WorkloadError;

use asap_logical_optimizer::pass1::logical_candidates::LogicalCandidateError;
use asap_plan_selection::{Selection, SelectionError};

use asap_plan_selection::PlanningModels;
pub use stage_pipeline::StagePipeline;

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

/// One query's selected post-ASAP DAG.
#[derive(Debug, Clone)]
pub struct QueryPlan {
    /// Index into `QueryWorkload::entries()`.
    pub entry_index: usize,
    pub root: Rc<OperatorNode>,
}

/// One multi-root workload DAG with query bindings in entry order;
/// [`check_contract`] enforces that.
///
/// Plans are not deduplicated across entries: a summary state that several
/// queries share appears in each of their plans as the same `Rc`, so a
/// consumer that deploys or costs the workload must dedupe by `Rc::ptr_eq`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PlanOutput {
    pub plans: Vec<QueryPlan>,
    /// Exact scalar expressions, keyed by workload entry; embedded plan reads remain visible.
    pub scalar_roots: Vec<(usize, asap_types::ir::ScalarExpr)>,
    /// How the plan was chosen, when the pass selects among priced candidates.
    pub selection: Option<Selection>,
}

impl PlanOutput {
    pub fn new(plans: Vec<QueryPlan>) -> Self {
        Self {
            plans,
            scalar_roots: Vec::new(),
            selection: None,
        }
    }

    /// Entry indices in output order.
    pub fn entry_indices(&self) -> Vec<usize> {
        let mut indices: Vec<_> = self
            .plans
            .iter()
            .map(|p| p.entry_index)
            .chain(self.scalar_roots.iter().map(|(i, _)| *i))
            .collect();
        if !self.scalar_roots.is_empty() {
            indices.sort_unstable();
        }
        indices
    }

    /// All query roots in workload order, including standalone scalars.
    pub fn roots(&self) -> Vec<asap_types::ir::QueryRoot> {
        let mut roots: Vec<_> = self
            .plans
            .iter()
            .map(|p| {
                (
                    p.entry_index,
                    asap_types::ir::QueryRoot::Operator(Rc::clone(&p.root)),
                )
            })
            .chain(
                self.scalar_roots
                    .iter()
                    .map(|(i, expr)| (*i, asap_types::ir::QueryRoot::Scalar(expr.clone()))),
            )
            .collect();
        roots.sort_by_key(|(i, _)| *i);
        roots.into_iter().map(|(_, root)| root).collect()
    }

    /// The selected operator roots. Use `roots()` to include scalar queries.
    pub fn operator_roots(&self) -> Vec<Rc<OperatorNode>> {
        self.plans.iter().map(|p| Rc::clone(&p.root)).collect()
    }

    /// Unique operators in the entire workload DAG, including scalar-plan dependencies.
    /// Several query roots can reach the same operator; it is returned once.
    pub fn operators(&self) -> Vec<Rc<OperatorNode>> {
        let mut seen = std::collections::HashSet::new();
        let mut nodes = Vec::new();
        for root in self.roots() {
            let inputs = match root {
                asap_types::ir::QueryRoot::Operator(node) => vec![node],
                asap_types::ir::QueryRoot::Scalar(expr) => {
                    expr.operator_refs().into_iter().cloned().collect()
                }
            };
            for input in inputs {
                for node in OperatorNode::reachable(&input) {
                    if seen.insert(Rc::as_ptr(&node)) {
                        nodes.push(node);
                    }
                }
            }
        }
        nodes
    }

    /// The workload as one physical ASAP DAG: a root per operator query, in
    /// plan order, with shared sub-DAGs exported once. Standalone scalar
    /// roots have no physical form yet and are left out.
    pub fn execution_timed_dag(
        &self,
    ) -> Result<asap_types::ir::physical_export::PhysicalASAPDAG, ExecutionDataStateError> {
        // One memo, so a node shared by several roots is timed and exported once.
        let mut memo = TimingMemo::new();
        let assignment = MaterializationAssignment::all_query_time();
        let timed = self
            .plans
            .iter()
            .map(|p| apply_materialization_timings(&p.root, &assignment, &mut memo))
            .collect::<Result<Vec<_>, _>>()?;
        compile_physical_asap_workload(&timed)
    }

    pub fn len(&self) -> usize {
        self.plans.len() + self.scalar_roots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OptimizeError {
    #[error("optimization input: {0}")]
    Input(#[from] OptimizationInputError),
    #[error("Stage 1: {0}")]
    LogicalCandidates(#[from] LogicalCandidateError),
    #[error("plan selection: {0}")]
    Selection(#[from] SelectionError),
    /// The pass returned something the downstream contract forbids. This is a
    /// defect in the pass, not in its input.
    #[error("pass `{pass}` violated the output contract: {detail}")]
    ContractViolation { pass: &'static str, detail: String },
}

// ── The pass ─────────────────────────────────────────────────────────────

/// The optimization stage, end to end.
///
/// Implementors are free to ignore any part of [`OptimizationInput`] they do
/// not use, but must uphold the contract [`check_contract`] enforces.
pub trait OptimizationPass {
    /// Registry key, also used in diagnostics.
    fn name(&self) -> &'static str;

    fn optimize(&self, input: OptimizationInput<'_>) -> Result<PlanOutput, OptimizeError>;
}

/// Run `pass` over `input`: validate once, then check the output contract.
///
/// Prefer this over calling [`OptimizationPass::optimize`] directly. A pass is
/// third-party code, but `ASAPQuery-backend` and `ASAPCollector` read every
/// pass's output against the same contract; checking here means a defective
/// pass fails at the boundary instead of corrupting a deployment.
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

/// Structural checks only — that every query is accounted for, in order.
/// Whether the pass chose *well* is not checked.
fn check_contract(
    output: &PlanOutput,
    pass: &'static str,
    expected_len: usize,
) -> Result<(), OptimizeError> {
    let violation = |detail: String| OptimizeError::ContractViolation { pass, detail };

    if output.len() != expected_len {
        return Err(violation(format!(
            "{} plan(s) for {expected_len} workload entry/entries",
            output.len()
        )));
    }
    for (position, entry_index) in output.entry_indices().into_iter().enumerate() {
        if entry_index != position {
            return Err(violation(format!(
                "plan at position {position} carries entry_index {entry_index}"
            )));
        }
    }
    Ok(())
}

// ── Registry ─────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
#[error("a pass named `{0}` is already registered")]
pub struct PassNameConflict(pub &'static str);

/// Name-keyed lookup, for callers that choose a pass from a string: a CLI flag,
/// a config file, or a harness sweeping every registered pass over one input.
///
/// Deliberately caller-owned rather than a link-time global: two tests in one
/// binary must not see each other's registrations.
#[derive(Default)]
pub struct PassRegistry {
    passes: BTreeMap<&'static str, Box<dyn OptimizationPass>>,
}

impl PassRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Only [`StagePipeline`], under the name `stage-pipeline`.
    pub fn with_builtin() -> Self {
        let mut registry = Self::new();
        registry
            .register(Box::new(StagePipeline))
            .expect("empty registry cannot conflict");
        registry
    }

    /// Keyed by `pass.name()`. Registering a name twice is an error rather
    /// than an overwrite: silently replacing one baseline with another would
    /// make a comparison run measure the same pass twice.
    pub fn register(&mut self, pass: Box<dyn OptimizationPass>) -> Result<(), PassNameConflict> {
        let name = pass.name();
        if self.passes.contains_key(name) {
            return Err(PassNameConflict(name));
        }
        self.passes.insert(name, pass);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&dyn OptimizationPass> {
        self.passes.get(name).map(AsRef::as_ref)
    }

    /// Registered names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.passes.keys().copied()
    }

    pub fn len(&self) -> usize {
        self.passes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.passes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub(&'static str);

    impl OptimizationPass for Stub {
        fn name(&self) -> &'static str {
            self.0
        }
        fn optimize(&self, _input: OptimizationInput<'_>) -> Result<PlanOutput, OptimizeError> {
            Ok(PlanOutput::new(Vec::new()))
        }
    }

    /// Dropping a query is rejected: the output is positionally aligned with
    /// the workload's entries, so a short vector is not a partial result.
    #[test]
    fn contract_rejects_a_plan_count_that_does_not_cover_every_entry() {
        let output = PlanOutput::new(Vec::new());
        let err = check_contract(&output, "stub", 2).unwrap_err();
        assert!(matches!(
            err,
            OptimizeError::ContractViolation { pass: "stub", .. }
        ));
    }

    /// The matching count passes.
    #[test]
    fn contract_accepts_an_empty_workload() {
        let output = PlanOutput::new(Vec::new());
        assert!(check_contract(&output, "stub", 0).is_ok());
    }

    /// Registering a name twice fails instead of overwriting, so a comparison
    /// run cannot silently measure one pass twice.
    #[test]
    fn registry_refuses_a_duplicate_name() {
        let mut registry = PassRegistry::with_builtin();
        assert!(registry.register(Box::new(Stub("greedy"))).is_ok());
        let err = registry.register(Box::new(Stub("greedy"))).unwrap_err();
        assert_eq!(err.0, "greedy");
    }

    /// The builtin registry resolves `stage-pipeline`, and names come back sorted so a
    /// sweep over every registered pass is reproducible.
    #[test]
    fn registry_resolves_builtin_and_lists_names_in_order() {
        let mut registry = PassRegistry::with_builtin();
        registry.register(Box::new(Stub("alpha"))).unwrap();
        assert!(registry.get("stage-pipeline").is_some());
        assert!(registry.get("absent").is_none());
        assert_eq!(
            registry.names().collect::<Vec<_>>(),
            vec!["alpha", "stage-pipeline"]
        );
    }
}
