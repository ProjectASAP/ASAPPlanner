//! Unit tests of Stage 2's configuration enumeration
//! (`asap_physical_asap_optimization::catalog`) that select with the solver.
use std::rc::Rc;

use asap_logical_asap_optimization::deployment_capabilities::DeploymentCapabilities;
use asap_physical_asap_optimization::catalog::test_support::{
    compatible_timing, enumerate_candidate_configurations_before,
    may_keep_across_evaluations_per_sole_reader_query, CandidateInventory,
};
use asap_physical_asap_optimization::catalog::ConfigurationError;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot, ScalarExpr};
use asap_types::physical::{Materialization, Medium};
use asap_types::stages::{
    CandidateForMemoGroupId, MemoGroupId, OperatorNodeOfCandidateForMemoGroup, PhysicalCandidateId,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::search::candidates::dag::PhysicalCandidatesWithCosts;

/// [`enumerate_candidate_configurations_before`] with no deadline, its
/// output references those of `source`, owned by the queries
/// `output_queries[reference]` ([`query_roots_for_tests`]), with the
/// engine's execution-scope copies and an unpriced DAG. Multiple references of one scalar query count once for shared-query
/// materialization.
#[cfg(test)]
pub(in crate::search) fn enumerate_candidate_configurations(
    source: &CandidateInventory,
    output_queries: &[usize],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    deployment: &DeploymentCapabilities,
) -> Result<PhysicalCandidatesWithCosts, ConfigurationError> {
    enumerate_candidate_configurations_before(
        source,
        &query_roots_for_tests(source, output_queries, entries.len()),
        entries,
        data,
        deployment.query_time_results_retained,
        deployment.raw_data_retained,
        None,
    )
    .map(|physical| {
        // As `select_plan` does: the engine's execution-scope copies.
        crate::search::candidates::scopes::expand_execution_scopes(&physical, entries, None)
            .expect("expansion without a deadline")
            .configured
    })
}

/// A hand-built inventory: node `i` runs `nodes[i].0` and reads, per input,
/// the nodes `nodes[i].1`; output reference `r` may be produced by the nodes
/// `query_output_nodes[r]`. The nodes each distinct output list names are the
/// implementation roots of one group (1, 2, … in order of first appearance);
/// every other node is a root of group 0.
#[cfg(test)]
pub(in crate::search) fn inventory_for_tests(
    nodes: Vec<(std::rc::Rc<OperatorNode>, Vec<Vec<PhysicalCandidateId>>)>,
    query_output_nodes: Vec<Vec<PhysicalCandidateId>>,
) -> CandidateInventory {
    let mut lists: Vec<Vec<PhysicalCandidateId>> = Vec::new();
    let mut group_of = vec![0; nodes.len()];
    for list in &query_output_nodes {
        let mut sorted = list.clone();
        sorted.sort();
        if !lists.contains(&sorted) {
            lists.push(sorted.clone());
        }
        let group = lists.iter().position(|known| *known == sorted).unwrap() + 1;
        for id in list {
            assert!(
                group_of[id.0] == 0 || group_of[id.0] == group,
                "test inventory gives node {} two different output lists",
                id.0
            );
            group_of[id.0] = group;
        }
    }
    let mut implementation_roots_of_group = vec![Vec::new(); lists.len() + 1];
    for (id, &group) in group_of.iter().enumerate() {
        implementation_roots_of_group[group].push(PhysicalCandidateId(id));
    }
    let (operator_nodes, suppliers): (Vec<_>, Vec<_>) = nodes
        .into_iter()
        .enumerate()
        .map(|(id, (node, inputs))| {
            (
                OperatorNodeOfCandidateForMemoGroup {
                    candidates_for_memo_group: vec![CandidateForMemoGroupId {
                        group: MemoGroupId(group_of[id] as u32),
                        index: id,
                    }],
                    is_implementation_root: true,
                    physical_node: node,
                },
                inputs.into_iter().map(Into::into).collect(),
            )
        })
        .unzip();
    CandidateInventory {
        operator_nodes_of_candidates_for_memo_groups: operator_nodes,
        producer_nodes_per_input: suppliers,
        query_output_nodes,
        implementation_roots_of_group,
    }
}

/// The roots of `query_count` queries over `source`'s groups: query `q`
/// reads, in order, the group of each output reference `r` with
/// `output_queries[r] == q` (a scalar root unless it reads exactly one); an
/// output no node may produce reads a group with no implementation roots.
#[cfg(test)]
pub(in crate::search) fn query_roots_for_tests(
    source: &CandidateInventory,
    output_queries: &[usize],
    query_count: usize,
) -> Vec<QueryRoot<MemoGroupId>> {
    let mut groups = vec![Vec::new(); query_count];
    for (output, &query) in output_queries.iter().enumerate() {
        groups[query].push(
            source.query_output_nodes[output]
                .first()
                .map_or(MemoGroupId(u32::MAX), |node| {
                    source.operator_nodes_of_candidates_for_memo_groups[node.0].group()
                }),
        );
    }
    groups
        .into_iter()
        .map(|groups| match groups.as_slice() {
            [group] => QueryRoot::Operator(*group),
            _ => QueryRoot::Scalar(ScalarExpr::FunctionCall {
                name: "refs".into(),
                args: groups
                    .into_iter()
                    .map(ScalarExpr::PromqlScalarFromVector)
                    .collect(),
            }),
        })
        .collect()
}

/// Test fixtures build candidates by hand; this fills each one's
/// `may_keep_across_evaluations_per_sole_reader_query` from its operator
/// node as [`enumerate_candidate_configurations_before`] does.
#[cfg(test)]
pub(in crate::search) fn fill_may_keep_across_evaluations_per_sole_reader_query(
    configured: &mut PhysicalCandidatesWithCosts,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    query_time_results_retained: bool,
) {
    let nodes = configured.nodes_for_tests();
    for (candidate, node) in configured.physical.candidates.iter_mut().zip(&nodes) {
        candidate.may_keep_across_evaluations_per_sole_reader_query =
            may_keep_across_evaluations_per_sole_reader_query(
                node,
                candidate.materialization,
                entries,
                data,
                query_time_results_retained,
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::candidates::inventory::{build_candidate_inventory, InventoryForTests};
    use crate::search::milp::model::select_candidates;
    use asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::pass2::memo::candidates::try_populate_pass1;
    use asap_logical_asap_optimization::pass2::memo::shared_configurations::PaneWidthDomain;
    use asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups;
    use asap_logical_asap_optimization::test_support::lower_promql;
    use asap_logical_asap_optimization::test_support::sharing_rules_and_shared_state_groups;
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
                &query_roots_for_tests(&source, &[0, 1], entries.len()),
                &entries,
                &data,
                DeploymentCapabilities::default().query_time_results_retained,
                DeploymentCapabilities::default().raw_data_retained,
                Some(std::time::Instant::now())
            ),
            Err(ConfigurationError::DeadlineExceeded(_))
        ));
    }

    fn fixture() -> (InventoryForTests, Vec<QueryWorkloadEntry>, DataWorkload) {
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
        let candidates = generate_per_computation_candidates(
            &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
                &roots,
            ),
        )
        .unwrap();
        let memo = sharing_rules_and_shared_state_groups(
            try_populate_pass1(initial, &candidates).unwrap(),
            PaneWidthDomain::Complete,
        )
        .unwrap();
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
    // storing repeated producer lists once, including query/ingestion modes.
    #[test]
    fn producer_index_preserves_all_edges_and_shares_storage() {
        let (source, entries, data) = fixture();
        let expanded = enumerate_candidate_configurations(
            &source,
            &[0, 1],
            &entries,
            &data,
            &DeploymentCapabilities::default(),
        )
        .unwrap();
        for (consumer, configuration) in expanded.physical.candidates.iter().enumerate() {
            let original = &source.graph.candidates[configuration.operator_node.0];
            let generated = &expanded.dag.candidates[consumer];
            assert_eq!(
                generated.producer_candidates_per_input.len(),
                original.producer_candidates_per_input.len()
            );
            for (original, generated) in original
                .producer_candidates_per_input
                .iter()
                .zip(&generated.producer_candidates_per_input)
            {
                let expected: Vec<_> = original
                    .candidates
                    .iter()
                    .flat_map(|source| {
                        expanded.physical.candidates.iter().enumerate().filter_map(
                            move |(id, producer)| {
                                (producer.operator_node.0 == source.0
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
                assert_eq!(generated.candidates.as_slice(), expected.as_slice());
            }
        }
        let metrics = asap_types::stages::producer_storage_metrics(&expanded.physical);
        let requirements = expanded
            .dag
            .candidates
            .iter()
            .map(|candidate| candidate.producer_candidates_per_input.len())
            .sum::<usize>();
        assert!(metrics.unique_producer_sets < requirements, "{metrics:?}");
        assert!(
            metrics.stored_producer_ids < metrics.logical_connections,
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
                .map(|candidate| candidate.producer_candidates_per_input.len())
                .sum::<usize>();
        assert!(
            expanded.physical.candidates.len()
                <= 4 * source.graph.candidates.len() * original_positions.max(1)
        );
        assert!(expanded
            .dag
            .candidates
            .iter()
            .all(|candidate| candidate.local_cost == 0.0));
        let builds = source
            .templates
            .iter()
            .enumerate()
            .filter_map(|(id, template)| kll(template).then_some(id))
            .collect::<Vec<_>>();
        assert!(
            builds.len() >= 3,
            "both local builds and explicit shared state survive"
        );
        for source in builds {
            let modes = expanded
                .physical
                .candidates
                .iter()
                .filter(|configuration| configuration.operator_node.0 == source)
                .map(|configuration| configuration.materialization)
                .collect::<Vec<_>>();
            assert!(modes.contains(&Materialization::NotMaterialized));
            assert!(modes.contains(&Materialization::MatAtQuery(Medium::Memory)));
            assert!(modes.contains(&Materialization::MatAtIngestion(Medium::Memory)));
            assert!(!expanded
                .physical
                .candidates
                .iter()
                .any(|configuration| configuration.operator_node.0 == source
                    && configuration.materialization == Materialization::NotMaterialized
                    && configuration.timing == ExecutionTiming::IngestionTime));
        }
        for (consumer, candidate) in expanded.dag.candidates.iter().enumerate() {
            for producer in candidate
                .producer_candidates_per_input
                .iter()
                .flat_map(|input| &input.candidates)
            {
                assert!(compatible_timing(
                    expanded.physical.candidates[consumer].timing,
                    &expanded.physical.candidates[producer.0]
                ));
            }
        }
        assert!(expanded
            .dag
            .query_roots
            .iter()
            .flat_map(|root| &root.candidates)
            .all(|id| expanded.physical.candidates[id.0].timing == ExecutionTiming::QueryTime));
    }

    // Identical logical readers need separate bindings when their whole-window
    // ingestion phases disagree. Every copy still passes the existing Stage 2.
    #[test]
    fn identical_quantiles_with_distinct_phases_have_separate_execution_scopes() {
        let (catalog, mut entries, data) = fixture();
        let root = catalog.graph.query_roots[0]
            .candidates
            .iter()
            .map(|id| &catalog.templates[id.0])
            .find(|node| OperatorNode::reachable(node).iter().any(|child| kll(child)))
            .unwrap()
            .clone();
        fn append(
            node: &Rc<OperatorNode>,
            nodes: &mut Vec<(Rc<OperatorNode>, Vec<Vec<PhysicalCandidateId>>)>,
        ) -> PhysicalCandidateId {
            let mut inputs = Vec::new();
            node.operator.map_children(|child| {
                inputs.push(vec![append(child, nodes)]);
                child.clone()
            });
            nodes.push((node.clone(), inputs));
            PhysicalCandidateId(nodes.len() - 1)
        }
        let mut nodes = Vec::new();
        let output = append(&root, &mut nodes);
        let source = inventory_for_tests(nodes, vec![vec![output]; 2]);
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
        let selected = select_candidates(&expanded.dag)
            .unwrap()
            .expect("both incompatible phases must be covered");
        let builds: Vec<_> = selected
            .selected_candidates
            .iter()
            .filter(|id| kll(expanded.node(id.0)))
            .collect();
        assert_eq!(builds.len(), 2);
        assert!(builds
            .iter()
            .all(|id| expanded.physical.candidates[id.0].materialization
                == Materialization::MatAtIngestion(Medium::Memory)));
        let mut modes = vec![None; expanded.physical.candidates.len()];
        for id in &selected.selected_candidates {
            modes[id.0] = Some(expanded.physical.candidates[id.0].materialization);
        }
        let query_roots = vec![QueryRoot::Operator(root); 2];
        let physical = asap_physical_asap_optimization::catalog::materialize_selected_candidates(
            &expanded.physical,
            &selected.to_selected_dag(),
            &selected.materializations_by_node(&modes),
            &query_roots,
            &entries,
            &data,
            deployment.query_time_results_retained,
        )
        .unwrap();
        physical.plan.dag.validate().unwrap();
        assert_eq!(physical.plan.roots.len(), 2);
    }

    // Repeating the same workload entry adds output references, not a quadratic
    // family of identical execution scopes and producer connections.
    #[test]
    fn repeated_identical_queries_keep_scope_and_connection_counts_constant() {
        let (mut source, entries, data) = fixture();
        let root = source.inventory.query_output_nodes[0].clone();
        let mut counts = Vec::new();
        for count in [2, 32] {
            source.inventory.query_output_nodes = vec![root.clone(); count];
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
                .dag
                .candidates
                .iter()
                .flat_map(|candidate| &candidate.producer_candidates_per_input)
                .map(|input| input.candidates.len())
                .sum();
            counts.push((expanded.physical.candidates.len(), edges));
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
            .physical
            .candidates
            .iter()
            .enumerate()
            .filter(|(id, _)| {
                matches!(
                    expanded.node(*id).operator,
                    asap_types::ir::Operator::NonASAP(asap_types::ir::NonASAPOp::Scan { .. })
                )
            })
            .collect::<Vec<_>>();
        assert!(!scans.is_empty());
        assert!(scans
            .iter()
            .all(|(_, config)| config.timing == ExecutionTiming::IngestionTime));
        assert!(select_candidates(&expanded.dag).unwrap().is_some());
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
        let selected = select_candidates(&expanded.dag).unwrap().unwrap();
        assert!(selected
            .selected_candidates
            .iter()
            .all(|id| expanded.physical.candidates[id.0].materialization
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
            .find(|id| kll(&source.templates[id.0]))
            .unwrap();
        let ingestion = expanded
            .physical
            .candidates
            .iter()
            .position(|configuration| {
                configuration.operator_node.0 == shared_build.0
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
                    .producer_candidates_per_input
                    .iter()
                    .any(|input| input.candidates.contains(shared_build))
            })
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        assert!(reader_sources.len() >= 2);
        for (id, configuration) in expanded.physical.candidates.iter().enumerate() {
            if reader_sources.contains(&configuration.operator_node.0)
                && configuration.timing == ExecutionTiming::QueryTime
            {
                for input in &mut expanded.dag.candidates[id].producer_candidates_per_input {
                    if input.candidates.iter().any(|producer| {
                        expanded.physical.candidates[producer.0].operator_node.0 == shared_build.0
                    }) {
                        // Query-specific readers have their original logical owner.
                        let group = |node: usize| {
                            source.operator_nodes_of_candidates_for_memo_groups[node].group()
                        };
                        let source_group = group(configuration.operator_node.0);
                        let is_first = source.graph.query_roots[0]
                            .candidates
                            .iter()
                            .any(|root| group(root.0) == source_group);
                        input.candidates.retain(|producer| {
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
        for (id, candidate) in expanded.dag.candidates.iter_mut().enumerate() {
            let source_id = expanded.physical.candidates[id].operator_node;
            candidate.local_cost = if source_id.0 == shared_build.0 {
                1.0
            } else if kll(&source.templates[source_id.0]) {
                50.0
            } else if matches!(
                &source.templates[source_id.0].asap(),
                Some(ASAPOp::SummaryAgg { .. })
            ) || matches!(
                &source.templates[source_id.0].non_asap(),
                Some(asap_types::ir::NonASAPOp::Aggregate { .. })
            ) {
                100.0
            } else {
                0.0
            };
        }
        let selected = select_candidates(&expanded.dag).unwrap().unwrap();
        assert!(selected
            .selected_candidates
            .contains(&PhysicalCandidateId(ingestion)));
        assert!(selected
            .selected_candidates
            .iter()
            .any(
                |id| expanded.physical.candidates[id.0].operator_node.0 == shared_build.0
                    && expanded.physical.candidates[id.0].timing == ExecutionTiming::QueryTime
            ));
    }
}
