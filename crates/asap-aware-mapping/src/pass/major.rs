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

use super::{OptimizationInput, OptimizationPass, OptimizeError, PlanOutput, QueryLifecyclePlan};
use crate::replacement::{default_strategies_with_models, search_workload_with_targets};
use crate::summary_maintenance_lifecycle::{
    global_selection_with_summary_maintenance_lifecycles, plan_assembled_dag, shared_state_cost,
    summary_states, WorkloadDemand,
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
        let strategies =
            default_strategies_with_models(models.cost, models.accuracy, models.evidence);

        // `Id` is the entry's position in `QueryWorkload::entries()`, so the
        // search result carries the workload binding the lifecycle stage and
        // the output both need. CSE may make two identical queries share one
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

        let lifecycle = input.lifecycle;

        // One index per root, in `CandidateLogicalASAPDAGs::roots` order — which is the order
        // the roots went in, which is `entries()` order.
        let entry_indices = workload.operator_indices().to_vec();
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
        // Each root's lifecycle is planned against the entries that consume
        // it — the same binding selection costed it with — not the whole
        // workload, so one query's reads never amortize another's state.
        let bindings = space
            .workload_entries_by_target(demand.workload, &entry_indices)
            .map_err(|error| OptimizeError::LifecycleSelection(error.into()))?;

        // Assemble every root, then intern structurally identical summary
        // producers across them once, so two queries that selected the same
        // `SummaryAgg` reach one `Rc` (consumers dedupe states by pointer).
        let mut assembled = Vec::with_capacity(space.roots.len());
        for (entry_index, root) in &space.roots {
            let dag = selection
                .assemble_selected_dag(root)
                .map_err(|source| OptimizeError::LifecycleAssembly {
                    entry_index: *entry_index,
                    source: source.into(),
                })?
                .ok_or_else(|| self.missing_group(*entry_index))?;
            assembled.push(dag);
        }
        let interned = share_common_sub_dags(assembled.iter().cloned().enumerate().collect());
        let states: Vec<_> = interned
            .iter()
            .map(|(_, dag)| summary_states(dag))
            .collect();

        // A state reached from several roots is planned once against all of
        // their entries, in every plan that reaches it, so each plan picks
        // the same lifecycle for it. When that union cannot be costed the
        // roots keep their own, unshared DAG and entries.
        let mut shared_entries: Vec<(Rc<OperatorNode>, Option<Vec<usize>>)> = Vec::new();
        for (position, (entry_index, root)) in space.roots.iter().enumerate() {
            for state in &states[position] {
                if shared_entries.iter().any(|(s, _)| Rc::ptr_eq(s, state)) {
                    continue;
                }
                let readers: Vec<_> = (0..space.roots.len())
                    .filter(|&other| states[other].iter().any(|s| Rc::ptr_eq(s, state)))
                    .map(|other| &space.roots[other].1)
                    .collect();
                if readers.iter().all(|reader| Rc::ptr_eq(reader, root)) {
                    continue;
                }
                let mut entries: Vec<usize> = readers
                    .iter()
                    .flat_map(|reader| bindings[&Rc::as_ptr(reader)].iter().copied())
                    .collect();
                entries.sort_unstable();
                entries.dedup();
                let cost = shared_state_cost(
                    state,
                    WorkloadDemand {
                        entry_indices: &entries,
                        ..demand
                    },
                    lifecycle.now_ms,
                    lifecycle.horizon,
                    lifecycle.capabilities,
                    models.cost,
                )
                .map_err(|source| OptimizeError::LifecycleAssembly {
                    entry_index: *entry_index,
                    source: source.into(),
                })?;
                shared_entries.push((Rc::clone(state), cost.map(|_| entries)));
            }
        }

        let mut plans = Vec::with_capacity(space.roots.len());
        for (position, (entry_index, root)) in space.roots.iter().enumerate() {
            let mut entries = bindings[&Rc::as_ptr(root)].clone();
            let mut dag = Rc::clone(&interned[position].1);
            for (state, shared) in &shared_entries {
                if !states[position].iter().any(|s| Rc::ptr_eq(s, state)) {
                    continue;
                }
                match shared {
                    Some(shared) => entries.extend(shared),
                    None => {
                        entries = bindings[&Rc::as_ptr(root)].clone();
                        dag = Rc::clone(&assembled[position]);
                        break;
                    }
                }
            }
            entries.sort_unstable();
            entries.dedup();
            let plan = plan_assembled_dag(
                dag,
                root,
                WorkloadDemand {
                    entry_indices: &entries,
                    ..demand
                },
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
                plan,
            });
        }
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
