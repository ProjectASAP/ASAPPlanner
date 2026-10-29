//! Apply an explicit selected materialization assignment through real Stage 2.
//!
//! No fallback modes are chosen. Physical rewrites or timing splits that change
//! candidate identity are rejected until represented by physical enumeration.
//! This validates execution legality, not cost estimates or accuracy targets.
use std::collections::HashMap;
use std::rc::Rc;

use asap_logical_asap_optimization::deployment_capabilities::DeploymentCapabilities;
use asap_physical_asap_optimization::{optimize_physical_asap_chosen, PhysicalWorkloadPlan};
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::physical::Materialization;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::search::candidates::graph::SelectedCandidateGraph;
use crate::search::candidates::inventory::CandidateInventory;
use crate::search::candidates::reconstruction::{
    output_contract_satisfied, paired_node_identities, reconstruct_selected_candidates,
};

/// Only rejected assignments are safe to exclude from the finite search.
/// Adapter/identity failures must stop with diagnostics, not become no-good cuts.
#[derive(Debug, thiserror::Error)]
pub(in crate::search) enum PhysicalCandidateError {
    #[error("selected physical assignment is infeasible: {0}")]
    RejectedAssignment(String),
    #[error("invalid or unsupported physical reconstruction: {0}")]
    InvalidOrUnsupported(String),
}

impl From<String> for PhysicalCandidateError {
    fn from(message: String) -> Self {
        Self::InvalidOrUnsupported(message)
    }
}
impl From<&str> for PhysicalCandidateError {
    fn from(message: &str) -> Self {
        Self::InvalidOrUnsupported(message.into())
    }
}

pub(in crate::search) struct SelectedPhysicalCandidate {
    pub physical: PhysicalWorkloadPlan,
    pub nodes_by_candidate: Vec<Option<Rc<OperatorNode>>>,
}

/// Output references must follow `map_operator_refs` traversal of the original
/// roots in workload order (one reference for an operator root). Repeated scalar
/// references remain separate positions; literal-only scalar roots use none.
pub(in crate::search) fn materialize_selected_candidates(
    inventory: &CandidateInventory,
    selected: &SelectedCandidateGraph,
    materializations: &[Option<Materialization>],
    original_roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    deployment: &DeploymentCapabilities,
) -> Result<SelectedPhysicalCandidate, PhysicalCandidateError> {
    if entries.len() != original_roots.len() {
        return Err("workload entries and query roots differ in length".into());
    }
    let rebuilt =
        reconstruct_selected_candidates(&inventory.graph, selected, &inventory.templates)?;
    if materializations.len() != rebuilt.nodes_by_candidate.len() {
        return Err("materialization vector must be indexed by candidate ID".into());
    }
    let mut ids_by_pointer = HashMap::new();
    for (id, node) in rebuilt.nodes_by_candidate.iter().enumerate() {
        if node.is_some() != materializations[id].is_some() {
            return Err(format!(
                "candidate {id} activation and materialization assignment disagree"
            )
            .into());
        }
        if let Some(node) = node {
            ids_by_pointer.insert(Rc::as_ptr(node), id);
        }
    }
    let roots = restore_query_roots(original_roots, &rebuilt.outputs)?;
    let mut assignment_error = None;
    let mut seen = vec![false; materializations.len()];
    let physical = optimize_physical_asap_chosen(
        "selected-candidate-connections",
        &roots,
        entries,
        data,
        deployment,
        &mut |logical, _, options| {
            let requested = if let Some(&id) = ids_by_pointer.get(&logical) {
                seen[id] = true;
                materializations[id].unwrap()
            } else {
                Materialization::NotMaterialized
            };
            if !options.contains(&requested) {
                let message = format!(
                    "selected materialization {requested:?} is not offered for candidate {:?}",
                    ids_by_pointer.get(&logical)
                );
                assignment_error = Some(if ids_by_pointer.contains_key(&logical) {
                    PhysicalCandidateError::RejectedAssignment(message)
                } else {
                    PhysicalCandidateError::InvalidOrUnsupported(message)
                });
            }
            requested
        },
    )
    .map_err(|error| error.to_string())?;
    if let Some(error) = assignment_error {
        return Err(error);
    }
    let physical = physical.ok_or_else(|| {
        PhysicalCandidateError::RejectedAssignment(
            "selected materialization assignment violates Stage 2 timing or dependency rules"
                .into(),
        )
    })?;
    physical.dag.validate().map_err(|error| error.to_string())?;
    let physical_outputs = ordered_output_references(&physical.roots);
    let identities = paired_node_identities(&rebuilt.outputs, &physical_outputs)?;
    for (id, mode) in materializations.iter().enumerate() {
        if mode.is_some_and(|mode| mode != Materialization::NotMaterialized) && !seen[id] {
            // The decoder already established candidate identity. Stage 2 did
            // not offer this selected node (e.g. a lower shared non-summary),
            // so the assignment is infeasible rather than an adapter failure.
            return Err(PhysicalCandidateError::RejectedAssignment(format!(
                "candidate {id} is not an eligible materialization node in the selected graph"
            )));
        }
    }
    let nodes_by_candidate = rebuilt
        .nodes_by_candidate
        .into_iter()
        .enumerate()
        .map(|(id, node)| {
            node.map(|node| {
                let physical = identities
                    .get(&Rc::as_ptr(&node))
                    .cloned()
                    .ok_or_else(|| format!("candidate {id} has no physical identity"))?;
                if Some(physical.materialization) != materializations[id] {
                    return Err(format!(
                        "candidate {id} materialization changed during physical reconstruction"
                    ));
                }
                Ok(physical)
            })
            .transpose()
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(SelectedPhysicalCandidate {
        physical,
        nodes_by_candidate,
    })
}

fn ordered_output_references(roots: &[QueryRoot]) -> Vec<Rc<OperatorNode>> {
    let mut references = Vec::new();
    for root in roots {
        match root {
            QueryRoot::Operator(node) => references.push(Rc::clone(node)),
            QueryRoot::Scalar(expression) => {
                expression.map_operator_refs(&mut |node| {
                    references.push(Rc::clone(node));
                    Rc::clone(node)
                });
            }
        }
    }
    references
}

fn restore_query_roots(
    original: &[QueryRoot],
    outputs: &[Rc<OperatorNode>],
) -> Result<Vec<QueryRoot>, String> {
    let references = ordered_output_references(original);
    if references.len() != outputs.len() {
        return Err("output references do not match original query-root mapping".into());
    }
    for (reference, output) in references.iter().zip(outputs) {
        if !output_contract_satisfied(reference, output)? {
            return Err(format!(
                "selected output reference has incompatible result or schema: expected {:?} {:?}, received {:?} {:?}",
                reference.result_kind, reference.schema, output.result_kind, output.schema
            ));
        }
    }
    let mut next = outputs.iter();
    Ok(original
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(_) => QueryRoot::Operator(Rc::clone(next.next().unwrap())),
            QueryRoot::Scalar(expression) => QueryRoot::Scalar(
                expression.map_operator_refs(&mut |_| Rc::clone(next.next().unwrap())),
            ),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::candidates::inventory::build_candidate_inventory;
    use crate::search::milp::model::select_candidates;
    use asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::pass2::memo::candidates::try_populate_pass1;
    use asap_logical_asap_optimization::pass2::memo::state_groups::expand_concrete_summary_state_groups;
    use asap_logical_asap_optimization::pass2::memo::{LogicalMemo, MemoGroupId};
    use asap_logical_asap_optimization::test_support::lower_promql;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::{FieldDataType, SketchAlgorithm};
    use asap_types::ir::{ASAPOp, NonASAPOp};
    use asap_types::physical::Medium;
    use asap_types::workload::{
        DataArrival, Predictability, Query, QueryRecurrence, QueryRequirements, RepeatedDemand,
        RepetitionInterval,
    };

    fn is_kll(node: &OperatorNode) -> bool {
        matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. }) if *kind.algorithm() == SketchAlgorithm::Kll)
    }

    fn fixture() -> (
        CandidateInventory,
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
            .enumerate()
            .map(|(id, query)| (id, QueryRoot::Operator(lower_promql(query, target.clone()))))
            .collect::<Vec<_>>();
        let initial = LogicalMemo::from_roots(&roots);
        let output_groups = (0..2)
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
        let candidates = generate_per_computation_candidates(roots.clone()).unwrap();
        let memo =
            expand_concrete_summary_state_groups(try_populate_pass1(initial, &candidates).unwrap())
                .unwrap()
                .memo;
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
            roots.into_iter().map(|(_, root)| root).collect(),
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
    ) -> (CandidateInventory, Vec<QueryRoot>, Vec<QueryWorkloadEntry>) {
        use crate::search::candidates::graph::{
            CandidateDependencyGraph, PhysicalCandidate, PhysicalCandidateId, ProducerRequirement,
            QueryRootCandidates,
        };
        let logical = lower_promql("latency", AccuracyTarget::exact());
        let scan = OperatorNode::reachable(&logical)
            .into_iter()
            .find(|node| matches!(node.non_asap(), Some(NonASAPOp::Scan { .. })))
            .unwrap();
        let mut templates = vec![Some(Rc::clone(&scan))];
        let mut candidates = vec![PhysicalCandidate {
            group: MemoGroupId(0),
            local_cost: 0.0,
            producer_requirements: Vec::new(),
        }];
        if with_shared_parent {
            let range =
                OperatorNode::new_shared(asap_types::ir::Operator::NonASAP(NonASAPOp::TimeRange {
                    range: std::time::Duration::from_secs(60),
                    kind: asap_types::ir::TimeRangeKind::Range,
                    child: scan,
                }))
                .unwrap();
            templates.push(Some(range));
            candidates.push(PhysicalCandidate {
                group: MemoGroupId(1),
                local_cost: 0.0,
                producer_requirements: vec![ProducerRequirement {
                    compatible_producers: vec![PhysicalCandidateId(0)].into(),
                }],
            });
        }
        let root_id = templates.len() - 1;
        let roots = vec![QueryRoot::Operator(Rc::clone(templates[root_id].as_ref().unwrap())); 2];
        let inventory = CandidateInventory {
            graph: CandidateDependencyGraph {
                candidates,
                query_roots: vec![
                    QueryRootCandidates {
                        compatible_candidates: vec![PhysicalCandidateId(root_id)]
                    };
                    2
                ],
                ..Default::default()
            },
            templates,
            group_candidates: Vec::new(),
        };
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
            .configurations
            .iter()
            .any(|configuration| configuration.materialization == mode));
        for (candidate, configuration) in configured
            .inventory
            .graph
            .candidates
            .iter_mut()
            .zip(&configured.configurations)
        {
            candidate.local_cost = if configuration.materialization == mode {
                0.0
            } else {
                1.0
            };
        }
        let selected = select_candidates(&configured.inventory.graph)
            .unwrap()
            .unwrap();
        let mut assignments = vec![None; configured.configurations.len()];
        for id in &selected.selected_candidates {
            assignments[id.0] = Some(configured.configurations[id.0].materialization);
        }
        let physical = materialize_selected_candidates(
            &configured.inventory,
            &selected,
            &assignments,
            &roots,
            &entries,
            &data,
            &deployment,
        )
        .unwrap();
        assert_eq!(physical.physical.dag.nodes.len(), 1);
        assert_eq!(physical.physical.dag.nodes[0].materialization, mode);
        let same_query =
            enumerate_candidate_configurations(&source, &[0, 0], &entries[..1], &data, &deployment)
                .unwrap();
        assert!(!same_query
            .configurations
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
                &inventory,
                &selected,
                &modes,
                &roots,
                &entries,
                &DataWorkload::default(),
                &DeploymentCapabilities::default()
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
            .find(|id| is_kll(inventory.templates[id.0].as_ref().unwrap()))
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
                &inventory,
                &selected,
                &modes,
                &roots,
                &entries,
                &data,
                &DeploymentCapabilities::default(),
            )
            .unwrap();
            assert_eq!(
                result.nodes_by_candidate[build]
                    .as_ref()
                    .unwrap()
                    .materialization,
                mode
            );
            let outputs = ordered_output_references(&result.physical.roots);
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
                result.nodes_by_candidate[build].as_ref().unwrap()
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
            modes[id.0] = Some(if is_kll(inventory.templates[id.0].as_ref().unwrap()) {
                Materialization::MatAtIngestion(Medium::Memory)
            } else {
                Materialization::NotMaterialized
            });
        }
        let result = materialize_selected_candidates(
            &inventory,
            &selected,
            &modes,
            &roots,
            &entries,
            &data,
            &DeploymentCapabilities::default(),
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
