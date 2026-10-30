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
use std::rc::Rc;

use crate::summary_materialization_eligibility;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode};
use asap_types::physical::{Materialization, Medium};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use asap_types::stages::{
    CandidateConfiguration, CandidateConfigurationInventory, CandidateDependencyGraph,
    CandidateInventory, PhysicalCandidate, PhysicalCandidateId, ProducerRequirement,
    QueryRootCandidates, ReaderConstraint,
};

/// [`CandidateConfiguration::may_keep_with_sole_reader`] for `node` configured
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
    #[error("source resource constraints require configuration-specific accounting")]
    UnpricedResources,
    #[error("candidate {candidate} already has timing or materialization assigned")]
    AssignedExecution { candidate: usize },
}

pub fn enumerate_candidate_configurations_before(
    source: &CandidateInventory,
    output_queries: &[usize],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    query_time_results_retained: bool,
    raw_data_retained: bool,
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
            let id = PhysicalCandidateId(result.configurations.len());
            source_configurations[source_id].push(id);
            result.configurations.push(CandidateConfiguration {
                source_candidate: PhysicalCandidateId(source_id),
                materialization,
                timing,
                may_keep_with_sole_reader: may_keep_with_sole_reader(
                    node,
                    materialization,
                    entries,
                    data,
                    query_time_results_retained,
                ),
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
                            query_time_results_retained,
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
                            query_time_results_retained,
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
    super::scopes::expand_execution_scopes_before(
        result,
        &original_uses,
        &output_contexts,
        deadline,
    )
}

pub fn compatible_timing(
    consumer_timing: ExecutionTiming,
    producer: &CandidateConfiguration,
) -> bool {
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
