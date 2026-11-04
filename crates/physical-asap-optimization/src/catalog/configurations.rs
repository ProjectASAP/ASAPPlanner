//! Finite, unpriced execution configurations over typed immediate candidates.
//!
//! This does not enumerate consumer subsets. Current summary eligibility is a
//! conjunction of individual-reader restrictions and pairwise interval/phase
//! compatibility; selected-reader constraints encode those conditions. Complete
//! Stage 2 validation still checks operator legality and upstream timing.
//! Potential shared non-summary nodes also receive query-time `OneEvaluation`; Stage 2
//! checks highest-shared eligibility on the selected graph, not all possible consumers.
use asap_types::stages::{ProducerCandidates, ProducerSetInterner};
use std::collections::HashMap;

use crate::{
    kept_across_evaluations_at_ingestion, kept_across_evaluations_at_query_time,
    kept_for_one_evaluation, physical_properties as properties, summary_keep_eligibility,
};
use asap_types::ir::operator::{agg_has_exact_per_entity_kernel, Reduction};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode};
use asap_types::physical::{Lifetime, PhysicalProperties};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::inventory::CandidateInventory;
use asap_types::ir::QueryRoot;
use asap_types::stages::{
    output_references, MemoGroupId, OperatorNodeOfCandidateForMemoGroupId,
    PhysicalASAPMemoCandidates, PhysicalCandidate, PhysicalCandidateId, PhysicalCandidateKey,
    PruneReason, PrunedCandidate, ReaderConstraint,
};

/// [`PhysicalCandidate::may_keep_across_evaluations_per_sole_reader_query`] for `node` configured
/// with `properties`: for each query in `entries`, whether
/// `summary_keep_eligibility` with that query as the only reader
/// allows keeping it across evaluations. Empty unless `node` is a tumbling-window chain,
/// `properties` is query time kept `AcrossEvaluations(Memory)` and the
/// deployment keeps query-time results.
pub fn may_keep_across_evaluations_per_sole_reader_query(
    node: &OperatorNode,
    properties: PhysicalProperties,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    query_time_results_retained: bool,
) -> Vec<bool> {
    if properties != kept_across_evaluations_at_query_time()
        || !matches!(node.asap(), Some(ASAPOp::SummaryTumblingWindowChain { .. }))
        || !query_time_results_retained
    {
        return Vec::new();
    }
    (0..entries.len())
        .map(|reader| {
            summary_keep_eligibility(node, &[reader], entries, data, query_time_results_retained)
                .is_ok_and(|eligibility| eligibility.may_keep_across_evaluations)
        })
        .collect()
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigurationError {
    #[error("invalid source candidate inventory: {0}")]
    InvalidInventory(String),
    #[error("candidate {candidate} already has timing or lifetime assigned")]
    AssignedExecution { candidate: usize },
}

/// Each operator node of `source` with every timing and lifetime
/// Stage 2 allows, one candidate each. `query_roots` are the memo's; their
/// group references are `source`'s output references.
pub fn enumerate_candidate_configurations(
    source: &CandidateInventory,
    query_roots: &[QueryRoot<MemoGroupId>],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    query_time_results_retained: bool,
    raw_data_retained: bool,
) -> Result<PhysicalASAPMemoCandidates, ConfigurationError> {
    let invalid = |message: &str| ConfigurationError::InvalidInventory(message.into());
    source.validate().map_err(|error| invalid(&error))?;
    let output_queries: Vec<usize> = output_references(query_roots)
        .into_iter()
        .map(|(query, _)| query)
        .collect();
    let source_count = source.operator_nodes_of_candidates_for_memo_groups.len();
    if output_queries.len() != source.query_output_nodes.len()
        || output_queries.iter().any(|&query| query >= entries.len())
    {
        return Err(invalid("template or output/query mapping is incomplete"));
    }
    if source
        .implementation_roots_of_group
        .iter()
        .flatten()
        .any(|id| id.0 >= source_count)
    {
        return Err(invalid("group references missing source candidate"));
    }
    let mut query_outputs = vec![Vec::new(); entries.len()];
    let mut potential = vec![vec![false; entries.len()]; source_count];
    let mut may_produce_query_output = vec![false; source_count];
    for (output, &query) in output_queries.iter().enumerate() {
        query_outputs[query].push(output);
        let mut pending = source.query_output_nodes[output].clone();
        for root in &pending {
            may_produce_query_output[root.0] = true;
        }
        while let Some(candidate) = pending.pop() {
            if !potential[candidate.0][query] {
                potential[candidate.0][query] = true;
                pending.extend(
                    source.producer_nodes_per_input[candidate.0]
                        .iter()
                        .flat_map(|input| input.iter().copied()),
                );
            }
        }
    }
    let mut candidates = Vec::new();
    let mut reader_constraints = Vec::new();
    let mut pruned_candidates = Vec::new();
    let mut source_configurations = vec![Vec::new(); source_count];
    for source_id in 0..source_count {
        let node = &source.operator_nodes_of_candidates_for_memo_groups[source_id].physical_node;
        if node.timing.is_some() || node.lifetime.is_kept() {
            return Err(ConfigurationError::AssignedExecution {
                candidate: source_id,
            });
        }
        let summary = matches!(
            node.asap(),
            Some(ASAPOp::SummaryAgg { .. } | ASAPOp::SummaryTumblingWindowChain { .. })
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
        let mut alternatives = vec![properties(ExecutionTiming::QueryTime, Lifetime::None)];
        if summary {
            alternatives.push(kept_for_one_evaluation());
            if queries.iter().any(|&query| {
                summary_properties_allowed(
                    node,
                    &[query],
                    kept_across_evaluations_at_ingestion(),
                    entries,
                    data,
                    query_time_results_retained,
                )
            }) {
                alternatives.push(kept_across_evaluations_at_ingestion());
            }
            if matches!(node.asap(), Some(ASAPOp::SummaryTumblingWindowChain { .. }))
                && queries.iter().any(|&query| {
                    summary_properties_allowed(
                        node,
                        &[query],
                        kept_across_evaluations_at_query_time(),
                        entries,
                        data,
                        query_time_results_retained,
                    )
                })
            {
                alternatives.push(kept_across_evaluations_at_query_time());
            }
        } else if !forced_query {
            alternatives.push(properties(ExecutionTiming::IngestionTime, Lifetime::None));
        }
        // Potential reach only generates the choice. Selected-reader cardinality
        // and Stage 2's highest-shared rule determine actual eligibility.
        if !summary && queries.len() >= 2 {
            alternatives.push(kept_for_one_evaluation());
        }
        // Configurations the deployment cannot run are not generated; each is
        // recorded as pruned.
        let group = source.operator_nodes_of_candidates_for_memo_groups[source_id].group();
        let mut prune_alternatives = |reason, applies: &dyn Fn(ExecutionTiming) -> bool| {
            alternatives.retain(|&properties| {
                let pruned = applies(properties.timing());
                if pruned {
                    pruned_candidates.push(PrunedCandidate {
                        group,
                        candidate: PhysicalCandidateKey {
                            operator_node: OperatorNodeOfCandidateForMemoGroupId(source_id),
                            properties,
                        },
                        reason,
                    });
                }
                !pruned
            });
        };
        if !per_entity_aggregate_executable(node) {
            prune_alternatives(PruneReason::PerEntityAggregateNotExecutable, &|_| true);
        }
        // Without raw data retained, only ingestion-time work reads raw rows.
        if !raw_data_retained && matches!(node.operator, Operator::NonASAP(NonASAPOp::Scan { .. }))
        {
            prune_alternatives(PruneReason::RawDataNotRetained, &|timing| {
                timing != ExecutionTiming::IngestionTime
            });
        }
        for properties in alternatives {
            let timing = properties.timing();
            let id = PhysicalCandidateId(candidates.len());
            source_configurations[source_id].push(id);
            candidates.push(PhysicalCandidate {
                operator_node: OperatorNodeOfCandidateForMemoGroupId(source_id),
                properties,
                producer_candidates_per_input: Vec::new(),
                may_produce_query_output: may_produce_query_output[source_id]
                    && timing == ExecutionTiming::QueryTime,
                may_keep_across_evaluations_per_sole_reader_query:
                    may_keep_across_evaluations_per_sole_reader_query(
                        node,
                        properties,
                        entries,
                        data,
                        query_time_results_retained,
                    ),
            });
            if properties == kept_for_one_evaluation() {
                if !summary {
                    reader_constraints.push(ReaderConstraint::HighestSharedOneEvaluation {
                        candidate: id,
                        query_outputs: query_outputs.clone(),
                    });
                }
                reader_constraints.push(ReaderConstraint::MinimumQueryReaders {
                    candidate: id,
                    query_outputs: query_outputs.clone(),
                    minimum: 2,
                });
            }
            if summary {
                let eligible = queries
                    .iter()
                    .copied()
                    .filter(|&query| {
                        summary_properties_allowed(
                            node,
                            &[query],
                            properties,
                            entries,
                            data,
                            query_time_results_retained,
                        )
                    })
                    .collect::<Vec<_>>();
                let outputs = eligible
                    .iter()
                    .flat_map(|&query| query_outputs[query].iter().copied())
                    .collect();
                reader_constraints.push(ReaderConstraint::AllowedOutputs {
                    candidate: id,
                    outputs,
                });
                // The current Stage 2 rules require common intervals/phases,
                // whose conflicts are pairwise. No arbitrary set predicate is
                // approximated by this encoding.
                for (position, &left) in eligible.iter().enumerate() {
                    for &right in &eligible[position + 1..] {
                        if !summary_properties_allowed(
                            node,
                            &[left, right],
                            properties,
                            entries,
                            data,
                            query_time_results_retained,
                        ) {
                            for &left_output in &query_outputs[left] {
                                for &right_output in &query_outputs[right] {
                                    reader_constraints.push(
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
    let mut producer_sets = ProducerSetInterner::default();
    // Compatibility depends on consumer timing and producer timing and lifetime only.
    // Evaluate each source configuration once per timing, then reuse this exact
    // index; reader eligibility remains expressed by separate graph constraints.
    let mut timing_index = Vec::with_capacity(source_configurations.len());
    for configurations in &source_configurations {
        let mut by_timing = [ProducerCandidates::default(), ProducerCandidates::default()];
        for (timing_index, timing) in [ExecutionTiming::QueryTime, ExecutionTiming::IngestionTime]
            .into_iter()
            .enumerate()
        {
            let mut compatible = Vec::new();
            for &producer in configurations {
                if candidates[producer.0].properties.readable_at(timing) {
                    compatible.push(producer);
                }
            }
            by_timing[timing_index] = producer_sets.intern(compatible);
        }
        timing_index.push(by_timing);
    }
    let mut input_cache: HashMap<(ProducerCandidates, ExecutionTiming), ProducerCandidates> =
        HashMap::new();
    for candidate in &mut candidates {
        let mut requirements = Vec::new();
        for input in &source.producer_nodes_per_input[candidate.operator_node.0] {
            let key = (input.clone(), candidate.properties.timing());
            let compatible_producers = if let Some(producer_candidates) = input_cache.get(&key) {
                producer_candidates.clone()
            } else {
                let timing = match candidate.properties.timing() {
                    ExecutionTiming::QueryTime => 0,
                    ExecutionTiming::IngestionTime => 1,
                };
                let mut compatible = Vec::new();
                for producer in input {
                    compatible.extend(timing_index[producer.0][timing].iter().copied());
                }
                let producer_candidates = producer_sets.intern(compatible);
                input_cache.insert(key, producer_candidates.clone());
                producer_candidates
            };
            requirements.push(compatible_producers);
        }
        candidate.producer_candidates_per_input = requirements;
    }
    let mut result = PhysicalASAPMemoCandidates::new(
        source.operator_nodes_of_candidates_for_memo_groups.clone(),
        candidates,
        query_roots.to_vec(),
        reader_constraints,
    );
    result.pruned_candidates = pruned_candidates;
    super::validate_candidates(&result).map_err(|error| invalid(&error))?;
    Ok(result)
}

/// Whether the executor can run `node`: false only for an exact per-entity
/// `Aggregate` (a per-series range reduction over raw data samples) that
/// has more than one measure, a measure without an exact per-entity kernel
/// ([`agg_has_exact_per_entity_kernel`]), a measure filter or a `HAVING`.
/// The rule reads one operator node, so Stage 2 does not generate the node
/// rather than Stage 3 rejecting each plan that reads it.
fn per_entity_aggregate_executable(node: &OperatorNode) -> bool {
    match &node.operator {
        Operator::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            measures,
            filters,
            having,
            ..
        }) => {
            matches!(measures.as_slice(), [measure] if agg_has_exact_per_entity_kernel(measure))
                && filters.iter().all(Option::is_none)
                && having.is_none()
        }
        _ => true,
    }
}

fn summary_properties_allowed(
    node: &OperatorNode,
    readers: &[usize],
    candidate_properties: PhysicalProperties,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    query_time_results_retained: bool,
) -> bool {
    let Ok(eligibility) =
        summary_keep_eligibility(node, readers, entries, data, query_time_results_retained)
    else {
        return false;
    };
    eligibility.range_compatibility.is_ok()
        && match (
            candidate_properties.timing(),
            candidate_properties.lifetime(),
        ) {
            (_, Lifetime::None | Lifetime::OneEvaluation(_)) => true,
            (ExecutionTiming::IngestionTime, Lifetime::AcrossEvaluations(_)) => {
                eligibility.at_ingestion.is_ok()
            }
            (ExecutionTiming::QueryTime, Lifetime::AcrossEvaluations(_)) => {
                eligibility.may_keep_across_evaluations
            }
        }
}
