//! Unit tests of Stage 2's realization of a selected assignment
//! (`asap_physical_asap_optimization::test_support::catalog`) that select with the solver.
use std::rc::Rc;

use asap_physical_asap_optimization::test_support::catalog::{
    materialize_selected_candidates, ordered_output_references, output_contract_satisfied,
    restore_query_roots,
};
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::physical::Materialization;
use asap_types::stages::PhysicalCandidateError;

use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::candidates::inventory::{build_candidate_inventory, InventoryForTests};
    use crate::search::milp::model::select_candidates;
    use asap_logical_asap_optimization::test_support::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::lower_promql;
    use asap_logical_asap_optimization::test_support::sharing_rules_and_shared_state_groups;
    use asap_logical_asap_optimization::test_support::try_populate_pass1;
    use asap_logical_asap_optimization::DagOfLogicalMemoGroups;
    use asap_logical_asap_optimization::DeploymentCapabilities;
    use asap_logical_asap_optimization::PaneWidthDomain;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::{FieldDataType, SketchAlgorithm};
    use asap_types::ir::{ASAPOp, NonASAPOp};
    use asap_types::physical::Medium;
    use asap_types::stages::*;

    use asap_types::workload::{
        DataArrival, Predictability, Query, QueryRecurrence, QueryRequirements, RepeatedDemand,
        RepetitionInterval,
    };

    fn is_kll(node: &OperatorNode) -> bool {
        matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. }) if *kind.algorithm() == SketchAlgorithm::Kll)
    }

    fn fixture() -> (
        InventoryForTests,
        Vec<QueryRoot>,
        Vec<QueryWorkloadEntry>,
        DataWorkload,
    ) {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let queries = [
            "quantile_over_time(0.95, latency[5m])",
            "quantile_over_time(0.99, latency[5m])",
        ];
        let roots = queries
            .iter()
            .map(|query| QueryRoot::Operator(lower_promql(query, target.clone())))
            .collect::<Vec<_>>();
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let output_groups = (0..2)
            .map(|query| *initial.query_roots[query].as_operator().unwrap())
            .collect::<Vec<_>>();
        let candidates = generate_per_computation_candidates(
            &asap_logical_asap_optimization::DagOfLogicalMemoGroups::from_roots(&roots.clone()),
        )
        .unwrap();
        let memo = sharing_rules_and_shared_state_groups(
            try_populate_pass1(initial, &candidates).unwrap(),
            PaneWidthDomain::Complete,
        )
        .unwrap();
        let inventory = build_candidate_inventory(&memo, &output_groups, |node| {
            Ok(if is_kll(node) {
                8.0
            } else if matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. })) {
                80.0
            } else if matches!(node.asap(), Some(ASAPOp::SummaryEstimate { .. })) {
                1.0
            } else if matches!(node.non_asap(), Some(NonASAPOp::Aggregate { .. })) {
                100.0
            } else {
                0.0
            })
        })
        .unwrap();
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
            inventory,
            roots,
            entries,
            DataWorkload {
                arrival: DataArrival::Ingesting {
                    rows_per_second: 1000.0,
                },
                ..Default::default()
            },
        )
    }

    fn shared_non_summary_fixture(
        with_shared_parent: bool,
    ) -> (InventoryForTests, Vec<QueryRoot>, Vec<QueryWorkloadEntry>) {
        use asap_types::stages::PhysicalCandidateId;
        let logical = lower_promql("latency", AccuracyTarget::exact());
        let scan = OperatorNode::reachable(&logical)
            .into_iter()
            .find(|node| matches!(node.non_asap(), Some(NonASAPOp::Scan { .. })))
            .unwrap();
        let mut nodes = vec![(Rc::clone(&scan), Vec::new())];
        if with_shared_parent {
            let range =
                OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::TimeRange {
                    range: std::time::Duration::from_secs(60),
                    kind: asap_types::ir::TimeRangeKind::Range,
                    child: scan,
                }))
                .unwrap();
            nodes.push((range, vec![vec![PhysicalCandidateId(0)]]));
        }
        let root_id = nodes.len() - 1;
        let roots = vec![QueryRoot::Operator(Rc::clone(&nodes[root_id].0)); 2];
        let inventory = InventoryForTests::new(
            crate::search::candidates::configurations::inventory_for_tests(
                nodes,
                vec![vec![PhysicalCandidateId(root_id)]; 2],
            ),
            |_| Ok(0.0),
        );
        let entries = (0..2)
            .map(|_| QueryWorkloadEntry {
                query: Query("latency".into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyTarget::exact(),
                    max_latency_ms: None,
                },
                predictability: Predictability::AdHoc,
                recurrence: QueryRecurrence::Unknown,
                range_evaluation: None,
            })
            .collect();
        (inventory, roots, entries)
    }

    // A real shared scan can be built once per evaluation for two distinct queries.
    #[test]
    fn shared_scan_enumerates_and_executes_query_materialization() {
        use crate::search::candidates::configurations::enumerate_candidate_configurations;
        let (source, roots, entries) = shared_non_summary_fixture(false);
        let data = DataWorkload::default();
        let deployment = DeploymentCapabilities::default();
        let mut configured =
            enumerate_candidate_configurations(&source, &[0, 1], &entries, &data, &deployment)
                .unwrap();
        let mode = Materialization::MatAtQuery(Medium::Memory);
        assert!(configured
            .physical
            .candidates
            .iter()
            .any(|configuration| configuration.materialization == mode));
        for (candidate, configuration) in configured
            .dag
            .candidates
            .iter_mut()
            .zip(&configured.physical.candidates)
        {
            candidate.local_cost = if configuration.materialization == mode {
                0.0
            } else {
                1.0
            };
        }
        let selected = select_candidates(&configured.dag).unwrap().unwrap();
        let mut assignments = vec![None; configured.physical.candidates.len()];
        for id in &selected.selected_candidates {
            assignments[id.0] = Some(configured.physical.candidates[id.0].materialization);
        }
        let physical = materialize_selected_candidates(
            &configured.physical,
            &selected.to_selected_dag(),
            &selected.materializations_by_node(&assignments),
            &roots,
            &entries,
            &data,
            deployment.query_time_results_retained,
        )
        .unwrap();
        assert_eq!(physical.plan.dag.nodes.len(), 1);
        assert_eq!(physical.plan.dag.nodes[0].materialization, mode);
        let same_query =
            enumerate_candidate_configurations(&source, &[0, 0], &entries[..1], &data, &deployment)
                .unwrap();
        assert!(!same_query
            .physical
            .candidates
            .iter()
            .any(|configuration| configuration.materialization == mode));
    }

    // A shared scan below an already shared range is not a highest shared node;
    // reject that selected assignment using Stage 2's exact eligibility result.
    #[test]
    fn query_materialization_below_shared_parent_is_rejected_assignment() {
        let (inventory, roots, entries) = shared_non_summary_fixture(true);
        let selected = select_candidates(&inventory.graph).unwrap().unwrap();
        let modes = vec![
            Some(Materialization::MatAtQuery(Medium::Memory)),
            Some(Materialization::NotMaterialized),
        ];
        assert!(matches!(
            materialize_selected_candidates(
                &inventory.configured_for_tests().physical,
                &selected.to_selected_dag(),
                &selected.materializations_by_node(&modes),
                &roots,
                &entries,
                &DataWorkload::default(),
                false
            ),
            Err(PhysicalCandidateError::RejectedAssignment(_))
        ));
    }

    // Exact selected modes survive Stage 2 and retain one producer identity.
    #[test]
    fn shared_kll_keeps_candidate_identity_at_query_and_ingestion() {
        let (inventory, roots, entries, data) = fixture();
        let selected = select_candidates(&inventory.graph).unwrap().unwrap();
        let build = selected
            .selected_candidates
            .iter()
            .find(|id| is_kll(&inventory.templates[id.0]))
            .unwrap()
            .0;
        for mode in [
            Materialization::MatAtQuery(Medium::Memory),
            Materialization::MatAtIngestion(Medium::Memory),
        ] {
            let mut modes = vec![None; inventory.graph.candidates.len()];
            for id in &selected.selected_candidates {
                modes[id.0] = Some(Materialization::NotMaterialized);
            }
            modes[build] = Some(mode);
            let result = materialize_selected_candidates(
                &inventory.configured_for_tests().physical,
                &selected.to_selected_dag(),
                &selected.materializations_by_node(&modes),
                &roots,
                &entries,
                &data,
                false,
            )
            .unwrap();
            assert_eq!(
                result.operator_node_of_selected_node
                    [selected.node_of(PhysicalCandidateId(build)).unwrap().0]
                    .materialization,
                mode
            );
            let outputs = ordered_output_references(&result.plan.roots);
            let states = outputs
                .iter()
                .map(|root| {
                    OperatorNode::reachable(root)
                        .into_iter()
                        .find(|node| is_kll(node))
                        .unwrap()
                })
                .collect::<Vec<_>>();
            assert!(Rc::ptr_eq(&states[0], &states[1]));
            assert!(Rc::ptr_eq(
                &states[0],
                &result.operator_node_of_selected_node
                    [selected.node_of(PhysicalCandidateId(build)).unwrap().0]
            ));
        }
    }

    // An inadmissible reader must fail the requested mode, not trigger fallback.
    #[test]
    fn ingestion_assignment_rejects_unpredictable_selected_reader() {
        let (inventory, roots, mut entries, data) = fixture();
        entries[1].predictability = Predictability::AdHoc;
        let selected = select_candidates(&inventory.graph).unwrap().unwrap();
        let mut modes = vec![None; inventory.graph.candidates.len()];
        for id in &selected.selected_candidates {
            modes[id.0] = Some(if is_kll(&inventory.templates[id.0]) {
                Materialization::MatAtIngestion(Medium::Memory)
            } else {
                Materialization::NotMaterialized
            });
        }
        let result = materialize_selected_candidates(
            &inventory.configured_for_tests().physical,
            &selected.to_selected_dag(),
            &selected.materializations_by_node(&modes),
            &roots,
            &entries,
            &data,
            false,
        );
        assert!(
            matches!(result, Err(PhysicalCandidateError::RejectedAssignment(error)) if error.contains("not offered"))
        );
    }

    // Only stronger completeness evidence may differ from the declared output.
    #[test]
    fn output_contract_allows_completeness_refinement_but_preserves_fields_and_keys() {
        let expected = lower_promql("latency", AccuracyTarget::exact());
        assert!(!expected.schema.closed);
        let mut supplied = (*expected).clone();
        supplied.schema.closed = true;
        assert!(output_contract_satisfied(&expected, &supplied).unwrap());
        assert!(!output_contract_satisfied(&supplied, &expected).unwrap());
        supplied.schema.fields[0].nullable = !supplied.schema.fields[0].nullable;
        assert!(!output_contract_satisfied(&expected, &supplied).unwrap());
        supplied = (*expected).clone();
        supplied.schema.time_index = None;
        assert!(!output_contract_satisfied(&expected, &supplied).unwrap());
        supplied = (*expected).clone();
        supplied.schema.unique_keys.push(vec![0]);
        assert!(output_contract_satisfied(&expected, &supplied).unwrap());
        assert!(!output_contract_satisfied(&supplied, &expected).unwrap());
        supplied = (*expected).clone();
        supplied.schema.fields[0].name.push_str("_different");
        assert!(!output_contract_satisfied(&expected, &supplied).unwrap());
    }

    // Scalar restoration uses mapper order and preserves repeated and literal-only references.
    #[test]
    fn scalar_output_mapping_preserves_positions() {
        use asap_types::ir::scalar::{ArithmeticOpKind, ScalarValue};
        use asap_types::ir::{ExprSemantics, ScalarExpr};
        let root = lower_promql("latency", AccuracyTarget::exact());
        let reference = ScalarExpr::PromqlScalarFromVector(Rc::clone(&root));
        let original = vec![
            QueryRoot::Scalar(ScalarExpr::Arithmetic {
                op: ArithmeticOpKind::Add,
                left: Box::new(reference.clone()),
                right: Box::new(reference),
                semantics: ExprSemantics::Promql,
            }),
            QueryRoot::Scalar(ScalarExpr::Literal(ScalarValue::Float64(7.0))),
        ];
        let first = lower_promql("latency", AccuracyTarget::exact());
        let second = lower_promql("latency", AccuracyTarget::exact());
        let restored =
            restore_query_roots(&original, &[Rc::clone(&first), Rc::clone(&second)]).unwrap();
        let references = ordered_output_references(&restored);
        assert_eq!(references.len(), 2);
        assert!(Rc::ptr_eq(&references[0], &first));
        assert!(Rc::ptr_eq(&references[1], &second));
        assert_eq!(restored[1], original[1]);
        assert!(restore_query_roots(&original, &[first]).is_err());
    }
}
