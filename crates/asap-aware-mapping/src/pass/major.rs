//! [`MajorPass`] — the shipped two-phase algorithm, behind the
//! [`OptimizationPass`](super::OptimizationPass) trait.
//!
//! This is the same pipeline the crate has always run (candidate search,
//! whole-workload selection, per-root assembly); moving it here is what makes
//! it *one* pass rather than *the* algorithm. `ReplacementStrategy` is
//! therefore a concept of this pass, not of the optimization interface.

use asap_types::ir::cse::share_common_sub_dags;
use std::rc::Rc;

use asap_types::ir::OperatorNode;
use asap_types::types::AccuracyTarget;

use super::{OptimizationInput, OptimizationPass, OptimizeError, PlanOutput, QueryPlan};
use crate::replacement::{default_strategies_with_evidence, search_workload_with_targets};

/// The shipped algorithm. Unit struct: its strategy set is the crate default,
/// and a caller who wants a different one now has a better option than
/// swapping rules — write another [`OptimizationPass`].
#[derive(Debug, Default, Clone, Copy)]
pub struct MajorPass;

impl OptimizationPass for MajorPass {
    fn name(&self) -> &'static str {
        "major"
    }

    fn optimize(&self, input: OptimizationInput<'_>) -> Result<PlanOutput, OptimizeError> {
        let workload = input.workload;
        let models = input.models;
        let strategies = default_strategies_with_evidence(models.cost, models.evidence);

        // `Id` is the entry's position in `QueryWorkload::entries()`, so the
        // search result carries the workload binding the output needs. CSE may make two identical queries share one
        // `Rc`, but it never drops or reorders a root, so this stays aligned.
        let roots: Vec<(usize, Rc<OperatorNode>, Option<AccuracyTarget>)> = workload
            .entries()
            .zip(workload.operator_indices().iter().copied())
            .map(|((entry, expr), index)| {
                (
                    index,
                    Rc::clone(expr),
                    Some(entry.requirements.accuracy.target()),
                )
            })
            .collect();

        let space = search_workload_with_targets(roots, &strategies, models.accuracy);

        let selection = space.global_selection(models.cost);

        // Assemble every root, then intern structurally identical summary
        // producers across them once, so two queries that selected the same
        // `SummaryAgg` reach one `Rc` (consumers dedupe states by pointer).
        let mut assembled = Vec::with_capacity(space.roots.len());
        for (entry_index, root) in &space.roots {
            let dag = selection
                .assemble_selected_dag(root)
                .map_err(|source| OptimizeError::Realization {
                    entry_index: *entry_index,
                    source,
                })?
                .ok_or_else(|| self.missing_group(*entry_index))?;
            assembled.push((*entry_index, dag));
        }
        let plans = share_common_sub_dags(assembled)
            .into_iter()
            .map(|(entry_index, root)| QueryPlan { entry_index, root })
            .collect();
        let mut output = PlanOutput::new(plans);
        output.scalar_roots = workload.scalar_roots().to_vec();
        Ok(output)
    }
}

impl MajorPass {
    /// Assembly returns `Ok(None)` only for a target that is not a discovered
    /// site. Every root is one — `discover_targets` walks each root and
    /// `search_cse_workload_with` gives every discovered target a group — so
    /// reaching this means the invariant broke, not that the query had no
    /// optimization available.
    fn missing_group(&self, entry_index: usize) -> OptimizeError {
        OptimizeError::ContractViolation {
            pass: self.name(),
            detail: format!("entry {entry_index}: root has no candidate group"),
        }
    }
}
