//! Price-interface regressions: compatible inputs must not disappear merely
//! because their statistics differ from the template's input.
use super::*;
use crate::search::candidates::configurations::CandidateConfiguration;
use crate::search::candidates::graph::{
    CandidateDependencyGraph, PhysicalCandidate, ProducerRequirement, QueryRootCandidates,
};
use crate::search::candidates::inventory::CandidateInventory;
use asap_logical_asap_optimization::memo::MemoGroupId;
use asap_logical_asap_optimization::test_support::lower_promql;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::{Operator, OperatorNode, ProjectItem, ScalarExpr};
use asap_types::workload::{
    DataWorkload, Predictability, Query, QueryRequirements, QueryWorkloadEntry, RepetitionInterval,
};

#[test]
fn differing_supplier_statistics_keep_both_price_interfaces() {
    let scan = |metric| {
        OperatorNode::reachable(&lower_promql(metric, AccuracyTarget::exact()))
            .into_iter()
            .find(|node| matches!(node.non_asap(), Some(NonASAPOp::Scan { .. })))
            .unwrap()
    };
    let m = scan("m");
    let n = scan("n");
    let project = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
        cols: m
            .schema
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| ProjectItem {
                alias: Some(field.name.clone()),
                expr: ScalarExpr::Column(index),
            })
            .collect(),
        qualifier: None,
        child: m.clone(),
    }))
    .unwrap();
    let mut configured = CandidateConfigurationInventory {
        inventory: CandidateInventory {
            graph: CandidateDependencyGraph {
                candidates: (0..3)
                    .map(|id| PhysicalCandidate {
                        group: MemoGroupId(id as u32),
                        local_cost: 0.0,
                        producer_requirements: if id == 2 {
                            vec![ProducerRequirement {
                                compatible_producers: vec![
                                    PhysicalCandidateId(0),
                                    PhysicalCandidateId(1),
                                ]
                                .into(),
                            }]
                        } else {
                            vec![]
                        },
                    })
                    .collect(),
                query_roots: vec![QueryRootCandidates {
                    compatible_candidates: vec![PhysicalCandidateId(2)],
                }],
                ..Default::default()
            },
            templates: vec![Some(m), Some(n), Some(project.clone())],
            group_candidates: vec![],
        },
        configurations: (0..3)
            .map(|id| CandidateConfiguration {
                source_candidate: PhysicalCandidateId(id),
                materialization: Materialization::NotMaterialized,
                timing: ExecutionTiming::QueryTime,
            })
            .collect(),
    };
    let roots = vec![QueryRoot::Operator(project)];
    let entries = vec![QueryWorkloadEntry {
        query: Query("m".into()),
        requirements: QueryRequirements {
            accuracy: AccuracyTarget::exact(),
            max_latency_ms: None,
        },
        predictability: Predictability::Predictable { known_at: None },
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
            60_000,
        ))),
        range_evaluation: None,
    }];
    let data: DataWorkload = serde_json::from_value(serde_json::json!({
        "arrival": {"ingesting": {"rows_per_second": 100.0}},
        "sources": {"m": {"rows_held":10,"series_count":1},"n":{"rows_held":100,"series_count":1}}
    }))
    .unwrap();
    let context = CandidatePlanningContext {
        deadline: None,
        refine_materialization: false,
        original_roots: &roots,
        entries: &entries,
        data: &data,
        models: crate::PlanningModels::builtin(),
        calibration: crate::CalibrationFit::default(),
    };
    let extracted = extract_price_options(&configured, &context)
        .unwrap()
        .unwrap();
    assert_eq!(extracted.candidates[0].len(), 1);
    assert_eq!(extracted.candidates[1].len(), 1);
    let reader_options = &extracted.candidates[2];
    assert_eq!(
        reader_options.len(),
        2,
        "both legal supplier statistics must reach the joint model"
    );
    for producer in [PhysicalCandidateId(0), PhysicalCandidateId(1)] {
        let option = reader_options
            .iter()
            .find(|option| {
                option
                    .compatible_suppliers
                    .iter()
                    .any(|binding| binding.producer == producer)
            })
            .expect("missing compatible supplier price option");
        assert_eq!(option.compatible_suppliers.len(), 1);
        assert_eq!(option.compatible_suppliers[0].price_option, 0);
    }
    let low = reader_options
        .iter()
        .find(|option| option.compatible_suppliers[0].producer == PhysicalCandidateId(0))
        .unwrap();
    let high = reader_options
        .iter()
        .find(|option| option.compatible_suppliers[0].producer == PhysicalCandidateId(1))
        .unwrap();
    assert!(
        high.profile.work_seconds > low.profile.work_seconds,
        "projection work must follow selected input rows, not template rows"
    );
    assert_eq!(
        configured.inventory.graph.candidates[2].producer_requirements[0]
            .compatible_producers
            .len(),
        2
    );
    // Force each structurally legal supplier in turn. The model must price
    // the selected binding rather than only returning the cheapest interface.
    for producer in [PhysicalCandidateId(0), PhysicalCandidateId(1)] {
        configured.inventory.graph.reader_constraints = vec![
            crate::search::candidates::graph::ReaderConstraint::ActivationRequires {
                candidate: PhysicalCandidateId(2),
                required_candidate: producer,
            },
        ];
        let proposal =
            crate::search::milp::direct_price_options::solve_joint_price_proposal_with_metrics(
                &configured.inventory.graph,
                &[],
                None,
                &mut Default::default(),
                &extracted,
            )
            .unwrap()
            .expect("forced supplier must remain feasible");
        assert!(proposal
            .selected
            .connections
            .iter()
            .any(|edge| edge.producer == producer));
        let mut rejections =
            crate::search::refinement::planner::CandidateRejectionSummary::default();
        let evaluated = crate::search::refinement::planner::evaluate_configured_candidate(
            &configured,
            &context,
            &proposal.selected,
            0,
            &mut rejections,
            &mut Vec::new(),
        )
        .unwrap()
        .unwrap_or_else(|| panic!("forced supplier rejected: {rejections:?}"));
        let actual = evaluated.0.costs[&evaluated.0.selected]
            .cost_per_second
            .unwrap();
        let modeled = proposal
            .direct_modeled_cost
            .expect("joint direct objective");
        assert!(
            (actual - modeled).abs() <= 1e-9 * actual.max(1e-15),
            "supplier {producer:?}: modeled {modeled}, actual {actual}"
        );
    }
    let timed = CandidatePlanningContext {
        deadline: Some(std::time::Instant::now()),
        ..context
    };
    assert!(matches!(
        extract_price_options(&configured, &timed),
        Err(DirectProfileError::DeadlineExceeded(_))
    ));
}

#[test]
fn downstream_prices_follow_the_selected_supplier_interface() {
    let scan = |metric| {
        OperatorNode::reachable(&lower_promql(metric, AccuracyTarget::exact()))
            .into_iter()
            .find(|node| matches!(node.non_asap(), Some(NonASAPOp::Scan { .. })))
            .unwrap()
    };
    let m = scan("m");
    let n = scan("n");
    let project = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
        cols: m
            .schema
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| ProjectItem {
                alias: Some(field.name.clone()),
                expr: ScalarExpr::Column(index),
            })
            .collect(),
        qualifier: None,
        child: m.clone(),
    }))
    .unwrap();
    let mut configured = CandidateConfigurationInventory {
        inventory: CandidateInventory {
            graph: CandidateDependencyGraph {
                candidates: (0..3)
                    .map(|id| PhysicalCandidate {
                        group: MemoGroupId(id as u32),
                        local_cost: 0.0,
                        producer_requirements: if id == 2 {
                            vec![ProducerRequirement {
                                compatible_producers: vec![
                                    PhysicalCandidateId(0),
                                    PhysicalCandidateId(1),
                                ]
                                .into(),
                            }]
                        } else {
                            vec![]
                        },
                    })
                    .collect(),
                query_roots: vec![QueryRootCandidates {
                    compatible_candidates: vec![PhysicalCandidateId(2)],
                }],
                ..Default::default()
            },
            templates: vec![Some(m), Some(n), Some(project.clone())],
            group_candidates: vec![],
        },
        configurations: (0..3)
            .map(|id| CandidateConfiguration {
                source_candidate: PhysicalCandidateId(id),
                materialization: Materialization::NotMaterialized,
                timing: ExecutionTiming::QueryTime,
            })
            .collect(),
    };
    let downstream = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
        cols: project
            .schema
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| ProjectItem {
                alias: Some(field.name.clone()),
                expr: ScalarExpr::Column(index),
            })
            .collect(),
        qualifier: None,
        child: project,
    }))
    .unwrap();
    configured
        .inventory
        .graph
        .candidates
        .push(PhysicalCandidate {
            group: MemoGroupId(3),
            local_cost: 0.0,
            producer_requirements: vec![ProducerRequirement {
                compatible_producers: vec![PhysicalCandidateId(2)].into(),
            }],
        });
    configured
        .inventory
        .templates
        .push(Some(downstream.clone()));
    configured.configurations.push(CandidateConfiguration {
        source_candidate: PhysicalCandidateId(3),
        materialization: Materialization::NotMaterialized,
        timing: ExecutionTiming::QueryTime,
    });
    configured.inventory.graph.query_roots[0].compatible_candidates = vec![PhysicalCandidateId(3)];
    let roots = vec![QueryRoot::Operator(downstream)];
    let entries = vec![QueryWorkloadEntry {
        query: Query("m".into()),
        requirements: QueryRequirements {
            accuracy: AccuracyTarget::exact(),
            max_latency_ms: None,
        },
        predictability: Predictability::Predictable { known_at: None },
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
            60_000,
        ))),
        range_evaluation: None,
    }];
    let data: DataWorkload = serde_json::from_value(serde_json::json!({
        "arrival": {"ingesting": {"rows_per_second": 100.0}},
        "sources": {"m": {"rows_held":10,"series_count":1},"n":{"rows_held":100,"series_count":1}}
    }))
    .unwrap();
    let context = CandidatePlanningContext {
        deadline: None,
        refine_materialization: false,
        original_roots: &roots,
        entries: &entries,
        data: &data,
        models: crate::PlanningModels::builtin(),
        calibration: crate::CalibrationFit::default(),
    };
    let extracted = extract_price_options(&configured, &context)
        .unwrap()
        .unwrap();
    assert_eq!(extracted.candidates[2].len(), 2);
    assert_eq!(
        extracted.candidates[3].len(),
        2,
        "a single supplier candidate may expose multiple downstream price interfaces"
    );
    let mut referenced = Vec::new();
    for option in &extracted.candidates[3] {
        assert_eq!(option.compatible_suppliers.len(), 1);
        let binding = &option.compatible_suppliers[0];
        assert_eq!(binding.producer, PhysicalCandidateId(2));
        referenced.push(binding.price_option);
        let supplier = &extracted.candidates[2][binding.price_option];
        assert!(
            (option.profile.work_seconds - supplier.profile.work_seconds).abs() < 1e-15,
            "identical projections over the same rows must receive the same work price"
        );
    }
    referenced.sort_unstable();
    assert_eq!(referenced, vec![0, 1]);
    // Optional identity cycles must not disable otherwise legal pricing paths
    // or create a zero-input phantom plan. All selected DAGs still need a scan.
    let solve =
        |configured: &CandidateConfigurationInventory,
         profile: &crate::search::milp::direct_price_options::JointPriceProfile| {
            crate::search::milp::direct_price_options::solve_joint_price_proposal_with_metrics(
                &configured.inventory.graph,
                &[],
                None,
                &mut Default::default(),
                profile,
            )
            .unwrap()
            .unwrap()
        };
    let baseline = solve(&configured, &extracted);
    configured.inventory.graph.candidates[2].producer_requirements[0].compatible_producers = vec![
        PhysicalCandidateId(0),
        PhysicalCandidateId(1),
        PhysicalCandidateId(2),
        PhysicalCandidateId(3),
    ]
    .into();
    let cyclic = extract_price_options(&configured, &context)
        .unwrap()
        .unwrap();
    assert_eq!(cyclic.candidates[2].len(), 2);
    assert_eq!(cyclic.candidates[3].len(), 2);
    assert!(cyclic.candidates[2]
        .iter()
        .flat_map(|option| &option.compatible_suppliers)
        .all(|binding| binding.producer != PhysicalCandidateId(2)));
    let chosen = solve(&configured, &cyclic);
    assert!(
        (chosen.direct_modeled_cost.unwrap() - baseline.direct_modeled_cost.unwrap()).abs() < 1e-12
    );
    assert!(chosen
        .selected
        .connections
        .iter()
        .any(|edge| edge.producer.0 < 2));
}

#[test]
fn supplier_worker_buckets_remain_joint_choices_and_match_evaluator() {
    use asap_logical_asap_optimization::per_computation_candidates::{
        compose_logical_candidate, generate_per_computation_candidates, ComputationCandidate,
    };
    use asap_types::ir::schema::SketchAlgorithm;
    use std::rc::Rc;
    fn append(
        node: Rc<OperatorNode>,
        configured: &mut CandidateConfigurationInventory,
        ingestion: bool,
        mode: Materialization,
    ) -> PhysicalCandidateId {
        let ingestion = ingestion
            || (matches!(node.asap(), Some(ASAPOp::PaneChain { .. }))
                && mode == Materialization::MatAtIngestion(Medium::Memory));
        let pane = matches!(node.asap(), Some(ASAPOp::PaneChain { .. }));
        let mut inputs = Vec::new();
        node.operator
            .map_children(|child| inputs.push(append(child.clone(), configured, ingestion, mode)));
        let id = PhysicalCandidateId(configured.configurations.len());
        configured.configurations.push(CandidateConfiguration {
            source_candidate: id,
            materialization: if pane {
                mode
            } else {
                Materialization::NotMaterialized
            },
            timing: if ingestion {
                ExecutionTiming::IngestionTime
            } else {
                ExecutionTiming::QueryTime
            },
        });
        configured.inventory.templates.push(Some(node));
        configured
            .inventory
            .graph
            .candidates
            .push(PhysicalCandidate {
                group: MemoGroupId(id.0 as u32),
                local_cost: 0.0,
                producer_requirements: inputs
                    .into_iter()
                    .map(|supplier| ProducerRequirement {
                        compatible_producers: vec![supplier].into(),
                    })
                    .collect(),
            });
        id
    }

    let target = AccuracyTarget::max_error(0.05)
        .failure_probability(0.01)
        .accuracy_scope(AccuracyScope::PerValue);
    let original = lower_promql("quantile_over_time(0.95, latency[5m])", target.clone());
    let generated =
        generate_per_computation_candidates(vec![(0, QueryRoot::Operator(original.clone()))])
            .unwrap();
    let choice = generated.targets[0]
        .choices
        .candidates
        .iter()
        .position(|candidate| {
            matches!(
                candidate,
                ComputationCandidate::Summary(SketchAlgorithm::Kll)
            )
        })
        .unwrap();
    let roots: Vec<_> = compose_logical_candidate(&generated, &[choice])
        .unwrap()
        .into_iter()
        .map(|(_, root)| root)
        .collect();
    let pane_rules =
        asap_logical_asap_optimization::asap_aware_cse::pane_candidates(&roots, &[Some(60_000)]);
    let roots =
        asap_logical_asap_optimization::asap_aware_cse::window_composition_pass2_candidates(
            &roots,
            &pane_rules,
        )
        .unwrap()
        .pop()
        .unwrap()
        .roots
        .unwrap();
    let data: DataWorkload = serde_json::from_value(serde_json::json!({
        "arrival":{"ingesting":{"rows_per_second":100.0}},
        "sources":{
            "latency":{"series_count":1,"sample_interval":100,"rows_held":3000,"values_non_negative":true}
        }
    })).unwrap();
    let sized = crate::resize_for_failure_probability(&roots, std::slice::from_ref(&target), &data)
        .unwrap();
    let mut configured = CandidateConfigurationInventory {
        inventory: CandidateInventory {
            graph: CandidateDependencyGraph::default(),
            templates: vec![],
            group_candidates: vec![],
        },
        configurations: vec![],
    };
    let QueryRoot::Operator(root) = &sized[0] else {
        panic!("operator")
    };
    let root_id = append(
        root.clone(),
        &mut configured,
        false,
        Materialization::MatAtIngestion(Medium::Memory),
    );
    configured
        .inventory
        .graph
        .query_roots
        .push(QueryRootCandidates {
            compatible_candidates: vec![root_id],
        });
    let chain = configured
        .inventory
        .templates
        .iter()
        .position(|node| {
            node.as_ref()
                .is_some_and(|node| matches!(node.asap(), Some(ASAPOp::PaneChain { .. })))
        })
        .unwrap();
    let raw_supplier = configured.inventory.graph.candidates[chain].producer_requirements[0]
        .compatible_producers[0];
    assert!(matches!(
        configured.inventory.templates[raw_supplier.0]
            .as_ref()
            .unwrap()
            .non_asap(),
        Some(NonASAPOp::Scan { .. })
    ));
    // This tests the pricing kernel, not executor installation of a projection
    // below a pane. Both suppliers return exactly the same stream. The identity projection
    // adds ingestion CPU without changing rows, values, or source identity.
    let source = configured.inventory.templates[raw_supplier.0]
        .as_ref()
        .unwrap()
        .clone();
    let projected = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
        cols: source
            .schema
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| ProjectItem {
                alias: Some(field.name.clone()),
                expr: ScalarExpr::Column(index),
            })
            .collect(),
        qualifier: None,
        child: source,
    }))
    .unwrap();
    let projected_supplier = PhysicalCandidateId(configured.configurations.len());
    configured.inventory.templates.push(Some(projected));
    configured.configurations.push(CandidateConfiguration {
        source_candidate: projected_supplier,
        materialization: Materialization::NotMaterialized,
        timing: ExecutionTiming::IngestionTime,
    });
    configured
        .inventory
        .graph
        .candidates
        .push(PhysicalCandidate {
            group: configured.inventory.graph.candidates[raw_supplier.0].group,
            local_cost: 0.0,
            producer_requirements: vec![ProducerRequirement {
                compatible_producers: vec![raw_supplier].into(),
            }],
        });
    let originals = vec![QueryRoot::Operator(original)];
    let entries = vec![QueryWorkloadEntry {
        query: Query("quantile_over_time(0.95, latency[5m])".into()),
        requirements: QueryRequirements {
            accuracy: target,
            max_latency_ms: None,
        },
        predictability: Predictability::Predictable { known_at: None },
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
            60_000,
        ))),
        range_evaluation: None,
    }];
    let context = CandidatePlanningContext {
        deadline: None,
        refine_materialization: false,
        original_roots: &originals,
        entries: &entries,
        data: &data,
        models: crate::PlanningModels::builtin(),
        calibration: crate::CalibrationFit::default(),
    };
    let initial = extract(&configured, &context).unwrap().unwrap();
    let raw_work: f64 = [raw_supplier.0, chain]
        .into_iter()
        .map(|id| initial.profile.nodes[id].work_seconds * 1000.0)
        .sum();
    assert!(raw_work > 0.0);
    let mut settings = crate::PlanSelectionSettings::default();
    settings.resource_speed.ms_per_cpu_op *= 850.0 / raw_work;
    settings.resource_speed.ms_per_scanned_byte *= 850.0 / raw_work;
    settings
        .measured_operation_costs
        .operators
        .push(crate::cost::measured::OperatorCost {
            operator: crate::OperatorKind::Project,
            phase: Some(ExecutionTiming::IngestionTime),
            sketch: None,
            exact: None,
            measure: None,
            ns_per_unit: 0.0,
            ns_per_invocation: 1_000_000_000.0,
            memory_bytes_per_state: None,
        });
    let context = CandidatePlanningContext {
        models: crate::PlanningModels {
            plan_selection: &settings,
            ..context.models
        },
        ..context
    };
    configured.inventory.graph.candidates[chain].producer_requirements[0].compatible_producers =
        vec![raw_supplier, projected_supplier].into();
    let old = extract(&configured, &context).unwrap().unwrap();
    assert_eq!(
        old.suppliers[chain],
        vec![raw_supplier],
        "old fixed-bucket extraction loses the second worker choice"
    );
    let joint = extract_price_options(&configured, &context)
        .unwrap()
        .unwrap();
    assert_eq!(joint.candidates[chain].len(), 2);
    let raw_option = joint.candidates[chain]
        .iter()
        .find(|option| {
            option
                .compatible_suppliers
                .iter()
                .any(|binding| binding.producer == raw_supplier)
        })
        .unwrap();
    let projected_option = joint.candidates[chain]
        .iter()
        .find(|option| {
            option
                .compatible_suppliers
                .iter()
                .any(|binding| binding.producer == projected_supplier)
        })
        .unwrap();
    assert_eq!(raw_option.profile.compaction_seconds, 0.0);
    assert!(projected_option.profile.compaction_seconds > 0.0);
    for supplier in [raw_supplier, projected_supplier] {
        configured.inventory.graph.reader_constraints = vec![
            crate::search::candidates::graph::ReaderConstraint::ActivationRequires {
                candidate: root_id,
                required_candidate: supplier,
            },
        ];
        if supplier == raw_supplier {
            configured.inventory.graph.reader_constraints.push(
                crate::search::candidates::graph::ReaderConstraint::AllowedOutputs {
                    candidate: projected_supplier,
                    outputs: vec![],
                },
            );
        }
        let proposal =
            crate::search::milp::direct_price_options::solve_joint_price_proposal_with_metrics(
                &configured.inventory.graph,
                &[],
                None,
                &mut Default::default(),
                &joint,
            )
            .unwrap()
            .unwrap();
        let mut rejections =
            crate::search::refinement::planner::CandidateRejectionSummary::default();
        let evaluated = crate::search::refinement::planner::evaluate_configured_candidate(
            &configured,
            &context,
            &proposal.selected,
            0,
            &mut rejections,
            &mut Vec::new(),
        )
        .unwrap()
        .unwrap_or_else(|| panic!("{rejections:?}"));
        let actual = evaluated.0.costs[&evaluated.0.selected]
            .cost_per_second
            .unwrap();
        let modeled = proposal.direct_modeled_cost.unwrap();
        assert!(
            (actual - modeled).abs() <= 1e-9 * actual.max(1e-15),
            "{supplier:?}: direct {modeled}, evaluator {actual}"
        );
    }
}
