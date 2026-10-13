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
        // search result carries the workload binding the output needs. CSE
        // may make two identical queries share one `Rc`, but it never drops
        // or reorders a root, so this stays aligned.
        let targets: Vec<AccuracyTarget> = workload
            .query_workload()
            .entries()
            .map(|entry| entry.requirements.accuracy.target())
            .collect();
        let mut roots: Vec<(usize, Rc<OperatorNode>, Option<AccuracyTarget>)> = workload
            .exprs()
            .iter()
            .zip(workload.operator_indices().iter().copied())
            .map(|(expr, index)| (index, Rc::clone(expr), Some(targets[index].clone())))
            .collect();
        // A standalone scalar query is not a node; the operator sub-DAGs it
        // reads are planned like operator roots, against its entry (#511
        // §2.3, operator-sharing §3). They follow the operator roots, each
        // scalar's distinct references in `operator_refs` order. References
        // are distinct by structure, not pointer: front ends build identical
        // references as separate nodes, and planning merges them into one.
        let scalar_refs: Vec<Vec<Rc<OperatorNode>>> = workload
            .scalar_roots()
            .iter()
            .map(|(_, expr)| {
                let mut refs: Vec<Rc<OperatorNode>> = Vec::new();
                for node in expr.operator_refs() {
                    if !refs.iter().any(|r| same_reference(r, node)) {
                        refs.push(Rc::clone(node));
                    }
                }
                refs
            })
            .collect();
        // Each reference is planned against the entry's accuracy target, so a
        // combination of approximate values (e.g. a difference of two
        // quantiles) keeps its summary candidates. The combined guarantee is
        // not composed (it is `None`, #669), so the vector form's candidate is
        // kept but selected automatically only with evidence about its output:
        // a root guarantee that meets the target, or the deployment's
        // empirical accuracy model (plan selection, #576). In the MVP a rank-interval
        // guarantee counts as `None`. The same holds for scalar references inside Project/Filter
        // expressions, which are planned as interior sites.
        for ((index, _), refs) in workload.scalar_roots().iter().zip(&scalar_refs) {
            for node in refs {
                roots.push((*index, Rc::clone(node), Some(targets[*index].clone())));
            }
        }

        let space = search_workload_with_targets(roots, &strategies, models.accuracy)
            .map_err(OptimizeError::Search)?;

        let selection = space.global_selection(models.cost);

        // Assemble every root as a query result (an exact state is read
        // through its finalize, scalar references included: `scalar(v)` reads
        // an instant vector's value, not accumulator state), then intern
        // structurally identical summary producers across them once, so two
        // queries that selected the same `SummaryAgg` reach one `Rc`
        // (consumers dedupe states by pointer; a finalize wraps the shared
        // state).
        let mut assembled = Vec::with_capacity(space.roots.len());
        for (entry_index, root) in &space.roots {
            let dag = selection
                .assemble_selected_query(root)
                .map_err(|source| OptimizeError::Realization {
                    entry_index: *entry_index,
                    source,
                })?
                .ok_or_else(|| self.missing_group(*entry_index))?;
            assembled.push((*entry_index, dag));
        }
        let mut plans: Vec<QueryPlan> = share_common_sub_dags(assembled)
            .into_iter()
            .map(|(entry_index, root)| QueryPlan { entry_index, root })
            .collect();
        // Rebuild each scalar root over its planned sub-DAGs.
        let scalar_plans = plans.split_off(workload.exprs().len());
        let mut planned = scalar_plans.iter();
        let scalar_roots = workload
            .scalar_roots()
            .iter()
            .zip(&scalar_refs)
            .map(|((index, expr), refs)| {
                let selected: Vec<_> = planned.by_ref().take(refs.len()).collect();
                let expr = expr.map_operator_refs(&mut |node| {
                    let position = refs
                        .iter()
                        .position(|r| same_reference(r, node))
                        .expect("every reference was planned");
                    Rc::clone(&selected[position].root)
                });
                (*index, expr)
            })
            .collect();
        let mut output = PlanOutput::new(plans);
        output.scalar_roots = scalar_roots;
        output.scalar_plans = scalar_plans;
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

/// Whether two scalar references read the same operator sub-DAG.
fn same_reference(a: &Rc<OperatorNode>, b: &Rc<OperatorNode>) -> bool {
    Rc::ptr_eq(a, b) || a.as_ref() == b.as_ref()
}
