//! [`MajorPass`] — the shipped two-phase algorithm, behind the
//! [`OptimizationPass`](super::OptimizationPass) trait.
//!
//! This is the same pipeline the crate has always run (candidate search,
//! whole-workload selection, per-root assembly); moving it here is what makes
//! it *one* pass rather than *the* algorithm. `ReplacementStrategy` is
//! therefore a concept of this pass, not of the optimization interface.

use std::rc::Rc;

use asap_types::pre_asap::query_expr::QueryExpr;
use asap_types::types::AccuracyTarget;

use super::{
    OptimizationInput, OptimizationPass, OptimizeError, PlanOutput, QueryLifecyclePlan, QueryPlan,
};
use crate::replacement::{default_strategies_with_evidence, search_workload_with_targets};
use crate::summary_maintenance_lifecycle::{
    assemble_selected_dag_with_summary_maintenance_lifecycles,
    global_selection_with_summary_maintenance_lifecycles, WorkloadDemand,
};

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
        // search result carries the workload binding the lifecycle stage and
        // the output both need. CSE may make two identical queries share one
        // `Rc`, but it never drops or reorders a root, so this stays aligned.
        let roots: Vec<(usize, Rc<QueryExpr>, Option<AccuracyTarget>)> = workload
            .entries()
            .enumerate()
            .map(|(index, (entry, expr))| {
                (
                    index,
                    Rc::clone(expr),
                    Some(entry.requirements.accuracy.target()),
                )
            })
            .collect();

        let space = search_workload_with_targets(roots, &strategies, models.accuracy);

        let Some(lifecycle) = input.lifecycle else {
            let selection = space.global_selection(models.cost);
            let mut plans = Vec::with_capacity(space.roots.len());
            for (entry_index, root) in &space.roots {
                let dag = selection.assemble_selected_dag(root).map_err(|source| {
                    OptimizeError::Realization {
                        entry_index: *entry_index,
                        source,
                    }
                })?;
                plans.push(QueryPlan {
                    entry_index: *entry_index,
                    dag: dag.ok_or_else(|| self.missing_group(*entry_index))?,
                });
            }
            return Ok(PlanOutput::Dag { plans });
        };

        // One index per root, in `PlanSpace::roots` order — which is the order
        // the roots went in, which is `entries()` order.
        let entry_indices: Vec<usize> = (0..workload.len()).collect();
        let demand = WorkloadDemand {
            workload: workload.query_workload(),
            data_workload: workload.data_workload(),
            entry_indices: &entry_indices,
        };

        let selection = global_selection_with_summary_maintenance_lifecycles(
            &space,
            demand,
            lifecycle.now_ms,
            lifecycle.horizon,
            lifecycle.capabilities,
            models.cost,
        )
        .map_err(OptimizeError::LifecycleSelection)?;

        let mut plans = Vec::with_capacity(space.roots.len());
        for (entry_index, root) in &space.roots {
            let plan = assemble_selected_dag_with_summary_maintenance_lifecycles(
                &selection,
                root,
                demand,
                lifecycle.now_ms,
                lifecycle.horizon,
                lifecycle.capabilities,
                models.cost,
            )
            .map_err(|source| OptimizeError::LifecycleAssembly {
                entry_index: *entry_index,
                source,
            })?;
            plans.push(QueryLifecyclePlan {
                entry_index: *entry_index,
                plan: plan.ok_or_else(|| self.missing_group(*entry_index))?,
            });
        }
        Ok(PlanOutput::DagWithLifecycle { plans })
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
