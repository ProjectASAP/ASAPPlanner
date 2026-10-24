//! A bounded, validated memo baseline, independent of legacy search.
//!
//! This is not an optimal per-query planner: try a small prefix of local
//! Pass 1 assignments and three legal materialization policies. No full
//! workload/physical-assignment Cartesian product is materialized.

use asap_logical_asap_optimization::memo::LogicalMemo;
use asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidates;
use asap_physical_asap_optimization::{optimize_physical_asap_chosen, PhysicalCandidate};
use asap_types::ir::QueryRoot;
use asap_types::physical::Materialization;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::{brute_force, groups};
use crate::{PlanSelector, PlanningModels, SelectionError};

const MAX_LOGICAL_ATTEMPTS: usize = 16;

pub(crate) fn feasible_plan<Id: Clone>(
    initial: LogicalMemo,
    inventory: &PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<Option<(PhysicalCandidate, Vec<QueryRoot>)>, SelectionError> {
    // Pass 2 coupling configurations belong in the joint solve, not an
    // unbounded pre-solve search. Pass 1 candidates have no coupling sites.
    let memo = groups::populate_pass1(initial, inventory);
    let Some(choices) = brute_force::group_choices_up_to(&memo, MAX_LOGICAL_ATTEMPTS) else {
        return Ok(None);
    };
    let mut selector = PlanSelector::new(entries, data, models, false)?;
    let mut logical_roots = std::collections::HashMap::new();
    for (attempt, choice) in choices.iter().enumerate() {
        let chosen: Vec<_> = choice
            .iter()
            .enumerate()
            .map(|(group, candidate)| candidate.map(|c| memo.candidates[group][c].clone()))
            .collect();
        let Some(roots) = brute_force::composed_roots(&memo, inventory, &chosen) else {
            continue;
        };
        for policy in 0..3 {
            let id = format!("baseline-{attempt}-{policy}");
            let mut choose = |_: *const _, _: &[usize], options: &[Materialization]| {
                options
                    .iter()
                    .copied()
                    .find(|m| match policy {
                        1 => matches!(m, Materialization::MatAtIngestion(_)),
                        2 => matches!(m, Materialization::MatAtQuery(_)),
                        _ => false,
                    })
                    .or_else(|| options.first().copied())
                    .unwrap_or(Materialization::NotMaterialized)
            };
            let Ok(Some(candidate)) = optimize_physical_asap_chosen(
                &id,
                &roots,
                entries,
                data,
                models.deployment_capabilities,
                &mut choose,
            ) else {
                continue;
            };
            if selector.offer(&candidate).is_ok() && selector.keeps_from_logical(&id) {
                logical_roots.insert(id, roots.clone());
            }
        }
    }
    match selector.finish() {
        Ok((_, candidate)) => {
            let roots = logical_roots
                .remove(&candidate.from_logical)
                .expect("the validated incumbent retains its logical roots");
            Ok(Some((candidate, roots)))
        }
        Err(SelectionError::NoValidCandidate(_)) => Ok(None),
        Err(error) => Err(error),
    }
}
