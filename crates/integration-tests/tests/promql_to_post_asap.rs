//! End-to-end query-string → post-ASAP IR pin (issue #98).
//!
//! Drives the full pipeline — PromQL text → non-ASAP `OperatorNode`
//! (`lower_promql`) → post-ASAP `OperatorNode` DAG (a Stage 1 logical
//! candidate, see [`realize`] below) — and pins
//! the summary-bound shape node by node, including the family `(Kind,
//! Params)` committed on each edge's schema.

use std::rc::Rc;

use asap_aware_mapping::accuracy::{AccuracyEvidenceProvider, OperandDomain};
use asap_aware_mapping::stage2_physical_asap_optimization::PhysicalCandidate;
use asap_aware_mapping::stage3_plan_selection::Selection;
use asap_aware_mapping::PlanningModels;
use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::planning::logical_candidates;
use asap_integration_tests::post_asap::{at_ingestion, at_ingestion_post_asap_dag, post_asap_dag};
use asap_types::ir::operator_properties::Reduction;
use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot, ScalarExpr};
use asap_types::post_asap::{
    EntityIdentity, ExactKind, ExactParams, FieldDataType, GroupingStrategy, Schema,
    SketchAlgorithm, SketchKind, SketchParams, SketchStatistic, SummaryInputExpr, SummaryUpdate,
};
use asap_types::pre_asap::expr_ir::ColumnRef;
use asap_types::pre_asap::schema::DataType;
use asap_types::types::AccuracyMetric;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{DataWorkload, QueryRequirements, QueryWorkloadEntry};

/// The first logical candidate (Stage 1, in enumeration order) of `root` as
/// a query at `accuracy` that uses an ASAP operator, or the plain exact one
/// when none does: the summary-bound form the pins below inspect.
fn realize(root: &Rc<OperatorNode>, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    let candidates = logical_candidates(Rc::clone(root), accuracy);
    let node = candidates
        .iter()
        .find(|candidate| candidate.contains_asap())
        .or(candidates.first())
        .cloned()
        .expect("Stage 1 offers at least the exact candidate");
    node.validate_structure()
        .expect("planned dag satisfies the unified IR contract");
    node
}

/// Whether `node` or anything below it is a `SummaryAgg` of `algorithm`.
fn uses_sketch(node: &OperatorNode, algorithm: &SketchAlgorithm) -> bool {
    match &node.operator {
        Operator::ASAP(ASAPOp::SummaryAgg {
            family: FieldDataType::Sketch(kind, _),
            ..
        }) if kind.algorithm() == algorithm => true,
        _ => node
            .children()
            .iter()
            .any(|child| uses_sketch(child, algorithm)),
    }
}

/// The logical candidates of `root` at `accuracy` that build a sketch of
/// `algorithm`, each with the `Project`s Stage 1 restores names with skipped.
fn candidates_using(
    root: &Rc<OperatorNode>,
    accuracy: AccuracyTarget,
    algorithm: SketchAlgorithm,
) -> Vec<Rc<OperatorNode>> {
    logical_candidates(Rc::clone(root), accuracy)
        .into_iter()
        .filter(|candidate| uses_sketch(candidate, &algorithm))
        .map(|candidate| Rc::clone(below_projects(&candidate)))
        .collect()
}

/// `node` with every `Project` above it skipped: Stage 1 restores a
/// rewritten aggregate's output names with a `Project`.
fn below_projects(mut node: &Rc<OperatorNode>) -> &Rc<OperatorNode> {
    while let Some(NonASAPOp::Project { child, .. }) = node.non_asap() {
        node = child;
    }
    node
}

/// `query` lowered at an exact target and realized ([`realize`]).
fn lower_and_realize(query: &str) -> Rc<OperatorNode> {
    let pre = lower_promql(query, AccuracyTarget::exact()).expect("lowering failed");
    realize(&pre, AccuracyTarget::exact())
}

#[test]
fn instant_topk_and_unsupported_child_remain_local_residuals() {
    for query in ["topk(3, memory_bytes)", "topk(3, deriv(memory_bytes[5m]))"] {
        let root = lower_and_realize(query);
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
    let root = lower_and_realize(query);
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

/// A vector-scalar division keeps its operand order in every Stage 1
/// logical candidate: `rate(a[1m]) / 2` divides the rate by 2, and
/// `2 / rate(a[1m])` divides 2 by the rate.
#[test]
fn promql_binary_arithmetic_preserves_both_scalar_operand_orders() {
    for (query, scalar_left) in [("rate(a[1m]) / 2", false), ("2 / rate(a[1m])", true)] {
        let pre = lower_promql(query, AccuracyTarget::exact()).expect("lowering failed");
        let candidates = logical_candidates(pre, AccuracyTarget::exact());
        assert!(!candidates.is_empty(), "{query}");
        for root in candidates {
            let Some(NonASAPOp::Project { cols, .. }) = root.non_asap() else {
                panic!("expected Project for {query}, got {:?}", root.operator)
            };
            let ScalarExpr::Arithmetic { left, right, .. } = &cols[1].expr else {
                panic!("expected arithmetic for {query}, got {:?}", cols[1].expr)
            };
            let (scalar, sample) = if scalar_left {
                (left, right)
            } else {
                (right, left)
            };
            assert_eq!(**scalar, ScalarExpr::literal_f64(2.0), "{query}");
            assert_eq!(**sample, ScalarExpr::Column(1), "{query}");
            assert!(root.schema.has_promql_series_identity(), "{query}");
        }
    }
}

/// An arm with no summary (`stddev_over_time`) stays plain exact work while
/// the other arm still binds its exact accumulator.
#[test]
fn promql_binary_arithmetic_keeps_an_unsupported_arm_exact() {
    let root = lower_and_realize("rate(a[1m]) + stddev_over_time(b[1m])");
    let Some(NonASAPOp::BinaryOp { lhs, rhs, .. }) = root.non_asap() else {
        panic!("expected BinaryOp, got {:?}", root.operator);
    };
    assert!(matches!(
        lhs.asap(),
        Some(ASAPOp::FinalizeExactAccumulator { .. })
    ));
    assert!(!rhs.contains_asap());
}

#[test]
fn promql_binary_arithmetic_preserves_nested_structure_and_vector_matching() {
    let nested = lower_and_realize("(rate(a[1m]) + rate(b[1m])) / 2");
    let Some(NonASAPOp::Project { child: lhs, .. }) = nested.non_asap() else {
        panic!("expected outer BinaryOp, got {:?}", nested.operator);
    };
    assert!(matches!(lhs.non_asap(), Some(NonASAPOp::BinaryOp { .. })));

    // A vector-matching modifier stays on the BinaryOp over the two
    // finalized rates.
    let modified = lower_and_realize("rate(a[1m]) + on(job) rate(b[1m])");
    let Some(NonASAPOp::BinaryOp {
        operator, lhs, rhs, ..
    }) = modified.non_asap()
    else {
        panic!("expected BinaryOp, got {:?}", modified.operator);
    };
    assert!(operator.vector_match.is_some());
    for operand in [lhs, rhs] {
        assert!(matches!(
            operand.asap(),
            Some(ASAPOp::FinalizeExactAccumulator { .. })
        ));
    }
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
            SketchAlgorithm::CmsWithHeap,
            vec![ColumnRef::Named("service".into())],
        ),
        (
            "topk(5, sum_over_time(requests[1m]))",
            SummaryInputExpr::Column(ColumnRef::SampleValue),
            SketchAlgorithm::CountSketchWithHeap,
            vec![],
        ),
    ];
    for (source, expected_update, expected_family, excluded_labels) in cases {
        let pre = lower_promql(
            source,
            AccuracyTarget::max_error(0.01).failure_probability(0.01),
        )
        .expect("lower temporal Top-K");
        let candidate = candidates_using(
            &pre,
            AccuracyTarget::max_error(0.01).failure_probability(0.01),
            expected_family.clone(),
        )
        .into_iter()
        .next()
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
        assert_eq!(kind.algorithm(), &expected_family);
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
        source: asap_types::pre_asap::Source::TimeSeries { metric },
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
            AccuracyTarget::max_error(0.01).failure_probability(0.01),
        )
        .unwrap();
        // This reference executor consumes keyed heap updates. The inventory
        // also contains exact values followed by sort/limit; those have a
        // different execution contract and must not enter this fixture.
        let accuracy = AccuracyTarget::max_error(0.01).failure_probability(0.01);
        let candidates: Vec<_> = [
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap,
        ]
        .into_iter()
        .flat_map(|algorithm| candidates_using(&pre, accuracy.clone(), algorithm))
        .collect();
        assert!(!candidates.is_empty(), "no heap candidate for {query}");
        for plan in candidates {
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
/// SummaryEstimate { query: Quantile{0.99} }          → {quantile: Float64}
/// └─ SummaryAgg { Kll{k:269}, input: SampleValue }   → {state: Sketch(Kll, {k:269})}
///    └─ FinalizeExactAccumulator
///       └─ SummaryAgg { Rate, input: SampleValue }   → {ts, state: ExactAggregate(Rate), …}
///          └─ TimeRange{5m} → Scan                   → {ts, value}
/// ```
///
/// The nested tree exercises both realizations: the approximate quantile
/// binds a KLL sketch + evaluation; the per-series `rate` binds the exact
/// counter-reset-aware accumulator (no estimate — its state is the value).
#[test]
fn promql_quantile_of_rate_binds_kll_over_rate_accumulator() {
    let pre_asap = lower_promql(
        "quantile(0.99, rate(http_requests_total[5m]))",
        AccuracyTarget::max_error(0.01),
    )
    .expect("lowering failed");
    // The KLL candidate over the per-series Rate accumulator.
    let root = &candidates_using(
        &pre_asap,
        AccuracyTarget::max_error(0.01),
        SketchAlgorithm::Kll,
    )
    .into_iter()
    .find(|plan| {
        matches!(plan.asap(), Some(ASAPOp::SummaryEstimate { summary_input, .. })
                if matches!(summary_input.asap(), Some(ASAPOp::SummaryAgg { child, .. })
                    if matches!(child.asap(), Some(ASAPOp::FinalizeExactAccumulator { .. }))))
    })
    .expect("a KLL over the finalized Rate accumulator");

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
        dtype(&root.schema, "quantile"),
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
            SketchKind::new(
                SketchAlgorithm::Kll,
                SketchParams::Kll {
                    k: 269,
                    failure_probability: None
                }
            ),
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
        dtype(&summary_input.schema, "state"),
        &FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::Kll,
                SketchParams::Kll {
                    k: 269,
                    failure_probability: None
                }
            ),
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
        dtype(&child.schema, "state"),
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

#[test]
fn nested_summary_explicitly_finalizes_exact_child_at_ingestion_time() {
    // Real workload selection must expose the state-to-value edge; an outer
    // sketch must not interpret exact accumulator bytes as input samples.
    let pre = lower_promql(
        "quantile(0.9, sum_over_time(m[1m]))",
        AccuracyTarget::max_error(0.05),
    )
    .unwrap();
    // The KLL candidate over the inner exact Sum accumulator.
    let plan = candidates_using(&pre, AccuracyTarget::max_error(0.05), SketchAlgorithm::Kll)
        .into_iter()
        .find(|plan| {
            matches!(plan.asap(), Some(ASAPOp::SummaryEstimate { summary_input, .. })
                if matches!(summary_input.asap(), Some(ASAPOp::SummaryAgg { child, .. })
                    if matches!(child.asap(), Some(ASAPOp::FinalizeExactAccumulator { .. }))))
        })
        .expect("a KLL over the finalized Sum accumulator");
    // Stored timings are gone: time the plan with its outer summary
    // maintained and read the timed copy.
    let timed_plan = at_ingestion(&plan);
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
        Some(asap_types::post_asap::ExecutionTiming::IngestionTime)
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
    use asap_types::post_asap::ExecutionTiming;
    // The one-selector case (both operands over the same series, so the
    // BinaryOp may run at ingestion) is `arithmetic_over_one_selector_executes`
    // in materialization_e2e.rs, ignored until #580.
    let query = "sum_over_time(m[1m]) + sum_over_time(n[1m])";
    let expected = ExecutionTiming::QueryTime;
    let input = lower_promql(query, AccuracyTarget::max_error(0.05)).unwrap();
    let plan = realize(&input, AccuracyTarget::max_error(0.05));
    let dag = at_ingestion_post_asap_dag(&plan);
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

struct FixtureQuantileDomain {
    lower: f64,
    upper: f64,
}
impl AccuracyEvidenceProvider for FixtureQuantileDomain {
    fn operand_domain(&self, _: &OperatorNode) -> Option<OperandDomain> {
        Some(OperandDomain {
            lower: self.lower,
            upper: self.upper,
            max_samples: 1000,
            contract: "enforced nonempty finite fixture window".into(),
        })
    }
}

/// `query` lowered at `planned` and selected with `evidence` at `accuracy`:
/// the physical candidates
/// whose two summaries are both DDSketch, and plan selection over all of them.
fn ddsketch_ratio_selection(
    query: &str,
    planned: AccuracyTarget,
    accuracy: AccuracyTarget,
    evidence: &dyn AccuracyEvidenceProvider,
) -> (Vec<PhysicalCandidate>, Selection) {
    let root = lower_promql(query, planned).unwrap();
    let run = asap_aware_mapping::run_stage1_to_stage3(
        vec![(0, QueryRoot::Operator(root))],
        &[QueryWorkloadEntry {
            requirements: QueryRequirements {
                accuracy,
                max_latency_ms: None,
            },
            ..Default::default()
        }],
        &DataWorkload::default(),
        PlanningModels::builtin().with_evidence(evidence),
    )
    .unwrap();
    let ratios = run
        .stage2_physical_asap_optimization
        .into_iter()
        .filter(|candidate| {
            let sketches: Vec<_> = candidate
                .dag
                .nodes
                .iter()
                .filter_map(|node| match &node.payload {
                    PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
                        family: FieldDataType::Sketch(kind, _),
                        ..
                    }) => Some(kind.algorithm().clone()),
                    _ => None,
                })
                .collect();
            sketches == [SketchAlgorithm::DDSketch, SketchAlgorithm::DDSketch]
        })
        .collect();
    (ratios, run.stage3_plan_selection.unwrap())
}

/// A missing proof for one side must not hide an invalid proof for the other.
#[test]
fn ddsketch_ratio_rejects_one_invalid_domain_when_the_other_is_missing() {
    struct PartialUnsafeDomain;
    impl AccuracyEvidenceProvider for PartialUnsafeDomain {
        fn operand_domain(&self, operand: &OperatorNode) -> Option<OperandDomain> {
            let Some(NonASAPOp::Aggregate { measures, .. }) = operand.non_asap() else {
                return None;
            };
            matches!(
                measures.as_slice(),
                [asap_types::pre_asap::agg_intent::AggIntent::Quantile { q, .. }] if *q == 0.9
            )
            .then(|| OperandDomain {
                lower: -1.0,
                upper: 1.0,
                max_samples: 1000,
                contract: "unsafe numerator".into(),
            })
        }
    }

    let (ratios, selection) = ddsketch_ratio_selection(
        "quantile_over_time(0.9, data[5m]) / quantile_over_time(0.5, data[5m])",
        AccuracyTarget::max_error(0.01),
        AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative),
        &PartialUnsafeDomain,
    );
    assert!(!ratios.is_empty());
    assert!(
        ratios
            .iter()
            .all(|candidate| !selection.costs.contains_key(&candidate.id)),
        "{selection:?}"
    );
}

/// The committed planner alpha is exercised against the pinned sketch implementation.
#[test]
fn ddsketch_ratio_bound_holds_for_signed_pinned_sketch_evaluations() {
    for sign in [-1., 1.] {
        let evidence = FixtureQuantileDomain {
            lower: if sign < 0. { -100. } else { 1. },
            upper: if sign < 0. { -1. } else { 100. },
        };
        let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
        let (ratios, selection) = ddsketch_ratio_selection(
            "quantile_over_time(0.9, data[5m]) / quantile_over_time(0.5, data[5m])",
            AccuracyTarget::max_error(0.01),
            target.clone(),
            &evidence,
        );
        let accepted: Vec<_> = ratios
            .iter()
            .filter(|candidate| selection.costs.contains_key(&candidate.id))
            .collect();
        assert!(!accepted.is_empty(), "{selection:?}");
        // Both operands read one alpha, which the accepted ratio's target bounds.
        let alphas: Vec<f64> = accepted[0]
            .dag
            .nodes
            .iter()
            .filter_map(|node| match &node.payload {
                PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) => match kind.params() {
                    SketchParams::DDSketch { alpha } => Some(*alpha),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(alphas.len(), 2);
        assert_eq!(alphas[0], alphas[1]);
        let alpha = alphas[0];
        let bound = target.max_error;
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
            let mut sketch = asap_sketchlib::DdSketch::new(alpha);
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

// Every `without` aggregation candidate exports a valid DAG: its summary state
// column carries the family instead of the evaluation's Float64 value.
#[test]
fn without_aggregation_candidates_export_valid_dags() {
    for accuracy in [
        AccuracyTarget::exact(),
        AccuracyTarget::max_error(0.01).failure_probability(0.01),
    ] {
        for query in ["sum without (pod) (m)", "quantile without (pod) (0.5, m)"] {
            let root = lower_promql(query, accuracy.clone()).unwrap();
            let candidates = logical_candidates(root, accuracy.clone());
            assert!(!candidates.is_empty(), "{query}");
            for node in &candidates {
                post_asap_dag(node);
            }
        }
    }
}
