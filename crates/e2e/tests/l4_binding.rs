//! End-to-end query-string → L4 pin (issue #98).
//!
//! Drives the full pipeline — PromQL text → L3 `QueryExpr` (`lower_promql`)
//! → L4 `SummaryExpr` DAG (`asap_plan::implement_tree`) — and pins the sketch-bound
//! shape node by node, including the `(SummaryKind, SummaryParams)` committed
//! on each edge's schema. This is the design doc's §"L4 — sketch algebra"
//! worked example, running for real.

use asap_frontend_promql::lower_promql;
use asap_ir::intent_algebra::expr_ir::ColumnRef;
use asap_ir::intent_algebra::query_expr::{QueryExpr, Reduction};
use asap_ir::intent_algebra::schema::DataType;
use asap_ir::types::AccuracyTarget;
use asap_plan::implement_tree;
use asap_sketch::{L4DataType, L4Schema, SketchQuery, SummaryExpr, SummaryKind, SummaryParams};

fn dtype<'a>(schema: &'a L4Schema, name: &str) -> &'a L4DataType {
    &schema
        .fields
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no field {name:?} in {schema:?}"))
        .dtype
}

/// `quantile(0.99, rate(http_requests_total[5m]))` at ε = 0.01:
///
/// ```text
/// SummaryEstimate { query: Quantile{0.99} }          → {quantile_0_99: Float64}
/// └─ SummaryAgg { Kll{k:200}, col: SampleValue }     → {quantile_0_99: Sketch(Kll, {k:200})}
///    └─ SummaryAgg { Rate, col: SampleValue }        → {ts, value: Sketch(Rate), …}
///       └─ Logical(TimeRange{5m} → Scan)             → {ts, value}
/// ```
///
/// The nested tree exercises both realizations: the approximate quantile
/// binds a KLL sketch + readout; the per-series `rate` binds the exact
/// counter-reset-aware accumulator (no estimate — its state is the value).
#[test]
fn promql_quantile_of_rate_binds_kll_over_rate_accumulator() {
    let l3 = lower_promql(
        "quantile(0.99, rate(http_requests_total[5m]))",
        AccuracyTarget::Epsilon(0.01),
    )
    .expect("lowering failed");
    let root = implement_tree(&l3).expect("binding failed");

    // Root: the sketch readout, back to a plain row shape.
    let SummaryExpr::SummaryEstimate {
        sketch_input,
        query,
    } = &root.expr
    else {
        panic!("expected SummaryEstimate root, got {:?}", root.expr);
    };
    assert!(matches!(query, SketchQuery::Quantile { q } if *q == 0.99));
    assert_eq!(
        dtype(&root.schema, "quantile_0_99"),
        &L4DataType::Primitive(DataType::Float64),
        "the Sketch(…) type must not propagate past the estimate"
    );

    // The quantile: KLL committed, k=200 sized from ε=0.01. `quantile(...)`
    // is an aggregation operator with no `by(...)`: a genuine full
    // reduction, one output row — not to be confused with the inner rate's
    // per-entity grouping below, even though both once collapsed to the
    // same empty `by: []` (issue #163).
    let SummaryExpr::SummaryAgg {
        child,
        sketch,
        params,
        col,
        reduction,
    } = &sketch_input.expr
    else {
        panic!("expected SummaryAgg, got {:?}", sketch_input.expr);
    };
    assert_eq!(sketch, &SummaryKind::Kll);
    assert_eq!(params, &SummaryParams::Kll { k: 200 });
    assert_eq!(col, &ColumnRef::SampleValue);
    assert_eq!(
        reduction,
        &Reduction::by(vec![]),
        "global quantile — no group keys, full reduction"
    );
    assert_eq!(
        dtype(&sketch_input.schema, "quantile_0_99"),
        &L4DataType::Sketch(SummaryKind::Kll, SummaryParams::Kll { k: 200 })
    );

    // The rate: exact counter-reset-aware accumulator, per-series (labels
    // and time axis preserved), no estimate wrapper. `rate(...)` has no
    // grouping concept at all — every entity stays its own summary.
    let SummaryExpr::SummaryAgg {
        child: leaf,
        sketch,
        params,
        reduction,
        ..
    } = &child.expr
    else {
        panic!("expected inner SummaryAgg for rate, got {:?}", child.expr);
    };
    assert_eq!(sketch, &SummaryKind::Rate);
    assert_eq!(params, &SummaryParams::Rate);
    assert_eq!(reduction, &Reduction::PerEntity);
    assert_eq!(
        dtype(&child.schema, "value"),
        &L4DataType::Sketch(SummaryKind::Rate, SummaryParams::Rate)
    );
    assert_eq!(
        child.schema.time_index,
        Some(0),
        "per-series keeps the time axis"
    );

    // The leaf: unrewritten L3 pass-through — TimeRange marker over the Scan.
    let SummaryExpr::Logical(l3_leaf) = &leaf.expr else {
        panic!("expected Logical leaf, got {:?}", leaf.expr);
    };
    let QueryExpr::TimeRange { range, child: scan } = l3_leaf.as_ref() else {
        panic!("expected TimeRange leaf, got {l3_leaf:?}");
    };
    assert_eq!(range.as_secs(), 300);
    assert!(matches!(scan.as_ref(), QueryExpr::Scan { .. }));
    assert!(
        leaf.schema
            .fields
            .iter()
            .all(|f| matches!(f.dtype, L4DataType::Primitive(_))),
        "logical edges carry only primitive columns"
    );
}

/// An exact workload binds zero sketches: `sum by (job) (m)` at
/// `AccuracyTarget::Exact` still gets its mergeable exact accumulator, and
/// `avg(m)` (non-mergeable) passes through as a whole logical subtree.
#[test]
fn promql_exact_workload_binds_accumulators_not_sketches() {
    let l3 = lower_promql("sum by (job) (http_requests_total)", AccuracyTarget::Exact)
        .expect("lowering failed");
    let root = implement_tree(&l3).expect("binding failed");
    let SummaryExpr::SummaryAgg {
        sketch,
        params,
        reduction,
        ..
    } = &root.expr
    else {
        panic!("expected SummaryAgg, got {:?}", root.expr);
    };
    assert_eq!(sketch, &SummaryKind::Sum);
    assert_eq!(params, &SummaryParams::Sum);
    assert_eq!(
        reduction,
        &Reduction::by(vec![2]),
        "job is col 2 in [ts, value, job]"
    );
    assert_eq!(
        dtype(&root.schema, "job"),
        &L4DataType::Primitive(DataType::Utf8),
        "group keys pass through verbatim"
    );

    let l3 =
        lower_promql("avg(http_requests_total)", AccuracyTarget::Exact).expect("lowering failed");
    let root = implement_tree(&l3).expect("binding failed");
    assert!(
        matches!(root.expr, SummaryExpr::Logical(_)),
        "avg has no mergeable accumulator — stays logical"
    );
}
