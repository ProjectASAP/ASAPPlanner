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
};
use asap_aware_mapping::cost_model::DefaultCostModel;
use asap_aware_mapping::replacement::{keep_pre_asap, ImplementError};
use asap_aware_mapping::{
    search_workload, Replacement, ReplacementStrategy, ReplacementSubDAG, SketchAlgorithmStrategy,
    TargetSubDAG,
};
use asap_frontend_promql::lower_promql;
use asap_types::post_asap::{
    CompositionOperator, ExactKind, ExactParams, GroupingStrategy, SketchAlgorithm, SketchKind,
    SketchParams, SketchQuery, SummaryExpr, SummaryFamilyType, SummaryInput, SummaryNode,
    SummarySchema, TopKInput, TopKItem, TopKUpdate,
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
fn realize(expr: &QueryExpr) -> Result<Rc<SummaryNode>, ImplementError> {
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

fn dtype<'a>(schema: &'a SummarySchema, name: &str) -> &'a SummaryFamilyType {
    &schema
        .fields
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no field {name:?} in {schema:?}"))
        .dtype
}

struct SeparatedTopK;

impl AccuracyEvidenceProvider for SeparatedTopK {
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

#[test]
fn temporal_topk_binds_constant_one_and_sample_value_weight_projections() {
    let cases = [
        (
            "topk by (service) (5, count_over_time(requests[1m]))",
            TopKUpdate::Constant(1.0),
            "CmsWithHeap",
        ),
        (
            "topk(5, sum_over_time(requests[1m]))",
            TopKUpdate::Value(ColumnRef::SampleValue),
            "CountSketchWithHeap",
        ),
    ];
    for (source, expected_update, expected_family) in cases {
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
        let strategy = SketchAlgorithmStrategy::with_models_and_evidence(
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
            query: SketchQuery::TopK { input: readout, .. },
        } = &candidate.expr
        else {
            panic!("expected Top-K estimate, got {:?}", candidate.expr)
        };
        let SummaryExpr::SummaryAgg {
            input: SummaryInput::TopK(state_input),
            child,
            ..
        } = &summary_input.expr
        else {
            panic!("expected structured Top-K state input")
        };
        assert_eq!(state_input, readout);
        assert_eq!(
            state_input,
            &TopKInput {
                item: TopKItem::SeriesIdentity,
                update: expected_update,
            }
        );
        assert!(matches!(child.expr, SummaryExpr::KeepPreAsap(_)));
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
    assert_eq!(input, &SummaryInput::Column(ColumnRef::SampleValue));
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
    let group = space.group_for(root).expect("root memo group");
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
