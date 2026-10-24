//! Finite, unpriced execution configurations over typed immediate candidates.
//!
//! This does not enumerate consumer subsets. Current summary eligibility is a
//! conjunction of individual-reader restrictions and pairwise interval/phase
//! compatibility; selected-reader constraints encode those conditions. Complete
//! Stage 2 validation still checks operator legality and upstream timing.
//! Non-summary materialization at highest-shared boundaries is not generated.
use std::rc::Rc;

use asap_logical_asap_optimization::deployment_capabilities::DeploymentCapabilities;
use asap_physical_asap_optimization::summary_materialization_eligibility;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{ASAPOp, OperatorNode};
use asap_types::physical::{Materialization, Medium};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::candidate_connections::{
    CandidateDependencyGraph, PhysicalCandidate, PhysicalCandidateId, ProducerRequirement,
    QueryRootCandidates, ReaderConstraint,
};
use super::candidate_inventory::CandidateInventory;

#[derive(Debug, Clone, Copy)]
pub(super) struct CandidateConfiguration {
    pub source_candidate: PhysicalCandidateId,
    pub materialization: Materialization,
    pub timing: ExecutionTiming,
}

#[derive(Debug)]
pub(super) struct CandidateConfigurationInventory {
    /// Costs are zero placeholders. Price/configure resource models before
    /// treating a selected solution as a deployment-cost optimization result.
    pub inventory: CandidateInventory,
    pub configurations: Vec<CandidateConfiguration>,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ConfigurationError {
    #[error("invalid source candidate inventory: {0}")]
    InvalidInventory(String),
    #[error("source resource constraints require configuration-specific accounting")]
    UnpricedResources,
    #[error("candidate {candidate} already has timing or materialization assigned")]
    AssignedExecution { candidate: usize },
}

/// `output_queries[reference]` identifies the owning workload query. Multiple
/// references of one scalar query count once for shared-query materialization.
/// Existing costs are intentionally not copied: timing changes their demand.
pub(super) fn enumerate_candidate_configurations(
    source: &CandidateInventory,
    output_queries: &[usize],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    deployment: &DeploymentCapabilities,
) -> Result<CandidateConfigurationInventory, ConfigurationError> {
    let invalid = |message: &str| ConfigurationError::InvalidInventory(message.into());
    source
        .graph
        .validate()
        .map_err(|error| invalid(&error.to_string()))?;
    if !source.graph.resource_budgets.is_empty() {
        return Err(ConfigurationError::UnpricedResources);
    }
    if source.templates.len() != source.graph.candidates.len()
        || output_queries.len() != source.graph.query_roots.len()
        || output_queries.iter().any(|&query| query >= entries.len())
    {
        return Err(invalid("template or output/query mapping is incomplete"));
    }
    if source
        .group_candidates
        .iter()
        .flatten()
        .any(|id| id.0 >= source.graph.candidates.len())
    {
        return Err(invalid("group references missing source candidate"));
    }
    let mut query_outputs = vec![Vec::new(); entries.len()];
    let mut potential = vec![vec![false; entries.len()]; source.graph.candidates.len()];
    for (output, &query) in output_queries.iter().enumerate() {
        query_outputs[query].push(output);
        let mut pending = source.graph.query_roots[output]
            .compatible_candidates
            .clone();
        while let Some(candidate) = pending.pop() {
            if !potential[candidate.0][query] {
                potential[candidate.0][query] = true;
                pending.extend(
                    source.graph.candidates[candidate.0]
                        .producer_requirements
                        .iter()
                        .flat_map(|input| input.compatible_producers.iter().copied()),
                );
            }
        }
    }
    let mut result = CandidateConfigurationInventory {
        inventory: CandidateInventory {
            graph: CandidateDependencyGraph::default(),
            templates: Vec::new(),
            group_candidates: vec![Vec::new(); source.group_candidates.len()],
        },
        configurations: Vec::new(),
    };
    let mut source_configurations = vec![Vec::new(); source.graph.candidates.len()];
    for (source_id, source_candidate) in source.graph.candidates.iter().enumerate() {
        let node = source.templates[source_id]
            .as_ref()
            .ok_or_else(|| invalid("candidate has no immediate operator template"))?;
        if node.timing.is_some() || node.materialization != Materialization::NotMaterialized {
            return Err(ConfigurationError::AssignedExecution {
                candidate: source_id,
            });
        }
        let summary = matches!(
            node.asap(),
            Some(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })
        );
        let forced_query = matches!(
            node.asap(),
            Some(
                ASAPOp::SummaryEstimate { .. }
                    | ASAPOp::EvaluatePopulation { .. }
                    | ASAPOp::MaintainPopulation { .. }
            )
        );
        let queries = potential[source_id]
            .iter()
            .enumerate()
            .filter_map(|(query, reached)| reached.then_some(query))
            .collect::<Vec<_>>();
        let mut modes = vec![(Materialization::NotMaterialized, ExecutionTiming::QueryTime)];
        if summary {
            modes.push((
                Materialization::MatAtQuery(Medium::Memory),
                ExecutionTiming::QueryTime,
            ));
            if queries.iter().any(|&query| {
                summary_mode_allowed(
                    node,
                    &[query],
                    Materialization::MatAtIngestion(Medium::Memory),
                    entries,
                    data,
                    deployment,
                )
            }) {
                modes.push((
                    Materialization::MatAtIngestion(Medium::Memory),
                    ExecutionTiming::IngestionTime,
                ));
            }
            if matches!(node.asap(), Some(ASAPOp::PaneChain { .. }))
                && queries.iter().any(|&query| {
                    summary_mode_allowed(
                        node,
                        &[query],
                        Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory),
                        entries,
                        data,
                        deployment,
                    )
                })
            {
                modes.push((
                    Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory),
                    ExecutionTiming::QueryTime,
                ));
            }
        } else if !forced_query {
            modes.push((
                Materialization::NotMaterialized,
                ExecutionTiming::IngestionTime,
            ));
        }
        for (materialization, timing) in modes {
            let id = PhysicalCandidateId(result.configurations.len());
            source_configurations[source_id].push(id);
            result.configurations.push(CandidateConfiguration {
                source_candidate: PhysicalCandidateId(source_id),
                materialization,
                timing,
            });
            result.inventory.templates.push(Some(Rc::clone(node)));
            result.inventory.graph.candidates.push(PhysicalCandidate {
                group: source_candidate.group,
                local_cost: 0.0,
                producer_requirements: Vec::new(),
            });
            if summary {
                let eligible = queries
                    .iter()
                    .copied()
                    .filter(|&query| {
                        summary_mode_allowed(
                            node,
                            &[query],
                            materialization,
                            entries,
                            data,
                            deployment,
                        )
                    })
                    .collect::<Vec<_>>();
                let outputs = eligible
                    .iter()
                    .flat_map(|&query| query_outputs[query].iter().copied())
                    .collect();
                result
                    .inventory
                    .graph
                    .reader_constraints
                    .push(ReaderConstraint::AllowedOutputs {
                        candidate: id,
                        outputs,
                    });
                if materialization == Materialization::MatAtQuery(Medium::Memory) {
                    result.inventory.graph.reader_constraints.push(
                        ReaderConstraint::MinimumQueryReaders {
                            candidate: id,
                            query_outputs: query_outputs.clone(),
                            minimum: 2,
                        },
                    );
                }
                // The current Stage 2 rules require common intervals/phases,
                // whose conflicts are pairwise. No arbitrary set predicate is
                // approximated by this encoding.
                for (position, &left) in eligible.iter().enumerate() {
                    for &right in &eligible[position + 1..] {
                        if !summary_mode_allowed(
                            node,
                            &[left, right],
                            materialization,
                            entries,
                            data,
                            deployment,
                        ) {
                            for &left_output in &query_outputs[left] {
                                for &right_output in &query_outputs[right] {
                                    result.inventory.graph.reader_constraints.push(
                                        ReaderConstraint::IncompatibleOutputs {
                                            candidate: id,
                                            left: left_output,
                                            right: right_output,
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    for (id, configuration) in result.configurations.iter().enumerate() {
        result.inventory.graph.candidates[id].producer_requirements = source.graph.candidates
            [configuration.source_candidate.0]
            .producer_requirements
            .iter()
            .map(|input| ProducerRequirement {
                compatible_producers: input
                    .compatible_producers
                    .iter()
                    .flat_map(|source| source_configurations[source.0].iter().copied())
                    .filter(|producer| {
                        compatible_timing(configuration, &result.configurations[producer.0])
                    })
                    .collect(),
            })
            .collect();
    }
    for (group, candidates) in source.group_candidates.iter().enumerate() {
        result.inventory.group_candidates[group] = candidates
            .iter()
            .flat_map(|source| source_configurations[source.0].iter().copied())
            .collect();
    }
    for output in &source.graph.query_roots {
        result
            .inventory
            .graph
            .query_roots
            .push(QueryRootCandidates {
                compatible_candidates: output
                    .compatible_candidates
                    .iter()
                    .flat_map(|source| source_configurations[source.0].iter().copied())
                    .filter(|id| result.configurations[id.0].timing == ExecutionTiming::QueryTime)
                    .collect(),
            });
    }
    for constraint in &source.graph.reader_constraints {
        let source_candidate = match constraint {
            ReaderConstraint::AllowedOutputs { candidate, .. }
            | ReaderConstraint::IncompatibleOutputs { candidate, .. }
            | ReaderConstraint::MinimumQueryReaders { candidate, .. } => candidate,
        };
        for &candidate in &source_configurations[source_candidate.0] {
            result
                .inventory
                .graph
                .reader_constraints
                .push(match constraint {
                    ReaderConstraint::AllowedOutputs { outputs, .. } => {
                        ReaderConstraint::AllowedOutputs {
                            candidate,
                            outputs: outputs.clone(),
                        }
                    }
                    ReaderConstraint::IncompatibleOutputs { left, right, .. } => {
                        ReaderConstraint::IncompatibleOutputs {
                            candidate,
                            left: *left,
                            right: *right,
                        }
                    }
                    ReaderConstraint::MinimumQueryReaders {
                        query_outputs,
                        minimum,
                        ..
                    } => ReaderConstraint::MinimumQueryReaders {
                        candidate,
                        query_outputs: query_outputs.clone(),
                        minimum: *minimum,
                    },
                });
        }
    }
    result
        .inventory
        .graph
        .validate()
        .map_err(|error| invalid(&error.to_string()))?;
    Ok(result)
}

fn compatible_timing(consumer: &CandidateConfiguration, producer: &CandidateConfiguration) -> bool {
    consumer.timing == producer.timing
        || (consumer.timing == ExecutionTiming::QueryTime
            && matches!(producer.materialization, Materialization::MatAtIngestion(_)))
}

fn summary_mode_allowed(
    node: &OperatorNode,
    readers: &[usize],
    mode: Materialization,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    deployment: &DeploymentCapabilities,
) -> bool {
    let Ok(eligibility) =
        summary_materialization_eligibility(node, readers, entries, data, deployment)
    else {
        return false;
    };
    eligibility.range_compatibility.is_ok()
        && match mode {
            Materialization::NotMaterialized | Materialization::MatAtQuery(_) => true,
            Materialization::MatAtIngestion(_) => eligibility.at_ingestion.is_ok(),
            Materialization::MatAtQueryKeptAcrossEvaluations(_) => {
                eligibility.may_keep_across_evaluations
            }
        }
}

#[cfg(test)]
mod tests {
    use super::super::candidate_connections::select_candidates;
    use super::super::candidate_inventory::build_candidate_inventory;
    use super::*;
    use asap_logical_asap_optimization::memo::candidates::try_populate_pass1;
    use asap_logical_asap_optimization::memo::state_groups::expand_concrete_summary_state_groups;
    use asap_logical_asap_optimization::memo::{LogicalMemo, MemoGroupId};
    use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::lower_promql;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::{FieldDataType, SketchAlgorithm};
    use asap_types::ir::QueryRoot;
    use asap_types::workload::{
        DataArrival, Predictability, Query, QueryRecurrence, QueryRequirements, RepeatedDemand,
        RepetitionInterval,
    };

    fn fixture() -> (CandidateInventory, Vec<QueryWorkloadEntry>, DataWorkload) {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let queries = [
            "quantile_over_time(0.95, latency[5m])",
            "quantile_over_time(0.99, latency[5m])",
        ];
        let roots = queries
            .iter()
            .enumerate()
            .map(|(id, query)| (id, QueryRoot::Operator(lower_promql(query, target.clone()))))
            .collect::<Vec<_>>();
        let initial = LogicalMemo::from_roots(&roots);
        let outputs = (0..2)
            .map(|query| {
                MemoGroupId(
                    initial
                        .groups
                        .iter()
                        .position(|group| group.roots_of.contains(&query))
                        .unwrap() as u32,
                )
            })
            .collect::<Vec<_>>();
        let candidates = generate_per_computation_candidates(roots).unwrap();
        let memo =
            expand_concrete_summary_state_groups(try_populate_pass1(initial, &candidates).unwrap())
                .unwrap()
                .memo;
        let source = build_candidate_inventory(&memo, &outputs, |_| Ok(7.0)).unwrap();
        let entries = queries
            .iter()
            .map(|query| QueryWorkloadEntry {
                query: Query((*query).into()),
                requirements: QueryRequirements {
                    accuracy: target.clone(),
                    max_latency_ms: None,
                },
                predictability: Predictability::Predictable { known_at: None },
                recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                    RepetitionInterval(300_000),
                )),
                range_evaluation: None,
            })
            .collect();
        (
            source,
            entries,
            DataWorkload {
                arrival: DataArrival::Ingesting {
                    rows_per_second: 1000.0,
                },
                ..Default::default()
            },
        )
    }

    fn kll(node: &OperatorNode) -> bool {
        matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. }) if *kind.algorithm() == SketchAlgorithm::Kll)
    }

    // Configuration expansion is bounded per node, and never crosses ephemeral timing.
    #[test]
    fn real_quantiles_offer_local_and_shared_timing_configurations() {
        let (source, entries, data) = fixture();
        let expanded = enumerate_candidate_configurations(
            &source,
            &[0, 1],
            &entries,
            &data,
            &DeploymentCapabilities::default(),
        )
        .unwrap();
        assert!(expanded.configurations.len() <= 4 * source.graph.candidates.len());
        assert!(expanded
            .inventory
            .graph
            .candidates
            .iter()
            .all(|candidate| candidate.local_cost == 0.0));
        let builds = source
            .templates
            .iter()
            .enumerate()
            .filter_map(|(id, template)| kll(template.as_ref().unwrap()).then_some(id))
            .collect::<Vec<_>>();
        assert!(
            builds.len() >= 3,
            "both local builds and explicit shared state survive"
        );
        for source in builds {
            let modes = expanded
                .configurations
                .iter()
                .filter(|configuration| configuration.source_candidate.0 == source)
                .map(|configuration| configuration.materialization)
                .collect::<Vec<_>>();
            assert!(modes.contains(&Materialization::NotMaterialized));
            assert!(modes.contains(&Materialization::MatAtQuery(Medium::Memory)));
            assert!(modes.contains(&Materialization::MatAtIngestion(Medium::Memory)));
            assert!(!expanded
                .configurations
                .iter()
                .any(|configuration| configuration.source_candidate.0 == source
                    && configuration.materialization == Materialization::NotMaterialized
                    && configuration.timing == ExecutionTiming::IngestionTime));
        }
        for (consumer, candidate) in expanded.inventory.graph.candidates.iter().enumerate() {
            for producer in candidate
                .producer_requirements
                .iter()
                .flat_map(|input| &input.compatible_producers)
            {
                assert!(compatible_timing(
                    &expanded.configurations[consumer],
                    &expanded.configurations[producer.0]
                ));
            }
        }
        assert!(expanded
            .inventory
            .graph
            .query_roots
            .iter()
            .flat_map(|root| &root.compatible_candidates)
            .all(|id| expanded.configurations[id.0].timing == ExecutionTiming::QueryTime));
    }

    // Two references belonging to one scalar query cannot justify shared-query materialization.
    #[test]
    fn repeated_scalar_references_are_not_two_query_readers() {
        let (source, entries, data) = fixture();
        let expanded = enumerate_candidate_configurations(
            &source,
            &[0, 0],
            &entries,
            &data,
            &DeploymentCapabilities::default(),
        )
        .unwrap();
        let selected = select_candidates(&expanded.inventory.graph)
            .unwrap()
            .unwrap();
        assert!(selected
            .selected_candidates
            .iter()
            .all(|id| expanded.configurations[id.0].materialization
                != Materialization::MatAtQuery(Medium::Memory)));
    }

    // Requiring ingestion for output zero remains feasible beside an incompatible
    // optional reader: output one can use another configuration of the same state.
    #[test]
    fn unpredictable_consumer_does_not_remove_predictable_ingestion_configuration() {
        let (source, mut entries, data) = fixture();
        entries[1].predictability = Predictability::AdHoc;
        let mut expanded = enumerate_candidate_configurations(
            &source,
            &[0, 1],
            &entries,
            &data,
            &DeploymentCapabilities::default(),
        )
        .unwrap();
        let shared_build = source
            .group_candidates
            .iter()
            .flatten()
            .find(|id| kll(source.templates[id.0].as_ref().unwrap()))
            .unwrap();
        let ingestion = expanded
            .configurations
            .iter()
            .position(|configuration| {
                configuration.source_candidate == *shared_build
                    && configuration.materialization
                        == Materialization::MatAtIngestion(Medium::Memory)
            })
            .unwrap();
        // Add a required output reference to this producer via a query-time
        // reader root that already depends on its state group, preserving valid roots.
        let reader_sources = source
            .graph
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                candidate
                    .producer_requirements
                    .iter()
                    .any(|input| input.compatible_producers.contains(shared_build))
            })
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        assert!(reader_sources.len() >= 2);
        for (id, configuration) in expanded.configurations.iter().enumerate() {
            if reader_sources.contains(&configuration.source_candidate.0)
                && configuration.timing == ExecutionTiming::QueryTime
            {
                for input in &mut expanded.inventory.graph.candidates[id].producer_requirements {
                    if input.compatible_producers.iter().any(|producer| {
                        expanded.configurations[producer.0].source_candidate == *shared_build
                    }) {
                        // Query-specific readers have their original logical owner.
                        let source_group =
                            source.graph.candidates[configuration.source_candidate.0].group;
                        let is_first = source.graph.query_roots[0]
                            .compatible_candidates
                            .iter()
                            .any(|root| source.graph.candidates[root.0].group == source_group);
                        input.compatible_producers.retain(|producer| {
                            if is_first {
                                producer.0 == ingestion
                            } else {
                                producer.0 != ingestion
                            }
                        });
                    }
                }
            }
        }
        // Make complete exact/local choices expensive, so the exercised solution
        // uses the shared-state reader path. These are test-only coefficients.
        for (id, candidate) in expanded.inventory.graph.candidates.iter_mut().enumerate() {
            let source_id = expanded.configurations[id].source_candidate;
            candidate.local_cost = if source_id == *shared_build {
                1.0
            } else if kll(source.templates[source_id.0].as_ref().unwrap()) {
                50.0
            } else if matches!(
                source.templates[source_id.0].as_ref().unwrap().asap(),
                Some(ASAPOp::SummaryAgg { .. })
            ) || matches!(
                source.templates[source_id.0].as_ref().unwrap().non_asap(),
                Some(asap_types::ir::NonASAPOp::Aggregate { .. })
            ) {
                100.0
            } else {
                0.0
            };
        }
        let selected = select_candidates(&expanded.inventory.graph)
            .unwrap()
            .unwrap();
        assert!(selected
            .selected_candidates
            .contains(&PhysicalCandidateId(ingestion)));
        assert!(selected
            .selected_candidates
            .iter()
            .any(
                |id| expanded.configurations[id.0].source_candidate == *shared_build
                    && expanded.configurations[id.0].timing == ExecutionTiming::QueryTime
            ));
    }
}
