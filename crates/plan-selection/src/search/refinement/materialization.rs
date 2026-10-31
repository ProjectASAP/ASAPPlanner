//! Restricted proposals over the same finite catalog, preserving computations
//! and producer bindings while changing one query-time materialization choice.
use crate::search::candidates::dag::DagOfPhysicalCandidatesWithCosts;
use crate::search::candidates::dag::PhysicalCandidatesWithCosts;
use crate::search::candidates::dag::ReaderConstraint;
use crate::search::candidates::graph::SelectedCandidateGraph;
use asap_types::physical::Materialization;
use asap_types::stages::PhysicalCandidateId;

pub(super) fn counterparts(
    configured: &PhysicalCandidatesWithCosts,
    deadline: Option<std::time::Instant>,
) -> Vec<(PhysicalCandidateId, PhysicalCandidateId)> {
    let expired = || deadline.is_some_and(|limit| std::time::Instant::now() >= limit);
    let mut materialized = std::collections::HashMap::new();
    for (id, configuration) in configured.physical.candidates.iter().enumerate() {
        if expired() {
            break;
        }
        if matches!(
            configuration.materialization,
            Materialization::MatAtQuery(_)
        ) {
            materialized
                .entry((configuration.operator_node, configuration.timing))
                .or_insert(PhysicalCandidateId(id));
        }
    }
    let mut changes = Vec::new();
    for (from, current) in configured.physical.candidates.iter().enumerate() {
        if expired() {
            break;
        }
        if current.materialization == Materialization::NotMaterialized {
            if let Some(&to) = materialized.get(&(current.operator_node, current.timing)) {
                changes.push((PhysicalCandidateId(from), to));
            }
        }
    }
    changes
}

pub(super) fn restrict(
    graph: &DagOfPhysicalCandidatesWithCosts,
    selected: &SelectedCandidateGraph,
    from: PhysicalCandidateId,
    to: PhysicalCandidateId,
    deadline: std::time::Instant,
) -> Option<DagOfPhysicalCandidatesWithCosts> {
    if std::time::Instant::now() >= deadline
        || !selected.selected_candidates.contains(&from)
        || selected.selected_candidates.contains(&to)
    {
        return None;
    }
    let replace = |id| if id == from { to } else { id };
    let mut active = vec![false; graph.candidates.len()];
    for &id in &selected.selected_candidates {
        active[replace(id).0] = true;
    }
    let bindings: std::collections::HashMap<_, _> = selected
        .connections
        .iter()
        .map(|edge| {
            (
                (replace(edge.consumer), edge.requirement),
                replace(edge.producer),
            )
        })
        .collect();
    if std::time::Instant::now() >= deadline {
        return None;
    }
    let mut restricted = graph.clone();
    for (id, candidate) in restricted.candidates.iter_mut().enumerate() {
        if std::time::Instant::now() >= deadline {
            return None;
        }
        if !active[id] {
            candidate.producer_candidates_per_input.clear();
            restricted
                .reader_constraints
                .push(ReaderConstraint::AllowedOutputs {
                    candidate: PhysicalCandidateId(id),
                    outputs: Vec::new(),
                });
            continue;
        }
        for (position, requirement) in candidate
            .producer_candidates_per_input
            .iter_mut()
            .enumerate()
        {
            let producer = *bindings.get(&(PhysicalCandidateId(id), position))?;
            if !requirement.candidates.contains(&producer) {
                return None;
            }
            requirement.candidates = vec![producer].into();
        }
    }
    for root in &selected.query_roots {
        let candidate = replace(root.candidate);
        let suppliers = &mut restricted.query_roots[root.query_root].candidates;
        if !suppliers.contains(&candidate) {
            return None;
        }
        *suppliers = vec![candidate];
    }
    Some(restricted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::candidates::dag::{
        InputProducerCandidates, PhysicalCandidateWithCost, QueryOutputCandidates,
    };
    use crate::search::refinement::search::{
        search_candidates_with_restricted_feedback, CandidateEvaluation, CandidateSearchLimits,
        CandidateSearchStatus,
    };

    use std::time::Duration;

    fn graph(readers: usize) -> DagOfPhysicalCandidatesWithCosts {
        let mut graph = DagOfPhysicalCandidatesWithCosts::default();
        for _ in 0..2 {
            graph.candidates.push(PhysicalCandidateWithCost {
                local_cost: 0.0,
                producer_candidates_per_input: vec![],
            });
        }
        for query in 0..readers {
            graph.candidates.push(PhysicalCandidateWithCost {
                local_cost: 0.0,
                producer_candidates_per_input: vec![InputProducerCandidates {
                    candidates: vec![PhysicalCandidateId(0), PhysicalCandidateId(1)].into(),
                }],
            });
            graph.query_roots.push(QueryOutputCandidates {
                candidates: vec![PhysicalCandidateId(query + 2)],
            });
        }
        graph
            .reader_constraints
            .push(ReaderConstraint::MinimumQueryReaders {
                candidate: PhysicalCandidateId(1),
                query_outputs: (0..readers).map(|query| vec![query]).collect(),
                minimum: 2,
            });
        graph
    }

    fn run(
        readers: usize,
        budget: usize,
        cancel_neighbor: bool,
    ) -> (super::super::search::CandidateSearchResult, Vec<usize>) {
        let graph = graph(readers);
        let weights: Vec<_> = (0..graph.candidates.len())
            .map(|id| if id == 1 { 20.0 } else { 1.0 })
            .collect();
        let mut visited = Vec::new();
        let result = search_candidates_with_restricted_feedback(
            &graph,
            CandidateSearchLimits {
                max_refinements: budget,
                wall_time: Duration::from_secs(5),
            },
            &weights,
            &[],
            &[(PhysicalCandidateId(0), PhysicalCandidateId(1))],
            |selected| {
                let materialized = selected
                    .selected_candidates
                    .contains(&PhysicalCandidateId(1));
                visited.push(usize::from(materialized));
                if materialized && cancel_neighbor {
                    return Ok(CandidateEvaluation {
                        budget_exhausted: true,
                        ..Default::default()
                    });
                }
                // Materializing the producer avoids repeated upstream work;
                // local certified costs deliberately cannot express that saving.
                Ok(Some(if materialized { 10.0 } else { 30.0 }).into())
            },
        )
        .unwrap();
        (result, visited)
    }

    #[test]
    fn materialization_proposal_improves_cost_without_global_bound() {
        let (result, visited) = run(2, 2, false);
        assert_eq!(visited, vec![0, 1]);
        assert_eq!(result.incumbent.unwrap().actual_cost, 10.0);
        assert_eq!(result.lower_bound, 0.0);
        assert_eq!(result.refinements, 2);
        assert_eq!(result.solve_metrics.solve_calls, 2);
        assert_eq!(result.status, CandidateSearchStatus::RefinementLimit);
    }

    #[test]
    fn materialization_proposal_preserves_reader_legality_and_budget() {
        let (single, visited) = run(1, 2, false);
        assert!(visited.iter().all(|&id| id == 0));
        assert_eq!(single.incumbent.unwrap().actual_cost, 30.0);
        let (limited, visited) = run(2, 1, false);
        assert_eq!(visited, vec![0]);
        assert_eq!(limited.refinements, 1);
        assert_eq!(limited.solve_metrics.solve_calls, 1);
    }

    #[test]
    fn canceled_materialization_validation_keeps_validated_incumbent() {
        let (result, visited) = run(2, 2, true);
        assert_eq!(visited, vec![0, 1]);
        assert_eq!(result.incumbent.unwrap().actual_cost, 30.0);
        assert_eq!(result.status, CandidateSearchStatus::TimeLimit);
        assert_eq!(result.certified_query_cuts, 0);
    }
}
