//! End-to-end query-string → post-ASAP IR pin (issue #98).
//!
//! Drives the full pipeline — PromQL text → pre-ASAP `QueryExpr`
//! (`lower_promql`) → post-ASAP `SummaryExpr` DAG (via
//! `SketchAlgorithmStrategy::replacements`, see [`realize`] below) — and pins
//! the summary-bound shape node by node, including the family `(Kind,
//! Params)` committed on each edge's schema.

use std::rc::Rc;

use asap_aware_mapping::accuracy::{
    AccuracyEvidenceProvider, DefaultAccuracyModel, EqualSplitAllocator, PropagationStats,
    QuantileInputDomain,
};
use asap_aware_mapping::cost_model::DefaultCostModel;
use asap_aware_mapping::replacement::{keep_pre_asap, RealizationError};
use asap_aware_mapping::{
    search_workload, search_workload_with_targets, AccuracyModel, Replacement, ReplacementStrategy,
    ReplacementSubDAG, SketchAlgorithmStrategy, TargetSubDAG,
};
use asap_integration_tests::fixtures::lower_promql;
use asap_types::post_asap::{
    compile_executable_dag, CompositionOperator, EntityIdentity, ExactKind, ExactParams,
    GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams, SketchQuery, SummaryExpr,
    SummaryFamilyType, SummaryInputExpr, SummaryNode, SummarySchema, SummaryUpdate, ValueOperation,
};
use asap_types::pre_asap::expr_ir::ColumnRef;
use asap_types::pre_asap::query_expr::{QueryExpr, Reduction};
use asap_types::pre_asap::schema::DataType;
use asap_types::types::AccuracyTarget;

/// This crate has no "bind me one tree" public API any more —
/// `SketchAlgorithmStrategy::replacements` always returns every candidate, and
/// a caller decides what to keep. This test-only helper reproduces the
/// take-the-first-(`cost_model`-preferred)-candidate pattern so the
/// single-answer pins below don't all repeat it by hand.
fn realize(expr: &QueryExpr) -> Result<Rc<SummaryNode>, RealizationError> {
    let root = Rc::new(expr.clone());
    let target = TargetSubDAG::new(&root);
    match SketchAlgorithmStrategy::default_cost_model()
        .replacements(&target)
        .into_iter()
        .next()
    {
        Some(ReplacementSubDAG {
            replacement: Replacement::Summary(node),
            ..
        }) => Ok(node),
        _ => keep_pre_asap(&root),
    }
}

#[test]
fn distinct_over_time_offers_hll_cardinality_readout() {
    // The real frontend must reach an existing HLL candidate without a
    // function-specific post-ASAP node or a sample-count rewrite.
    let root = Rc::new(
        lower_promql(
            "distinct_over_time(cpu_usage{job=\"worker\"}[5m])",
            AccuracyTarget::Epsilon(0.02),
        )
        .unwrap(),
    );
    let candidates =
        SketchAlgorithmStrategy::default_cost_model().replacements(&TargetSubDAG::new(&root));
    assert!(candidates.iter().any(|candidate| {
        let Replacement::Summary(node) = &candidate.replacement else { return false };
        let SummaryExpr::SummaryEstimate { summary_input, query, .. } = &node.expr else { return false };
        matches!(query, SketchQuery::Cardinality)
            && matches!(&summary_input.expr, SummaryExpr::SummaryAgg { family: SummaryFamilyType::Sketch(kind, _), .. }
                if kind.algorithm() == &SketchAlgorithm::Hll)
    }), "no HLL cardinality candidate: {candidates:?}");
}

fn lower_search_and_materialize(query: &str) -> Rc<SummaryNode> {
    let pre = Rc::new(lower_promql(query, AccuracyTarget::Exact).expect("lowering failed"));
    let space = search_workload(vec![("query", pre)]);
    let selection = space.global_selection(&DefaultCostModel);
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
        let root = lower_search_and_materialize(query);
        let SummaryExpr::ValueOperation {
            operation: ValueOperation::Limit { n, offset, .. },
            child: sort,
            ..
        } = &root.expr
        else {
            panic!("expected query-time Limit for {query}, got {:?}", root.expr);
        };
        assert!(*n > 0 && *offset == 0);
        let SummaryExpr::ValueOperation {
            operation: ValueOperation::Sort { .. },
            child,
            ..
        } = &sort.expr
        else {
            panic!("expected query-time Sort under Limit for {query}");
        };
        let SummaryExpr::ValueOperation {
            operation: ValueOperation::FinalizeExactAccumulator,
            child: state,
            ..
        } = &child.expr
        else {
            panic!(
                "Sort must consume finalized values for {query}: {:?}",
                child.expr
            );
        };
        assert!(matches!(state.expr, SummaryExpr::SummaryAgg { .. }));
        assert!(child
            .schema
            .fields
            .iter()
            .all(|field| matches!(field.dtype, SummaryFamilyType::Plain(_))));
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
            matches!(root.expr, SummaryExpr::KeepPreAsap(_)),
            "exact target must not accept an uncertified membership sidecar for {query}: {:?}",
            root.expr
        );
    }
}

#[test]
fn instant_topk_and_unsupported_child_remain_local_residuals() {
    for query in ["topk(3, memory_bytes)", "topk(3, deriv(memory_bytes[5m]))"] {
        let root = lower_search_and_materialize(query);
        let SummaryExpr::ValueOperation {
            operation: ValueOperation::Limit { .. },
            child: sort,
            ..
        } = &root.expr
        else {
            panic!("expected Limit for {query}");
        };
        let SummaryExpr::ValueOperation {
            child, operation, ..
        } = &sort.expr
        else {
            panic!("expected Sort for {query}");
        };
        assert!(matches!(operation, ValueOperation::Sort { .. }));
        assert!(
            matches!(child.expr, SummaryExpr::KeepPreAsap(_)),
            "only the unsupported child should remain exact for {query}"
        );
    }
}

fn dtype<'a>(schema: &'a SummarySchema, name: &str) -> &'a SummaryFamilyType {
    &schema
        .fields
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no field {name:?} in {schema:?}"))
        .dtype
}

fn lower_and_realize(query: &str) -> Rc<SummaryNode> {
    let pre = lower_promql(query, AccuracyTarget::Exact).expect("lowering failed");
    realize(&pre).expect("binding failed")
}

#[test]
fn promql_binary_arithmetic_retains_two_summary_leaves() {
    for op in ["+", "-", "*", "/", "%", "^", "atan2"] {
        let root = lower_and_realize(&format!("rate(a[1m]) {op} rate(b[1m])"));
        let SummaryExpr::BinaryOp { lhs, rhs, .. } = &root.expr else {
            panic!("expected BinaryOp for {op}, got {:?}", root.expr);
        };
        for operand in [lhs, rhs] {
            let SummaryExpr::ValueOperation {
                child,
                operation: ValueOperation::FinalizeExactAccumulator,
                ..
            } = &operand.expr
            else {
                panic!("expected an explicit exact readout, got {:?}", operand.expr);
            };
            assert!(matches!(child.expr, SummaryExpr::SummaryAgg { .. }));
        }
    }
}

#[test]
fn value_ranked_topk_over_binary_ratio_finalizes_both_summary_operands() {
    let query = "topk(1, sum by(job)(increase(a[6h])) / sum by(job)(increase(b[6h])))";
    let root = lower_search_and_materialize(query);
    let SummaryExpr::ValueOperation {
        operation: ValueOperation::Limit {
            n: 1, offset: 0, ..
        },
        child: sort,
        ..
    } = &root.expr
    else {
        panic!("expected Limit root, got {:?}", root.expr);
    };
    let SummaryExpr::ValueOperation {
        operation: ValueOperation::Sort { .. },
        child: binary,
        ..
    } = &sort.expr
    else {
        panic!("expected Sort below Limit, got {:?}", sort.expr);
    };
    let SummaryExpr::BinaryOp { lhs, rhs, .. } = &binary.expr else {
        panic!("expected BinaryOp below Sort, got {:?}", binary.expr);
    };
    for operand in [lhs, rhs] {
        let SummaryExpr::ValueOperation {
            operation: ValueOperation::FinalizeExactAccumulator,
            child,
            ..
        } = &operand.expr
        else {
            panic!(
                "expected exact accumulator finalization, got {:?}",
                operand.expr
            );
        };
        assert!(matches!(child.expr, SummaryExpr::SummaryAgg { .. }));
    }
}

struct SeparatedTopK;

impl AccuracyEvidenceProvider for SeparatedTopK {
    fn topk_max_distinct_items(&self, _: &QueryExpr) -> Option<u64> {
        Some(1000)
    }

    fn propagation_stats(
        &self,
        op: &CompositionOperator,
        _family: &SummaryFamilyType,
        _query: Option<&SketchQuery>,
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
    let root = Rc::new(
        lower_promql(
            "topk by(job)(2, sum by(service, job)(rate(m[1m])))",
            AccuracyTarget::Epsilon(0.01),
        )
        .unwrap(),
    );
    let strategy = SketchAlgorithmStrategy::new_with_planning_inputs_and_evidence(
        &DefaultCostModel,
        &DefaultAccuracyModel,
        &EqualSplitAllocator,
        &SeparatedTopK,
    );
    let plan = strategy
        .replacements(&TargetSubDAG::new(&root))
        .into_iter()
        .find_map(|candidate| match candidate.replacement {
            Replacement::Summary(node) if candidate.rationale.contains("CmsWithHeap") => Some(node),
            _ => None,
        })
        .expect("rate-weighted CMS plan");
    let dag = compile_executable_dag(&plan).unwrap();
    assert!(!dag.nodes.iter().any(|node| matches!(
        node.payload,
        asap_types::post_asap::ExecutableOperatorPayload::RelationalJoin { .. }
    )));
    let node = dag.nodes.iter().find(|node| matches!(&node.payload,
        asap_types::post_asap::ExecutableOperatorPayload::SummaryAgg { family: SummaryFamilyType::Sketch(kind, _), .. }
        if kind.algorithm() == &SketchAlgorithm::CmsWithHeap)).unwrap();
    assert_eq!(
        node.output_state.timing,
        asap_types::post_asap::ExecutionTiming::QueryTime
    );
    let asap_types::post_asap::ExecutableOperatorPayload::SummaryAgg { input, .. } = &node.payload
    else {
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
fn weighted_topk_requires_a_complete_readout_population_bound() {
    struct NoPopulationBound;
    impl AccuracyEvidenceProvider for NoPopulationBound {
        fn propagation_stats(
            &self,
            op: &CompositionOperator,
            family: &SummaryFamilyType,
            query: Option<&SketchQuery>,
        ) -> PropagationStats {
            SeparatedTopK.propagation_stats(op, family, query)
        }
    }
    let root = Rc::new(
        lower_promql(
            "topk by(job)(2, sum by(service, job)(rate(m[1m])))",
            AccuracyTarget::Epsilon(0.01),
        )
        .unwrap(),
    );
    let strategy = SketchAlgorithmStrategy::new_with_planning_inputs_and_evidence(
        &DefaultCostModel,
        &DefaultAccuracyModel,
        &EqualSplitAllocator,
        &NoPopulationBound,
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
        let root = Rc::new(
            lower_promql(
                query,
                AccuracyTarget::EpsilonDelta {
                    epsilon: 0.01,
                    delta: 0.01,
                },
            )
            .unwrap(),
        );
        let strategy = SketchAlgorithmStrategy::new_with_planning_inputs_and_evidence(
            &DefaultCostModel,
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &SeparatedTopK,
        );
        let plan = strategy
            .replacements(&TargetSubDAG::new(&root))
            .into_iter()
            .find_map(|candidate| match candidate.replacement {
                Replacement::Summary(node) if candidate.rationale.contains("CmsWithHeap") => {
                    Some(node)
                }
                _ => None,
            })
            .expect("weighted summary");
        let SummaryExpr::ValueOperation {
            child: sorted,
            operation:
                ValueOperation::Limit {
                    n: 2,
                    offset: 0,
                    partition_by,
                },
            ..
        } = &plan.expr
        else {
            panic!("grouped limit")
        };
        let SummaryExpr::ValueOperation {
            child: projected,
            operation:
                ValueOperation::Sort {
                    partition_by: sort_groups,
                    ..
                },
            ..
        } = &sorted.expr
        else {
            panic!("grouped sort")
        };
        assert_eq!(partition_by, sort_groups);
        assert_eq!(partition_by.len(), usize::from(query.contains("topk by")));
        let SummaryExpr::ValueOperation {
            child: readout,
            operation: ValueOperation::Project { .. },
            ..
        } = &projected.expr
        else {
            panic!("logical output projection")
        };
        let SummaryExpr::SummaryEstimate {
            summary_input,
            query: SketchQuery::TopK { k },
        } = &readout.expr
        else {
            panic!("heap readout")
        };
        assert!(*k > 2, "candidate capacity is independent of output count");
        let SummaryExpr::SummaryAgg {
            child: rates,
            input,
            ..
        } = &summary_input.expr
        else {
            panic!("weighted summary")
        };
        assert_eq!(
            input.weight,
            SummaryInputExpr::Column(ColumnRef::SampleValue)
        );
        assert!(matches!(
            rates.expr,
            SummaryExpr::ValueOperation {
                operation: ValueOperation::FinalizeExactAccumulator,
                ..
            }
        ));
        let dag = compile_executable_dag(&plan).unwrap();
        for phase in [
            asap_types::post_asap::ExecutionTiming::IngestionTime,
            asap_types::post_asap::ExecutionTiming::QueryTime,
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
            asap_types::post_asap::GuaranteeSource::ChildGuarantee { guarantee, .. }
            if guarantee.metric == asap_types::post_asap::ErrorMetric::Frequency)));
    }
}

#[test]
fn promql_binary_arithmetic_preserves_both_scalar_operand_orders() {
    fn is_exact_readout_or_scalar(node: &SummaryNode) -> bool {
        matches!(node.expr, SummaryExpr::KeepPreAsap(_))
            || matches!(
                node.expr,
                SummaryExpr::ValueOperation {
                    operation: ValueOperation::FinalizeExactAccumulator,
                    ..
                }
            )
    }
    for query in ["rate(a[1m]) / 2", "2 / rate(a[1m])"] {
        let root = lower_and_realize(query);
        let SummaryExpr::BinaryOp { lhs, rhs, .. } = &root.expr else {
            panic!("expected BinaryOp for {query}, got {:?}", root.expr);
        };
        assert!(is_exact_readout_or_scalar(lhs));
        assert!(is_exact_readout_or_scalar(rhs));
        assert!(
            matches!(
                lhs.expr,
                SummaryExpr::ValueOperation {
                    operation: ValueOperation::FinalizeExactAccumulator,
                    ..
                }
            ) || matches!(
                rhs.expr,
                SummaryExpr::ValueOperation {
                    operation: ValueOperation::FinalizeExactAccumulator,
                    ..
                }
            )
        );
    }
}

#[test]
fn promql_binary_arithmetic_falls_back_as_a_whole_for_unsupported_arm() {
    let root = lower_and_realize("rate(a[1m]) + stddev_over_time(b[1m])");
    assert!(matches!(root.expr, SummaryExpr::KeepPreAsap(_)));
}

#[test]
fn promql_binary_arithmetic_preserves_nested_structure_and_rejects_modifiers() {
    let nested = lower_and_realize("(rate(a[1m]) + rate(b[1m])) / 2");
    let SummaryExpr::BinaryOp { lhs, .. } = &nested.expr else {
        panic!("expected outer BinaryOp, got {:?}", nested.expr);
    };
    assert!(matches!(lhs.expr, SummaryExpr::BinaryOp { .. }));

    let modified = lower_and_realize("rate(a[1m]) + on(job) rate(b[1m])");
    assert!(matches!(modified.expr, SummaryExpr::KeepPreAsap(_)));
}

#[test]
fn promql_binary_arithmetic_never_relabels_approximate_children_as_exact() {
    let pre = lower_promql(
        "quantile_over_time(0.9, a[1m]) + quantile_over_time(0.9, b[1m])",
        AccuracyTarget::Epsilon(0.01),
    )
    .expect("lowering failed");
    let root = realize(&pre).expect("binding failed");
    let SummaryExpr::BinaryOp { lhs, rhs, .. } = &root.expr else {
        panic!("expected BinaryOp, got {:?}", root.expr);
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
    let query = Rc::new(
        lower_promql(
            "quantile_over_time(0.9, data[5m]) / quantile_over_time(0.5, data[5m])",
            target.clone(),
        )
        .expect("lowering failed"),
    );

    let evidence = FixtureQuantileDomain {
        lower: 1.0,
        upper: 100.0,
    };
    let space = search_workload_with_targets(
        vec![("ratio", query, Some(target.clone()))],
        &asap_aware_mapping::replacement::default_strategies_with_evidence(
            &DefaultCostModel,
            &evidence,
        ),
        &DefaultAccuracyModel,
    );
    let root = &space.roots[0].1;
    let selected = space.global_selection(&DefaultCostModel);
    let chosen = selected
        .for_target(root)
        .and_then(|selection| selection.chosen.as_ref())
        .expect("the certified DDSketch ratio should be selectable");
    let Replacement::Summary(node) = &chosen.replacement else {
        panic!("expected a summary candidate")
    };
    let guarantee = node.guarantee.as_ref().expect("ratio guarantee");
    assert_eq!(guarantee.failure_probability.evaluate(), Some(0.0));
    assert!(
        DefaultAccuracyModel.satisfies(guarantee, &target),
        "ratio guarantee should satisfy the requested target: {guarantee:?}"
    );

    let shared =
        asap_types::post_asap::share_common_summary_subtrees(vec![("ratio", node.clone())]);
    let SummaryExpr::BinaryOp { lhs, rhs, .. } = &shared[0].1.expr else {
        panic!("expected binary ratio")
    };
    let producer = |readout: &Rc<SummaryNode>| match &readout.expr {
        SummaryExpr::SummaryEstimate { summary_input, .. } => Rc::clone(summary_input),
        other => panic!("expected DDSketch readout, got {other:?}"),
    };
    assert!(
        Rc::ptr_eq(&producer(lhs), &producer(rhs)),
        "the two quantile readouts should share one DDSketch producer"
    );
}

#[test]
fn planner_only_e2e_temporal_topk_preserves_query_update_and_readout_contract() {
    // Self-contained Planner E2E: each case starts from PromQL text and ends
    // at the executable post-ASAP summary DAG. No controller/backend types,
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
        let pre = Rc::new(
            lower_promql(
                source,
                AccuracyTarget::EpsilonDelta {
                    epsilon: 0.01,
                    delta: 0.01,
                },
            )
            .expect("lower temporal Top-K"),
        );
        let strategy = SketchAlgorithmStrategy::new_with_planning_inputs_and_evidence(
            &DefaultCostModel,
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &SeparatedTopK,
        );
        let candidate = strategy
            .replacements(&TargetSubDAG::new(&pre))
            .into_iter()
            .find_map(|candidate| match candidate.replacement {
                Replacement::Summary(node) if candidate.rationale.contains(expected_family) => {
                    Some(node)
                }
                _ => None,
            })
            .expect("heap-backed temporal Top-K candidate");
        let SummaryExpr::SummaryEstimate {
            summary_input,
            query: SketchQuery::TopK { k, .. },
        } = &candidate.expr
        else {
            panic!("expected Top-K estimate, got {:?}", candidate.expr)
        };
        assert_eq!(
            *k, 5,
            "the requested Top-K cardinality must survive binding"
        );
        let SummaryExpr::SummaryAgg {
            input: state_input,
            family,
            child,
            ..
        } = &summary_input.expr
        else {
            panic!("expected structured Top-K state input")
        };
        let SummaryFamilyType::Sketch(kind, _) = family else {
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
        assert!(matches!(child.expr, SummaryExpr::KeepPreAsap(_)));
    }
}

/// Execute the ungrouped temporal TopK subset with exact state. This tests
/// the emitted update contract, not sketch approximation or backend execution.
fn execute_topk_reference(plan: &SummaryNode) -> Vec<(String, f64)> {
    use std::collections::BTreeMap;
    let SummaryExpr::SummaryEstimate {
        summary_input,
        query: SketchQuery::TopK { k },
    } = &plan.expr
    else {
        panic!("expected TopK readout")
    };
    let SummaryExpr::SummaryAgg {
        input,
        child,
        reduction,
        ..
    } = &summary_input.expr
    else {
        panic!("expected summary updates")
    };
    assert_eq!(reduction, &Reduction::by(vec![]));
    let SummaryExpr::KeepPreAsap(raw) = &child.expr else {
        panic!("expected fused raw input")
    };
    let QueryExpr::TimeRange { range, child } = raw.as_ref() else {
        panic!("expected temporal input")
    };
    let QueryExpr::Scan {
        source: asap_types::pre_asap::Source::TimeSeries { metric },
        predicates,
        ..
    } = child.as_ref()
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
fn planner_topk_reference_execution_matches_ground_truth() {
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
        let pre = Rc::new(
            lower_promql(
                query,
                AccuracyTarget::EpsilonDelta {
                    epsilon: 0.01,
                    delta: 0.01,
                },
            )
            .unwrap(),
        );
        let strategy = SketchAlgorithmStrategy::new_with_planning_inputs_and_evidence(
            &DefaultCostModel,
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &SeparatedTopK,
        );
        let candidates = strategy.replacements(&TargetSubDAG::new(&pre));
        assert!(!candidates.is_empty(), "no plan for {query}");
        for candidate in candidates {
            let Replacement::Summary(plan) = candidate.replacement else {
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
/// └─ SummaryAgg { Kll{k:269}, input: SampleValue }   → {quantile_0_99: Sketch(Kll, {k:269})}
///    └─ SummaryAgg { Rate, input: SampleValue }      → {ts, value: ExactAggregate(Rate), …}
///       └─ KeepPreAsap(TimeRange{5m} → Scan)         → {ts, value}
/// ```
///
/// The nested tree exercises both realizations: the approximate quantile
/// binds a KLL sketch + readout; the per-series `rate` binds the exact
/// counter-reset-aware accumulator (no estimate — its state is the value).
#[test]
fn promql_quantile_of_rate_binds_kll_over_rate_accumulator() {
    let pre_asap = lower_promql(
        "quantile(0.99, rate(http_requests_total[5m]))",
        AccuracyTarget::Epsilon(0.01),
    )
    .expect("lowering failed");
    let root = realize(&pre_asap).expect("binding failed");

    // Root: the sketch readout, back to a plain row shape.
    let SummaryExpr::SummaryEstimate {
        summary_input,
        query,
    } = &root.expr
    else {
        panic!("expected SummaryEstimate root, got {:?}", root.expr);
    };
    assert!(matches!(query, SketchQuery::Quantile { q } if *q == 0.99));
    assert_eq!(
        dtype(&root.schema, "quantile_0_99"),
        &SummaryFamilyType::Plain(DataType::Float64),
        "the summary-state type must not propagate past the estimate"
    );

    // The quantile: KLL committed, k=269 sized for ε=0.01 at 99% confidence.
    // is an aggregation operator with no `by(...)`: a genuine full
    // reduction, one output row — not to be confused with the inner rate's
    // per-entity grouping below, even though both once collapsed to the
    // same empty `by: []` (issue #163).
    let SummaryExpr::SummaryAgg {
        child,
        family,
        input,
        reduction,
        ..
    } = &summary_input.expr
    else {
        panic!("expected SummaryAgg, got {:?}", summary_input.expr);
    };
    assert_eq!(
        family,
        &SummaryFamilyType::Sketch(
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
        dtype(&summary_input.schema, "quantile_0_99"),
        &SummaryFamilyType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 269 }),
            GroupingStrategy::default()
        )
    );

    let SummaryExpr::ValueOperation {
        child,
        operation: ValueOperation::FinalizeExactAccumulator,
        timing: asap_types::post_asap::ExecutionTiming::IngestionTime,
    } = &child.expr
    else {
        panic!("rate needs a maintenance readout");
    };

    // The rate: exact counter-reset-aware accumulator, per-series (labels
    // and time axis preserved), no estimate wrapper. `rate(...)` has no
    // grouping concept at all — every entity stays its own summary.
    let SummaryExpr::SummaryAgg {
        child: leaf,
        family,
        reduction,
        ..
    } = &child.expr
    else {
        panic!("expected inner SummaryAgg for rate, got {:?}", child.expr);
    };
    assert_eq!(
        family,
        &SummaryFamilyType::ExactAggregate(ExactKind::Rate, ExactParams::Rate)
    );
    assert_eq!(reduction, &Reduction::PerEntity);
    assert_eq!(
        dtype(&child.schema, "value"),
        &SummaryFamilyType::ExactAggregate(ExactKind::Rate, ExactParams::Rate)
    );
    assert_eq!(
        child.schema.time_index,
        Some(0),
        "per-series keeps the time axis"
    );

    // The leaf: unrewritten pass-through — TimeRange marker over the Scan.
    let SummaryExpr::KeepPreAsap(kept_leaf) = &leaf.expr else {
        panic!("expected KeepPreAsap leaf, got {:?}", leaf.expr);
    };
    let QueryExpr::TimeRange { range, child: scan } = kept_leaf.as_ref() else {
        panic!("expected TimeRange leaf, got {kept_leaf:?}");
    };
    assert_eq!(range.as_secs(), 300);
    assert!(matches!(scan.as_ref(), QueryExpr::Scan { .. }));
    assert!(
        leaf.schema
            .fields
            .iter()
            .all(|f| matches!(f.dtype, SummaryFamilyType::Plain(_))),
        "logical edges carry only plain columns"
    );
}

/// An exact workload binds zero sketches: `sum by (job) (m)` at
/// `AccuracyTarget::Exact` still gets its mergeable exact accumulator, and
/// `avg(m)` (non-mergeable) passes through as a whole logical subtree.
#[test]
fn promql_exact_workload_binds_accumulators_not_sketches() {
    let pre_asap = lower_promql("sum by (job) (http_requests_total)", AccuracyTarget::Exact)
        .expect("lowering failed");
    let root = realize(&pre_asap).expect("binding failed");
    let SummaryExpr::SummaryAgg {
        family, reduction, ..
    } = &root.expr
    else {
        panic!("expected SummaryAgg, got {:?}", root.expr);
    };
    assert_eq!(
        family,
        &SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
    );
    assert_eq!(
        reduction,
        &Reduction::by(vec![2]),
        "job is col 2 in [ts, value, job]"
    );
    assert_eq!(
        dtype(&root.schema, "job"),
        &SummaryFamilyType::Plain(DataType::Utf8),
        "group keys pass through verbatim"
    );

    let pre_asap =
        lower_promql("avg(http_requests_total)", AccuracyTarget::Exact).expect("lowering failed");
    let root = realize(&pre_asap).expect("binding failed");
    assert!(
        matches!(root.expr, SummaryExpr::KeepPreAsap(_)),
        "avg has no mergeable accumulator — stays logical"
    );
}

#[test]
fn promql_sum_of_count_over_time_is_composed_by_default_search() {
    let original = Rc::new(
        lower_promql(
            "sum by (service) (count_over_time(metrics[5m]))",
            AccuracyTarget::Exact,
        )
        .expect("lowering failed"),
    );
    let original_schema = original.output_schema().unwrap();
    let space = search_workload(vec![("query", original)]);
    let root = &space.roots[0].1;
    let group = space.candidates_for_target(root).expect("root memo group");
    let candidate = group
        .candidates
        .iter()
        .find(|candidate| candidate.strategy == "SemanticEquivalentRewriteStrategy")
        .expect("default search should compose the lowered PromQL query");
    let Replacement::Rewrite(rewritten) = &candidate.replacement else {
        panic!("expected logical rewrite")
    };

    assert_eq!(rewritten.output_schema().unwrap(), original_schema);
    let QueryExpr::Project { child, .. } = rewritten.as_ref() else {
        panic!("sum(count_over_time) needs a Float64 cast Project")
    };
    let QueryExpr::Aggregate {
        reduction: Reduction::Reduce(by),
        measures,
        child,
        ..
    } = child.as_ref()
    else {
        panic!("expected one composed aggregate")
    };
    assert_eq!(by.keys(), &[2]);
    assert!(matches!(
        measures.as_slice(),
        [asap_types::pre_asap::AggIntent::Count {
            accuracy: AccuracyTarget::Exact
        }]
    ));
    assert!(matches!(
        child.as_ref(),
        QueryExpr::TimeRange { range, child }
            if range.as_secs() == 300 && matches!(child.as_ref(), QueryExpr::Scan { .. })
    ));
}

#[test]
fn nested_summary_explicitly_finalizes_exact_child_at_ingestion_time() {
    // Real workload selection must expose the state-to-value edge; an outer
    // sketch must not interpret exact accumulator bytes as input samples.
    let pre = Rc::new(
        lower_promql(
            "quantile(0.9, sum_over_time(m[1m]))",
            AccuracyTarget::Epsilon(0.05),
        )
        .unwrap(),
    );
    let space = search_workload(vec![("query", pre)]);
    let selected = space.global_selection(&DefaultCostModel);
    let plan = selected
        .assemble_selected_dag(&space.roots[0].1)
        .unwrap()
        .unwrap();
    let SummaryExpr::SummaryEstimate { summary_input, .. } = &plan.expr else {
        panic!("expected selected quantile summary");
    };
    let SummaryExpr::SummaryAgg { child, .. } = &summary_input.expr else {
        panic!("expected maintained outer summary");
    };
    let SummaryExpr::ValueOperation {
        child: source,
        operation,
        timing,
    } = &child.expr
    else {
        panic!(
            "missing explicit accumulator finalization: {:?}",
            child.expr
        );
    };
    assert!(matches!(
        operation,
        ValueOperation::FinalizeExactAccumulator
    ));
    assert_eq!(
        *timing,
        asap_types::post_asap::ExecutionTiming::IngestionTime
    );
    assert!(matches!(
        source.expr,
        SummaryExpr::SummaryAgg {
            family: SummaryFamilyType::ExactAggregate(ExactKind::Sum, _),
            ..
        }
    ));
    assert!(child
        .schema
        .fields
        .iter()
        .all(|field| matches!(field.dtype, SummaryFamilyType::Plain(_))));
    assert!(child
        .schema
        .fields
        .iter()
        .any(|field| matches!(field.dtype, SummaryFamilyType::Plain(DataType::Float64))));
    compile_executable_dag(&plan).expect("explicit boundary is a valid executable DAG");
}

#[test]
fn physical_node_owns_phase_independently_of_binary_payload() {
    use asap_types::post_asap::{ExecutableOperatorPayload, ExecutionTiming};
    for (query, expected) in [
        (
            "quantile(0.9, sum_over_time(m[1m]) + sum_over_time(n[1m]))",
            ExecutionTiming::IngestionTime,
        ),
        (
            "sum_over_time(m[1m]) + sum_over_time(n[1m])",
            ExecutionTiming::QueryTime,
        ),
    ] {
        let input = lower_promql(query, AccuracyTarget::Epsilon(0.05)).unwrap();
        let search = search_workload(vec![("q", Rc::new(input))]);
        let choice = search.global_selection(&DefaultCostModel);
        let plan = choice
            .assemble_selected_dag(&search.roots[0].1)
            .unwrap()
            .unwrap();
        let dag = compile_executable_dag(&plan).unwrap();
        let node = dag
            .nodes
            .iter()
            .find(|node| matches!(node.payload, ExecutableOperatorPayload::Binary { .. }))
            .unwrap();
        assert_eq!(node.output_state.timing, expected);
        let wire = serde_json::to_value(&node.payload).unwrap();
        assert!(wire.get("timing").is_none());
        let mut obsolete = wire.clone();
        obsolete["timing"] = serde_json::json!(expected.as_str());
        assert!(serde_json::from_value::<ExecutableOperatorPayload>(obsolete).is_err());
        let restored: ExecutableOperatorPayload = serde_json::from_value(wire).unwrap();
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
    assert!(matches!(root.expr, SummaryExpr::BinaryOp { .. }));
    assert!(root.guarantee.is_none());
    let space = search_workload_with_targets(
        vec![(
            "unproven",
            Rc::new(pre),
            Some(AccuracyTarget::Epsilon(0.01)),
        )],
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
                Replacement::Summary(node)
                    if matches!(node.expr, SummaryExpr::BinaryOp { .. })
                        && node.guarantee.is_none()
            )
        }),
        "backend must receive the uncertified ratio candidate for its own selection"
    );

    let selection = space.global_selection(&DefaultCostModel);
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
    assert!(matches!(materialized.expr, SummaryExpr::KeepPreAsap(_)));
}

struct FixtureQuantileDomain {
    lower: f64,
    upper: f64,
}
impl AccuracyEvidenceProvider for FixtureQuantileDomain {
    fn quantile_input_domain(&self, _: &QueryExpr) -> Option<QuantileInputDomain> {
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
        let pre = Rc::new(
            lower_promql(
                "quantile_over_time(0.9, data[5m]) / quantile_over_time(0.5, data[5m])",
                AccuracyTarget::Epsilon(0.01),
            )
            .unwrap(),
        );
        let strategy = SketchAlgorithmStrategy::new_with_planning_inputs_and_evidence(
            &DefaultCostModel,
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
        fn quantile_input_domain(&self, operand: &QueryExpr) -> Option<QuantileInputDomain> {
            let QueryExpr::Aggregate { measures, .. } = operand else {
                return None;
            };
            matches!(
                measures.as_slice(),
                [asap_types::pre_asap::agg_intent::AggIntent::Quantile { q, .. }] if *q == 0.9
            )
            .then(|| QuantileInputDomain {
                lower: -1.0,
                upper: 1.0,
                max_samples: 1000,
                contract: "unsafe numerator".into(),
            })
        }
    }

    let pre = Rc::new(
        lower_promql(
            "quantile_over_time(0.9, data[5m]) / quantile_over_time(0.5, data[5m])",
            AccuracyTarget::Epsilon(0.01),
        )
        .unwrap(),
    );
    let strategy = SketchAlgorithmStrategy::new_with_planning_inputs_and_evidence(
        &DefaultCostModel,
        &DefaultAccuracyModel,
        &EqualSplitAllocator,
        &PartialUnsafeDomain,
    );
    assert!(strategy.replacements(&TargetSubDAG::new(&pre)).is_empty());
}

/// The committed planner alpha is exercised against the pinned sketch implementation.
#[test]
fn ddsketch_ratio_bound_holds_for_signed_pinned_sketch_readouts() {
    for sign in [-1., 1.] {
        let evidence = FixtureQuantileDomain {
            lower: if sign < 0. { -100. } else { 1. },
            upper: if sign < 0. { -1. } else { 100. },
        };
        let pre = Rc::new(
            lower_promql(
                "quantile_over_time(0.9, data[5m]) / quantile_over_time(0.5, data[5m])",
                AccuracyTarget::Epsilon(0.01),
            )
            .unwrap(),
        );
        let strategy = SketchAlgorithmStrategy::new_with_planning_inputs_and_evidence(
            &DefaultCostModel,
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &evidence,
        );
        let candidates = strategy.replacements(&TargetSubDAG::new(&pre));
        let Replacement::Summary(node) = &candidates[0].replacement else {
            panic!("summary")
        };
        let SummaryExpr::BinaryOp { lhs, rhs, .. } = &node.expr else {
            panic!("ratio")
        };
        let alpha = |node: &SummaryNode| {
            let SummaryExpr::SummaryEstimate { summary_input, .. } = &node.expr else {
                panic!("readout")
            };
            let SummaryExpr::SummaryAgg {
                family: SummaryFamilyType::Sketch(kind, _),
                ..
            } = &summary_input.expr
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

/// Empty or overlarge population contracts cannot promise a supported readout.
#[test]
fn ddsketch_ratio_requires_a_supported_population_size() {
    struct PopulationEvidence(u64);
    impl AccuracyEvidenceProvider for PopulationEvidence {
        fn quantile_input_domain(&self, _: &QueryExpr) -> Option<QuantileInputDomain> {
            Some(QuantileInputDomain {
                lower: 1.,
                upper: 10.,
                max_samples: self.0,
                contract: "enforced fixture count and range".into(),
            })
        }
    }
    let pre = Rc::new(
        lower_promql(
            "quantile_over_time(0.9, data[5m]) / quantile_over_time(0.5, data[5m])",
            AccuracyTarget::Epsilon(0.01),
        )
        .unwrap(),
    );
    for count in [0, (1u64 << 53) + 1] {
        let evidence = PopulationEvidence(count);
        let strategy = SketchAlgorithmStrategy::new_with_planning_inputs_and_evidence(
            &DefaultCostModel,
            &DefaultAccuracyModel,
            &EqualSplitAllocator,
            &evidence,
        );
        assert!(strategy.replacements(&TargetSubDAG::new(&pre)).is_empty());
    }
}
