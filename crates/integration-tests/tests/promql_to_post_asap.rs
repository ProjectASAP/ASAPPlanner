//! End-to-end query-string → post-ASAP IR pin (issue #98).
//!
//! Drives the full pipeline — PromQL text → non-ASAP `OperatorNode`
//! (`lower_promql`) → post-ASAP `OperatorNode` DAG (a Stage 1 logical
//! candidate, see [`realize`] below) — and pins
//! the summary-bound shape node by node, including the family `(Kind,
//! Params)` committed on each edge's schema.

use std::rc::Rc;

use asap_integration_tests::fixtures::lower_promql;
use asap_integration_tests::physical_export::physical_asap_dag;
use asap_integration_tests::planning::{logical_candidates, plan_with_summary_type};
use asap_types::ir::operator::operator_properties::Reduction;

use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::scalar::expr_ir::ColumnRef;
use asap_types::ir::schema::DataType;
use asap_types::ir::schema::{
    EntityIdentity, ExactKind, ExactParams, FieldDataType, Schema, SketchAlgorithm,
    SketchStatistic, SummaryInputExpr, SummaryUpdate,
};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, ScalarExpr};

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
        // The by-job sum is either the accumulator's own reduction or an
        // exact `Aggregate` over the per-series finalized values.
        let operand = match operand.non_asap() {
            Some(NonASAPOp::Aggregate { child, .. }) => child,
            _ => operand,
        };
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
    // The state: built over the window (`SummaryAgg` over `TimeRange`), or
    // merged from a chain's windows the `TimeRange` selects (the window
    // read is the range's whatever the chain's width).
    let (input, reduction, range, child) = match summary_input.asap() {
        Some(ASAPOp::SummaryAgg {
            input,
            child,
            reduction,
            ..
        }) => {
            // The fused raw input is the retained non-ASAP sub-DAG itself.
            assert!(!child.contains_asap(), "expected fused raw input");
            let Some(NonASAPOp::TimeRange { range, child, .. }) = child.non_asap() else {
                panic!("expected temporal input")
            };
            (input, reduction, range, child)
        }
        Some(ASAPOp::SummaryMerge { children, .. }) => {
            let [windows] = children.as_slice() else {
                panic!("expected one merged input")
            };
            let Some(NonASAPOp::TimeRange { range, child, .. }) = windows.non_asap() else {
                panic!("expected the chain's windows")
            };
            let Some(ASAPOp::SummaryTumblingWindowChain {
                input,
                child,
                reduction,
                ..
            }) = child.asap()
            else {
                panic!("expected a tumbling-window chain")
            };
            (input, reduction, range, child)
        }
        _ => panic!("expected summary updates"),
    };
    assert_eq!(reduction, &Reduction::by(vec![]));
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
    // Count-Min heaps need non-negative weights: a count's (1) are, the
    // samples `sum_over_time` adds (of unknown sign, `worker` has −50) are
    // not, so Stage 2 prunes those and only Count Sketch heaps sum them.
    let both = [
        SketchAlgorithm::CmsWithHeap,
        SketchAlgorithm::CountSketchWithHeap,
    ];
    let count_sketch = [SketchAlgorithm::CountSketchWithHeap];
    for (query, expected, algorithms) in [
        (
            "topk(1, count_over_time(requests[1m]))",
            vec![("api", 4.0)],
            &both[..],
        ),
        (
            "topk(2, count_over_time(requests[1m]))",
            vec![("api", 4.0), ("cron", 3.0)],
            &both[..],
        ),
        (
            "topk(1, sum_over_time(requests[1m]))",
            vec![("worker", 100.0)],
            &count_sketch[..],
        ),
        (
            "topk(2, sum_over_time(requests[1m]))",
            vec![("worker", 100.0), ("cron", 30.0)],
            &count_sketch[..],
        ),
    ] {
        let pre = lower_promql(
            query,
            AccuracyTarget::max_error(0.01).failure_probability(0.01),
        )
        .unwrap();
        // This reference executor consumes keyed heap updates: the plan
        // Stage 3 selects when the deployment builds only `algorithm`.
        let accuracy = AccuracyTarget::max_error(0.01).failure_probability(0.01);
        for &algorithm in algorithms {
            let dag = plan_with_summary_type(Rc::clone(&pre), accuracy.clone(), algorithm);
            let roots = dag.query_roots().unwrap();
            let [asap_types::ir::QueryRoot::Operator(root)] = roots.as_slice() else {
                panic!("one operator root for {query}: {roots:?}");
            };
            let plan = below_projects(root);
            assert!(
                dag.nodes.iter().any(|node| matches!(
                    &node.payload,
                    Operator::ASAP(
                        ASAPOp::SummaryAgg { summary_type, .. }
                            | ASAPOp::SummaryTumblingWindowChain { summary_type, .. }
                    ) if *summary_type == FieldDataType::Sketch(algorithm)
                )),
                "no {algorithm:?} heap for {query}"
            );
            let expected: Vec<_> = expected
                .iter()
                .map(|(job, score)| (format!("__name__=requests,job={job}"), *score))
                .collect();
            assert_eq!(execute_topk_reference(plan), expected, "{query}");
        }
    }
}

/// `quantile(0.99, rate(http_requests_total[5m]))` at ε = 0.01:
///
/// ```text
/// SummaryEstimate { query: Quantile{0.99} }          → {quantile: Float64}
/// └─ SummaryAgg { Kll, input: SampleValue }          → {state: Sketch(Kll)}
///    └─ the rate's group, one candidate: FinalizeExactAccumulator
///       └─ SummaryAgg { Rate, input: SampleValue }   → {ts, state: ExactAggregate(Rate), …}
///          └─ TimeRange{5m} → Scan                   → {ts, value}
/// ```
///
/// The nested tree exercises both: the approximate quantile reads a KLL
/// sketch and its estimate; the per-series `rate` reads the exact
/// counter-reset-aware accumulator (no estimate — its state is the value).
/// The memo after Pass 2 holds these groups (the rules generate them;
/// whether Stage 3 selects them is a matter of cost).
#[test]
fn promql_quantile_of_rate_binds_kll_over_rate_accumulator() {
    use asap_types::stages::{Memo, MemoGroup, MemoGroupId};
    let pre_asap = lower_promql(
        "quantile(0.99, rate(http_requests_total[5m]))",
        AccuracyTarget::max_error(0.01),
    )
    .expect("lowering failed");
    let run =
        asap_integration_tests::planning::run_all_stages(pre_asap, AccuracyTarget::max_error(0.01));
    let memo: &Memo = &run.run.logical_asap_candidates.as_ref().unwrap().memo;
    let group = |id: &MemoGroupId| -> &MemoGroup { &memo.groups()[id.0 as usize] };
    // The group of `id`'s finalized exact accumulator: the accumulator group
    // a `FinalizeExactAccumulator` candidate of `id` reads.
    let finalized = |id: &MemoGroupId| -> Option<MemoGroupId> {
        group(id)
            .candidates
            .iter()
            .find_map(|candidate| match &candidate.expression.operator {
                Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }) => Some(*child),
                _ => None,
            })
    };
    // The KLL estimate whose state group reads the rate, which a candidate
    // computes by finalizing the Rate accumulator.
    let (estimate, state) = memo
        .groups()
        .iter()
        .find_map(|estimate| match &estimate.expression {
            Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input,
                query: SketchStatistic::Quantile { q },
            }) if *q == 0.99 => {
                let state = group(summary_input);
                matches!(
                    &state.expression,
                    Operator::ASAP(ASAPOp::SummaryAgg { summary_type, child, .. })
                        if *summary_type == FieldDataType::Sketch(SketchAlgorithm::Kll)
                            && finalized(child).is_some()
                )
                .then_some((estimate, state))
            }
            _ => None,
        })
        .expect("a KLL over the finalized Rate accumulator");
    assert_eq!(
        dtype(&estimate.group_properties.schema, "quantile"),
        &FieldDataType::Plain(DataType::Float64),
        "the summary-state type must not propagate past the estimate"
    );

    // The quantile: a full reduction, one output row, not to be confused
    // with the inner rate's per-entity grouping below (issue #163).
    let Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        input,
        reduction,
        ..
    }) = &state.expression
    else {
        unreachable!("matched above");
    };
    assert_eq!(input, &SummaryUpdate::column(ColumnRef::SampleValue));
    assert_eq!(
        reduction,
        &Reduction::by(vec![]),
        "global quantile — no group keys, full reduction"
    );
    assert_eq!(
        dtype(&state.group_properties.schema, "state"),
        &FieldDataType::Sketch(SketchAlgorithm::Kll)
    );
    // The rate: exact counter-reset-aware accumulator, per-series (labels
    // and time axis preserved), no estimate wrapper.
    let rate = group(&finalized(child).expect("matched above"));
    let Operator::ASAP(ASAPOp::SummaryAgg {
        child: leaf,
        summary_type,
        reduction,
        ..
    }) = &rate.expression
    else {
        panic!(
            "expected inner SummaryAgg for rate, got {:?}",
            rate.expression
        );
    };
    assert_eq!(
        summary_type,
        &FieldDataType::ExactAggregate(ExactKind::Rate, ExactParams::Rate)
    );
    assert_eq!(reduction, &Reduction::PerEntity);
    assert_eq!(
        dtype(&rate.group_properties.schema, "state"),
        &FieldDataType::ExactAggregate(ExactKind::Rate, ExactParams::Rate)
    );
    assert_eq!(
        rate.group_properties.schema.time_index,
        Some(0),
        "per-series keeps the time axis"
    );

    // The leaf: the 5 m TimeRange over the Scan, plain columns only.
    let leaf = group(leaf);
    let Operator::NonASAP(NonASAPOp::TimeRange {
        range, child: scan, ..
    }) = &leaf.expression
    else {
        panic!("expected TimeRange leaf, got {:?}", leaf.expression);
    };
    assert_eq!(range.as_secs(), 300);
    assert!(matches!(
        group(scan).expression,
        Operator::NonASAP(NonASAPOp::Scan { .. })
    ));
    assert!(
        leaf.group_properties
            .schema
            .fields
            .iter()
            .all(|f| matches!(f.dtype, FieldDataType::Plain(_))),
        "logical edges carry only plain columns"
    );
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
                physical_asap_dag(node);
            }
        }
    }
}
