//! End-to-end query-string → post-ASAP IR pin (issue #98).
//!
//! Drives the full pipeline — PromQL text → non-ASAP `OperatorNode`
//! (`lower_promql`) → post-ASAP `OperatorNode` DAG (via
//! `ASAPStrategies::replacements`, see [`realize`] below) — and pins
//! the summary-bound shape node by node, including the family `(Kind,
//! Params)` committed on each edge's schema.

use std::rc::Rc;

use asap_aware_mapping::accuracy::{
    AccuracyEvidenceProvider, DefaultAccuracyModel, EqualSplitAllocator, PropagationStats,
    QuantileInputDomain,
};
use asap_aware_mapping::cost_model::DefaultCostModel;
use asap_aware_mapping::plan_selection::candidate_selection::global_selection;
use asap_aware_mapping::replacement::{is_logical_rewrite, retain_exact, RealizationError};
use asap_aware_mapping::{
    search_workload, search_workload_with_targets, ASAPStrategies, AccuracyModel, Replacement,
    ReplacementStrategy, ReplacementSubDAG, TargetSubDAG,
};
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::post_asap::{
    maintained, maintained_post_asap_dag, post_asap_dag, timed,
};
use asap_types::ir::operator::Reduction;
use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
use asap_types::ir::properties::CompositionOperator;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::DataType;
use asap_types::ir::schema::{
    EntityIdentity, ExactKind, ExactParams, FieldDataType, GroupingStrategy, Schema,
    SketchAlgorithm, SketchKind, SketchParams, SketchStatistic, SummaryInputExpr, SummaryUpdate,
};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, ScalarExpr};
use asap_types::types::AccuracyTarget;

/// This crate has no "bind me one tree" public API any more —
/// `ASAPStrategies::replacements` always returns every candidate, and
/// a caller decides what to keep. This test-only helper reproduces the
/// take-the-first-(`cost_model`-preferred)-candidate pattern so the
/// single-answer pins below don't all repeat it by hand.
fn realize(root: &Rc<OperatorNode>) -> Result<Rc<OperatorNode>, RealizationError> {
    let target = TargetSubDAG::new(root);
    match ASAPStrategies::default()
        .replacements(&target)
        .into_iter()
        .next()
    {
        // A bound decision (summary DAG or kept sub-DAG); a logical rewrite
        // is not a binding, so it falls back to keeping the target.
        Some(ReplacementSubDAG {
            replacement: Replacement::SubDAG(node),
            ..
        }) if !is_logical_rewrite(&node) => Ok(node),
        _ => retain_exact(root),
    }
    .inspect(|node| {
        node.validate_structure()
            .expect("planned dag satisfies the unified IR contract")
    })
}

#[test]
fn distinct_over_time_offers_hll_cardinality_evaluation() {
    // The real frontend must reach an existing HLL candidate without a
    // function-specific post-ASAP node or a sample-count rewrite.
    let root = lower_promql(
        "distinct_over_time(cpu_usage{job=\"worker\"}[5m])",
        AccuracyTarget::Epsilon(0.02),
    )
    .unwrap();
    let candidates = ASAPStrategies::default().replacements(&TargetSubDAG::new(&root));
    for candidate in &candidates {
        if let Replacement::SubDAG(node) = &candidate.replacement {
            node.validate_structure().unwrap();
        }
    }
    assert!(candidates.iter().any(|candidate| {
        let Replacement::SubDAG(node) = &candidate.replacement else { return false };
        let Some(ASAPOp::SummaryEstimate { summary_input, query, .. }) = node.asap() else { return false };
        matches!(query, SketchStatistic::Cardinality)
            && matches!(summary_input.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. })
                if kind.algorithm() == &SketchAlgorithm::Hll)
    }), "no HLL cardinality candidate: {candidates:?}");
}

fn lower_search_and_materialize(query: &str) -> Rc<OperatorNode> {
    let pre = lower_promql(query, AccuracyTarget::Exact).expect("lowering failed");
    let space = search_workload(vec![("query", pre)]);
    let selection = global_selection(&space, &DefaultCostModel);
    selection
        .assemble_selected_dag(&space.roots[0].1)
        .expect("materialization failed")
        .expect("root must be discovered")
}

#[test]
fn value_ranked_topk_preserves_summary_children_in_post_asap_dag() {
    for query in [
        "topk(3, rate(cpu_seconds_total[5m]))",
        "topk by (job) (2, max_over_time(memory_bytes[6h]))",
    ] {
        let root = timed(&lower_search_and_materialize(query));
        let Some(NonASAPOp::Limit {
            n,
            offset,
            child: sort,
            ..
        }) = root.non_asap()
        else {
            panic!(
                "expected query-time Limit for {query}, got {:?}",
                root.operator
            );
        };
        assert_eq!(
            root.timing,
            Some(asap_types::ir::properties::ExecutionTiming::QueryTime)
        );
        assert!(n.is_some_and(|n| n > 0) && *offset == 0);
        let Some(NonASAPOp::Sort { child, .. }) = sort.non_asap() else {
            panic!("expected query-time Sort under Limit for {query}");
        };
        assert_eq!(
            sort.timing,
            Some(asap_types::ir::properties::ExecutionTiming::QueryTime)
        );
        let Some(ASAPOp::FinalizeExactAccumulator { child: state }) = child.asap() else {
            panic!(
                "Sort must consume finalized values for {query}: {:?}",
                child.operator
            );
        };
        assert!(matches!(state.asap(), Some(ASAPOp::SummaryAgg { .. })));
        assert!(child
            .schema
            .fields
            .iter()
            .all(|field| matches!(field.dtype, FieldDataType::Plain(_))));
    }
}

#[test]
fn exact_counter_weighted_topk_fails_closed_without_membership_certificate() {
    for query in [
        "topk(2, sum by(job)(rate(m[1m])))",
        "topk(3, sum by(job)(rate(cpu_seconds_total[1h])))",
        "topk(3, sum by(job)(increase(requests_total[6h])))",
    ] {
        let root = lower_search_and_materialize(query);
        assert!(
            !root.contains_asap(),
            "exact target must not accept an uncertified membership sidecar for {query}: {:?}",
            root.operator
        );
    }
}

#[test]
fn instant_topk_and_unsupported_child_remain_local_residuals() {
    for query in ["topk(3, memory_bytes)", "topk(3, deriv(memory_bytes[5m]))"] {
        let root = lower_search_and_materialize(query);
        let Some(NonASAPOp::Limit { child: sort, .. }) = root.non_asap() else {
            panic!("expected Limit for {query}");
        };
        let Some(NonASAPOp::Sort { child, .. }) = sort.non_asap() else {
            panic!("expected Sort for {query}");
        };
        assert!(
            !child.contains_asap(),
            "only the unsupported child should remain exact for {query}"
        );
    }
}

fn dtype<'a>(schema: &'a Schema, name: &str) -> &'a FieldDataType {
    &schema
        .fields
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no field {name:?} in {schema:?}"))
        .dtype
}

fn lower_and_realize(query: &str) -> Rc<OperatorNode> {
    let pre = lower_promql(query, AccuracyTarget::Exact).expect("lowering failed");
    realize(&pre).expect("binding failed")
}

#[test]
fn promql_binary_arithmetic_retains_two_summary_leaves() {
    for op in ["+", "-", "*", "/", "%", "^", "atan2"] {
        let root = lower_and_realize(&format!("rate(a[1m]) {op} rate(b[1m])"));
        let Some(NonASAPOp::BinaryOp { lhs, rhs, .. }) = root.non_asap() else {
            panic!("expected BinaryOp for {op}, got {:?}", root.operator);
        };
        for operand in [lhs, rhs] {
            let Some(ASAPOp::FinalizeExactAccumulator { child }) = operand.asap() else {
                panic!(
                    "expected an explicit exact evaluation, got {:?}",
                    operand.operator
                );
            };
            assert!(matches!(child.asap(), Some(ASAPOp::SummaryAgg { .. })));
        }
    }
}

#[test]
fn value_ranked_topk_over_binary_ratio_finalizes_both_summary_operands() {
    let query = "topk(1, sum by(job)(increase(a[6h])) / sum by(job)(increase(b[6h])))";
    let root = lower_search_and_materialize(query);
    let Some(NonASAPOp::Limit {
        n: Some(1),
        offset: 0,
        child: sort,
        ..
    }) = root.non_asap()
    else {
        panic!("expected Limit root, got {:?}", root.operator);
    };
    let Some(NonASAPOp::Sort { child: binary, .. }) = sort.non_asap() else {
        panic!("expected Sort below Limit, got {:?}", sort.operator);
    };
    let Some(NonASAPOp::BinaryOp { lhs, rhs, .. }) = binary.non_asap() else {
        panic!("expected BinaryOp below Sort, got {:?}", binary.operator);
    };
    for operand in [lhs, rhs] {
        let Some(ASAPOp::FinalizeExactAccumulator { child }) = operand.asap() else {
            panic!(
                "expected exact accumulator finalization, got {:?}",
                operand.operator
            );
        };
        assert!(matches!(child.asap(), Some(ASAPOp::SummaryAgg { .. })));
    }
}

struct SeparatedTopK;

impl AccuracyEvidenceProvider for SeparatedTopK {
    fn topk_max_distinct_items(&self, _: &OperatorNode) -> Option<u64> {
        Some(1000)
    }

    fn propagation_stats(
        &self,
        op: &CompositionOperator,
        _family: &FieldDataType,
        _query: Option<&SketchStatistic>,
    ) -> PropagationStats {
        matches!(op, CompositionOperator::TopKSelection)
            .then_some(PropagationStats {
                topk_selected_lower_bound: Some(101.0),
                topk_excluded_upper_bound: Some(100.0),
                topk_interval_failure_probability: Some(0.001),
                ..Default::default()
            })
            .unwrap_or_default()
    }
}

// Rate-weighted summaries must consume finalized rates, never raw counter deltas.
#[test]
fn grouped_rate_topk_consumes_finalized_rate_values() {
    let root = lower_promql(
        "topk by(job)(2, sum by(service, job)(rate(m[1m])))",
        AccuracyTarget::Epsilon(0.01),
    )
    .unwrap();
    let strategy = ASAPStrategies::new_with_planning_inputs_and_evidence(
        &DefaultAccuracyModel,
        &EqualSplitAllocator,
        &SeparatedTopK,
    );
    let plan = strategy
        .replacements(&TargetSubDAG::new(&root))
        .into_iter()
        .find_map(|candidate| match candidate.replacement {
            Replacement::SubDAG(node) if candidate.rationale.contains("CmsWithHeap") => Some(node),
            _ => None,
        })
        .expect("rate-weighted CMS plan");
    let dag = post_asap_dag(&plan);
    assert!(!dag.nodes.iter().any(|node| matches!(
        node.payload,
        PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Join { .. })
    )));
    let node = dag
        .nodes
        .iter()
        .find(|node| {
            matches!(&node.payload,
        PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. })
        if kind.algorithm() == &SketchAlgorithm::CmsWithHeap)
        })
        .unwrap();
    assert_eq!(
        node.output_state.timing,
        asap_types::ir::properties::ExecutionTiming::QueryTime
    );
    let PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg { input, .. }) = &node.payload else {
        unreachable!()
    };
    assert_eq!(
        input.weight,
        SummaryInputExpr::Column(ColumnRef::SampleValue)
    );
    assert_eq!(
        input.item,
        Some(SummaryInputExpr::Column(ColumnRef::Named("service".into())))
    );
}

// Selection is adaptive: a per-key score bound alone cannot certify all returned rows.
#[test]
fn weighted_topk_keeps_candidates_with_missing_population_evidence() {
    struct NoPopulationBound;
    impl AccuracyEvidenceProvider for NoPopulationBound {
        fn propagation_stats(
            &self,
            op: &CompositionOperator,
            family: &FieldDataType,
            query: Option<&SketchStatistic>,
        ) -> PropagationStats {
            SeparatedTopK.propagation_stats(op, family, query)
        }
    }
    let root = lower_promql(
        "topk by(job)(2, sum by(service, job)(rate(m[1m])))",
        AccuracyTarget::Epsilon(0.01),
    )
    .unwrap();
    let strategy = ASAPStrategies::new_with_planning_inputs_and_evidence(
        &DefaultAccuracyModel,
        &EqualSplitAllocator,
        &NoPopulationBound,
    );
    let candidates = strategy.replacements(&TargetSubDAG::new(&root));
    assert!(candidates
        .iter()
        .any(|candidate| candidate.rationale.contains("CmsWithHeap")
            && candidate.has_missing_accuracy_evidence()));
}

// Unknown requirements must survive physical export for deployment to inspect.
#[test]
fn weighted_topk_exports_symbolic_evidence_requirements() {
    let root = lower_promql(
        "topk by(job)(2, sum by(service, job)(rate(m[1m])))",
        AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.01,
        },
    )
    .unwrap();
    let candidates = ASAPStrategies::default().replacements(&TargetSubDAG::new(&root));
    let candidate = candidates
        .iter()
        .find(|candidate| candidate.rationale.contains("CmsWithHeap"))
        .unwrap();
    assert!(candidate.has_missing_accuracy_evidence());
    let Replacement::SubDAG(node) = &candidate.replacement else {
        panic!("summary candidate")
    };
    let dag = post_asap_dag(node);
    let exported = serde_json::to_string(&dag).unwrap();
    assert!(exported.contains("topk_max_distinct_items"));
    assert!(exported.contains("topk_membership_margin"));
    assert!(node.guarantee.as_ref().unwrap().has_unknown());
}

// Supplied invalid facts are distinct from absent evidence.
#[test]
fn weighted_topk_rejects_invalid_population_evidence() {
    struct InvalidPopulation;
    impl AccuracyEvidenceProvider for InvalidPopulation {
        fn topk_max_distinct_items(&self, _: &OperatorNode) -> Option<u64> {
            Some(0)
        }
    }
    let root = lower_promql(
        "topk by(job)(2, sum by(service, job)(rate(m[1m])))",
        AccuracyTarget::Epsilon(0.01),
    )
    .unwrap();
    let strategy = ASAPStrategies::new_with_planning_inputs_and_evidence(
        &DefaultAccuracyModel,
        &EqualSplitAllocator,
        &InvalidPopulation,
    );
    assert!(strategy.replacements(&TargetSubDAG::new(&root)).is_empty());
}

// The summary's estimate is projected back to logical service/job score rows.
#[test]
fn rate_and_increase_topk_use_summary_scores_and_grouped_limits() {
    for query in [
        "topk(2, sum by(job)(rate(m[1m])))",
        "topk by(job)(2, sum by(service, job)(rate(m[1m])))",
        "topk(2, sum by(job)(increase(m[6h])))",
    ] {
        let root = lower_promql(
            query,
            AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            },
        )
        .unwrap();
        let strategy = ASAPStrategies::new_with_planning_inputs_and_evidence(
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &SeparatedTopK,
        );
        let plan = strategy
            .replacements(&TargetSubDAG::new(&root))
            .into_iter()
            .find_map(|candidate| match candidate.replacement {
                Replacement::SubDAG(node) if candidate.rationale.contains("CmsWithHeap") => {
                    Some(node)
                }
                _ => None,
            })
            .expect("weighted summary");
        let Some(NonASAPOp::Limit {
            n: Some(2),
            offset: 0,
            partition_by,
            child: sorted,
        }) = plan.non_asap()
        else {
            panic!("grouped limit")
        };
        let Some(NonASAPOp::Sort {
            partition_by: sort_groups,
            child: projected,
            ..
        }) = sorted.non_asap()
        else {
            panic!("grouped sort")
        };
        assert_eq!(partition_by, sort_groups);
        assert_eq!(partition_by.len(), usize::from(query.contains("topk by")));
        let Some(NonASAPOp::Project {
            child: evaluation, ..
        }) = projected.non_asap()
        else {
            panic!("logical output projection")
        };
        let Some(ASAPOp::SummaryEstimate {
            summary_input,
            query: SketchStatistic::TopK { k },
        }) = evaluation.asap()
        else {
            panic!("heap evaluation")
        };
        assert!(*k > 2, "candidate capacity is independent of output count");
        let Some(ASAPOp::SummaryAgg {
            child: rates,
            input,
            ..
        }) = summary_input.asap()
        else {
            panic!("weighted summary")
        };
        assert_eq!(
            input.weight,
            SummaryInputExpr::Column(ColumnRef::SampleValue)
        );
        assert!(matches!(
            rates.asap(),
            Some(ASAPOp::FinalizeExactAccumulator { .. })
        ));
        let dag = post_asap_dag(&plan);
        for phase in [
            asap_types::ir::properties::ExecutionTiming::IngestionTime,
            asap_types::ir::properties::ExecutionTiming::QueryTime,
        ] {
            let phases = dag.nodes.iter().map(|node| (node.id, phase)).collect();
            let placed = dag.with_execution_phases(&phases).unwrap();
            assert!(placed
                .nodes
                .iter()
                .all(|node| node.output_state.timing == phase));
        }
        let guarantee = plan.guarantee.as_ref().unwrap();
        assert!(guarantee.failure_probability.evaluate().unwrap() <= 0.01);
        assert!(guarantee.provenance.iter().any(|source| matches!(source,
            asap_types::ir::properties::GuaranteeSource::ChildGuarantee { guarantee, .. }
            if guarantee.metric == asap_types::ir::properties::ErrorMetric::Frequency)));
    }
}

#[test]
fn promql_binary_arithmetic_preserves_both_scalar_operand_orders() {
    for (query, scalar_left) in [("rate(a[1m]) / 2", false), ("2 / rate(a[1m])", true)] {
        let root = lower_and_realize(query);
        let Some(NonASAPOp::Project { cols, .. }) = root.non_asap() else {
            panic!("expected Project")
        };
        let ScalarExpr::Arithmetic { left, right, .. } = &cols[1].expr else {
            panic!()
        };
        let (scalar, sample) = if scalar_left {
            (left, right)
        } else {
            (right, left)
        };
        assert_eq!(**scalar, ScalarExpr::literal_f64(2.0));
        assert_eq!(**sample, ScalarExpr::Column(1));
        assert!(root.schema.has_promql_series_identity());
    }
}

#[test]
fn promql_binary_arithmetic_falls_back_as_a_whole_for_unsupported_arm() {
    let root = lower_and_realize("rate(a[1m]) + stddev_over_time(b[1m])");
    assert!(!root.contains_asap());
}

#[test]
fn promql_binary_arithmetic_preserves_nested_structure_and_rejects_modifiers() {
    let nested = lower_and_realize("(rate(a[1m]) + rate(b[1m])) / 2");
    let Some(NonASAPOp::Project { child: lhs, .. }) = nested.non_asap() else {
        panic!("expected outer BinaryOp, got {:?}", nested.operator);
    };
    assert!(matches!(lhs.non_asap(), Some(NonASAPOp::BinaryOp { .. })));

    let modified = lower_and_realize("rate(a[1m]) + on(job) rate(b[1m])");
    assert!(!modified.contains_asap());
}

#[test]
fn promql_binary_arithmetic_never_relabels_approximate_children_as_exact() {
    let pre = lower_promql(
        "quantile_over_time(0.9, a[1m]) + quantile_over_time(0.9, b[1m])",
        AccuracyTarget::Epsilon(0.01),
    )
    .expect("lowering failed");
    let root = realize(&pre).expect("binding failed");
    let Some(NonASAPOp::BinaryOp { lhs, rhs, .. }) = root.non_asap() else {
        panic!("expected BinaryOp, got {:?}", root.operator);
    };
    assert!(lhs.guarantee.as_ref().is_some_and(|g| !g.is_exact()));
    assert!(rhs.guarantee.as_ref().is_some_and(|g| !g.is_exact()));
    assert!(
        root.guarantee.is_none(),
        "unknown composed error must fail closed"
    );
}

#[test]
fn ddsketch_quantile_ratio_meets_the_shared_relative_error_target() {
    // Enforced finite, positive, nonempty windows justify both DDSketch
    // interpolation bounds and a nonzero denominator. Shared state does not
    // require independence for the deterministic ratio bound.
    let target = AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.01,
    };
    let query = lower_promql(
        "quantile_over_time(0.9, data[5m]) / quantile_over_time(0.5, data[5m])",
        target.clone(),
    )
    .expect("lowering failed");

    let evidence = FixtureQuantileDomain {
        lower: 1.0,
        upper: 100.0,
    };
    let space = search_workload_with_targets(
        vec![("ratio", query, Some(target.clone()))],
        &asap_aware_mapping::replacement::default_strategies_with_evidence(&evidence),
        &DefaultAccuracyModel,
    );
    let root = &space.roots[0].1;
    let selected = global_selection(&space, &DefaultCostModel);
    let chosen = selected
        .for_target(root)
        .and_then(|selection| selection.chosen.as_ref())
        .expect("the certified DDSketch ratio should be selectable");
    let Replacement::SubDAG(node) = &chosen.replacement else {
        panic!("expected a summary candidate")
    };
    let guarantee = node.guarantee.as_ref().expect("ratio guarantee");
    assert_eq!(guarantee.failure_probability.evaluate(), Some(0.0));
    assert!(
        DefaultAccuracyModel.satisfies(guarantee, &target),
        "ratio guarantee should satisfy the requested target: {guarantee:?}"
    );

    let shared = asap_types::ir::cse::share_common_sub_dags(vec![("ratio", node.clone())]);
    let Some(NonASAPOp::BinaryOp { lhs, rhs, .. }) = shared[0].1.non_asap() else {
        panic!("expected binary ratio")
    };
    let producer = |evaluation: &Rc<OperatorNode>| match &evaluation.operator {
        Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => Rc::clone(summary_input),
        other => panic!("expected DDSketch evaluation, got {other:?}"),
    };
    assert!(
        Rc::ptr_eq(&producer(lhs), &producer(rhs)),
        "the two quantile evaluations should share one DDSketch producer"
    );
}

#[test]
fn planner_only_e2e_temporal_topk_preserves_query_update_and_evaluation_contract() {
    // Self-contained Planner E2E: each case starts from PromQL text and ends
    // at the post-ASAP summary DAG. No controller/backend types,
    // fixtures, configuration, or runtime are involved.
    let cases = [
        (
            "topk by (service) (5, count_over_time(requests[1m]))",
            SummaryInputExpr::Constant(1.0),
            "CmsWithHeap",
            vec![ColumnRef::Named("service".into())],
        ),
        (
            "topk(5, sum_over_time(requests[1m]))",
            SummaryInputExpr::Column(ColumnRef::SampleValue),
            "CountSketchWithHeap",
            vec![],
        ),
    ];
    for (source, expected_update, expected_family, excluded_labels) in cases {
        let pre = lower_promql(
            source,
            AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            },
        )
        .expect("lower temporal Top-K");
        let strategy = ASAPStrategies::new_with_planning_inputs_and_evidence(
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &SeparatedTopK,
        );
        let candidate = strategy
            .replacements(&TargetSubDAG::new(&pre))
            .into_iter()
            .find_map(|candidate| match candidate.replacement {
                Replacement::SubDAG(node) if candidate.rationale.contains(expected_family) => {
                    Some(node)
                }
                _ => None,
            })
            .expect("heap-backed temporal Top-K candidate");
        let Some(ASAPOp::SummaryEstimate {
            summary_input,
            query: SketchStatistic::TopK { k, .. },
        }) = candidate.asap()
        else {
            panic!("expected Top-K estimate, got {:?}", candidate.operator)
        };
        assert_eq!(
            *k, 5,
            "the requested Top-K cardinality must survive binding"
        );
        let Some(ASAPOp::SummaryAgg {
            input: state_input,
            family,
            child,
            ..
        }) = summary_input.asap()
        else {
            panic!("expected structured Top-K state input")
        };
        let FieldDataType::Sketch(kind, _) = family else {
            panic!("expected a heap-backed sketch family, got {family:?}")
        };
        assert_eq!(format!("{:?}", kind.algorithm()), expected_family);
        let heap_size = match kind.params() {
            SketchParams::CmsWithHeap { heap_size, .. }
            | SketchParams::CountSketchWithHeap { heap_size, .. } => *heap_size,
            params => panic!("expected heap-bearing Top-K parameters, got {params:?}"),
        };
        assert_eq!(heap_size, 100u32.max(*k as u32));
        assert_eq!(
            state_input.item.as_ref(),
            Some(&SummaryInputExpr::EntityIdentity(
                EntityIdentity::PromqlLabelSet {
                    excluding: excluded_labels
                }
            ))
        );
        assert_eq!(state_input.weight, expected_update);
        assert!(!child.contains_asap());
    }
}

/// Execute the ungrouped temporal TopK subset with exact state. This tests
/// the emitted update contract, not sketch approximation or backend execution.
fn execute_topk_reference(plan: &OperatorNode) -> Vec<(String, f64)> {
    use std::collections::BTreeMap;
    let Some(ASAPOp::SummaryEstimate {
        summary_input,
        query: SketchStatistic::TopK { k },
    }) = plan.asap()
    else {
        panic!("expected TopK evaluation")
    };
    let Some(ASAPOp::SummaryAgg {
        input,
        child,
        reduction,
        ..
    }) = summary_input.asap()
    else {
        panic!("expected summary updates")
    };
    assert_eq!(reduction, &Reduction::by(vec![]));
    // The fused raw input is the kept non-ASAP sub-DAG itself.
    assert!(!child.contains_asap(), "expected fused raw input");
    let Some(NonASAPOp::TimeRange { range, child, .. }) = child.non_asap() else {
        panic!("expected temporal input")
    };
    let Some(NonASAPOp::Scan {
        source: asap_types::ir::operator::Source::TimeSeries { metric },
        predicates,
        ..
    }) = child.non_asap()
    else {
        panic!("expected metric scan")
    };
    assert!(
        predicates.is_empty(),
        "fixture executor does not support filters"
    );
    let Some(SummaryInputExpr::EntityIdentity(EntityIdentity::PromqlLabelSet { excluding })) =
        &input.item
    else {
        panic!("expected PromQL item identity")
    };
    // api wins by sample count; worker wins by sum. Negative updates must
    // subtract, and samples outside (evaluation - range, evaluation] cannot rank.
    let samples = [
        ("requests", "api", 10, 1.0),
        ("requests", "api", 20, 2.0),
        ("requests", "api", 30, 3.0),
        ("requests", "api", 60, 4.0),
        ("requests", "worker", 10, 150.0),
        ("requests", "worker", 20, -50.0),
        ("requests", "cron", 10, 10.0),
        ("requests", "cron", 20, 10.0),
        ("requests", "cron", 30, 10.0),
        ("requests", "expired", 0, 10000.0),
        ("requests", "future", 61, 10000.0),
        ("other", "unrelated", 30, 10000.0),
    ];
    let mut totals = BTreeMap::<String, f64>::new();
    for (name, job, timestamp, value) in samples {
        if name != metric || timestamp <= 60_i64 - range.as_secs() as i64 || timestamp > 60 {
            continue;
        }
        let labels = [("__name__", name), ("job", job)]
            .into_iter()
            .filter(|(label, _)| !excluding.contains(&ColumnRef::Named((*label).into())))
            .map(|(label, value)| format!("{label}={value}"))
            .collect::<Vec<_>>()
            .join(",");
        let weight = match &input.weight {
            SummaryInputExpr::Constant(weight) => *weight,
            SummaryInputExpr::Column(ColumnRef::SampleValue) => value,
            unsupported => panic!("unsupported fixture update: {unsupported:?}"),
        };
        *totals.entry(labels).or_default() += weight;
    }
    let mut ranked: Vec<_> = totals.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(*k);
    ranked
}

#[test]
fn planner_heap_topk_reference_execution_matches_ground_truth() {
    // Pin numeric results independently of the emitted IR: swapping weights,
    // losing identity, changing the window, or dropping k changes the answer.
    for (query, expected) in [
        ("topk(1, count_over_time(requests[1m]))", vec![("api", 4.0)]),
        (
            "topk(2, count_over_time(requests[1m]))",
            vec![("api", 4.0), ("cron", 3.0)],
        ),
        (
            "topk(1, sum_over_time(requests[1m]))",
            vec![("worker", 100.0)],
        ),
        (
            "topk(2, sum_over_time(requests[1m]))",
            vec![("worker", 100.0), ("cron", 30.0)],
        ),
    ] {
        let pre = lower_promql(
            query,
            AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            },
        )
        .unwrap();
        let strategy = ASAPStrategies::new_with_planning_inputs_and_evidence(
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &SeparatedTopK,
        );
        // This reference executor consumes keyed heap updates. The inventory
        // also contains maintained exact values followed by sort/limit; those
        // have a different execution contract and must not enter this fixture.
        let candidates: Vec<_> = strategy.replacements(&TargetSubDAG::new(&pre)).into_iter().filter(|candidate| matches!(&candidate.replacement, Replacement::SubDAG(plan) if matches!(plan.asap(), Some(ASAPOp::SummaryEstimate { query: SketchStatistic::TopK { .. }, .. })))).collect();
        assert!(!candidates.is_empty(), "no heap candidate for {query}");
        for candidate in candidates {
            let Replacement::SubDAG(plan) = candidate.replacement else {
                panic!("expected summary plan for {query}")
            };
            let expected: Vec<_> = expected
                .iter()
                .map(|(job, score)| (format!("__name__=requests,job={job}"), *score))
                .collect();
            assert_eq!(execute_topk_reference(&plan), expected, "{query}");
        }
    }
}

/// `quantile(0.99, rate(http_requests_total[5m]))` at ε = 0.01:
///
/// ```text
/// SummaryEstimate { query: Quantile{0.99} }          → {quantile_0_99: Float64}
/// └─ SummaryAgg { Kll{k:269}, input: SampleValue }   → {value: Sketch(Kll, {k:269})}
///    └─ SummaryAgg { Rate, input: SampleValue }      → {ts, value: ExactAggregate(Rate), …}
///       └─ TimeRange{5m} → Scan                      → {ts, value}
/// ```
///
/// The nested tree exercises both realizations: the approximate quantile
/// binds a KLL sketch + evaluation; the per-series `rate` binds the exact
/// counter-reset-aware accumulator (no estimate — its state is the value).
#[test]
fn promql_quantile_of_rate_binds_kll_over_rate_accumulator() {
    let pre_asap = lower_promql(
        "quantile(0.99, rate(http_requests_total[5m]))",
        AccuracyTarget::Epsilon(0.01),
    )
    .expect("lowering failed");
    let root = realize(&pre_asap).expect("binding failed");

    // Root: the sketch evaluation, back to a plain row shape.
    let Some(ASAPOp::SummaryEstimate {
        summary_input,
        query,
    }) = root.asap()
    else {
        panic!("expected SummaryEstimate root, got {:?}", root.operator);
    };
    assert!(matches!(query, SketchStatistic::Quantile { q } if *q == 0.99));
    assert_eq!(
        dtype(&root.schema, "quantile_0_99"),
        &FieldDataType::Plain(DataType::Float64),
        "the summary-state type must not propagate past the estimate"
    );

    // The quantile: KLL committed, k=269 sized for ε=0.01 at 99% confidence.
    // is an aggregation operator with no `by(...)`: a genuine full
    // reduction, one output row — not to be confused with the inner rate's
    // per-entity grouping below, even though both once collapsed to the
    // same empty `by: []` (issue #163).
    let Some(ASAPOp::SummaryAgg {
        child,
        family,
        input,
        reduction,
        ..
    }) = summary_input.asap()
    else {
        panic!("expected SummaryAgg, got {:?}", summary_input.operator);
    };
    assert_eq!(
        family,
        &FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 269 }),
            GroupingStrategy::default()
        )
    );
    assert_eq!(input, &SummaryUpdate::column(ColumnRef::SampleValue));
    assert_eq!(
        reduction,
        &Reduction::by(vec![]),
        "global quantile — no group keys, full reduction"
    );
    assert_eq!(
        dtype(&summary_input.schema, "value"),
        &FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 269 }),
            GroupingStrategy::default()
        )
    );

    let Some(ASAPOp::FinalizeExactAccumulator { child }) = child.asap() else {
        panic!("rate needs a maintenance evaluation");
    };

    // The rate: exact counter-reset-aware accumulator, per-series (labels
    // and time axis preserved), no estimate wrapper. `rate(...)` has no
    // grouping concept at all — every entity stays its own summary.
    let Some(ASAPOp::SummaryAgg {
        child: leaf,
        family,
        reduction,
        ..
    }) = child.asap()
    else {
        panic!(
            "expected inner SummaryAgg for rate, got {:?}",
            child.operator
        );
    };
    assert_eq!(
        family,
        &FieldDataType::ExactAggregate(ExactKind::Rate, ExactParams::Rate)
    );
    assert_eq!(reduction, &Reduction::PerEntity);
    assert_eq!(
        dtype(&child.schema, "value"),
        &FieldDataType::ExactAggregate(ExactKind::Rate, ExactParams::Rate)
    );
    assert_eq!(
        child.schema.time_index,
        Some(0),
        "per-series keeps the time axis"
    );

    // The leaf: unrewritten pass-through — TimeRange marker over the Scan.
    // The kept leaf is the non-ASAP sub-DAG itself.
    assert!(
        !leaf.contains_asap(),
        "expected kept leaf, got {:?}",
        leaf.operator
    );
    let Some(NonASAPOp::TimeRange {
        range, child: scan, ..
    }) = leaf.non_asap()
    else {
        panic!("expected TimeRange leaf, got {:?}", leaf.operator);
    };
    assert_eq!(range.as_secs(), 300);
    assert!(matches!(scan.non_asap(), Some(NonASAPOp::Scan { .. })));
    assert!(
        leaf.schema
            .fields
            .iter()
            .all(|f| matches!(f.dtype, FieldDataType::Plain(_))),
        "logical edges carry only plain columns"
    );
}

/// An exact workload binds zero sketches: `sum by (job) (m)` at
/// `AccuracyTarget::Exact` still gets its mergeable exact accumulator, and
/// `avg(m)` (non-mergeable) passes through as a whole logical sub-DAG.
#[test]
fn promql_exact_workload_binds_accumulators_not_sketches() {
    let pre_asap = lower_promql("sum by (job) (http_requests_total)", AccuracyTarget::Exact)
        .expect("lowering failed");
    let root = realize(&pre_asap).expect("binding failed");
    let Some(ASAPOp::SummaryAgg {
        family, reduction, ..
    }) = root.asap()
    else {
        panic!("expected SummaryAgg, got {:?}", root.operator);
    };
    assert_eq!(
        family,
        &FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
    );
    assert_eq!(
        reduction,
        &Reduction::by(vec![2]),
        "job is col 2 in [ts, value, job]"
    );
    assert_eq!(
        dtype(&root.schema, "job"),
        &FieldDataType::Plain(DataType::Utf8),
        "group keys pass through verbatim"
    );

    let pre_asap =
        lower_promql("avg(http_requests_total)", AccuracyTarget::Exact).expect("lowering failed");
    let root = realize(&pre_asap).expect("binding failed");
    assert!(
        !root.contains_asap(),
        "avg has no mergeable accumulator — stays logical"
    );
}

#[test]
fn promql_sum_of_count_over_time_is_composed_by_default_search() {
    let original = lower_promql(
        "sum by (service) (count_over_time(metrics[5m]))",
        AccuracyTarget::Exact,
    )
    .expect("lowering failed");
    let original_schema = original.schema.clone();
    let space = search_workload(vec![("query", original)]);
    let root = &space.roots[0].1;
    let group = space.candidates_for_target(root).expect("root memo group");
    let candidate = group
        .candidates
        .iter()
        .find(|candidate| candidate.strategy == "SemanticEquivalentRewriteStrategy")
        .expect("default search should compose the lowered PromQL query");
    let Replacement::SubDAG(rewritten) = &candidate.replacement else {
        panic!("expected logical rewrite")
    };
    assert!(is_logical_rewrite(rewritten), "expected logical rewrite");

    assert_eq!(rewritten.schema, original_schema);
    let Some(NonASAPOp::Project { child, .. }) = rewritten.non_asap() else {
        panic!("sum(count_over_time) needs a Float64 cast Project")
    };
    let Some(NonASAPOp::Aggregate {
        reduction: Reduction::Reduce(by),
        measures,
        child,
        ..
    }) = child.non_asap()
    else {
        panic!("expected one composed aggregate")
    };
    assert_eq!(by.keys(), &[2]);
    assert!(matches!(
        measures.as_slice(),
        [asap_types::ir::operator::AggIntent::Count {
            accuracy: AccuracyTarget::Exact
        }]
    ));
    assert!(matches!(
        child.non_asap(),
        Some(NonASAPOp::TimeRange { range, child, .. })
            if range.as_secs() == 300 && matches!(child.non_asap(), Some(NonASAPOp::Scan { .. }))
    ));
}

#[test]
fn nested_summary_explicitly_finalizes_exact_child_at_ingestion_time() {
    // Real workload selection must expose the state-to-value edge; an outer
    // sketch must not interpret exact accumulator bytes as input samples.
    let pre = lower_promql(
        "quantile(0.9, sum_over_time(m[1m]))",
        AccuracyTarget::Epsilon(0.05),
    )
    .unwrap();
    let space = search_workload(vec![("query", pre)]);
    let selected = global_selection(&space, &DefaultCostModel);
    let plan = selected
        .assemble_selected_dag(&space.roots[0].1)
        .unwrap()
        .unwrap();
    // Stored timings are gone: time the plan with its outer summary
    // maintained and read the timed copy.
    let timed_plan = maintained(&plan);
    let Some(ASAPOp::SummaryEstimate { summary_input, .. }) = timed_plan.asap() else {
        panic!("expected selected quantile summary");
    };
    let Some(ASAPOp::SummaryAgg { child, .. }) = summary_input.asap() else {
        panic!("expected maintained outer summary");
    };
    let Some(ASAPOp::FinalizeExactAccumulator { child: source }) = child.asap() else {
        panic!(
            "missing explicit accumulator finalization: {:?}",
            child.operator
        );
    };
    assert_eq!(
        child.timing,
        Some(asap_types::ir::properties::ExecutionTiming::IngestionTime)
    );
    assert!(matches!(
        source.asap(),
        Some(ASAPOp::SummaryAgg {
            family: FieldDataType::ExactAggregate(ExactKind::Sum, _),
            ..
        })
    ));
    assert!(child
        .schema
        .fields
        .iter()
        .all(|field| matches!(field.dtype, FieldDataType::Plain(_))));
    assert!(child
        .schema
        .fields
        .iter()
        .any(|field| matches!(field.dtype, FieldDataType::Plain(DataType::Float64))));
    // Explicit boundary is a valid post-ASAP DAG.
    post_asap_dag(&plan);
}

#[test]
fn physical_node_owns_phase_independently_of_binary_payload() {
    use asap_types::ir::properties::ExecutionTiming;
    for (query, expected) in [
        (
            // One selector: both operands cover the same series.
            "quantile(0.9, sum_over_time(m[1m]) + sum_over_time(m[1m]))",
            ExecutionTiming::IngestionTime,
        ),
        (
            "sum_over_time(m[1m]) + sum_over_time(n[1m])",
            ExecutionTiming::QueryTime,
        ),
    ] {
        let input = lower_promql(query, AccuracyTarget::Epsilon(0.05)).unwrap();
        let search = search_workload(vec![("q", input)]);
        let choice = global_selection(&search, &DefaultCostModel);
        let plan = choice
            .assemble_selected_dag(&search.roots[0].1)
            .unwrap()
            .unwrap();
        let dag = maintained_post_asap_dag(&plan);
        let node = dag
            .nodes
            .iter()
            .find(|node| {
                matches!(
                    node.payload,
                    PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::BinaryOp { .. })
                )
            })
            .unwrap();
        assert_eq!(node.output_state.timing, expected);
        let wire = serde_json::to_value(&node.payload).unwrap();
        assert!(wire.get("timing").is_none());
        let mut obsolete = wire.clone();
        obsolete["timing"] = serde_json::json!(expected.as_str());
        assert!(serde_json::from_value::<PhysicalASAPOperatorPayload>(obsolete).is_err());
        let restored: PhysicalASAPOperatorPayload = serde_json::from_value(wire).unwrap();
        assert_eq!(restored, node.payload);
    }
}

/// Missing domain evidence permits a candidate but cannot certify its accuracy.
#[test]
fn ddsketch_ratio_without_domain_proof_is_uncertified() {
    let pre = lower_promql(
        "quantile_over_time(0.9, data[5m]) / quantile_over_time(0.5, data[5m])",
        AccuracyTarget::Epsilon(0.01),
    )
    .unwrap();
    let root = realize(&pre).unwrap();
    assert!(matches!(root.non_asap(), Some(NonASAPOp::BinaryOp { .. })));
    assert!(root.guarantee.is_none());
    let space = search_workload_with_targets(
        vec![("unproven", pre, Some(AccuracyTarget::Epsilon(0.01)))],
        &asap_aware_mapping::default_strategies(),
        &DefaultAccuracyModel,
    );
    let root_group = space
        .target_subdag_candidates()
        .find(|group| Rc::ptr_eq(&group.target, &space.roots[0].1))
        .expect("root memo group");
    assert!(
        root_group.candidates.iter().any(|candidate| {
            matches!(
                &candidate.replacement,
                Replacement::SubDAG(node)
                    if matches!(node.non_asap(), Some(NonASAPOp::BinaryOp { .. }))
                        && node.guarantee.is_none()
            )
        }),
        "backend must receive the uncertified ratio candidate for its own selection"
    );

    let selection = global_selection(&space, &DefaultCostModel);
    assert!(
        selection
            .for_target(&space.roots[0].1)
            .expect("selected root group")
            .chosen
            .is_none(),
        "Planner must not automatically select an uncertified ratio"
    );
    let materialized = selection
        .assemble_selected_dag(&space.roots[0].1)
        .unwrap()
        .expect("materialized root");
    assert!(!materialized.contains_asap());
}

struct FixtureQuantileDomain {
    lower: f64,
    upper: f64,
}
impl AccuracyEvidenceProvider for FixtureQuantileDomain {
    fn quantile_input_domain(&self, _: &OperatorNode) -> Option<QuantileInputDomain> {
        Some(QuantileInputDomain {
            lower: self.lower,
            upper: self.upper,
            max_samples: 1000,
            contract: "enforced nonempty finite fixture window".into(),
        })
    }
}

/// Unknown, zero, mixed-sign, nonfinite and zero-mapped domains cannot certify a ratio.
#[test]
fn ddsketch_ratio_rejects_unsafe_domains() {
    for (lower, upper) in [
        (0., 0.),
        (-1., 1.001),
        (0., 100.),
        (f64::NAN, 100.),
        (1., f64::INFINITY),
        (2., 1.),
        (f64::MIN_POSITIVE / 2., f64::MIN_POSITIVE / 2.),
    ] {
        let evidence = FixtureQuantileDomain { lower, upper };
        let pre = lower_promql(
            "quantile_over_time(0.9, data[5m]) / quantile_over_time(0.5, data[5m])",
            AccuracyTarget::Epsilon(0.01),
        )
        .unwrap();
        let strategy = ASAPStrategies::new_with_planning_inputs_and_evidence(
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &evidence,
        );
        let replacements = strategy.replacements(&TargetSubDAG::new(&pre));
        assert!(
            replacements.is_empty(),
            "unsafe domain [{lower}, {upper}] got {replacements:?}"
        );
    }
}

/// A missing proof for one side must not hide an invalid proof for the other.
#[test]
fn ddsketch_ratio_rejects_one_invalid_domain_when_the_other_is_missing() {
    struct PartialUnsafeDomain;
    impl AccuracyEvidenceProvider for PartialUnsafeDomain {
        fn quantile_input_domain(&self, operand: &OperatorNode) -> Option<QuantileInputDomain> {
            let Some(NonASAPOp::Aggregate { measures, .. }) = operand.non_asap() else {
                return None;
            };
            matches!(
                measures.as_slice(),
                [asap_types::ir::operator::agg_intent::AggIntent::Quantile { q, .. }] if *q == 0.9
            )
            .then(|| QuantileInputDomain {
                lower: -1.0,
                upper: 1.0,
                max_samples: 1000,
                contract: "unsafe numerator".into(),
            })
        }
    }

    let pre = lower_promql(
        "quantile_over_time(0.9, data[5m]) / quantile_over_time(0.5, data[5m])",
        AccuracyTarget::Epsilon(0.01),
    )
    .unwrap();
    let strategy = ASAPStrategies::new_with_planning_inputs_and_evidence(
        &DefaultAccuracyModel,
        &EqualSplitAllocator,
        &PartialUnsafeDomain,
    );
    assert!(strategy.replacements(&TargetSubDAG::new(&pre)).is_empty());
}

/// The committed planner alpha is exercised against the pinned sketch implementation.
#[test]
fn ddsketch_ratio_bound_holds_for_signed_pinned_sketch_evaluations() {
    for sign in [-1., 1.] {
        let evidence = FixtureQuantileDomain {
            lower: if sign < 0. { -100. } else { 1. },
            upper: if sign < 0. { -1. } else { 100. },
        };
        let pre = lower_promql(
            "quantile_over_time(0.9, data[5m]) / quantile_over_time(0.5, data[5m])",
            AccuracyTarget::Epsilon(0.01),
        )
        .unwrap();
        let strategy = ASAPStrategies::new_with_planning_inputs_and_evidence(
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &evidence,
        );
        let candidates = strategy.replacements(&TargetSubDAG::new(&pre));
        let Replacement::SubDAG(node) = &candidates[0].replacement else {
            panic!("summary")
        };
        let Some(NonASAPOp::BinaryOp { lhs, rhs, .. }) = node.non_asap() else {
            panic!("ratio")
        };
        let alpha = |node: &OperatorNode| {
            let Some(ASAPOp::SummaryEstimate { summary_input, .. }) = node.asap() else {
                panic!("evaluation")
            };
            let Some(ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(kind, _),
                ..
            }) = summary_input.asap()
            else {
                panic!("sketch")
            };
            let SketchParams::DDSketch { alpha } = kind.params() else {
                panic!("DDSketch")
            };
            *alpha
        };
        assert_eq!(alpha(lhs), alpha(rhs));
        let bound = node.guarantee.as_ref().unwrap().bound.evaluate().unwrap();
        for values in [
            vec![sign; 30],
            (1..=100).map(|i| sign * i as f64).collect(),
            vec![sign, sign, sign, sign * 30., sign * 100.],
        ] {
            let mut sorted = values.clone();
            sorted.sort_by(f64::total_cmp);
            let exact = |q: f64| {
                let r = q * (sorted.len() - 1) as f64;
                sorted[r.floor() as usize] * (1. - r.fract())
                    + sorted[r.ceil() as usize] * r.fract()
            };
            let mut sketch = asap_sketchlib::DdSketch::new(alpha(lhs));
            for v in values {
                sketch.try_update(v).unwrap();
            }
            let want = exact(0.9) / exact(0.5);
            let got = sketch.quantile_interpolated(0.9).unwrap()
                / sketch.quantile_interpolated(0.5).unwrap();
            assert!((got - want).abs() / want.abs() <= bound + 1e-12);
        }
    }
}

/// Empty or overlarge population contracts cannot promise a supported evaluation.
#[test]
fn ddsketch_ratio_requires_a_supported_population_size() {
    struct PopulationEvidence(u64);
    impl AccuracyEvidenceProvider for PopulationEvidence {
        fn quantile_input_domain(&self, _: &OperatorNode) -> Option<QuantileInputDomain> {
            Some(QuantileInputDomain {
                lower: 1.,
                upper: 10.,
                max_samples: self.0,
                contract: "enforced fixture count and range".into(),
            })
        }
    }
    let pre = lower_promql(
        "quantile_over_time(0.9, data[5m]) / quantile_over_time(0.5, data[5m])",
        AccuracyTarget::Epsilon(0.01),
    )
    .unwrap();
    for count in [0, (1u64 << 53) + 1] {
        let evidence = PopulationEvidence(count);
        let strategy = ASAPStrategies::new_with_planning_inputs_and_evidence(
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &evidence,
        );
        assert!(strategy.replacements(&TargetSubDAG::new(&pre)).is_empty());
    }
}

// Every `without` aggregation candidate exports a valid DAG: its summary state
// column carries the family instead of the evaluation's Float64 value.
#[test]
fn without_aggregation_candidates_export_valid_dags() {
    for accuracy in [
        AccuracyTarget::Exact,
        AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.01,
        },
    ] {
        for query in ["sum without (pod) (m)", "quantile without (pod) (0.5, m)"] {
            let root = lower_promql(query, accuracy.clone()).unwrap();
            let space = search_workload_with_targets(
                vec![(0, root, Some(accuracy.clone()))],
                &asap_aware_mapping::default_strategies(),
                &DefaultAccuracyModel,
            );
            let inventory = space.enumerate_candidate_dags_for_root(&0, 65_536).unwrap();
            assert!(!inventory.candidates.is_empty(), "{query}");
            for (_, node) in inventory.candidates.iter().flatten() {
                post_asap_dag(node);
            }
        }
    }
}
