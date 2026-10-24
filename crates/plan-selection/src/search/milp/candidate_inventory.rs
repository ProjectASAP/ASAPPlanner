//! Opt-in bridge from logical candidate memo to immediate candidate connections.
//!
//! The caller supplies node-local costs for one fixed accounting context. These
//! are not runtime cost estimates. Consumer-dependent demand, composed accuracy validation,
//! physical materialization assignments, and legacy coupling constraints remain
//! unsupported and are never silently erased by this adapter. This is a logical
//! inventory bridge for solver experiments, not Stage 2 physical enumeration.
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
            if candidate.implements != [MemoGroupId(group as u32)] {
                return Err(unsupported("fused or differently owned computation"));
            }
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
            let id = PhysicalCandidateId(inventory.graph.candidates.len());
            inventory.group_candidates[group].push(id);
            inventory.templates.push(Some(Rc::clone(&candidate.node)));
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
            builder.add(&candidate.node, Some(root_id))?;
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
