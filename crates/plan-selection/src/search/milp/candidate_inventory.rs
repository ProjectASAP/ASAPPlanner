//! Opt-in bridge from logical candidate memo to immediate candidate connections.
//!
//! The caller supplies node-local costs for one fixed accounting context. These
//! are not runtime cost estimates. Consumer-dependent demand, composed accuracy validation,
//! physical materialization assignments, and legacy coupling constraints remain
//! unsupported and are never silently erased by this adapter. This is a logical
//! inventory bridge for solver experiments. Private exact operators are lowered
//! before decomposition, but complete Stage 2 configuration validation remains
//! required.
//! A cost callback cannot certify execution legality: callers must run Stage 2
//! and validate the reconstructed plan before execution.
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_logical_asap_optimization::memo::candidates::CandidateMemo;
use asap_logical_asap_optimization::memo::MemoGroupId;
use asap_types::ir::OperatorNode;
use asap_types::physical::Materialization;

use super::candidate_connections::{
    CandidateDependencyGraph, PhysicalCandidate, PhysicalCandidateId, ProducerRequirement,
    QueryRootCandidates,
};
use super::candidate_reconstruction::CandidateOperatorTemplates;

#[derive(Debug)]
pub(super) struct CandidateInventory {
    pub graph: CandidateDependencyGraph,
    pub templates: CandidateOperatorTemplates,
    /// Root implementation IDs, excluding each implementation's private nodes.
    pub group_candidates: Vec<Vec<PhysicalCandidateId>>,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum CandidateInventoryError {
    #[error("invalid candidate memo: {0}")]
    InvalidMemo(String),
    #[error("unsupported candidate {candidate} in group {group}: {reason}")]
    Unsupported {
        group: usize,
        candidate: usize,
        reason: &'static str,
    },
    #[error(
        "physical operator implementation for candidate {candidate} in group {group}: {reason}"
    )]
    PhysicalImplementation {
        group: usize,
        candidate: usize,
        reason: String,
    },
    #[error("node-local fixed-context cost: {0}")]
    Cost(String),
}

/// Each output reference is an operator group, not necessarily an entire query.
/// Scalar query assembly and Stage 2/accuracy validation remain with the caller.
pub(super) fn build_candidate_inventory(
    memo: &CandidateMemo,
    output_groups: &[MemoGroupId],
    mut node_local_cost: impl FnMut(&OperatorNode) -> Result<f64, String>,
) -> Result<CandidateInventory, CandidateInventoryError> {
    let invalid = |message: &str| CandidateInventoryError::InvalidMemo(message.into());
    if memo.groups.len() != memo.candidates.len() {
        return Err(invalid("group and candidate vector lengths differ"));
    }
    if output_groups
        .iter()
        .any(|group| group.0 as usize >= memo.groups.len())
    {
        return Err(invalid("query output references missing group"));
    }
    let mut inventory = CandidateInventory {
        graph: CandidateDependencyGraph::default(),
        templates: Vec::new(),
        group_candidates: vec![Vec::new(); memo.groups.len()],
    };
    for (group, candidates) in memo.candidates.iter().enumerate() {
        for (index, candidate) in candidates.iter().enumerate() {
            let unsupported = |reason| CandidateInventoryError::Unsupported {
                group,
                candidate: index,
                reason,
            };
            if candidate.implements.first() != Some(&MemoGroupId(group as u32)) {
                return Err(unsupported("candidate output belongs to a different group"));
            }
            let implemented: HashSet<_> = candidate.implements.iter().copied().collect();
            if implemented.len() != candidate.implements.len()
                || implemented
                    .iter()
                    .any(|id| id.0 as usize >= memo.groups.len())
            {
                return Err(invalid(
                    "candidate names duplicate or missing absorbed groups",
                ));
            }
            // Only the first group is this candidate's output contract. Further
            // groups describe absorbed computations inside its private subtree,
            // not additional results it can supply. Private-node decomposition
            // charges that work; independent consumers of an absorbed group
            // still select one of that group's own candidates. Reverse support
            // leaves unused absorbed groups inactive without exclusion rules.
            if !candidate.coupling_sites.is_empty()
                || !candidate.requires_sites.is_empty()
                || !candidate.requires_exact_children.is_empty()
                || candidate.rollup_of.is_some()
            {
                return Err(unsupported(
                    "legacy shared-decision or implementation constraints",
                ));
            }
            if candidate.budget_share == 0 {
                return Err(unsupported("zero accuracy budget share"));
            }
            if !candidate.materializations.is_empty() {
                return Err(unsupported("materialization eligibility restrictions"));
            }
            if candidate
                .children
                .iter()
                .any(|child| child.0 as usize >= memo.groups.len())
            {
                return Err(invalid("candidate references missing input group"));
            }
            let inputs = candidate
                .children
                .iter()
                .map(|child| Rc::clone(&memo.groups[child.0 as usize].node))
                .collect::<Vec<_>>();
            if inputs
                .iter()
                .any(|input| Rc::ptr_eq(input, &candidate.node))
            {
                return Err(unsupported(
                    "root aliases an input group without an operator",
                ));
            }
            let implemented = asap_physical_asap_optimization::implement_candidate_operators(
                &candidate.node,
                &inputs,
            )
            .map_err(|error| CandidateInventoryError::PhysicalImplementation {
                group,
                candidate: index,
                reason: error.to_string(),
            })?;
            let id = PhysicalCandidateId(inventory.graph.candidates.len());
            inventory.group_candidates[group].push(id);
            inventory.templates.push(Some(implemented));
            inventory.graph.candidates.push(PhysicalCandidate {
                group: MemoGroupId(group as u32),
                local_cost: 0.0,
                producer_requirements: Vec::new(),
            });
        }
    }
    for (group, candidates) in memo.candidates.iter().enumerate() {
        for (index, candidate) in candidates.iter().enumerate() {
            let mut boundaries = HashMap::new();
            for &child in &candidate.children {
                let pointer = Rc::as_ptr(&memo.groups[child.0 as usize].node);
                if boundaries
                    .insert(pointer, child)
                    .is_some_and(|previous| previous != child)
                {
                    return Err(invalid("one boundary node names different input groups"));
                }
            }
            if boundaries.contains_key(&Rc::as_ptr(&candidate.node)) {
                return Err(CandidateInventoryError::Unsupported {
                    group,
                    candidate: index,
                    reason: "root aliases an input group without an operator",
                });
            }
            let mut builder = ImmediateCandidateBuilder {
                inventory: &mut inventory,
                boundaries,
                private_nodes: HashMap::new(),
                used_boundaries: HashSet::new(),
                owner: MemoGroupId(group as u32),
                candidate_index: index,
                node_local_cost: &mut node_local_cost,
            };
            let root_id = builder.inventory.group_candidates[group][index];
            let implemented = builder.inventory.templates[root_id.0]
                .as_ref()
                .cloned()
                .ok_or_else(|| invalid("missing implemented root template"))?;
            builder.add(&implemented, Some(root_id))?;
            if candidate
                .children
                .iter()
                .any(|group| !builder.used_boundaries.contains(group))
            {
                return Err(invalid(
                    "declared input group has no canonical node boundary",
                ));
            }
        }
    }
    for group in output_groups {
        let expected = &memo.groups[group.0 as usize].node;
        let mut compatible_candidates = Vec::new();
        for &id in &inventory.group_candidates[group.0 as usize] {
            let template = inventory.templates[id.0]
                .as_ref()
                .ok_or_else(|| invalid("missing root template"))?;
            if compatible_output(expected, template)? {
                compatible_candidates.push(id);
            }
        }
        inventory.graph.query_roots.push(QueryRootCandidates {
            compatible_candidates,
        });
    }
    inventory
        .graph
        .validate()
        .map_err(|error| invalid(&error.to_string()))?;
    Ok(inventory)
}

struct ImmediateCandidateBuilder<'a, F> {
    inventory: &'a mut CandidateInventory,
    boundaries: HashMap<*const OperatorNode, MemoGroupId>,
    private_nodes: HashMap<*const OperatorNode, PhysicalCandidateId>,
    used_boundaries: HashSet<MemoGroupId>,
    owner: MemoGroupId,
    candidate_index: usize,
    node_local_cost: &'a mut F,
}

impl<F: FnMut(&OperatorNode) -> Result<f64, String>> ImmediateCandidateBuilder<'_, F> {
    fn add(
        &mut self,
        node: &Rc<OperatorNode>,
        reserved: Option<PhysicalCandidateId>,
    ) -> Result<PhysicalCandidateId, CandidateInventoryError> {
        if let Some(&existing) = self.private_nodes.get(&Rc::as_ptr(node)) {
            return Ok(existing);
        }
        if node.timing.is_some() || node.materialization != Materialization::NotMaterialized {
            return Err(CandidateInventoryError::Unsupported {
                group: self.owner.0 as usize,
                candidate: self.candidate_index,
                reason: "assigned execution timing or materialization",
            });
        }
        let mut inputs = Vec::new();
        node.operator.map_children(|child| {
            inputs.push(Rc::clone(child));
            Rc::clone(child)
        });
        let mut requirements = Vec::new();
        for input in inputs {
            let producers = if let Some(&group) = self.boundaries.get(&Rc::as_ptr(&input)) {
                self.used_boundaries.insert(group);
                let mut compatible = Vec::new();
                for &candidate in &self.inventory.group_candidates[group.0 as usize] {
                    let template =
                        self.inventory.templates[candidate.0]
                            .as_ref()
                            .ok_or_else(|| {
                                CandidateInventoryError::InvalidMemo("missing root template".into())
                            })?;
                    if compatible_output(&input, template)? {
                        compatible.push(candidate);
                    }
                }
                compatible
            } else {
                vec![self.add(&input, None)?]
            };
            requirements.push(ProducerRequirement {
                compatible_producers: producers,
            });
        }
        let cost = (self.node_local_cost)(node).map_err(CandidateInventoryError::Cost)?;
        if !cost.is_finite() || cost < 0.0 {
            return Err(CandidateInventoryError::Cost(
                "cost must be finite and nonnegative".into(),
            ));
        }
        let candidate = PhysicalCandidate {
            group: self.owner,
            local_cost: cost,
            producer_requirements: requirements,
        };
        let id = if let Some(id) = reserved {
            self.inventory.graph.candidates[id.0] = candidate;
            id
        } else {
            let id = PhysicalCandidateId(self.inventory.graph.candidates.len());
            self.inventory.graph.candidates.push(candidate);
            self.inventory.templates.push(Some(Rc::clone(node)));
            id
        };
        self.private_nodes.insert(Rc::as_ptr(node), id);
        Ok(id)
    }
}

fn compatible_output(
    expected: &OperatorNode,
    producer: &OperatorNode,
) -> Result<bool, CandidateInventoryError> {
    super::candidate_reconstruction::output_contract_satisfied(expected, producer)
        .map_err(CandidateInventoryError::InvalidMemo)
}

#[cfg(test)]
mod tests {
    use super::super::candidate_connections::select_candidates;
    use super::super::candidate_reconstruction::reconstruct_candidate_outputs;
    use super::*;
    use asap_logical_asap_optimization::memo::candidates::try_populate_pass1;
    use asap_logical_asap_optimization::memo::state_groups::expand_concrete_summary_state_groups;
    use asap_logical_asap_optimization::memo::LogicalMemo;
    use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::lower_promql;
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::{FieldDataType, SketchAlgorithm};
    use asap_types::ir::{ASAPOp, NonASAPOp, QueryRoot};

    fn quantile_memo() -> (CandidateMemo, Vec<MemoGroupId>) {
        let roots = [0.95, 0.99]
            .into_iter()
            .enumerate()
            .map(|(query, quantile)| {
                (
                    query,
                    QueryRoot::Operator(lower_promql(
                        &format!("quantile_over_time({quantile}, latency[5m])"),
                        AccuracyTarget::max_error(0.01).failure_probability(0.001),
                    )),
                )
            })
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
            .collect();
        let candidates = generate_per_computation_candidates(roots).unwrap();
        let memo = try_populate_pass1(initial, &candidates).unwrap();
        (
            expand_concrete_summary_state_groups(memo).unwrap().memo,
            output_groups,
        )
    }

    fn is_kll(node: &OperatorNode) -> bool {
        matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. }) if *kind.algorithm() == SketchAlgorithm::Kll)
    }

    // Synthetic fixed node costs favor one KLL producer; no runtime cost claim.
    #[test]
    fn expanded_quantiles_select_and_reconstruct_one_shared_kll() {
        let (memo, outputs) = quantile_memo();
        let inventory = build_candidate_inventory(&memo, &outputs, |node| {
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
        for (group, candidates) in memo.candidates.iter().enumerate() {
            assert_eq!(inventory.group_candidates[group].len(), candidates.len());
        }
        let selected = select_candidates(&inventory.graph).unwrap().unwrap();
        assert!((selected.modeled_cost - 10.0).abs() < 1e-8);
        let reconstructed =
            reconstruct_candidate_outputs(&inventory.graph, &selected, &inventory.templates)
                .unwrap();
        let builds: Vec<_> = reconstructed
            .iter()
            .map(|root| {
                OperatorNode::reachable(root)
                    .into_iter()
                    .find(|node| is_kll(node))
                    .unwrap()
            })
            .collect();
        assert!(Rc::ptr_eq(&builds[0], &builds[1]));
        // Legality is independently checked by the real physical stage; the
        // synthetic objective above is deliberately not an evaluator estimate.
        stage2_smoke(&reconstructed);
        assert_eq!(
            selected
                .selected_candidates
                .iter()
                .filter(|id| is_kll(inventory.templates[id.0].as_ref().unwrap()))
                .count(),
            1
        );
    }

    fn stage2_smoke(outputs: &[Rc<OperatorNode>]) {
        use asap_physical_asap_optimization::optimize_physical_asap_chosen;
        use asap_types::physical::Medium;
        use asap_types::workload::{
            DataArrival, DataWorkload, DurationMs, MetricType, Predictability, Query,
            QueryRecurrence, QueryRequirements, QueryWorkloadEntry, RepeatedDemand,
            RepetitionInterval, SourceFacts,
        };
        let entries: Vec<_> = [0.95, 0.99]
            .into_iter()
            .map(|quantile| QueryWorkloadEntry {
                query: Query(format!("quantile_over_time({quantile}, latency[5m])")),
                requirements: QueryRequirements {
                    accuracy: AccuracyTarget::max_error(0.01).failure_probability(0.001),
                    max_latency_ms: None,
                },
                predictability: Predictability::Predictable { known_at: None },
                recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                    RepetitionInterval(60_000),
                )),
                range_evaluation: None,
            })
            .collect();
        let data = DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 2_000.0 / 15.0,
            },
            sources: [(
                "latency".into(),
                SourceFacts {
                    series_count: Some(1_000),
                    metric_type: Some(MetricType::Gauge),
                    sample_interval: Some(DurationMs(15_000)),
                    ..SourceFacts::default()
                },
            )]
            .into(),
        };
        let roots: Vec<_> = outputs.iter().cloned().map(QueryRoot::Operator).collect();
        let physical = optimize_physical_asap_chosen(
            "candidate-inventory-smoke",
            &roots,
            &entries,
            &data,
            crate::PlanningModels::builtin().deployment_capabilities,
            &mut |_, _, options| {
                let query_materialization = Materialization::MatAtQuery(Medium::Memory);
                if options.contains(&query_materialization) {
                    query_materialization
                } else {
                    options
                        .first()
                        .copied()
                        .unwrap_or(Materialization::NotMaterialized)
                }
            },
        )
        .unwrap()
        .expect("reconstructed logical connections admit a legal Stage 2 plan");
        physical.dag.validate().unwrap();
        assert_eq!(physical.roots.len(), 2);
    }

    // Union-label pane sharing survives typed connection selection and real
    // Stage 2, while each query receives only its original output columns.
    #[test]
    fn widened_label_readers_materialize_one_shared_pane_chain() {
        check_shared_pane_plan(
            [
                "avg by (job) (quantile_over_time(0.95, latency[5m]))",
                "avg by (region) (quantile_over_time(0.99, latency[5m]))",
            ],
            [0.05, 0.05],
            &["job", "region"],
            None,
        );
    }

    // Different capacities and windows can share a larger chain while the short
    // query reads only its newest panes; Stage 2, accuracy and pricing all validate.
    #[test]
    fn different_capacity_and_horizon_readers_share_one_validated_pane_chain() {
        check_shared_pane_plan(
            [
                "avg by (job) (quantile_over_time(0.95, latency[5m]))",
                "avg by (job) (quantile_over_time(0.99, latency[15m]))",
            ],
            [0.03, 0.05],
            &["job"],
            Some(300),
        );
    }

    fn check_shared_pane_plan(
        queries: [&str; 2],
        errors: [f64; 2],
        labels: &[&str],
        newest: Option<u32>,
    ) {
        use super::super::candidate_configurations::enumerate_candidate_configurations;
        use super::super::candidate_physical::materialize_selected_candidates;
        use asap_logical_asap_optimization::test_support::entry_with;
        use asap_types::workload::{
            DataArrival, DataWorkload, Predictability, QueryRecurrence, RepeatedDemand,
            RepetitionInterval, SourceFacts,
        };
        let targets: Vec<_> = errors
            .into_iter()
            .map(|error| AccuracyTarget::max_error(error).failure_probability(0.3))
            .collect();
        let roots: Vec<_> = queries
            .into_iter()
            .enumerate()
            .map(|(id, query)| {
                (
                    id,
                    QueryRoot::Operator(lower_promql(query, targets[id].clone())),
                )
            })
            .collect();
        let initial = LogicalMemo::from_roots(&roots);
        let outputs: Vec<_> = (0..roots.len())
            .map(|query| {
                MemoGroupId(
                    initial
                        .groups
                        .iter()
                        .position(|group| group.roots_of.contains(&query))
                        .unwrap() as u32,
                )
            })
            .collect();
        let generated = generate_per_computation_candidates(roots.clone()).unwrap();
        let mut memo = try_populate_pass1(initial, &generated).unwrap();
        let entries: Vec<_> = (0..roots.len())
            .map(|query| {
                let mut entry = entry_with(
                    targets[query].clone(),
                    QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
                        1000,
                    ))),
                );
                entry.predictability = Predictability::Predictable { known_at: None };
                entry
            })
            .collect();
        crate::search::memo::rules::windows::add_local_window_composition(&mut memo, &entries);
        let memo = expand_concrete_summary_state_groups(memo).unwrap().memo;
        let source = build_candidate_inventory(&memo, &outputs, |_| Ok(0.0)).unwrap();
        let deployment = asap_logical_asap_optimization::DeploymentCapabilities {
            raw_data_retained: false,
            ..Default::default()
        };
        let data = DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 1000.0,
            },
            sources: [(
                "latency".into(),
                SourceFacts {
                    series_count: Some(10),
                    values_non_negative: true,
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
        };
        let mut configured =
            enumerate_candidate_configurations(&source, &[0, 1], &entries, &data, &deployment)
                .unwrap();
        // Synthetic activation objective exposes reuse; it is not runtime pricing.
        for (id, candidate) in configured.inventory.graph.candidates.iter_mut().enumerate() {
            let node = configured.inventory.templates[id].as_ref().unwrap();
            candidate.local_cost = if node.guarantee.as_ref().is_some_and(|guarantee| {
                !guarantee.is_exact()
                    && guarantee.spec.accuracy_metric
                        == asap_types::ir::properties::AccuracyMetric::Rank
            }) {
                100_000.0
            } else if matches!(
                configured.inventory.templates[id].as_ref().unwrap().asap(),
                Some(ASAPOp::PaneChain { .. })
            ) {
                1000.0
            } else {
                1.0
            };
        }
        let selected = select_candidates(&configured.inventory.graph)
            .unwrap()
            .unwrap();
        let chains: Vec<_> = selected
            .selected_candidates
            .iter()
            .filter(|id| {
                matches!(
                    configured.inventory.templates[id.0]
                        .as_ref()
                        .unwrap()
                        .asap(),
                    Some(ASAPOp::PaneChain { .. })
                )
            })
            .collect();
        assert_eq!(chains.len(), 1);
        let chain = configured.inventory.templates[chains[0].0]
            .as_ref()
            .unwrap();
        assert!(labels.iter().all(|name| chain
            .schema
            .fields
            .iter()
            .any(|field| field.name == *name)));
        let mut modes = vec![None; configured.configurations.len()];
        for id in &selected.selected_candidates {
            modes[id.0] = Some(configured.configurations[id.0].materialization);
        }
        let roots: Vec<_> = roots.into_iter().map(|(_, root)| root).collect();
        let mut physical = materialize_selected_candidates(
            &configured.inventory,
            &selected,
            &modes,
            &roots,
            &entries,
            &data,
            &deployment,
        )
        .unwrap();
        physical.physical.dag.validate().unwrap();
        if let Some(newest) = newest {
            assert!(physical.physical.dag.nodes.iter().any(|node| matches!(
                node.payload, asap_types::ir::Operator::ASAP(ASAPOp::SummaryMerge {newest_panes: Some(count), ..}) if count == newest
            )));
        }

        assert!(physical.nodes_by_candidate[chains[0].0].is_some());
        // Validate this exact selected shared graph. Bounded search incumbents
        // need not choose it, and synthetic activation prices are never bills.
        physical.physical.id = "shared-pane-regression".into();
        let mut evaluator = crate::PlanSelector::new(
            &entries,
            &data,
            crate::PlanningModels {
                deployment_capabilities: &deployment,
                ..crate::PlanningModels::builtin()
            },
            false,
        )
        .unwrap();
        evaluator.offer(&physical.physical).unwrap();
        let (selection, physical) = evaluator
            .finish()
            .expect("shared pane graph must pass accuracy and cost validation");
        assert!(selection.costs[&selection.selected]
            .cost_per_second
            .unwrap()
            .is_finite());
        assert_eq!(
            physical
                .dag
                .nodes
                .iter()
                .filter(|node| matches!(
                    node.payload,
                    asap_types::ir::Operator::ASAP(ASAPOp::PaneChain { .. })
                ))
                .count(),
            1
        );
    }

    // Exact top-k's executable Sort/Limit/Project nodes are selected and charged
    // explicitly, so final Stage 2 does not introduce unmodeled identities.
    #[test]
    fn exact_topk_is_lowered_before_decomposition_and_survives_stage2() {
        use super::super::candidate_physical::materialize_selected_candidates;
        use asap_logical_asap_optimization::test_support::entry_with;
        use asap_types::ir::operator::AggIntent;
        use asap_types::workload::{
            DataWorkload, QueryRecurrence, RepeatedDemand, RepetitionInterval,
        };
        let query = "topk by (job) (3, sum_over_time(m[1m]))";
        let root = lower_promql(query, AccuracyTarget::exact());
        // Top-k returns several ranked rows per partition, with numeric values.
        assert_eq!(root.schema.fields[1].name, "topk_3");
        assert_eq!(
            root.schema.fields[1].dtype,
            asap_types::ir::schema::FieldDataType::Plain(asap_types::ir::schema::DataType::Float64)
        );
        assert!(root.schema.unique_keys.is_empty());
        let roots = vec![(0, QueryRoot::Operator(Rc::clone(&root)))];
        let initial = LogicalMemo::from_roots(&roots);
        let output = initial.group_for_node(&root).unwrap();
        let original_input = Rc::clone(
            &initial.groups[initial.groups[output.0 as usize].children[0].0 as usize].node,
        );
        let generated = generate_per_computation_candidates(roots.clone()).unwrap();
        let memo = try_populate_pass1(initial, &generated).unwrap();
        let mut priced_operators = Vec::new();
        let inventory = build_candidate_inventory(&memo, &[output], |node| {
            priced_operators.push(node.operator.kind_name());
            Ok(1.0)
        })
        .unwrap();
        assert!(priced_operators.contains(&"Sort"));
        assert!(priced_operators.contains(&"Limit"));
        let exact_root = inventory.group_candidates[output.0 as usize][0];
        let root_template = inventory.templates[exact_root.0].as_ref().unwrap();
        assert!(
            OperatorNode::reachable(root_template)
                .iter()
                .any(|node| Rc::ptr_eq(node, &original_input)),
            "lowering preserves canonical input boundary"
        );
        assert!(inventory.templates.iter().flatten().all(|node| !matches!(node.non_asap(), Some(NonASAPOp::Aggregate { measures, .. }) if measures.iter().any(|measure| matches!(measure, AggIntent::TopK { .. })))));
        let selected = select_candidates(&inventory.graph).unwrap().unwrap();
        let mut modes = vec![None; inventory.graph.candidates.len()];
        for id in &selected.selected_candidates {
            modes[id.0] = Some(Materialization::NotMaterialized);
        }
        let entries = vec![entry_with(
            AccuracyTarget::exact(),
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(60_000))),
        )];
        let physical = materialize_selected_candidates(
            &inventory,
            &selected,
            &modes,
            &roots.into_iter().map(|(_, root)| root).collect::<Vec<_>>(),
            &entries,
            &DataWorkload::default(),
            crate::PlanningModels::builtin().deployment_capabilities,
        )
        .unwrap();
        physical.physical.dag.validate().unwrap();
        assert_eq!(
            physical.nodes_by_candidate.iter().flatten().count(),
            selected.selected_candidates.len()
        );
        assert_eq!(
            selected.modeled_cost,
            selected.selected_candidates.len() as f64
        );
    }

    fn nested_sum_memo(external_reader: bool) -> (CandidateMemo, Vec<MemoGroupId>) {
        let mut queries = vec!["sum by (region) (sum by (region, job) (m))"];
        if external_reader {
            queries.push("sum by (region, job) (m)");
        }
        let roots = queries
            .iter()
            .enumerate()
            .map(|(query, text)| {
                (
                    query,
                    QueryRoot::Operator(lower_promql(text, AccuracyTarget::exact())),
                )
            })
            .collect::<Vec<_>>();
        let initial = LogicalMemo::from_roots(&roots);
        let outputs = (0..queries.len())
            .map(|query| {
                MemoGroupId(
                    initial
                        .groups
                        .iter()
                        .position(|group| group.roots_of.contains(&query))
                        .unwrap() as u32,
                )
            })
            .collect();
        let generated = generate_per_computation_candidates(roots).unwrap();
        (try_populate_pass1(initial, &generated).unwrap(), outputs)
    }

    // Fusion owns its private work but does not publish another logical result.
    #[test]
    fn fused_nested_sum_activates_only_its_actual_input_groups() {
        let (memo, outputs) = nested_sum_memo(false);
        let owner = outputs[0].0 as usize;
        let fused_index = memo.candidates[owner]
            .iter()
            .position(|candidate| candidate.implements.len() > 1)
            .expect("nested sum has an absorbed computation");
        let absorbed = memo.candidates[owner][fused_index].implements[1];
        let mut inventory = build_candidate_inventory(&memo, &outputs, |_| Ok(1.0)).unwrap();
        let fused = inventory.group_candidates[owner][fused_index];
        assert!(!inventory.group_candidates[absorbed.0 as usize].contains(&fused));
        assert!(
            inventory.graph.query_roots[0]
                .compatible_candidates
                .contains(&fused),
            "fused output satisfies its owning group's contract"
        );
        inventory.graph.query_roots[0].compatible_candidates = vec![fused];
        let selected = select_candidates(&inventory.graph).unwrap().unwrap();
        assert!(inventory.group_candidates[absorbed.0 as usize]
            .iter()
            .all(|id| !selected.selected_candidates.contains(id)));
        let rebuilt =
            reconstruct_candidate_outputs(&inventory.graph, &selected, &inventory.templates)
                .unwrap();
        rebuilt[0].validate_structure().unwrap();
        assert_eq!(
            OperatorNode::reachable(&rebuilt[0])
                .iter()
                .filter(|node| matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. })))
                .count(),
            1
        );
    }

    // A separately requested inner result is computed independently of fusion.
    #[test]
    fn fused_nested_sum_preserves_an_external_inner_query() {
        let (memo, outputs) = nested_sum_memo(true);
        let owner = outputs[0].0 as usize;
        let inner = outputs[1];
        assert_eq!(memo.groups[owner].children, vec![inner]);
        let fused_index = memo.candidates[owner]
            .iter()
            .position(|candidate| {
                candidate.node.contains_asap() && !candidate.children.contains(&inner)
            })
            .expect("fusion can privately recompute the shared inner work");
        let mut inventory = build_candidate_inventory(&memo, &outputs, |_| Ok(1.0)).unwrap();
        let fused = inventory.group_candidates[owner][fused_index];
        inventory.graph.query_roots[0].compatible_candidates = vec![fused];
        assert!(!inventory.graph.query_roots[1]
            .compatible_candidates
            .contains(&fused));
        let selected = select_candidates(&inventory.graph).unwrap().unwrap();
        assert!(inventory.group_candidates[inner.0 as usize]
            .iter()
            .any(|id| selected.selected_candidates.contains(id)));
        let rebuilt =
            reconstruct_candidate_outputs(&inventory.graph, &selected, &inventory.templates)
                .unwrap();
        assert_eq!(rebuilt.len(), 2);
        for root in &rebuilt {
            root.validate_structure().unwrap();
        }
        assert!(!Rc::ptr_eq(&rebuilt[0], &rebuilt[1]));
        assert_eq!(
            rebuilt[0].schema.fields.len(),
            memo.groups[owner].node.schema.fields.len()
        );
        assert_eq!(
            rebuilt[1].schema.fields.len(),
            memo.groups[inner.0 as usize].node.schema.fields.len()
        );
    }

    // A boundary-only alias has no immediate operator for the decoder to rebuild.
    #[test]
    fn rejects_root_alias_instead_of_duplicating_boundary_work() {
        let (mut memo, outputs) = quantile_memo();
        let group = outputs[0].0 as usize;
        let boundary = memo.candidates[group][0].children[0];
        memo.candidates[group][0].node = Rc::clone(&memo.groups[boundary.0 as usize].node);
        assert!(matches!(
            build_candidate_inventory(&memo, &outputs, |_| Ok(1.0)),
            Err(CandidateInventoryError::Unsupported {
                reason: "root aliases an input group without an operator",
                ..
            })
        ));
    }

    // Unsupported decisions must fail adaptation, rather than disappear.
    #[test]
    fn rejects_zero_accuracy_share_materialization_restrictions_and_invalid_costs() {
        let (mut memo, outputs) = quantile_memo();
        memo.candidates[0][0].budget_share = 0;
        assert!(matches!(
            build_candidate_inventory(&memo, &outputs, |_| Ok(1.0)),
            Err(CandidateInventoryError::Unsupported {
                reason: "zero accuracy budget share",
                ..
            })
        ));
        memo.candidates[0][0].budget_share = 1;
        memo.candidates[0][0]
            .materializations
            .push(Materialization::NotMaterialized);
        assert!(matches!(
            build_candidate_inventory(&memo, &outputs, |_| Ok(1.0)),
            Err(CandidateInventoryError::Unsupported {
                reason: "materialization eligibility restrictions",
                ..
            })
        ));
        memo.candidates[0][0].materializations.clear();
        assert!(matches!(
            build_candidate_inventory(&memo, &outputs, |_| Ok(f64::NAN)),
            Err(CandidateInventoryError::Cost(_))
        ));
    }
    // Budget shares are concrete sizes: composed root accuracy, not the share
    // label, decides feasibility. Oversizing one remaining estimate stays legal.
    #[test]
    fn ratio_accuracy_is_validated_after_selecting_concrete_budget_shares() {
        use crate::{CalibrationFit, PlanSelector, PlanningModels, SelectionError};
        use asap_logical_asap_optimization::accuracy::satisfies;
        use asap_logical_asap_optimization::per_computation_candidates::root_guarantee;
        use asap_logical_asap_optimization::test_support::entry_with;
        use asap_physical_asap_optimization::optimize_physical_asap_chosen;
        use asap_types::ir::properties::AccuracyMetric;
        use asap_types::workload::{
            DataArrival, DataWorkload, QueryRecurrence, RepeatedDemand, RepetitionInterval,
            SourceFacts,
        };

        let target = AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Relative);
        let query = "quantile_over_time(0.99, m[1m]) / quantile_over_time(0.5, m[1m])";
        let roots = vec![(0, QueryRoot::Operator(lower_promql(query, target.clone())))];
        let logical = LogicalMemo::from_roots(&roots);
        let output = MemoGroupId(
            logical
                .groups
                .iter()
                .position(|group| group.roots_of.contains(&0))
                .unwrap() as u32,
        );
        let generated = generate_per_computation_candidates(roots).unwrap();
        let original = try_populate_pass1(logical, &generated).unwrap();
        assert!(original
            .candidates
            .iter()
            .flatten()
            .any(|candidate| candidate.budget_share == 2));
        // The complete generated inventory must no longer fail merely because
        // it includes composed-budget sizes alongside whole-budget sizes.
        build_candidate_inventory(&original, &[output], |_| Ok(0.0)).unwrap();
        let data = DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 1000.0,
            },
            sources: [(
                "m".into(),
                SourceFacts {
                    series_count: Some(1),
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
        };
        let entries = [entry_with(
            target.clone(),
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(60_000))),
        )];
        let models = PlanningModels::builtin();
        for (share, one_exact, valid) in [(1, false, false), (2, false, true), (2, true, true)] {
            let mut memo = original.clone();
            let mut made_exact = false;
            let mut approximate_groups = 0;
            for candidates in &mut memo.candidates {
                let sized: Vec<_> = candidates.iter().filter(|candidate| candidate.budget_share == share
                    && OperatorNode::reachable(&candidate.node).iter().any(|node|
                        matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. }) if *kind.algorithm() == SketchAlgorithm::DDSketch)))
                    .cloned().collect();
                if sized.is_empty() {
                    continue;
                }
                approximate_groups += 1;
                if one_exact && !made_exact {
                    candidates.retain(|candidate| !candidate.node.contains_asap());
                    assert!(!candidates.is_empty());
                    made_exact = true;
                } else {
                    *candidates = sized;
                }
            }
            assert_eq!(approximate_groups, 2);
            let inventory = build_candidate_inventory(&memo, &[output], |_| Ok(1.0)).unwrap();
            let selected = select_candidates(&inventory.graph).unwrap().unwrap();
            let outputs =
                reconstruct_candidate_outputs(&inventory.graph, &selected, &inventory.templates)
                    .unwrap();
            let roots = vec![QueryRoot::Operator(Rc::clone(&outputs[0]))];
            let guarantee =
                root_guarantee(&roots[0]).expect("relative ratio has a composed guarantee");
            assert_eq!(
                satisfies(&guarantee, &target),
                valid,
                "share={share}, one_exact={one_exact}, {guarantee:?}"
            );
            let physical = optimize_physical_asap_chosen(
                "ratio-shares",
                &roots,
                &entries,
                &data,
                models.deployment_capabilities,
                &mut |_, _, _| Materialization::NotMaterialized,
            )
            .unwrap()
            .unwrap();
            let mut selector = PlanSelector::new(&entries, &data, models, false).unwrap();
            selector.set_calibration(CalibrationFit::default());
            selector.offer(&physical).unwrap();
            match selector.finish() {
                Ok(_) => assert!(valid, "undersized ratio must fail end-to-end accuracy"),
                Err(SelectionError::NoValidCandidate(_)) => {
                    assert!(!valid, "properly sized ratio must be accepted")
                }
                Err(error) => panic!("unexpected evaluation error: {error}"),
            }
        }
    }
}
