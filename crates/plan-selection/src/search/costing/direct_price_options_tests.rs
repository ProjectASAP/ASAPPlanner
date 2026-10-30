//! Price-interface regressions: compatible inputs must not disappear merely
//! because their statistics differ from the template's input.
use super::*;
use asap_logical_asap_optimization::test_support::lower_promql;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::{Operator, OperatorNode, ProjectItem, ScalarExpr};

use crate::search::candidates::dag::{
    DagOfPhysicalCandidatesWithCosts, InputSupplierCandidates, PhysicalCandidateWithCost,
    QueryOutputCandidates,
};
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
    let mut configured = PhysicalCandidatesWithCosts::from_dag_for_tests(
        DagOfPhysicalCandidatesWithCosts {
            candidates: (0..3)
                .map(|id| PhysicalCandidateWithCost {
                    local_cost: 0.0,
                    supplier_candidates_per_input: if id == 2 {
                        vec![InputSupplierCandidates {
                            candidates: vec![PhysicalCandidateId(0), PhysicalCandidateId(1)].into(),
                        }]
                    } else {
                        vec![]
                    },
                })
                .collect(),
            query_roots: vec![QueryOutputCandidates {
                candidates: vec![PhysicalCandidateId(2)],
            }],
            ..Default::default()
        },
        vec![m, n, project.clone()]
            .into_iter()
            .zip((0..3).map(|_| (ExecutionTiming::QueryTime, Materialization::NotMaterialized)))
            .collect(),
    );
    let roots = vec![QueryRoot::Operator(project)];
    let entries = [QueryWorkloadEntry {
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
        seed_separate_plans: false,
        original_roots: &roots,
        entries: &entries,
        data: &data,
        models: crate::PlanningModels::builtin(),
        calibration: asap_cost::CalibrationFit::default(),
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
                    .compatible_input_tuples
                    .iter()
                    .flatten()
                    .any(|binding| binding.producer == producer)
            })
            .expect("missing compatible supplier price option");
        assert_eq!(option.compatible_input_tuples.len(), 1);
        assert_eq!(option.compatible_input_tuples[0][0].price_option, 0);
    }
    let low = reader_options
        .iter()
        .find(|option| option.compatible_input_tuples[0][0].producer == PhysicalCandidateId(0))
        .unwrap();
    let high = reader_options
        .iter()
        .find(|option| option.compatible_input_tuples[0][0].producer == PhysicalCandidateId(1))
        .unwrap();
    assert!(
        high.profile.work_seconds > low.profile.work_seconds,
        "projection work must follow selected input rows, not template rows"
    );
    assert_eq!(
        configured.dag.candidates[2].supplier_candidates_per_input[0]
            .candidates
            .len(),
        2
    );
    // Force each structurally legal supplier in turn. The model must price
    // the selected binding rather than only returning the cheapest interface.
    for producer in [PhysicalCandidateId(0), PhysicalCandidateId(1)] {
        configured.dag.reader_constraints =
            vec![asap_types::stages::ReaderConstraint::ActivationRequires {
                candidate: PhysicalCandidateId(2),
                required_candidate: producer,
            }];
        configured.sync_physical_for_tests();
        let proposal =
            crate::search::milp::direct_price_options::solve_joint_price_proposal_with_metrics(
                &configured.dag,
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
    let mut configured = PhysicalCandidatesWithCosts::from_dag_for_tests(
        DagOfPhysicalCandidatesWithCosts {
            candidates: (0..3)
                .map(|id| PhysicalCandidateWithCost {
                    local_cost: 0.0,
                    supplier_candidates_per_input: if id == 2 {
                        vec![InputSupplierCandidates {
                            candidates: vec![PhysicalCandidateId(0), PhysicalCandidateId(1)].into(),
                        }]
                    } else {
                        vec![]
                    },
                })
                .collect(),
            query_roots: vec![QueryOutputCandidates {
                candidates: vec![PhysicalCandidateId(2)],
            }],
            ..Default::default()
        },
        vec![m, n, project.clone()]
            .into_iter()
            .zip((0..3).map(|_| (ExecutionTiming::QueryTime, Materialization::NotMaterialized)))
            .collect(),
    );
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
    configured.push_for_tests(
        downstream.clone(),
        ExecutionTiming::QueryTime,
        Materialization::NotMaterialized,
        0.0,
        vec![InputSupplierCandidates {
            candidates: vec![PhysicalCandidateId(2)].into(),
        }],
    );
    configured.dag.query_roots[0].candidates = vec![PhysicalCandidateId(3)];
    configured.sync_physical_for_tests();
    let roots = vec![QueryRoot::Operator(downstream)];
    let entries = [QueryWorkloadEntry {
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
        seed_separate_plans: false,
        original_roots: &roots,
        entries: &entries,
        data: &data,
        models: crate::PlanningModels::builtin(),
        calibration: asap_cost::CalibrationFit::default(),
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
        assert_eq!(option.compatible_input_tuples.len(), 1);
        let binding = &option.compatible_input_tuples[0][0];
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
        |configured: &PhysicalCandidatesWithCosts,
         profile: &crate::search::milp::direct_price_options::JointPriceProfile| {
            crate::search::milp::direct_price_options::solve_joint_price_proposal_with_metrics(
                &configured.dag,
                &[],
                None,
                &mut Default::default(),
                profile,
            )
            .unwrap()
            .unwrap()
        };
    let baseline = solve(&configured, &extracted);
    configured.dag.candidates[2].supplier_candidates_per_input[0].candidates = vec![
        PhysicalCandidateId(0),
        PhysicalCandidateId(1),
        PhysicalCandidateId(2),
        PhysicalCandidateId(3),
    ]
    .into();
    configured.sync_physical_for_tests();
    let cyclic = extract_price_options(&configured, &context)
        .unwrap()
        .unwrap();
    assert_eq!(cyclic.candidates[2].len(), 2);
    assert_eq!(cyclic.candidates[3].len(), 2);
    assert!(cyclic.candidates[2]
        .iter()
        .flat_map(|option| option.compatible_input_tuples.iter().flatten())
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
    use asap_logical_asap_optimization::pass1::per_computation_candidates::{
        compose_logical_candidate, generate_per_computation_candidates,
        HowToComputeAggregateMeasure,
    };
    use asap_types::ir::schema::SketchAlgorithm;
    use std::rc::Rc;
    fn append(
        node: Rc<OperatorNode>,
        configured: &mut PhysicalCandidatesWithCosts,
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
        let id = PhysicalCandidateId(configured.physical.candidates.len());
        configured.push_for_tests(
            node,
            if ingestion {
                ExecutionTiming::IngestionTime
            } else {
                ExecutionTiming::QueryTime
            },
            if pane {
                mode
            } else {
                Materialization::NotMaterialized
            },
            0.0,
            inputs
                .into_iter()
                .map(|supplier| InputSupplierCandidates {
                    candidates: vec![supplier].into(),
                })
                .collect(),
        );
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
        .ways_to_compute
        .iter()
        .position(|candidate| {
            matches!(
                candidate,
                HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Kll)
            )
        })
        .unwrap();
    let roots: Vec<_> = compose_logical_candidate(&generated, &[choice])
        .unwrap()
        .into_iter()
        .map(|(_, root)| root)
        .collect();
    let pane_rules = asap_logical_asap_optimization::pass2::asap_aware_cse::pane_candidates(
        &roots,
        &[Some(60_000)],
    );
    let roots =
        asap_logical_asap_optimization::pass2::asap_aware_cse::window_composition_pass2_candidates(
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
    let mut configured = PhysicalCandidatesWithCosts::empty_for_tests();
    let QueryRoot::Operator(root) = &sized[0] else {
        panic!("operator")
    };
    let root_id = append(
        root.clone(),
        &mut configured,
        false,
        Materialization::MatAtIngestion(Medium::Memory),
    );
    configured.dag.query_roots.push(QueryOutputCandidates {
        candidates: vec![root_id],
    });
    configured.sync_physical_for_tests();
    let chain = configured
        .nodes_for_tests()
        .iter()
        .position(|node| matches!(node.asap(), Some(ASAPOp::PaneChain { .. })))
        .unwrap();
    let raw_supplier =
        configured.dag.candidates[chain].supplier_candidates_per_input[0].candidates[0];
    configured.sync_physical_for_tests();
    assert!(matches!(
        configured.node(raw_supplier.0).non_asap(),
        Some(NonASAPOp::Scan { .. })
    ));
    // This tests the pricing kernel, not executor installation of a projection
    // below a pane. Both suppliers return exactly the same stream. The identity projection
    // adds ingestion CPU without changing rows, values, or source identity.
    let source = configured.node(raw_supplier.0).clone();
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
    let projected_supplier = configured.push_for_tests(
        projected,
        ExecutionTiming::IngestionTime,
        Materialization::NotMaterialized,
        0.0,
        vec![InputSupplierCandidates {
            candidates: vec![raw_supplier].into(),
        }],
    );
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
        seed_separate_plans: false,
        original_roots: &originals,
        entries: &entries,
        data: &data,
        models: crate::PlanningModels::builtin(),
        calibration: asap_cost::CalibrationFit::default(),
    };
    let initial = extract(&configured, &context).unwrap().unwrap();
    let raw_work: f64 = [raw_supplier.0, chain]
        .into_iter()
        .map(|id| initial.profile.nodes[id].work_seconds * 1000.0)
        .sum();
    assert!(raw_work > 0.0);
    let mut settings = asap_cost::CostInputs::default();
    settings.resource_speed.ms_per_cpu_op *= 850.0 / raw_work;
    settings.resource_speed.ms_per_scanned_byte *= 850.0 / raw_work;
    settings
        .measured_operation_costs
        .operators
        .push(asap_cost::measured::OperatorCost {
            operator: asap_cost::OperatorKind::Project,
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
    configured.dag.candidates[chain].supplier_candidates_per_input[0].candidates =
        vec![raw_supplier, projected_supplier].into();
    configured.sync_physical_for_tests();
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
                .compatible_input_tuples
                .iter()
                .flatten()
                .any(|binding| binding.producer == raw_supplier)
        })
        .unwrap();
    let projected_option = joint.candidates[chain]
        .iter()
        .find(|option| {
            option
                .compatible_input_tuples
                .iter()
                .flatten()
                .any(|binding| binding.producer == projected_supplier)
        })
        .unwrap();
    assert_eq!(raw_option.profile.compaction_seconds, 0.0);
    assert!(projected_option.profile.compaction_seconds > 0.0);
    for supplier in [raw_supplier, projected_supplier] {
        configured.dag.reader_constraints =
            vec![asap_types::stages::ReaderConstraint::ActivationRequires {
                candidate: root_id,
                required_candidate: supplier,
            }];
        configured.sync_physical_for_tests();
        if supplier == raw_supplier {
            configured.dag.reader_constraints.push(
                asap_types::stages::ReaderConstraint::AllowedOutputs {
                    candidate: projected_supplier,
                    outputs: vec![],
                },
            );
            configured.sync_physical_for_tests();
        }
        let proposal =
            crate::search::milp::direct_price_options::solve_joint_price_proposal_with_metrics(
                &configured.dag,
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

#[test]
fn mixed_schedule_joint_prices_match_the_real_evaluator() {
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
    let mut configured = PhysicalCandidatesWithCosts::from_dag_for_tests(
        DagOfPhysicalCandidatesWithCosts {
            candidates: (0..3)
                .map(|id| PhysicalCandidateWithCost {
                    local_cost: 0.0,
                    supplier_candidates_per_input: if id == 2 {
                        vec![InputSupplierCandidates {
                            candidates: vec![PhysicalCandidateId(0), PhysicalCandidateId(1)].into(),
                        }]
                    } else {
                        vec![]
                    },
                })
                .collect(),
            query_roots: vec![QueryOutputCandidates {
                candidates: vec![PhysicalCandidateId(2)],
            }],
            ..Default::default()
        },
        vec![m, n, project.clone()]
            .into_iter()
            .zip((0..3).map(|_| (ExecutionTiming::QueryTime, Materialization::NotMaterialized)))
            .collect(),
    );
    let roots = vec![QueryRoot::Operator(project); 5];
    let entries = [QueryWorkloadEntry {
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
    use asap_types::workload::{DemandEstimate, Rate, TimestampMs};
    let mut entries = vec![entries[0].clone(); 5];
    entries[0].recurrence =
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(59_000)));
    entries[1].recurrence =
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(61_000)));
    entries[2].recurrence =
        QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(DemandEstimate {
            expected_rate: Rate(0.137),
            observed_at: None,
            valid_for: None,
        }));
    entries[3].recurrence = QueryRecurrence::Repeated(RepeatedDemand::Scheduled(vec![
        TimestampMs(10),
        TimestampMs(20),
    ]));
    entries[4].recurrence = QueryRecurrence::OneTime {
        invocations: 3,
        execute_at: None,
    };
    configured.dag.query_roots = vec![configured.dag.query_roots[0].clone(); 5];
    configured.sync_physical_for_tests();
    // All roots use the original source; this parity check has no synthetic
    // equivalence between different source names.
    configured.dag.candidates[2].supplier_candidates_per_input[0].candidates =
        vec![PhysicalCandidateId(0)].into();
    configured.sync_physical_for_tests();
    let context = CandidatePlanningContext {
        deadline: None,
        refine_materialization: false,
        seed_separate_plans: false,
        original_roots: &roots,
        entries: &entries,
        data: &data,
        models: crate::PlanningModels::builtin(),
        calibration: asap_cost::CalibrationFit::default(),
    };
    let profile = extract_price_options(&configured, &context)
        .unwrap()
        .unwrap();
    assert!(profile.schedules.is_some());
    let proposal =
        crate::search::milp::direct_price_options::solve_joint_price_proposal_with_metrics(
            &configured.dag,
            &[],
            None,
            &mut Default::default(),
            &profile,
        )
        .unwrap()
        .unwrap();
    let mut rejections = crate::search::refinement::planner::CandidateRejectionSummary::default();
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
        "mixed schedules: {modeled} vs {actual}"
    );
}
