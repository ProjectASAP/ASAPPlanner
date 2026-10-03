//! Exhaustive workload search. Unknown costs and budget exhaustion are explicit.

use std::{collections::HashMap, rc::Rc};

use asap_types::ir::{cse::share_common_sub_dags, OperatorNode};

use super::{OptimizationInput, OptimizationPass, OptimizeError, PlanOutput, QueryLifecyclePlan};
use crate::{
    replacement::{default_strategies_with_models, search_workload_with_targets},
    summary_maintenance_lifecycle::{
        enumerate_assembled_plans, SummaryMaintenanceLifecyclePlan, WorkloadDemand,
    },
    window_composition::enumerate_window_compositions,
};

/// Strict complete-cost alternative to `MajorPass`. A distinct pass is needed
/// because the legacy default supports rank-only models, which cannot certify
/// the cheapest complete workload. No heuristic result is returned by this pass.
#[derive(Debug, Clone, Copy)]
pub struct CompletePass {
    /// Maximum inventory/assignment count at each exhaustive stage. Exceeding
    /// it aborts the search, even if a priced candidate was already discovered.
    pub max_candidates: usize,
}

impl Default for CompletePass {
    fn default() -> Self {
        Self {
            max_candidates: 65_536,
        }
    }
}

/// Reviewable inventory, including explanations for invalid/unpriced assignments.
#[derive(Debug)]
pub struct CompleteWorkloadInventory {
    pub candidates: Vec<PlanOutput>,
    pub rejected: Vec<String>,
}

fn failure(message: impl ToString) -> OptimizeError {
    OptimizeError::CompleteSelection(message.to_string())
}

fn product(options: &[usize], limit: usize) -> Result<usize, OptimizeError> {
    if limit == 0 {
        return Err(failure("complete search requires a positive budget"));
    }
    options
        .iter()
        .try_fold(1usize, |n, m| n.checked_mul(*m))
        .filter(|n| *n <= limit)
        .ok_or_else(|| {
            failure(
                "complete workload search exceeds candidate budget; no partial inventory returned",
            )
        })
}

impl CompletePass {
    /// Enumerate logical choices, exact pane compositions, independent/partial
    /// state-sharing partitions and all compatible lifecycle assignments.
    pub fn enumerate(
        &self,
        input: OptimizationInput<'_>,
    ) -> Result<CompleteWorkloadInventory, OptimizeError> {
        input.validate()?;
        let models = input.models;
        let workload = input.workload;
        let entries: Vec<_> = workload.query_workload().entries().collect();
        let strategies =
            default_strategies_with_models(models.cost, models.accuracy, models.evidence);
        let roots = workload
            .entries()
            .zip(workload.operator_indices().iter().copied())
            .map(|((entry, root), id)| {
                (
                    id,
                    Rc::clone(root),
                    Some(entry.requirements.accuracy.target()),
                )
            })
            .collect();
        let space = search_workload_with_targets(roots, &strategies, models.accuracy);
        let logical = space
            .enumerate_candidate_dags(self.max_candidates)
            .map_err(failure)?;
        let mut inventory = CompleteWorkloadInventory {
            candidates: Vec::new(),
            rejected: logical.rejected_assemblies,
        };
        let mut assignments = 0usize;
        let mut assemblies = 0usize;
        for logical in logical.candidates {
            let windows = logical
                .iter()
                .map(|(id, root)| {
                    enumerate_window_compositions(root, &entries[*id], self.max_candidates)
                        .map_err(failure)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let count = product(
                &windows.iter().map(Vec::len).collect::<Vec<_>>(),
                self.max_candidates,
            )?;
            for mut ordinal in 0..count {
                let roots = windows
                    .iter()
                    .enumerate()
                    .map(|(index, choices)| {
                        let root = Rc::clone(&choices[ordinal % choices.len()]);
                        ordinal /= choices.len();
                        (logical[index].0, root)
                    })
                    .collect();
                for roots in sharing_partitions(roots, self.max_candidates)? {
                    assemblies += 1;
                    if assemblies > self.max_candidates {
                        return Err(failure("complete workload assembly budget exceeded; no partial inventory returned"));
                    }
                    let mut state_entries = HashMap::new();
                    // Lifecycle enumeration also collects maintained populations;
                    // bind every reachable node so those states get exact demand.
                    for (id, root) in &roots {
                        for node in OperatorNode::reachable(root) {
                            let indices: &mut Vec<usize> =
                                state_entries.entry(Rc::as_ptr(&node)).or_default();
                            if !indices.contains(id) {
                                indices.push(*id);
                            }
                        }
                    }
                    let mut choices = Vec::new();
                    let mut invalid = false;
                    for (index, (id, root)) in roots.iter().enumerate() {
                        match enumerate_assembled_plans(
                            Rc::clone(root),
                            &space.roots[index].1,
                            WorkloadDemand {
                                workload: workload.query_workload(),
                                data_workload: workload.data_workload(),
                                entry_indices: &[*id],
                            },
                            &state_entries,
                            input.lifecycle.now_ms,
                            input.lifecycle.horizon,
                            models.capabilities,
                            models.cost,
                            self.max_candidates,
                        ) {
                            Ok(plans) => choices.push(plans),
                            Err(reason) if reason.contains("budget") => {
                                return Err(failure(reason))
                            }
                            Err(reason) => {
                                inventory.rejected.push(reason);
                                invalid = true;
                                break;
                            }
                        }
                    }
                    if invalid {
                        continue;
                    }
                    let count = product(
                        &choices.iter().map(Vec::len).collect::<Vec<_>>(),
                        self.max_candidates,
                    )?;
                    assignments = assignments.checked_add(count).filter(|n| *n <= self.max_candidates)
                        .ok_or_else(|| failure("complete workload assignment budget exceeded; no partial inventory returned"))?;
                    for mut ordinal in 0..count {
                        let plans: Vec<_> = choices
                            .iter()
                            .map(|choices| {
                                let plan = choices[ordinal % choices.len()].clone();
                                ordinal /= choices.len();
                                plan
                            })
                            .collect();
                        if !compatible(&plans)
                            || plans.iter().any(|plan| plan.execution_timed_dag().is_err())
                        {
                            inventory.rejected.push(
                                "shared state has inconsistent lifecycle/window assignment".into(),
                            );
                            continue;
                        }
                        let Some(cost) = models
                            .cost
                            .complete_workload_candidate_cost(&plans, workload.scalar_roots())
                        else {
                            inventory
                                .rejected
                                .push("complete workload cost is unknown".into());
                            continue;
                        };
                        if !cost.0.is_finite() || cost.0 < 0.0 {
                            inventory
                                .rejected
                                .push("complete workload cost is invalid".into());
                            continue;
                        }
                        let plans = plans
                            .into_iter()
                            .zip(&roots)
                            .map(|(plan, (id, _))| QueryLifecyclePlan {
                                entry_index: *id,
                                plan,
                            })
                            .collect();
                        let mut output = PlanOutput::new(plans);
                        output.scalar_roots = workload.scalar_roots().to_vec();
                        output.workload_total_cost = Some(cost);
                        inventory.candidates.push(output);
                    }
                }
            }
        }
        inventory.rejected.sort();
        inventory.rejected.dedup();
        Ok(inventory)
    }
}

impl OptimizationPass for CompletePass {
    fn name(&self) -> &'static str {
        "complete"
    }

    fn optimize(&self, input: OptimizationInput<'_>) -> Result<PlanOutput, OptimizeError> {
        let inventory = self.enumerate(input)?;
        inventory
            .candidates
            .into_iter()
            .min_by(|a, b| {
                a.workload_total_cost
                    .unwrap()
                    .0
                    .total_cmp(&b.workload_total_cost.unwrap().0)
            })
            .ok_or_else(|| {
                failure(format!(
                    "no feasible priced complete workload: {}",
                    inventory.rejected.join("; ")
                ))
            })
    }
}

fn compatible(plans: &[SummaryMaintenanceLifecyclePlan]) -> bool {
    let mut assignments = HashMap::new();
    for plan in plans {
        for deployment in &plan.deployments {
            let assignment = (
                &deployment.summary_maintenance_lifecycle_guarantee,
                &deployment.selected_window_framework,
            );
            if assignments
                .insert(Rc::as_ptr(&deployment.summary), assignment)
                .is_some_and(|previous| previous != assignment)
            {
                return false;
            }
        }
    }
    true
}

/// Enumerate every set partition for each structurally identical state class.
/// Parents are rebuilt per reader so sharing an estimate cannot accidentally
/// force its producer to be shared after choosing independent states.
type WorkloadRoots = Vec<(usize, Rc<OperatorNode>)>;

fn sharing_partitions(
    roots: WorkloadRoots,
    limit: usize,
) -> Result<Vec<WorkloadRoots>, OptimizeError> {
    let roots = share_common_sub_dags(roots);
    let mut classes: Vec<(Rc<OperatorNode>, Vec<usize>)> = Vec::new();
    for (reader, (_, root)) in roots.iter().enumerate() {
        for node in OperatorNode::reachable(root) {
            if let Some((_, readers)) = classes
                .iter_mut()
                .find(|(state, _)| Rc::ptr_eq(state, &node))
            {
                if !readers.contains(&reader) {
                    readers.push(reader);
                }
            } else {
                classes.push((node, vec![reader]));
            }
        }
    }
    classes.retain(|(_, readers)| readers.len() > 1);
    fn partitions(n: usize, limit: usize) -> Result<Vec<Vec<usize>>, OptimizeError> {
        let mut result = vec![vec![0]];
        for _ in 1..n {
            let mut next = Vec::new();
            for partition in result {
                for group in 0..=partition.iter().max().unwrap() + 1 {
                    let mut choice = partition.clone();
                    choice.push(group);
                    next.push(choice);
                    if next.len() > limit {
                        return Err(failure("state-sharing partition budget exceeded"));
                    }
                }
            }
            result = next;
        }
        Ok(result)
    }
    let partitions = classes
        .iter()
        .map(|(_, readers)| partitions(readers.len(), limit))
        .collect::<Result<Vec<_>, _>>()?;
    let count = product(&partitions.iter().map(Vec::len).collect::<Vec<_>>(), limit)?;
    let mut result = Vec::new();
    for mut ordinal in 0..count {
        let selected: Vec<_> = partitions
            .iter()
            .map(|partitions| {
                let choice = &partitions[ordinal % partitions.len()];
                ordinal /= partitions.len();
                choice
            })
            .collect();
        let mut memo = HashMap::new();
        fn rebuild(
            node: &Rc<OperatorNode>,
            reader: usize,
            classes: &[(Rc<OperatorNode>, Vec<usize>)],
            selected: &[&Vec<usize>],
            memo: &mut HashMap<(*const OperatorNode, usize), Rc<OperatorNode>>,
        ) -> Rc<OperatorNode> {
            let group = classes
                .iter()
                .enumerate()
                .find_map(|(index, (state, readers))| {
                    Rc::ptr_eq(state, node).then(|| {
                        selected[index][readers.iter().position(|r| *r == reader).unwrap()]
                    })
                });
            let key = (Rc::as_ptr(node), group.map_or(reader, roots_namespace));
            if let Some(node) = memo.get(&key) {
                return Rc::clone(node);
            }
            let mut rebuilt = (**node).clone();
            rebuilt.operator = node
                .operator
                .map_children(|child| rebuild(child, reader, classes, selected, memo));
            let rebuilt = Rc::new(rebuilt);
            memo.insert(key, Rc::clone(&rebuilt));
            rebuilt
        }
        let rebuilt = roots
            .iter()
            .enumerate()
            .map(|(reader, (id, node))| {
                (*id, rebuild(node, reader, &classes, &selected, &mut memo))
            })
            .collect();
        result.push(rebuilt);
    }
    Ok(result)
}

fn roots_namespace(group: usize) -> usize {
    usize::MAX - group
}
