//! Finite, unpriced execution configurations over typed immediate candidates.
//!
//! This does not enumerate consumer subsets. Current summary eligibility is a
//! conjunction of individual-reader restrictions and pairwise interval/phase
//! compatibility; selected-reader constraints encode those conditions. Complete
//! Stage 2 validation still checks operator legality and upstream timing.
//! Potential shared non-summary nodes also receive query materialization; Stage 2
//! checks highest-shared eligibility on the selected graph, not all possible consumers.
use super::suppliers::{SupplierCandidates, SupplierSetInterner};
use std::collections::HashMap;
use std::rc::Rc;

use asap_logical_asap_optimization::deployment_capabilities::DeploymentCapabilities;
use asap_physical_asap_optimization::summary_materialization_eligibility;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode};
use asap_types::physical::{Materialization, Medium};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::search::candidates::graph::{
    CandidateDependencyGraph, PhysicalCandidate, PhysicalCandidateId, ProducerRequirement,
    QueryRootCandidates, ReaderConstraint,
};
use crate::search::candidates::inventory::CandidateInventory;

#[derive(Debug, Clone, Copy)]
pub(in crate::search) struct CandidateConfiguration {
    pub source_candidate: PhysicalCandidateId,
    pub materialization: Materialization,
    pub timing: ExecutionTiming,
}

#[derive(Debug)]
pub(in crate::search) struct CandidateConfigurationInventory {
    /// Costs are zero placeholders. Price/configure resource models before
    /// treating a selected solution as a deployment-cost optimization result.
    pub inventory: CandidateInventory,
    pub configurations: Vec<CandidateConfiguration>,
}

#[derive(Debug, thiserror::Error)]
pub(in crate::search) enum ConfigurationError {
    #[error(transparent)]
    DeadlineExceeded(
        #[from] asap_logical_asap_optimization::pass2::memo::CandidateGenerationTimeout,
    ),
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
#[cfg(test)]
pub(in crate::search) fn enumerate_candidate_configurations(
    source: &CandidateInventory,
    output_queries: &[usize],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    deployment: &DeploymentCapabilities,
) -> Result<CandidateConfigurationInventory, ConfigurationError> {
    enumerate_candidate_configurations_before(
        source,
        output_queries,
        entries,
        data,
        deployment,
        None,
    )
}

pub(in crate::search) fn enumerate_candidate_configurations_before(
    source: &CandidateInventory,
    output_queries: &[usize],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    deployment: &DeploymentCapabilities,
    deadline: Option<std::time::Instant>,
) -> Result<CandidateConfigurationInventory, ConfigurationError> {
    super::check_deadline(deadline)?;
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
        super::check_deadline(deadline)?;
        query_outputs[query].push(output);
        let mut pending = source.graph.query_roots[output]
            .compatible_candidates
            .clone();
        while let Some(candidate) = pending.pop() {
            super::check_deadline(deadline)?;
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
        super::check_deadline(deadline)?;
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
        // Potential reach only generates the choice. Selected-reader cardinality
        // and Stage 2's highest-shared rule determine actual eligibility.
        if !summary && queries.len() >= 2 {
            modes.push((
                Materialization::MatAtQuery(Medium::Memory),
                ExecutionTiming::QueryTime,
            ));
        }
        // This is the same deployment restriction enforced by PlanSelector's
        // raw_data_violation. Encode it before solving instead of excluding
        // each complete plan that uses an unavailable query-time raw scan.
        if !deployment.raw_data_retained
            && matches!(node.operator, Operator::NonASAP(NonASAPOp::Scan { .. }))
        {
            modes.retain(|(_, timing)| *timing == ExecutionTiming::IngestionTime);
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
            if materialization == Materialization::MatAtQuery(Medium::Memory) {
                if !summary {
                    result.inventory.graph.reader_constraints.push(
                        ReaderConstraint::HighestSharedQueryMaterialization {
                            candidate: id,
                            query_outputs: query_outputs.clone(),
                        },
                    );
                }
                result.inventory.graph.reader_constraints.push(
                    ReaderConstraint::MinimumQueryReaders {
                        candidate: id,
                        query_outputs: query_outputs.clone(),
                        minimum: 2,
                    },
                );
            }
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
    let mut supplier_sets = SupplierSetInterner::default();
    // Compatibility depends on consumer timing and producer mode/timing only.
    // Evaluate each source configuration once per timing, then reuse this exact
    // index; reader eligibility remains expressed by separate graph constraints.
    let mut timing_index = Vec::with_capacity(source_configurations.len());
    for configurations in &source_configurations {
        super::check_deadline(deadline)?;
        let mut by_timing = [SupplierCandidates::default(), SupplierCandidates::default()];
        for (timing_index, timing) in [ExecutionTiming::QueryTime, ExecutionTiming::IngestionTime]
            .into_iter()
            .enumerate()
        {
            let mut compatible = Vec::new();
            for &producer in configurations {
                super::check_deadline(deadline)?;
                if compatible_timing(timing, &result.configurations[producer.0]) {
                    compatible.push(producer);
                }
            }
            by_timing[timing_index] = supplier_sets.intern(compatible);
        }
        timing_index.push(by_timing);
    }
    let mut input_cache: HashMap<(SupplierCandidates, ExecutionTiming), SupplierCandidates> =
        HashMap::new();
    for (id, configuration) in result.configurations.iter().enumerate() {
        super::check_deadline(deadline)?;
        let mut requirements = Vec::new();
        for input in
            &source.graph.candidates[configuration.source_candidate.0].producer_requirements
        {
            let key = (input.compatible_producers.clone(), configuration.timing);
            let compatible_producers = if let Some(suppliers) = input_cache.get(&key) {
                suppliers.clone()
            } else {
                let timing = match configuration.timing {
                    ExecutionTiming::QueryTime => 0,
                    ExecutionTiming::IngestionTime => 1,
                };
                let mut compatible = Vec::new();
                for producer in &input.compatible_producers {
                    super::check_deadline(deadline)?;
                    compatible.extend(timing_index[producer.0][timing].iter().copied());
                }
                let suppliers = supplier_sets.intern(compatible);
                input_cache.insert(key, suppliers.clone());
                suppliers
            };
            requirements.push(ProducerRequirement {
                compatible_producers,
            });
        }
        result.inventory.graph.candidates[id].producer_requirements = requirements;
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
        super::check_deadline(deadline)?;
        let source_candidate = match constraint {
            ReaderConstraint::ActivationRequires { .. } => {
                return Err(invalid(
                    "source inventory must precede execution-scope expansion",
                ))
            }
            ReaderConstraint::AllowedOutputs { candidate, .. }
            | ReaderConstraint::IncompatibleOutputs { candidate, .. }
            | ReaderConstraint::HighestSharedQueryMaterialization { candidate, .. }
            | ReaderConstraint::MinimumQueryReaders { candidate, .. } => candidate,
        };
        for &candidate in &source_configurations[source_candidate.0] {
            result
                .inventory
                .graph
                .reader_constraints
                .push(match constraint {
                    ReaderConstraint::ActivationRequires { .. } => {
                        return Err(invalid(
                            "source inventory must precede execution-scope expansion",
                        ))
                    }
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
                    ReaderConstraint::HighestSharedQueryMaterialization {
                        query_outputs, ..
                    } => ReaderConstraint::HighestSharedQueryMaterialization {
                        candidate,
                        query_outputs: query_outputs.clone(),
                    },
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
    // Repeated identical workload entries and identical logical outputs have
    // identical timing, accuracy and pricing requirements. Their request counts
    // remain in the evaluator, but they do not require distinct binding scopes.
    let mut context_ids = std::collections::HashMap::new();
    let mut output_contexts = Vec::new();
    for (output, root) in source.graph.query_roots.iter().enumerate() {
        let suppliers: Vec<_> = root.compatible_candidates.iter().map(|id| id.0).collect();
        let key = serde_json::to_string(&(&entries[output_queries[output]], suppliers))
            .map_err(|error| invalid(&error.to_string()))?;
        let next = context_ids.len();
        output_contexts.push(*context_ids.entry(key).or_insert(next));
    }
    let mut uses = vec![std::collections::HashSet::new(); source.graph.candidates.len()];
    for candidate in &source.graph.candidates {
        super::check_deadline(deadline)?;
        for (position, input) in candidate.producer_requirements.iter().enumerate() {
            for producer in &input.compatible_producers {
                uses[producer.0].insert((candidate.group, position));
            }
        }
    }
    let mut root_uses = vec![std::collections::HashSet::new(); source.graph.candidates.len()];
    for (output, root) in source.graph.query_roots.iter().enumerate() {
        for candidate in &root.compatible_candidates {
            root_uses[candidate.0].insert(output_contexts[output]);
        }
    }
    let original_uses: Vec<_> = uses
        .iter()
        .zip(root_uses)
        .map(|(uses, roots)| uses.len() + roots.len())
        .collect();
    crate::search::candidates::scopes::expand_execution_scopes_before(
        result,
        &original_uses,
        &output_contexts,
        deadline,
    )
}

fn compatible_timing(consumer_timing: ExecutionTiming, producer: &CandidateConfiguration) -> bool {
    consumer_timing == producer.timing
        || (consumer_timing == ExecutionTiming::QueryTime
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
    use super::*;
    use crate::search::candidates::inventory::build_candidate_inventory;
    use crate::search::milp::model::select_candidates;
    use asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::pass2::memo::candidates::try_populate_pass1;
    use asap_logical_asap_optimization::pass2::memo::state_groups::expand_concrete_summary_state_groups;
    use asap_logical_asap_optimization::pass2::memo::{DagOfLogicalMemoGroups, MemoGroupId};
    use asap_logical_asap_optimization::test_support::lower_promql;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::{FieldDataType, SketchAlgorithm};
    use asap_types::ir::QueryRoot;
    use asap_types::workload::{
        DataArrival, Predictability, Query, QueryRecurrence, QueryRequirements, RepeatedDemand,
        RepetitionInterval,
    };

    // Configuration expansion must stop before returning an incomplete catalog.
    #[test]
    fn configuration_deadline_returns_typed_timeout() {
        let (source, entries, data) = fixture();
        assert!(matches!(
            enumerate_candidate_configurations_before(
                &source,
                &[0, 1],
                &entries,
                &data,
                &DeploymentCapabilities::default(),
                Some(std::time::Instant::now())
            ),
            Err(ConfigurationError::DeadlineExceeded(_))
        ));
    }

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
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let outputs = (0..2)
            .map(|query| *initial.query_roots[query].as_operator().unwrap())
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

    // Indexed compatibility must reproduce every original ordered edge while
    // storing repeated supplier lists once, including query/ingestion modes.
    #[test]
    fn supplier_index_preserves_all_edges_and_shares_storage() {
        let (source, entries, data) = fixture();
        let expanded = enumerate_candidate_configurations(
            &source,
            &[0, 1],
            &entries,
            &data,
            &DeploymentCapabilities::default(),
        )
        .unwrap();
        for (consumer, configuration) in expanded.configurations.iter().enumerate() {
            let original = &source.graph.candidates[configuration.source_candidate.0];
            let generated = &expanded.inventory.graph.candidates[consumer];
            assert_eq!(
                generated.producer_requirements.len(),
                original.producer_requirements.len()
            );
            for (original, generated) in original
                .producer_requirements
                .iter()
                .zip(&generated.producer_requirements)
            {
                let expected: Vec<_> = original
                    .compatible_producers
                    .iter()
                    .flat_map(|source| {
                        expanded.configurations.iter().enumerate().filter_map(
                            move |(id, producer)| {
                                (producer.source_candidate == *source
                                    && (configuration.timing == producer.timing
                                        || (configuration.timing == ExecutionTiming::QueryTime
                                            && matches!(
                                                producer.materialization,
                                                Materialization::MatAtIngestion(_)
                                            ))))
                                .then_some(PhysicalCandidateId(id))
                            },
                        )
                    })
                    .collect();
                assert_eq!(
                    generated.compatible_producers.as_slice(),
                    expected.as_slice()
                );
            }
        }
        let metrics = super::super::suppliers::supplier_storage_metrics(&expanded.inventory.graph);
        let requirements = expanded
            .inventory
            .graph
            .candidates
            .iter()
            .map(|candidate| candidate.producer_requirements.len())
            .sum::<usize>();
        assert!(metrics.unique_supplier_sets < requirements, "{metrics:?}");
        assert!(
            metrics.stored_supplier_ids < metrics.logical_connections,
            "{metrics:?}"
        );
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
        let original_positions = source.graph.query_roots.len()
            + source
                .graph
                .candidates
                .iter()
                .map(|candidate| candidate.producer_requirements.len())
                .sum::<usize>();
        assert!(
            expanded.configurations.len()
                <= 4 * source.graph.candidates.len() * original_positions.max(1)
        );
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
                    expanded.configurations[consumer].timing,
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

    // Identical logical readers need separate bindings when their whole-window
    // ingestion phases disagree. Every copy still passes the existing Stage 2.
    #[test]
    fn identical_quantiles_with_distinct_phases_have_separate_execution_scopes() {
        let (catalog, mut entries, data) = fixture();
        let root = catalog.graph.query_roots[0]
            .compatible_candidates
            .iter()
            .filter_map(|id| catalog.templates[id.0].as_ref())
            .find(|node| OperatorNode::reachable(node).iter().any(|child| kll(child)))
            .unwrap()
            .clone();
        let mut source = CandidateInventory {
            graph: CandidateDependencyGraph::default(),
            templates: Vec::new(),
            group_candidates: Vec::new(),
        };
        fn append(node: &Rc<OperatorNode>, source: &mut CandidateInventory) -> PhysicalCandidateId {
            let mut requirements = Vec::new();
            node.operator.map_children(|child| {
                requirements.push(ProducerRequirement {
                    compatible_producers: vec![append(child, source)].into(),
                });
                child.clone()
            });
            let id = PhysicalCandidateId(source.templates.len());
            source.templates.push(Some(node.clone()));
            source.graph.candidates.push(PhysicalCandidate {
                group: MemoGroupId(id.0 as u32),
                local_cost: 0.0,
                producer_requirements: requirements,
            });
            source.group_candidates.push(vec![id]);
            id
        }
        let output = append(&root, &mut source);
        source.graph.query_roots = vec![
            QueryRootCandidates {
                compatible_candidates: vec![output]
            };
            2
        ];
        for (index, entry) in entries.iter_mut().enumerate() {
            entry.recurrence = QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
                interval: RepetitionInterval(600_000),
                evaluation_phase: asap_types::workload::TimestampMs(index as u64 * 300_000),
            });
        }
        let deployment = DeploymentCapabilities {
            raw_data_retained: false,
            ..Default::default()
        };
        let expanded =
            enumerate_candidate_configurations(&source, &[0, 1], &entries, &data, &deployment)
                .unwrap();
        let selected = select_candidates(&expanded.inventory.graph)
            .unwrap()
            .expect("both incompatible phases must be covered");
        let builds: Vec<_> = selected
            .selected_candidates
            .iter()
            .filter(|id| kll(expanded.inventory.templates[id.0].as_ref().unwrap()))
            .collect();
        assert_eq!(builds.len(), 2);
        assert!(builds
            .iter()
            .all(|id| expanded.configurations[id.0].materialization
                == Materialization::MatAtIngestion(Medium::Memory)));
        let mut modes = vec![None; expanded.configurations.len()];
        for id in &selected.selected_candidates {
            modes[id.0] = Some(expanded.configurations[id.0].materialization);
        }
        let original_roots = vec![QueryRoot::Operator(root); 2];
        let physical = crate::search::candidates::physical::materialize_selected_candidates(
            &expanded.inventory,
            &selected,
            &modes,
            &original_roots,
            &entries,
            &data,
            &deployment,
        )
        .unwrap();
        physical.physical.dag.validate().unwrap();
        assert_eq!(physical.physical.roots.len(), 2);
    }

    // Repeating the same workload entry adds output references, not a quadratic
    // family of identical execution scopes and producer connections.
    #[test]
    fn repeated_identical_queries_keep_scope_and_connection_counts_constant() {
        let (mut source, entries, data) = fixture();
        let root = source.graph.query_roots[0].clone();
        let mut counts = Vec::new();
        for count in [2, 32] {
            source.graph.query_roots = vec![root.clone(); count];
            let entries = vec![entries[0].clone(); count];
            let outputs: Vec<_> = (0..count).collect();
            let expanded = enumerate_candidate_configurations(
                &source,
                &outputs,
                &entries,
                &data,
                &DeploymentCapabilities::default(),
            )
            .unwrap();
            let edges: usize = expanded
                .inventory
                .graph
                .candidates
                .iter()
                .flat_map(|candidate| &candidate.producer_requirements)
                .map(|input| input.compatible_producers.len())
                .sum();
            counts.push((expanded.configurations.len(), edges));
        }
        assert_eq!(counts[0], counts[1]);
    }

    // A deployment without raw retention cannot execute a query-time scan;
    // reject those configurations before spending refinement rounds on them.
    #[test]
    fn no_raw_retention_excludes_query_time_scan_configurations() {
        let (source, entries, data) = fixture();
        let expanded = enumerate_candidate_configurations(
            &source,
            &[0, 1],
            &entries,
            &data,
            &DeploymentCapabilities {
                raw_data_retained: false,
                ..Default::default()
            },
        )
        .unwrap();
        let scans = expanded
            .configurations
            .iter()
            .enumerate()
            .filter(|(id, _)| {
                matches!(
                    expanded.inventory.templates[*id].as_ref().unwrap().operator,
                    asap_types::ir::Operator::NonASAP(asap_types::ir::NonASAPOp::Scan { .. })
                )
            })
            .collect::<Vec<_>>();
        assert!(!scans.is_empty());
        assert!(scans
            .iter()
            .all(|(_, config)| config.timing == ExecutionTiming::IngestionTime));
        assert!(select_candidates(&expanded.inventory.graph)
            .unwrap()
            .is_some());
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
