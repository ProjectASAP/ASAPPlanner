//! Reference execution demand for an already selected and timing-scoped graph.
//!
//! This applies the evaluator's existing rate policy, not a physical-legality
//! check or a complete cost model. Stage 2 must supply executable configurations.
//! In particular, mixed ingestion/query work must use separate candidate IDs,
//! as Stage 2's timing split does; this module rejects an unsplit candidate.
use std::collections::{BTreeSet, HashSet};

use asap_types::physical::Materialization;
use asap_types::workload::QueryWorkloadEntry;

use crate::cost::execution_demand::evaluation_rate;
use crate::search::candidates::graph::{
    validate_selected_graph, CandidateDependencyGraph, PhysicalCandidateId, SelectedCandidateGraph,
};
use crate::PlanSelectionSettings;

/// A computation boundary, not merely a rate: equal-rate materialized states
/// still trigger separate upstream computations when their inputs are not kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::search) enum BoundaryConsumer {
    Query(usize),
    Materialized(PhysicalCandidateId),
}

#[derive(Debug, Clone, PartialEq)]
pub(in crate::search) struct CandidateDemand {
    pub queries: Vec<usize>,
    pub consumers: Vec<BoundaryConsumer>,
    /// Same convention as the evaluator: one for a materialized candidate,
    /// otherwise the number of distinct immediate computation boundaries.
    pub multiplier: u64,
    pub executions_per_second: f64,
    pub at_ingestion: bool,
}

/// Results are indexed by candidate ID; inactive candidates have no demand.
/// `output_queries` maps every output reference to its workload query, so scalar
/// queries with several operator references are counted as one consumer.
/// Literal-only queries may have no references. `materializations` is `Some`
/// exactly for selected candidates, including `Some(NotMaterialized)`.
/// Materialization eligibility and accuracy still require downstream validation.
pub(in crate::search) fn derive_candidate_demand(
    graph: &CandidateDependencyGraph,
    selected: &SelectedCandidateGraph,
    materializations: &[Option<Materialization>],
    output_queries: &[usize],
    entries: &[QueryWorkloadEntry],
    settings: &PlanSelectionSettings,
) -> Result<Vec<Option<CandidateDemand>>, String> {
    graph.validate().map_err(|e| e.to_string())?;
    validate_selected_graph(graph, selected)?;
    let count = graph.candidates.len();
    let active: HashSet<_> = selected.selected_candidates.iter().map(|id| id.0).collect();
    if materializations.len() != count
        || materializations
            .iter()
            .enumerate()
            .any(|(id, mode)| mode.is_some() != active.contains(&id))
    {
        return Err(
            "materialization assignments must cover exactly the selected candidates".into(),
        );
    }
    if output_queries.len() != graph.query_roots.len()
        || output_queries.iter().any(|&query| query >= entries.len())
    {
        return Err("output references require valid workload query IDs".into());
    }
    let horizon = settings.non_repeating_query_horizon_seconds;
    if !horizon.is_finite() || horizon <= 0.0 {
        return Err("execution demand requires a finite positive accounting horizon".into());
    }
    let mut inputs = vec![Vec::new(); count];
    for connection in &selected.connections {
        inputs[connection.consumer.0].push(connection.producer.0);
    }
    let mut starts = vec![Vec::new(); entries.len()];
    for output in &selected.query_roots {
        starts[output_queries[output.query_root]].push(output.candidate.0);
    }
    let mut queries = vec![BTreeSet::new(); count];
    for (query, roots) in starts.iter().enumerate() {
        let mut pending = roots.clone();
        while let Some(id) = pending.pop() {
            if queries[id].insert(query) {
                pending.extend(&inputs[id]);
            }
        }
    }
    let is_materialized =
        |id: usize| materializations[id] != Some(Materialization::NotMaterialized);
    let ingestion = |id: usize| {
        matches!(
            materializations[id],
            Some(Materialization::MatAtIngestion(_))
        )
    };
    let mut rates = vec![0.0; count];
    for &id in &active {
        if is_materialized(id) {
            rates[id] = if ingestion(id) {
                1.0
            } else {
                let per_request = matches!(
                    materializations[id],
                    Some(Materialization::MatAtQueryKeptAcrossEvaluations(_))
                );
                evaluation_rate(
                    queries[id].iter().map(|&q| {
                        (
                            &entries[q].recurrence,
                            if per_request {
                                None
                            } else {
                                entries[q].range_evaluation
                            },
                        )
                    }),
                    horizon,
                )
                .map_err(|e| e.to_string())?
            };
        }
    }
    // Boundaries are identities. A per-boundary walk deduplicates diamond paths
    // and repeated output/input positions, but never two materialized instances.
    let mut boundaries = Vec::new();
    for (query, roots) in starts.into_iter().enumerate() {
        if !roots.is_empty() {
            let rate = evaluation_rate(
                [(&entries[query].recurrence, entries[query].range_evaluation)],
                horizon,
            )
            .map_err(|e| e.to_string())?;
            boundaries.push((BoundaryConsumer::Query(query), roots, rate, false));
        }
    }
    for &id in &active {
        if is_materialized(id) {
            boundaries.push((
                BoundaryConsumer::Materialized(PhysicalCandidateId(id)),
                inputs[id].clone(),
                rates[id],
                ingestion(id),
            ));
        }
    }
    let mut consumers = vec![BTreeSet::new(); count];
    let mut contexts: Vec<Option<bool>> = vec![None; count];
    // Stable boundary order keeps floating-point summation reproducible.
    boundaries.sort_by_key(|(boundary, _, _, _)| *boundary);
    for (boundary, mut pending, rate, at_ingestion) in boundaries {
        let mut visited = HashSet::new();
        while let Some(id) = pending.pop() {
            if !visited.insert(id) {
                continue;
            }
            consumers[id].insert(boundary);
            if is_materialized(id) {
                if at_ingestion && !ingestion(id) {
                    return Err(format!(
                        "ingestion consumer depends on query-materialized candidate {id}"
                    ));
                }
                continue;
            }
            if contexts[id].is_some_and(|previous| previous != at_ingestion) {
                return Err(format!(
                    "candidate {id} needs separate ingestion and query timing instances"
                ));
            }
            contexts[id] = Some(at_ingestion);
            rates[id] += rate;
            pending.extend(&inputs[id]);
        }
    }
    let mut demand = vec![None; count];
    for id in selected.selected_candidates.iter().map(|id| id.0) {
        if !rates[id].is_finite() || rates[id] < 0.0 {
            return Err(format!("candidate {id} has an invalid execution rate"));
        }
        demand[id] = Some(CandidateDemand {
            queries: queries[id].iter().copied().collect(),
            consumers: consumers[id].iter().copied().collect(),
            multiplier: if is_materialized(id) {
                1
            } else {
                consumers[id].len() as u64
            },
            executions_per_second: rates[id],
            at_ingestion: if is_materialized(id) {
                ingestion(id)
            } else {
                contexts[id].unwrap_or(false)
            },
        });
    }
    Ok(demand)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::candidates::graph::{
        PhysicalCandidate, ProducerRequirement, QueryRootCandidates,
    };
    use crate::search::milp::model::select_candidates;
    use asap_logical_asap_optimization::memo::MemoGroupId;
    use asap_logical_asap_optimization::test_support::entry_with;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::physical::Medium;
    use asap_types::workload::{
        QueryRecurrence, RangeEvaluation, RepeatedDemand, RepetitionInterval,
    };

    fn entry() -> QueryWorkloadEntry {
        entry_with(
            AccuracyTarget::exact(),
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(60_000))),
        )
    }

    fn graph(inputs: &[&[usize]], outputs: &[usize]) -> CandidateDependencyGraph {
        CandidateDependencyGraph {
            candidates: inputs
                .iter()
                .enumerate()
                .map(|(id, inputs)| PhysicalCandidate {
                    group: MemoGroupId(id as u32),
                    local_cost: 1.0,
                    producer_requirements: inputs
                        .iter()
                        .map(|&id| ProducerRequirement {
                            compatible_producers: vec![PhysicalCandidateId(id)],
                        })
                        .collect(),
                })
                .collect(),
            query_roots: outputs
                .iter()
                .map(|&id| QueryRootCandidates {
                    compatible_candidates: vec![PhysicalCandidateId(id)],
                })
                .collect(),
            ..Default::default()
        }
    }

    fn demand(
        graph: &CandidateDependencyGraph,
        modes: &[Materialization],
        outputs: &[usize],
        entries: &[QueryWorkloadEntry],
    ) -> Vec<Option<CandidateDemand>> {
        let selected = select_candidates(graph).unwrap().unwrap();
        derive_candidate_demand(
            graph,
            &selected,
            &modes.iter().copied().map(Some).collect::<Vec<_>>(),
            outputs,
            entries,
            &PlanSelectionSettings::default(),
        )
        .unwrap()
    }

    fn rate(demand: &[Option<CandidateDemand>], id: usize, expected: f64) {
        let actual = demand[id].as_ref().unwrap().executions_per_second;
        assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
    }

    // A shared materialization unions aligned readers; an unkept producer sums consumers.
    #[test]
    fn aligned_sharing_and_unmaterialized_recomputation_have_different_demand() {
        let graph = graph(&[&[], &[0], &[0]], &[1, 2]);
        let entries = [entry(), entry()];
        let plain = Materialization::NotMaterialized;
        let shared = demand(
            &graph,
            &[Materialization::MatAtQuery(Medium::Memory), plain, plain],
            &[0, 1],
            &entries,
        );
        rate(&shared, 0, 1.0 / 60.0);
        assert_eq!(shared[0].as_ref().unwrap().multiplier, 1);
        let separate = demand(&graph, &[plain; 3], &[0, 1], &entries);
        rate(&separate, 0, 2.0 / 60.0);
        assert_eq!(separate[0].as_ref().unwrap().multiplier, 2);
    }

    // Diamond paths and several scalar output references of one query count once.
    #[test]
    fn diamonds_and_multiple_output_references_deduplicate_one_query() {
        let graph = graph(&[&[], &[0], &[0], &[1, 2]], &[3, 1]);
        let actual = demand(
            &graph,
            &[Materialization::NotMaterialized; 4],
            &[0, 0],
            &[entry()],
        );
        rate(&actual, 0, 1.0 / 60.0);
        assert_eq!(
            actual[0].as_ref().unwrap().consumers,
            vec![BoundaryConsumer::Query(0)]
        );
        assert_eq!(actual[0].as_ref().unwrap().queries, vec![0]);
    }

    // Retained state is refreshed per request while readers still evaluate every range step.
    #[test]
    fn retained_state_uses_request_rate_instead_of_range_step_rate() {
        let graph = graph(&[&[], &[0]], &[1]);
        let mut entry = entry();
        entry.range_evaluation = Some(RangeEvaluation {
            step_ms: 60_000,
            span_ms: 600_000,
        });
        let actual = demand(
            &graph,
            &[
                Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory),
                Materialization::NotMaterialized,
            ],
            &[0],
            &[entry],
        );
        rate(&actual, 0, 1.0 / 60.0);
        rate(&actual, 1, 11.0 / 60.0);
    }

    // Equal rates and equal reader sets do not merge distinct materialized boundaries.
    #[test]
    fn equal_rate_materialized_candidates_remain_distinct_consumers() {
        let graph = graph(&[&[], &[0], &[0], &[1, 2]], &[3]);
        let kept = Materialization::MatAtQuery(Medium::Memory);
        let plain = Materialization::NotMaterialized;
        let actual = demand(&graph, &[plain, kept, kept, plain], &[0], &[entry()]);
        rate(&actual, 0, 2.0 / 60.0);
        assert_eq!(
            actual[0].as_ref().unwrap().consumers,
            vec![
                BoundaryConsumer::Materialized(PhysicalCandidateId(1)),
                BoundaryConsumer::Materialized(PhysicalCandidateId(2))
            ]
        );
    }

    // Ingestion uses the evaluator's one-second unit for a state and its unkept inputs.
    #[test]
    fn ingestion_state_propagates_ingestion_context() {
        let graph = graph(&[&[], &[0], &[1]], &[2]);
        let plain = Materialization::NotMaterialized;
        let actual = demand(
            &graph,
            &[
                plain,
                Materialization::MatAtIngestion(Medium::Memory),
                plain,
            ],
            &[0],
            &[entry()],
        );
        rate(&actual, 0, 1.0);
        rate(&actual, 1, 1.0);
        assert!(actual[0].as_ref().unwrap().at_ingestion);
        assert!(!actual[2].as_ref().unwrap().at_ingestion);
    }

    // Mixed timings need separate candidate IDs; ingestion cannot read query-maintained state.
    #[test]
    fn unsplit_timing_and_invalid_assignments_are_rejected() {
        let graph = graph(&[&[], &[0], &[0]], &[1, 2]);
        let selected = select_candidates(&graph).unwrap().unwrap();
        let entries = [entry(), entry()];
        let plain = Some(Materialization::NotMaterialized);
        let ingestion = Some(Materialization::MatAtIngestion(Medium::Memory));
        let derive = |modes: &[Option<Materialization>], outputs: &[usize]| {
            derive_candidate_demand(
                &graph,
                &selected,
                modes,
                outputs,
                &entries,
                &PlanSelectionSettings::default(),
            )
        };
        assert!(derive(&[plain, ingestion, plain], &[0, 1])
            .unwrap_err()
            .contains("separate ingestion and query"));
        assert!(derive(
            &[
                Some(Materialization::MatAtQuery(Medium::Memory)),
                ingestion,
                plain
            ],
            &[0, 1]
        )
        .unwrap_err()
        .contains("ingestion consumer"));
        assert!(derive(&[None, plain, plain], &[0, 1])
            .unwrap_err()
            .contains("assignments"));
        assert!(derive(&[plain; 3], &[0, 2])
            .unwrap_err()
            .contains("query IDs"));
    }
}
