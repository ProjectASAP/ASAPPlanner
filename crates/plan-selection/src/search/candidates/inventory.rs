//! Unit tests of Stage 2's candidate inventory
//! (`asap_physical_asap_optimization::test_support::catalog`) that select with the solver.
use std::collections::HashMap;
use std::rc::Rc;

use asap_physical_asap_optimization::test_support::catalog::{
    build_candidate_inventory_before, build_with_compatible_producers_cache, CandidateInventory,
    MemoGroupOutputToCompatibleProducersCache,
};
use asap_physical_asap_optimization::CandidateInventoryError;
use asap_types::ir::OperatorNode;
use asap_types::physical::Materialization;
use asap_types::stages::{CandidateMemo, MemoGroupId, PhysicalCandidateId};

use crate::search::candidates::dag::{
    DagOfPhysicalCandidatesWithCosts, InputProducerCandidates, PhysicalCandidateWithCost,
    QueryOutputCandidates,
};

/// A Stage 2 inventory, and its operator nodes as a DAG with test-chosen
/// costs, for tests that solve the inventory before configuration.
#[cfg(test)]
pub(in crate::search) struct InventoryForTests {
    pub inventory: CandidateInventory,
    /// Node `i` as candidate `i`, priced by the test's node cost, reading the
    /// inventory's producers; query roots are its output references.
    pub graph: DagOfPhysicalCandidatesWithCosts,
    /// Node `i`'s operator node.
    pub templates: Vec<Rc<OperatorNode>>,
    /// The inventory's implementation roots per group.
    pub group_candidates: Vec<Vec<PhysicalCandidateId>>,
}

#[cfg(test)]
impl InventoryForTests {
    pub(in crate::search) fn new(
        inventory: CandidateInventory,
        mut node_local_cost: impl FnMut(&OperatorNode) -> Result<f64, String>,
    ) -> Self {
        let templates: Vec<_> = inventory
            .operator_nodes_of_candidates_for_memo_groups
            .iter()
            .map(|node| Rc::clone(&node.physical_node))
            .collect();
        let graph = DagOfPhysicalCandidatesWithCosts {
            candidates: templates
                .iter()
                .zip(&inventory.producer_nodes_per_input)
                .map(|(node, inputs)| PhysicalCandidateWithCost {
                    local_cost: node_local_cost(node).unwrap(),
                    producer_candidates_per_input: inputs
                        .iter()
                        .map(|suppliers| InputProducerCandidates {
                            candidates: suppliers.clone(),
                        })
                        .collect(),
                })
                .collect(),
            query_roots: inventory
                .query_output_nodes
                .iter()
                .map(|nodes| QueryOutputCandidates {
                    candidates: nodes.clone(),
                })
                .collect(),
            ..Default::default()
        };
        let group_candidates = inventory.implementation_roots_of_group.clone();
        Self {
            inventory,
            graph,
            templates,
            group_candidates,
        }
    }

    /// Each node as one query-time, unmaterialized candidate with the DAG's
    /// costs.
    pub(in crate::search) fn configured_for_tests(
        &self,
    ) -> crate::search::candidates::dag::PhysicalCandidatesWithCosts {
        use asap_types::ir::properties::ExecutionTiming;
        crate::search::candidates::dag::PhysicalCandidatesWithCosts::from_dag_for_tests(
            self.graph.clone(),
            self.templates
                .iter()
                .map(|node| {
                    (
                        Rc::clone(node),
                        (ExecutionTiming::QueryTime, Materialization::NotMaterialized),
                    )
                })
                .collect(),
        )
    }
}

#[cfg(test)]
impl std::ops::Deref for InventoryForTests {
    type Target = CandidateInventory;
    fn deref(&self) -> &CandidateInventory {
        &self.inventory
    }
}

/// Each output reference is an operator group, not necessarily an entire query.
/// Scalar query assembly and Stage 2/accuracy validation remain with the caller.
#[cfg(test)]
pub(in crate::search) fn build_candidate_inventory(
    memo: &CandidateMemo,
    output_groups: &[MemoGroupId],
    node_local_cost: impl FnMut(&OperatorNode) -> Result<f64, String>,
) -> Result<InventoryForTests, CandidateInventoryError> {
    build_candidate_inventory_before(memo, output_groups, None)
        .map(|inventory| InventoryForTests::new(inventory, node_local_cost))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::candidates::reconstruction::reconstruct_candidate_outputs;
    use crate::search::milp::model::select_candidates;
    use asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::pass2::memo::candidates::try_populate_pass1;
    use asap_logical_asap_optimization::pass2::memo::shared_configurations::PaneWidthDomain;
    use asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups;
    use asap_logical_asap_optimization::test_support::lower_promql;
    use asap_logical_asap_optimization::test_support::sharing_rules_and_shared_state_groups;
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
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let output_groups = (0..2)
            .map(|query| *initial.query_roots[query].as_operator().unwrap())
            .collect();
        let candidates = generate_per_computation_candidates(
            &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
                &roots,
            ),
        )
        .unwrap();
        let memo = try_populate_pass1(initial, &candidates).unwrap();
        (
            sharing_rules_and_shared_state_groups(memo, PaneWidthDomain::Complete).unwrap(),
            output_groups,
        )
    }

    // Reusing a typed compatibility result must leave every activation and
    // connection decision unchanged, including shared-producer cost ownership.
    #[test]
    fn compatibility_memo_preserves_complete_candidate_graph_and_optimum() {
        let (memo, outputs) = quantile_memo();
        let cost = |node: &OperatorNode| Ok(if is_kll(node) { 8.0 } else { 1.0 });
        let mut compatible_producers_cache = MemoGroupOutputToCompatibleProducersCache::default();
        let cached = InventoryForTests::new(
            build_with_compatible_producers_cache(
                &memo,
                &outputs,
                None,
                &mut compatible_producers_cache,
            )
            .unwrap(),
            cost,
        );
        assert!(
            compatible_producers_cache.hits() > 0,
            "repeated typed supplier checks were actually reused"
        );
        let uncached = InventoryForTests::new(
            build_with_compatible_producers_cache(
                &memo,
                &outputs,
                None,
                &mut MemoGroupOutputToCompatibleProducersCache::disabled(),
            )
            .unwrap(),
            cost,
        );
        assert_eq!(
            format!("{:?}", cached.graph),
            format!("{:?}", uncached.graph)
        );
        assert_eq!(cached.group_candidates, uncached.group_candidates);
        assert_eq!(cached.templates.len(), uncached.templates.len());
        for (cached, uncached) in cached.templates.iter().zip(&uncached.templates) {
            assert!(Rc::ptr_eq(cached, uncached) || cached == uncached);
        }
        let cached_plan = select_candidates(&cached.graph).unwrap().unwrap();
        let uncached_plan = select_candidates(&uncached.graph).unwrap().unwrap();
        assert_eq!(cached_plan.modeled_cost, uncached_plan.modeled_cost);
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
                .filter(|id| is_kll(&inventory.templates[id.0]))
                .count(),
            1
        );
    }

    fn stage2_smoke(outputs: &[Rc<OperatorNode>]) {
        use asap_physical_asap_optimization::test_support::optimize_physical_asap_chosen;
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
            false,
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
            Some(300_000),
        );
    }

    fn check_shared_pane_plan(
        queries: [&str; 2],
        errors: [f64; 2],
        labels: &[&str],
        newest_window_ms: Option<u64>,
    ) {
        use crate::search::candidates::configurations::enumerate_candidate_configurations;
        use asap_logical_asap_optimization::test_support::entry_with;
        use asap_physical_asap_optimization::test_support::catalog::materialize_selected_candidates;
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
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let outputs: Vec<_> = (0..roots.len())
            .map(|query| *initial.query_roots[query].as_operator().unwrap())
            .collect();
        let generated = generate_per_computation_candidates(
            &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
                &roots.clone(),
            ),
        )
        .unwrap();
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
        asap_logical_asap_optimization::pass2::memo::window_candidates::add_local_window_composition(
            &mut memo, &entries,
        );
        let memo = sharing_rules_and_shared_state_groups(memo, PaneWidthDomain::Complete).unwrap();
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
        let nodes = configured.nodes_for_tests();
        for (id, candidate) in configured.dag.candidates.iter_mut().enumerate() {
            let node = &nodes[id];
            candidate.local_cost = if node.guarantee.as_ref().is_some_and(|guarantee| {
                !guarantee.is_exact()
                    && guarantee.spec.accuracy_metric
                        == asap_types::ir::properties::AccuracyMetric::Rank
            }) {
                100_000.0
            } else if matches!(node.asap(), Some(ASAPOp::PaneChain { .. })) {
                1000.0
            } else {
                1.0
            };
        }
        let selected =
            crate::search::milp::model::solve_candidate_proposal(&configured.dag, &[], None)
                .unwrap()
                .unwrap()
                .selected;
        let chains: Vec<_> = selected
            .selected_candidates
            .iter()
            .filter(|id| matches!(configured.node(id.0).asap(), Some(ASAPOp::PaneChain { .. })))
            .collect();
        assert_eq!(chains.len(), 1);
        let chain = configured.node(chains[0].0);
        assert!(labels.iter().all(|name| chain
            .schema
            .fields
            .iter()
            .any(|field| field.name == *name)));
        let mut modes = vec![None; configured.physical.candidates.len()];
        for id in &selected.selected_candidates {
            modes[id.0] = Some(configured.physical.candidates[id.0].materialization);
        }
        let roots: Vec<_> = roots.into_iter().map(|(_, root)| root).collect();
        let mut physical = materialize_selected_candidates(
            &configured.physical,
            &selected.to_selected_dag(),
            &selected.materializations_by_node(&modes),
            &roots,
            &entries,
            &data,
            deployment.query_time_results_retained,
        )
        .unwrap();
        physical.plan.dag.validate().unwrap();
        if let Some(window_ms) = newest_window_ms {
            let Some(ASAPOp::PaneChain {
                pane_width_ms,
                pane_count,
                ..
            }) = chain.asap()
            else {
                unreachable!("selected chain checked above")
            };
            // The complete width domain permits different pane counts. Verify
            // the short reader's coverage, not one traversal-dependent width.
            assert!(physical.plan.dag.nodes.iter().any(|node| matches!(
                node.payload, asap_types::ir::Operator::ASAP(ASAPOp::SummaryMerge {newest_panes: Some(count), ..})
                    if u64::from(count) * pane_width_ms == window_ms && count < *pane_count
            )));
        }

        assert!(selected.node_of(*chains[0]).is_some());
        // Validate this exact selected shared graph. Bounded search incumbents
        // need not choose it, and synthetic activation prices are never bills.
        physical.plan.id = "shared-pane-regression".into();
        let mut evaluator = crate::PlanSelector::new(
            &entries,
            &data,
            crate::SelectionModels {
                ..crate::SelectionModels::builtin()
            },
            false,
        )
        .unwrap();
        evaluator.offer(&physical.plan).unwrap();
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

    // Producer-level rollup and ranking candidates survive concrete selection,
    // Stage 2, end-to-end accuracy checks and the actual billing evaluator.
    #[test]
    fn producer_reuse_rules_produce_validated_shared_plans() {
        use asap_logical_asap_optimization::test_support::entry_with;
        use asap_physical_asap_optimization::test_support::catalog::materialize_selected_candidates;
        use asap_types::workload::{
            DataWorkload, QueryRecurrence, RepeatedDemand, RepetitionInterval,
        };
        // A summary build over rows, as opposed to a rollup over an already
        // aggregated result: a finalized accumulator, or (since an exact
        // rollup reads the finer query's own result) the finer query's
        // aggregate.
        let builds_over_rows = |node: &OperatorNode| {
            matches!(node.asap(), Some(ASAPOp::SummaryAgg { child, .. })
                if !matches!(child.asap(), Some(ASAPOp::FinalizeExactAccumulator { .. }))
                    && !matches!(child.non_asap(), Some(NonASAPOp::Aggregate { .. })))
        };
        for (queries, kind) in [
            (["avg(m)", "sum(m)"], "partial"),
            (["sum by (job) (m)", "sum by (job, instance) (m)"], "rollup"),
            (
                [
                    "topk by (job) (3, sum_over_time(m[1m]))",
                    "topk by (job) (5, sum_over_time(m[1m]))",
                ],
                "ranking",
            ),
            (["quantile(0.9, m)", "quantile by (job) (0.9, m)"], "sketch"),
        ] {
            let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
            let roots: Vec<_> = queries
                .into_iter()
                .enumerate()
                .map(|(id, query)| (id, QueryRoot::Operator(lower_promql(query, target.clone()))))
                .collect();
            let initial = DagOfLogicalMemoGroups::from_roots(&roots);
            let outputs: Vec<_> = (0..2)
                .map(|query| *initial.query_roots[query].as_operator().unwrap())
                .collect();
            let generated = generate_per_computation_candidates(
                &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
                    &roots.clone(),
                ),
            )
            .unwrap();
            let memo = sharing_rules_and_shared_state_groups(
                try_populate_pass1(initial, &generated).unwrap(),
                PaneWidthDomain::Complete,
            )
            .unwrap();
            let mut inventory = build_candidate_inventory(&memo, &outputs, |node| {
                // Structural selection forces the reuse being tested. Actual
                // prices are obtained only from PlanSelector below.
                if node.guarantee.as_ref().is_some_and(|g| {
                    !g.is_exact()
                        && g.spec.accuracy_metric
                            == asap_types::ir::properties::AccuracyMetric::Rank
                }) {
                    return Ok(100_000.0);
                }
                Ok(
                    if builds_over_rows(node)
                        || matches!(node.non_asap(), Some(NonASAPOp::Sort { .. }))
                    {
                        1000.0
                    } else {
                        1.0
                    },
                )
            })
            .unwrap();
            if matches!(kind, "ranking" | "sketch") {
                for (output, group) in inventory.graph.query_roots.iter_mut().zip(&outputs) {
                    output.candidates.retain(|id| inventory.group_candidates[group.0 as usize].iter().position(|candidate| candidate == id).is_some_and(|index| memo.candidates[group.0 as usize][index].origin == if kind == "ranking" { asap_types::stages::OriginOfCandidateForMemoGroup::TopKReuse } else { asap_types::stages::OriginOfCandidateForMemoGroup::SharedBuild }));
                    assert!(!output.candidates.is_empty(), "{kind} reader generated");
                }
            }
            let selected = select_candidates(&inventory.graph).unwrap().expect(kind);
            let chosen: Vec<_> = selected
                .selected_candidates
                .iter()
                .map(|id| &inventory.templates[id.0])
                .collect();
            if kind == "ranking" {
                assert_eq!(
                    chosen
                        .iter()
                        .filter(|node| matches!(node.non_asap(), Some(NonASAPOp::Sort { .. })))
                        .count(),
                    1
                );
            } else {
                assert_eq!(
                    chosen.iter().filter(|node| builds_over_rows(node)).count(),
                    if kind == "partial" { 2 } else { 1 },
                    "{kind}"
                );
            }
            let entries: Vec<_> = (0..2)
                .map(|_| {
                    entry_with(
                        target.clone(),
                        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                            RepetitionInterval(60_000),
                        )),
                    )
                })
                .collect();
            let mut modes = vec![None; inventory.graph.candidates.len()];
            for id in &selected.selected_candidates {
                modes[id.0] = Some(Materialization::NotMaterialized);
            }
            let roots: Vec<_> = roots.into_iter().map(|(_, root)| root).collect();
            let mut data = DataWorkload {
                arrival: asap_types::workload::DataArrival::Ingesting {
                    rows_per_second: 1000.0,
                },
                ..DataWorkload::default()
            };
            data.sources.entry("m".into()).or_default().series_count = Some(10);
            data.sources
                .entry("m".into())
                .or_default()
                .values_non_negative = true;
            let mut realized = materialize_selected_candidates(
                &inventory.configured_for_tests().physical,
                &selected.to_selected_dag(),
                &selected.materializations_by_node(&modes),
                &roots,
                &entries,
                &data,
                false,
            )
            .unwrap();
            realized.plan.id = format!("shared-{kind}");
            let mut evaluator =
                crate::PlanSelector::new(&entries, &data, crate::SelectionModels::builtin(), false)
                    .unwrap();
            evaluator.offer(&realized.plan).unwrap();
            let (selection, physical) = evaluator
                .finish()
                .unwrap_or_else(|error| panic!("{kind}: {error:?}"));
            physical.dag.validate().unwrap();
            assert!(selection.costs[&selection.selected]
                .cost_per_second
                .unwrap()
                .is_finite());
        }
    }

    // A fused two-sketch computation chooses each readout's capacity separately:
    // one or both states can serve external queries without a configuration product.
    #[test]
    fn factored_summary_results_choose_independent_capacities_and_sharing() {
        use asap_logical_asap_optimization::test_support::entry_with;
        use asap_types::ir::schema::sketch::SketchParams;
        use asap_types::stages::OriginOfCandidateForMemoGroup;
        use asap_types::workload::{
            DataWorkload, QueryRecurrence, RepeatedDemand, RepetitionInterval,
        };
        for share_both in [false, true] {
            let mut queries = vec![
                (
                    "quantile_over_time(0.5, a[5m]) * quantile_over_time(0.99, b[5m])",
                    0.03,
                ),
                ("quantile_over_time(0.95, a[5m])", 0.005),
            ];
            if share_both {
                queries.push(("quantile_over_time(0.9, b[5m])", 0.008));
            }
            let targets: Vec<_> = queries
                .iter()
                .map(|(_, error)| AccuracyTarget::max_error(*error).failure_probability(0.001))
                .collect();
            let roots: Vec<_> = queries
                .iter()
                .enumerate()
                .map(|(id, (query, _))| {
                    (
                        id,
                        QueryRoot::Operator(lower_promql(query, targets[id].clone())),
                    )
                })
                .collect();
            let initial = DagOfLogicalMemoGroups::from_roots(&roots);
            let outputs: Vec<_> = (0..roots.len())
                .map(|query| *initial.query_roots[query].as_operator().unwrap())
                .collect();
            let generated = generate_per_computation_candidates(
                &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
                    &roots.clone(),
                ),
            )
            .unwrap();
            let mut memo = try_populate_pass1(initial, &generated).unwrap();
            let owner = outputs[0].0 as usize;
            let mut fused = memo.candidates[owner][0].clone();
            let original_inputs = fused.input_groups.clone();
            assert_eq!(original_inputs.len(), 2);
            let mut replacements = HashMap::new();
            let mut raw_inputs = Vec::new();
            for input in &original_inputs {
                let selected = memo.candidates[input.0 as usize].iter().filter(|candidate| OperatorNode::reachable(&candidate.root_node).iter().any(|node| matches!(node.asap(), Some(ASAPOp::SummaryAgg {family: FieldDataType::Sketch(kind, _), ..}) if kind.algorithm() == &SketchAlgorithm::DDSketch))).max_by_key(|candidate| candidate.accuracy_budget_parts).unwrap();
                replacements.insert(
                    Rc::as_ptr(&memo.groups[input.0 as usize].logical_node),
                    Rc::clone(&selected.root_node),
                );
                for raw in &selected.input_groups {
                    if !raw_inputs.contains(raw) {
                        raw_inputs.push(*raw);
                    }
                }
            }
            fused.root_node = Rc::new(
                fused
                    .root_node
                    .with_new_children(|child| {
                        replacements
                            .get(&Rc::as_ptr(child))
                            .cloned()
                            .unwrap_or_else(|| Rc::clone(child))
                    })
                    .unwrap(),
            );
            fused.input_groups = raw_inputs;
            fused.fused_groups.extend(original_inputs);
            fused.accuracy_budget_parts = 2;
            memo.candidates[owner].push(fused);
            let memo =
                sharing_rules_and_shared_state_groups(memo, PaneWidthDomain::Complete).unwrap();
            let mut inventory = build_candidate_inventory(&memo, &outputs, |node| {
                Ok(if matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. })) {
                    1000.0
                } else {
                    1.0
                })
            })
            .unwrap();
            for (query, (output, group)) in inventory
                .graph
                .query_roots
                .iter_mut()
                .zip(&outputs)
                .enumerate()
            {
                output.candidates.retain(|id| inventory.group_candidates[group.0 as usize].iter().position(|candidate| candidate == id).is_some_and(|index| {
                    let candidate = &memo.candidates[group.0 as usize][index];
                    if query == 0 { candidate.origin == OriginOfCandidateForMemoGroup::SummaryResult } else {
                        candidate.origin == OriginOfCandidateForMemoGroup::SharedBuild && OperatorNode::reachable(&candidate.root_node).iter().any(|node| matches!(node.asap(), Some(ASAPOp::SummaryAgg {family: FieldDataType::Sketch(kind, _), ..}) if kind.algorithm() == &SketchAlgorithm::DDSketch))
                    }
                }));
                assert!(
                    !output.candidates.is_empty(),
                    "factored/shared output {query}"
                );
            }
            let selected = select_candidates(&inventory.graph).unwrap().unwrap();
            let alphas: Vec<_> = selected
                .selected_candidates
                .iter()
                .filter_map(|id| match &inventory.templates[id.0].asap() {
                    Some(ASAPOp::SummaryAgg {
                        family: FieldDataType::Sketch(kind, _),
                        ..
                    }) => match kind.params() {
                        SketchParams::DDSketch { alpha } => Some(*alpha),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            assert_eq!(
                alphas.len(),
                2,
                "two independent sources, shared when requested"
            );
            assert_ne!(alphas[0], alphas[1], "capacities selected independently");
            let mut modes = vec![None; inventory.graph.candidates.len()];
            for id in &selected.selected_candidates {
                modes[id.0] = Some(Materialization::NotMaterialized);
            }
            let entries: Vec<_> = targets
                .iter()
                .map(|target| {
                    entry_with(
                        target.clone(),
                        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                            RepetitionInterval(60_000),
                        )),
                    )
                })
                .collect();
            let roots: Vec<_> = roots.into_iter().map(|(_, root)| root).collect();
            let mut data = DataWorkload {
                arrival: asap_types::workload::DataArrival::Ingesting {
                    rows_per_second: 1000.0,
                },
                ..DataWorkload::default()
            };
            for source in ["a", "b"] {
                let facts = data.sources.entry(source.into()).or_default();
                facts.series_count = Some(10);
                facts.values_non_negative = true;
            }
            let mut realized =
                asap_physical_asap_optimization::test_support::catalog::materialize_selected_candidates(
                    &inventory.configured_for_tests().physical,
                    &selected.to_selected_dag(),
                    &selected.materializations_by_node(&modes),
                    &roots,
                    &entries,
                    &data,
                    false,
                )
                .unwrap();
            realized.plan.id = format!("independent-capacities-{share_both}");
            let mut evaluator =
                crate::PlanSelector::new(&entries, &data, crate::SelectionModels::builtin(), false)
                    .unwrap();
            evaluator.offer(&realized.plan).unwrap();
            let (selection, physical) = evaluator.finish().unwrap();
            physical.dag.validate().unwrap();
            assert!(selection.costs[&selection.selected]
                .cost_per_second
                .unwrap()
                .is_finite());
        }
    }

    // Exact top-k's executable Sort/Limit/Project nodes are selected and charged
    // explicitly, so final Stage 2 does not introduce unmodeled identities.
    #[test]
    fn exact_topk_is_lowered_before_decomposition_and_survives_stage2() {
        use asap_logical_asap_optimization::test_support::entry_with;
        use asap_physical_asap_optimization::test_support::catalog::materialize_selected_candidates;
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
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let output = initial.group_for_node(&root).unwrap();
        let original_input = Rc::clone(
            &initial.groups[initial.groups[output.0 as usize].child_groups[0].0 as usize]
                .logical_node,
        );
        let generated = generate_per_computation_candidates(
            &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
                &roots.clone(),
            ),
        )
        .unwrap();
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
        let root_template = &inventory.templates[exact_root.0];
        assert!(
            OperatorNode::reachable(root_template)
                .iter()
                .any(|node| Rc::ptr_eq(node, &original_input)),
            "lowering preserves canonical input boundary"
        );
        assert!(inventory.templates.iter().all(|node| !matches!(node.non_asap(), Some(NonASAPOp::Aggregate { measures, .. }) if measures.iter().any(|measure| matches!(measure, AggIntent::TopK { .. })))));
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
            &inventory.configured_for_tests().physical,
            &selected.to_selected_dag(),
            &selected.materializations_by_node(&modes),
            &roots.into_iter().map(|(_, root)| root).collect::<Vec<_>>(),
            &entries,
            &DataWorkload::default(),
            false,
        )
        .unwrap();
        physical.plan.dag.validate().unwrap();
        assert_eq!(
            physical.operator_node_of_selected_node.len(),
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
        let initial = DagOfLogicalMemoGroups::from_roots(&roots);
        let outputs = (0..queries.len())
            .map(|query| *initial.query_roots[query].as_operator().unwrap())
            .collect();
        let generated = generate_per_computation_candidates(
            &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
                &roots,
            ),
        )
        .unwrap();
        (try_populate_pass1(initial, &generated).unwrap(), outputs)
    }

    #[test]
    fn terminal_reader_factoring_reconstructs_the_same_minimum_cost_computation() {
        let (memo, outputs) = nested_sum_memo(false);
        let original = InventoryForTests::new(
            build_with_compatible_producers_cache(
                &memo,
                &outputs,
                None,
                &mut MemoGroupOutputToCompatibleProducersCache::default(),
            )
            .unwrap(),
            |_| Ok(1.0),
        );
        let compact = build_candidate_inventory(&memo, &outputs, |_| Ok(1.0)).unwrap();
        assert!(compact.graph.candidates.len() < original.graph.candidates.len());
        let before = select_candidates(&original.graph).unwrap().unwrap();
        let after = select_candidates(&compact.graph).unwrap().unwrap();
        assert_eq!(before.modeled_cost, after.modeled_cost);
        for (inventory, selected) in [(&original, before), (&compact, after)] {
            let roots =
                reconstruct_candidate_outputs(&inventory.graph, &selected, &inventory.templates)
                    .unwrap();
            assert_eq!(roots.len(), 1);
            roots[0].validate_structure().unwrap();
            assert!(
                asap_physical_asap_optimization::test_support::catalog::output_contract_satisfied(
                    &memo.groups[outputs[0].0 as usize].logical_node,
                    &roots[0]
                )
                .unwrap()
            );
        }
    }

    // Fusion owns its private work but does not publish another logical result.
    // Inspect the uncompressed catalog here because the fixture identifies its
    // particular fusion rule by its original memo ordinal.
    #[test]
    fn fused_nested_sum_activates_only_its_actual_input_groups() {
        let (memo, outputs) = nested_sum_memo(false);
        let owner = outputs[0].0 as usize;
        let fused_index = memo.candidates[owner]
            .iter()
            .position(|candidate| !candidate.fused_groups.is_empty())
            .expect("nested sum has an absorbed computation");
        let absorbed = memo.candidates[owner][fused_index].fused_groups[0];
        let mut inventory = InventoryForTests::new(
            build_with_compatible_producers_cache(
                &memo,
                &outputs,
                None,
                &mut MemoGroupOutputToCompatibleProducersCache::default(),
            )
            .unwrap(),
            |_| Ok(1.0),
        );
        let fused = inventory.group_candidates[owner][fused_index];
        assert!(!inventory.group_candidates[absorbed.0 as usize].contains(&fused));
        assert!(
            inventory.graph.query_roots[0].candidates.contains(&fused),
            "fused output satisfies its owning group's contract"
        );
        inventory.graph.query_roots[0].candidates = vec![fused];
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
        assert_eq!(memo.groups[owner].child_groups, vec![inner]);
        let fused_index = memo.candidates[owner]
            .iter()
            .position(|candidate| {
                candidate.root_node.contains_asap() && !candidate.input_groups.contains(&inner)
            })
            .expect("fusion can privately recompute the shared inner work");
        let mut inventory = InventoryForTests::new(
            build_with_compatible_producers_cache(
                &memo,
                &outputs,
                None,
                &mut MemoGroupOutputToCompatibleProducersCache::default(),
            )
            .unwrap(),
            |_| Ok(1.0),
        );
        let fused = inventory.group_candidates[owner][fused_index];
        inventory.graph.query_roots[0].candidates = vec![fused];
        assert!(!inventory.graph.query_roots[1].candidates.contains(&fused));
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
            memo.groups[owner].logical_node.schema.fields.len()
        );
        assert_eq!(
            rebuilt[1].schema.fields.len(),
            memo.groups[inner.0 as usize]
                .logical_node
                .schema
                .fields
                .len()
        );
    }

    // A boundary-only alias has no immediate operator for the decoder to rebuild.
    #[test]
    fn rejects_root_alias_instead_of_duplicating_boundary_work() {
        let (mut memo, outputs) = quantile_memo();
        let group = outputs[0].0 as usize;
        let boundary = memo.candidates[group][0].input_groups[0];
        memo.candidates[group][0].root_node =
            Rc::clone(&memo.groups[boundary.0 as usize].logical_node);
        assert!(matches!(
            build_candidate_inventory(&memo, &outputs, |_| Ok(1.0)),
            Err(CandidateInventoryError::Unsupported {
                reason: "root aliases an input group without an operator",
                ..
            })
        ));
    }

    // An expired generation budget returns no partially constructed inventory.
    #[test]
    fn inventory_deadline_returns_typed_timeout() {
        let (memo, outputs) = quantile_memo();
        assert!(matches!(
            build_candidate_inventory_before(&memo, &outputs, Some(std::time::Instant::now())),
            Err(CandidateInventoryError::DeadlineExceeded(_))
        ));
    }

    // Unsupported decisions must fail adaptation, rather than disappear.
    #[test]
    fn rejects_zero_accuracy_share() {
        let (mut memo, outputs) = quantile_memo();
        memo.candidates[0][0].accuracy_budget_parts = 0;
        assert!(matches!(
            build_candidate_inventory(&memo, &outputs, |_| Ok(1.0)),
            Err(CandidateInventoryError::Unsupported {
                reason: "zero accuracy budget share",
                ..
            })
        ));
    }
    // Budget shares are concrete sizes: composed root accuracy, not the share
    // label, decides feasibility. Oversizing one remaining estimate stays legal.
    #[test]
    fn ratio_accuracy_is_validated_after_selecting_concrete_budget_shares() {
        use crate::{ErrorSelectingPlan, PlanSelector, SelectionModels};
        use asap_accuracy::test_support::satisfies;
        use asap_cost::CalibrationFit;
        use asap_logical_asap_optimization::test_support::entry_with;
        use asap_physical_asap_optimization::test_support::optimize_physical_asap_chosen;
        use asap_types::ir::properties::AccuracyMetric;
        use asap_types::workload::{
            DataArrival, DataWorkload, QueryRecurrence, RepeatedDemand, RepetitionInterval,
            SourceFacts,
        };

        let target = AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Relative);
        let query = "quantile_over_time(0.99, m[1m]) / quantile_over_time(0.5, m[1m])";
        let roots = vec![(0, QueryRoot::Operator(lower_promql(query, target.clone())))];
        let logical = DagOfLogicalMemoGroups::from_roots(&roots);
        let output = *logical.query_roots[0].as_operator().unwrap();
        let generated = generate_per_computation_candidates(
            &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
                &roots,
            ),
        )
        .unwrap();
        let original = try_populate_pass1(logical, &generated).unwrap();
        assert!(original
            .candidates
            .iter()
            .flatten()
            .any(|candidate| candidate.accuracy_budget_parts == 2));
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
                    values_non_negative: true,
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
        let models = SelectionModels::builtin();
        for (share, one_exact, valid) in [(1, false, false), (2, false, true), (2, true, true)] {
            let mut memo = original.clone();
            let mut made_exact = false;
            let mut approximate_groups = 0;
            for candidates in &mut memo.candidates {
                let sized: Vec<_> = candidates.iter().filter(|candidate| candidate.accuracy_budget_parts == share
                    && OperatorNode::reachable(&candidate.root_node).iter().any(|node|
                        matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. }) if *kind.algorithm() == SketchAlgorithm::DDSketch)))
                    .cloned().collect();
                if sized.is_empty() {
                    continue;
                }
                approximate_groups += 1;
                if one_exact && !made_exact {
                    candidates.retain(|candidate| !candidate.root_node.contains_asap());
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
            let guarantee = crate::bound_root_guarantee(&roots[0], Some(1), &data, &models)
                .expect("proven nonnegative relative ratio has a composed guarantee");
            assert_eq!(
                satisfies(&guarantee, &target),
                valid,
                "share={share}, one_exact={one_exact}, {guarantee:?}"
            );
            let mut unproved_data = data.clone();
            unproved_data
                .sources
                .get_mut("m")
                .unwrap()
                .values_non_negative = false;
            assert!(
                crate::bound_root_guarantee(&roots[0], Some(1), &unproved_data, &models)
                    .is_none_or(|guarantee| !satisfies(&guarantee, &target)),
                "capacity sizing must not establish an unproved source-sign contract"
            );
            let physical = optimize_physical_asap_chosen(
                "ratio-shares",
                &roots,
                &entries,
                &data,
                false,
                &mut |_, _, _| Materialization::NotMaterialized,
            )
            .unwrap()
            .unwrap();
            let mut selector = PlanSelector::new(&entries, &data, models, false).unwrap();
            selector.set_calibration(CalibrationFit::default());
            selector.offer(&physical).unwrap();
            match selector.finish() {
                Ok(_) => assert!(valid, "undersized ratio must fail end-to-end accuracy"),
                Err(ErrorSelectingPlan::NoValidCandidate(_)) => {
                    assert!(!valid, "properly sized ratio must be accepted")
                }
                Err(error) => panic!("unexpected evaluation error: {error}"),
            }
        }
    }
}
