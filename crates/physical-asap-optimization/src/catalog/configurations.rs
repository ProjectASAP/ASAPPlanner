//! Finite, unpriced execution configurations over typed immediate candidates.
//!
//! This does not enumerate consumer subsets. Current summary eligibility is a
//! conjunction of individual-reader restrictions and pairwise interval/phase
//! compatibility; selected-reader constraints encode those conditions. Complete
//! Stage 2 validation still checks operator legality and upstream timing.
//! Potential shared non-summary nodes also receive query materialization; Stage 2
//! checks highest-shared eligibility on the selected graph, not all possible consumers.
use asap_types::stages::{SupplierCandidates, SupplierSetInterner};
use std::collections::HashMap;

use crate::summary_materialization_eligibility;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode};
use asap_types::physical::{Materialization, Medium};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::inventory::CandidateInventory;
use asap_types::ir::QueryRoot;
use asap_types::stages::{
    MemoGroupId, OperatorNodeOfCandidateForMemoGroupId, PhysicalASAPMemoCandidates,
    PhysicalCandidate, PhysicalCandidateId, ReaderConstraint,
};

/// [`PhysicalCandidate::may_keep_across_evaluations_per_sole_reader_query`] for `node` configured
/// with `materialization`: for each query in `entries`, whether
/// [`summary_materialization_eligibility`] with that query as the only reader
/// allows keeping it across evaluations. Empty unless `node` is a pane chain,
/// `materialization` is `MatAtQueryKeptAcrossEvaluations(Memory)` and the
/// deployment keeps query-time results.
pub fn may_keep_with_sole_reader(
    node: &OperatorNode,
    materialization: Materialization,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    query_time_results_retained: bool,
) -> Vec<bool> {
    if materialization != Materialization::MatAtQueryKeptAcrossEvaluations(Medium::Memory)
        || !matches!(node.asap(), Some(ASAPOp::PaneChain { .. }))
        || !query_time_results_retained
    {
        return Vec::new();
    }
    (0..entries.len())
        .map(|reader| {
            summary_materialization_eligibility(
                node,
                &[reader],
                entries,
                data,
                query_time_results_retained,
            )
            .is_ok_and(|eligibility| eligibility.may_keep_across_evaluations)
        })
        .collect()
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigurationError {
    #[error(transparent)]
    DeadlineExceeded(#[from] asap_types::stages::CandidateGenerationTimeout),
    #[error("invalid source candidate inventory: {0}")]
    InvalidInventory(String),
    #[error("candidate {candidate} already has timing or materialization assigned")]
    AssignedExecution { candidate: usize },
}

/// The output reference queries of `query_roots`: for each group
/// reference, in order (a scalar query's in `map_operator_refs` order), the
/// query whose root it is.
pub fn output_reference_queries(query_roots: &[QueryRoot<MemoGroupId>]) -> Vec<usize> {
    let mut queries = Vec::new();
    for (query, root) in query_roots.iter().enumerate() {
        match root {
            QueryRoot::Operator(_) => queries.push(query),
            QueryRoot::Scalar(expression) => {
                expression.map_operator_refs(&mut |_| queries.push(query));
            }
        }
    }
    queries
}

/// Each operator node of `source` with every timing and materialization
/// Stage 2 allows, and its execution-scope copies. `query_roots` are the
/// memo's; their group references are `source`'s output references.
pub fn enumerate_candidate_configurations_before(
    source: &CandidateInventory,
    query_roots: &[QueryRoot<MemoGroupId>],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    query_time_results_retained: bool,
    raw_data_retained: bool,
    deadline: Option<std::time::Instant>,
) -> Result<PhysicalASAPMemoCandidates, ConfigurationError> {
    super::check_deadline(deadline)?;
    let invalid = |message: &str| ConfigurationError::InvalidInventory(message.into());
    source.validate().map_err(|error| invalid(&error))?;
    let output_queries = output_reference_queries(query_roots);
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
        super::check_deadline(deadline)?;
        query_outputs[query].push(output);
        let mut pending = source.query_output_nodes[output].clone();
        for root in &pending {
            may_produce_query_output[root.0] = true;
        }
        while let Some(candidate) = pending.pop() {
            super::check_deadline(deadline)?;
            if !potential[candidate.0][query] {
                potential[candidate.0][query] = true;
                pending.extend(
                    source.supplier_nodes_per_input[candidate.0]
                        .iter()
                        .flat_map(|input| input.iter().copied()),
                );
            }
        }
    }
    let mut candidates = Vec::new();
    let mut reader_constraints = Vec::new();
    let mut source_configurations = vec![Vec::new(); source_count];
    for source_id in 0..source_count {
        super::check_deadline(deadline)?;
        let node = &source.operator_nodes_of_candidates_for_memo_groups[source_id].physical_node;
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
                    query_time_results_retained,
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
                        query_time_results_retained,
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
        if !raw_data_retained && matches!(node.operator, Operator::NonASAP(NonASAPOp::Scan { .. }))
        {
            modes.retain(|(_, timing)| *timing == ExecutionTiming::IngestionTime);
        }
        for (materialization, timing) in modes {
            let id = PhysicalCandidateId(candidates.len());
            source_configurations[source_id].push(id);
            candidates.push(PhysicalCandidate {
                operator_node: OperatorNodeOfCandidateForMemoGroupId(source_id),
                timing,
                materialization,
                supplier_candidates_per_input: Vec::new(),
                may_produce_query_output: may_produce_query_output[source_id]
                    && timing == ExecutionTiming::QueryTime,
                may_keep_across_evaluations_per_sole_reader_query: may_keep_with_sole_reader(
                    node,
                    materialization,
                    entries,
                    data,
                    query_time_results_retained,
                ),
            });
            if materialization == Materialization::MatAtQuery(Medium::Memory) {
                if !summary {
                    reader_constraints.push(ReaderConstraint::HighestSharedQueryMaterialization {
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
                        summary_mode_allowed(
                            node,
                            &[query],
                            materialization,
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
                        if !summary_mode_allowed(
                            node,
                            &[left, right],
                            materialization,
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
                if compatible_timing(timing, &candidates[producer.0]) {
                    compatible.push(producer);
                }
            }
            by_timing[timing_index] = supplier_sets.intern(compatible);
        }
        timing_index.push(by_timing);
    }
    let mut input_cache: HashMap<(SupplierCandidates, ExecutionTiming), SupplierCandidates> =
        HashMap::new();
    for candidate in &mut candidates {
        super::check_deadline(deadline)?;
        let mut requirements = Vec::new();
        for input in &source.supplier_nodes_per_input[candidate.operator_node.0] {
            let key = (input.clone(), candidate.timing);
            let compatible_producers = if let Some(suppliers) = input_cache.get(&key) {
                suppliers.clone()
            } else {
                let timing = match candidate.timing {
                    ExecutionTiming::QueryTime => 0,
                    ExecutionTiming::IngestionTime => 1,
                };
                let mut compatible = Vec::new();
                for producer in input {
                    super::check_deadline(deadline)?;
                    compatible.extend(timing_index[producer.0][timing].iter().copied());
                }
                let suppliers = supplier_sets.intern(compatible);
                input_cache.insert(key, suppliers.clone());
                suppliers
            };
            requirements.push(compatible_producers);
        }
        candidate.supplier_candidates_per_input = requirements;
    }
    let result = PhysicalASAPMemoCandidates::new(
        source.operator_nodes_of_candidates_for_memo_groups.clone(),
        candidates,
        query_roots.to_vec(),
        reader_constraints,
    );
    super::validate_candidates(&result).map_err(|error| invalid(&error))?;
    // Repeated identical workload entries and identical logical outputs have
    // identical timing, accuracy and pricing requirements. Their request counts
    // remain in the evaluator, but they do not require distinct binding scopes.
    let mut context_ids = std::collections::HashMap::new();
    let mut output_contexts = Vec::new();
    for (output, root) in source.query_output_nodes.iter().enumerate() {
        let suppliers: Vec<_> = root.iter().map(|id| id.0).collect();
        let key = serde_json::to_string(&(&entries[output_queries[output]], suppliers))
            .map_err(|error| invalid(&error.to_string()))?;
        let next = context_ids.len();
        output_contexts.push(*context_ids.entry(key).or_insert(next));
    }
    let mut uses = vec![std::collections::HashSet::new(); source_count];
    for (consumer, inputs) in source.supplier_nodes_per_input.iter().enumerate() {
        super::check_deadline(deadline)?;
        let group = source.operator_nodes_of_candidates_for_memo_groups[consumer].group();
        for (position, input) in inputs.iter().enumerate() {
            for producer in input {
                uses[producer.0].insert((group, position));
            }
        }
    }
    let mut root_uses = vec![std::collections::HashSet::new(); source_count];
    for (output, root) in source.query_output_nodes.iter().enumerate() {
        for candidate in root {
            root_uses[candidate.0].insert(output_contexts[output]);
        }
    }
    let original_uses: Vec<_> = uses
        .iter()
        .zip(root_uses)
        .map(|(uses, roots)| uses.len() + roots.len())
        .collect();
    super::scopes::expand_execution_scopes_before(
        result,
        &original_uses,
        &output_contexts,
        deadline,
    )
}

pub fn compatible_timing(consumer_timing: ExecutionTiming, producer: &PhysicalCandidate) -> bool {
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
    query_time_results_retained: bool,
) -> bool {
    let Ok(eligibility) = summary_materialization_eligibility(
        node,
        readers,
        entries,
        data,
        query_time_results_retained,
    ) else {
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
