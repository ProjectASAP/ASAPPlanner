//! End-to-end SQL query-string → post-ASAP IR pin (issue #191).
//!
//! The SQL counterpart of `promql_to_post_asap.rs`: drives SQL text —
//! `lower_sql` (text → pre-ASAP `QueryExpr`) →
//! `SketchAlgorithmStrategy::replacements` (pre-ASAP → post-ASAP
//! `SummaryExpr`, see [`realize`] below) — and pins the resulting
//! sketch-vs-exact-accumulator shape node by node, the way
//! `promql_to_post_asap.rs` does for PromQL.
//!
//! ## A structural wrinkle PromQL doesn't have
//!
//! `lower_promql` returns a *bare* `QueryExpr::Aggregate` for a top-level
//! aggregation (`sum by (job) (m)`, `quantile(0.99, …)`), so [`realize`] can
//! bind it directly at the tree root. `lower_sql` never does: DataFusion's
//! planner always wraps even a single, unaliased aggregate in an identity
//! `Project` (confirmed below), so a SQL tree's *root* is always `Project {
//! child: Aggregate { .. } }`. Construction only fires when the node
//! `replacement.rs`'s `construct_summary` is looking at is itself a bindable
//! `QueryExpr::Aggregate` (see `replacement.rs`'s module docs on the "logical
//! parent subsumes bindable child" conservative fallback); a `Project` at the
//! root is exactly such a logical parent, so feeding a raw `lower_sql` result
//! straight into [`realize`] always yields a whole-tree
//! `SummaryExpr::KeepPreAsap` — never a genuine sketch or accumulator
//! binding.
//!
//! The tests below extract the inner `Aggregate` node the same way this
//! crate's own `frontend-sql/tests/sql_lowering.rs` does (its
//! `find_aggregate`/`find_aggregate_node` helpers) and hand that to
//! [`realize`] directly, which is the shape a future Project-elision rewrite
//! (tracked with the rest of the post-ASAP rule engine, issues #6/#33) would
//! present to this pass in production.

use std::rc::Rc;

use asap_aware_mapping::replacement::{keep_pre_asap, ImplementError};
use asap_aware_mapping::{
    Replacement, ReplacementStrategy, ReplacementSubDAG, SketchAlgorithmStrategy, TargetSubDAG,
};
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::post_asap::{
    ExactKind, ExactParams, GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams,
    SketchQuery, SummaryExpr, SummaryFamilyType, SummaryNode, SummarySchema,
};
use asap_types::pre_asap::expr_ir::ColumnRef;
use asap_types::pre_asap::query_expr::{QueryExpr, Reduction};
use asap_types::pre_asap::schema::{Column, DataType, Schema};
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

fn col(name: &str, dtype: DataType) -> Column {
    Column::new(name, dtype, false)
}

/// `metrics(ts, service, latency, bytes)` — mirrors
/// `frontend-sql/tests/sql_lowering.rs`'s catalog.
fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "metrics",
        Schema::with_time_index(
            vec![
                col("ts", DataType::Timestamp),
                col("service", DataType::Utf8),
                col("latency", DataType::Float64),
                col("bytes", DataType::Int64),
            ],
            0,
            vec![],
        ),
    )
}

async fn lower(sql: &str, accuracy: AccuracyTarget) -> QueryExpr {
    lower_sql(sql, &catalog(), accuracy)
        .await
        .unwrap_or_else(|e| panic!("lower failed for {sql:?}: {e}"))
}

/// The `Aggregate` node beneath the identity `Project` DataFusion's planner
/// always wraps a top-level aggregate in — see the module docs above.
fn inner_aggregate(qe: &QueryExpr) -> &QueryExpr {
    match qe {
        QueryExpr::Project { child, .. } => inner_aggregate(child),
        QueryExpr::Aggregate { .. } => qe,
        other => panic!("expected a Project{{Aggregate}} shape, got {other:?}"),
    }
}

/// Sanity + documentation: feeding a raw `lower_sql` root straight into
/// [`realize`] never binds anything — the wrapping `Project` always subsumes
/// the `Aggregate` beneath it into one logical passthrough. This is the
/// "conservative fallback" `replacement.rs`'s module docs describe, hitting
/// unconditionally for SQL because of the Project DataFusion always inserts.
#[tokio::test]
async fn sql_full_query_root_stays_logical_under_the_identity_projection() {
    let pre_asap = lower(
        "SELECT approx_percentile_cont(latency, 0.99) FROM metrics",
        AccuracyTarget::Epsilon(0.01),
    )
    .await;
    assert!(
        matches!(pre_asap, QueryExpr::Project { .. }),
        "sanity: a SQL root is a Project, unlike lower_promql's bare Aggregate"
    );
    let root = realize(&pre_asap).expect("binding failed");
    assert!(
        matches!(root.expr, SummaryExpr::KeepPreAsap(ref e) if **e == pre_asap),
        "bind does not look inside a Project to find a bindable Aggregate \
         child, so the whole Project{{Aggregate}} tree stays logical"
    );
}

/// `SELECT approx_percentile_cont(latency, 0.99) FROM metrics` at ε = 0.01,
/// with the wrapping `Project` stripped (see module docs):
///
/// ```text
/// SummaryEstimate { query: Quantile{0.99} }            → {…: Float64}
/// └─ SummaryAgg { Kll{k:200}, col: metrics.latency }    → {…: Sketch(Kll, {k:200})}
///    └─ KeepPreAsap(Scan)                                → {ts, service, latency, bytes}
/// ```
///
/// The SQL counterpart of `promql_to_post_asap.rs`'s
/// `promql_quantile_of_rate_binds_kll_over_rate_accumulator`: same intent
/// (`Quantile`), same KLL sizing (k=200 from ε=0.01), but the summarised
/// column is the intent's own *named* SQL column rather than PromQL's
/// synthetic sample value.
#[tokio::test]
async fn sql_quantile_binds_kll_sketch_over_named_column() {
    let pre_asap = lower(
        "SELECT approx_percentile_cont(latency, 0.99) FROM metrics",
        AccuracyTarget::Epsilon(0.01),
    )
    .await;
    let agg = inner_aggregate(&pre_asap);
    let root = realize(agg).expect("binding failed");

    let SummaryExpr::SummaryEstimate {
        summary_input,
        query,
    } = &root.expr
    else {
        panic!("expected SummaryEstimate root, got {:?}", root.expr);
    };
    assert!(matches!(query, SketchQuery::Quantile { q } if *q == 0.99));
    assert_eq!(
        root.schema.fields.len(),
        1,
        "no GROUP BY — a single output column"
    );
    assert_eq!(
        root.schema.fields[0].dtype,
        SummaryFamilyType::Plain(DataType::Float64),
        "the summary-state type must not propagate past the estimate"
    );

    let SummaryExpr::SummaryAgg {
        child,
        family,
        col,
        reduction,
        ..
    } = &summary_input.expr
    else {
        panic!("expected SummaryAgg, got {:?}", summary_input.expr);
    };
    assert_eq!(
        family,
        &SummaryFamilyType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
            GroupingStrategy::default()
        )
    );
    assert_eq!(
        col,
        &ColumnRef::Qualified {
            table: "metrics".into(),
            name: "latency".into(),
        },
        "SQL binds the intent's own named input column, not a synthetic sample value"
    );
    assert_eq!(
        reduction,
        &Reduction::by(vec![]),
        "global quantile — no GROUP BY, full reduction"
    );
    assert_eq!(
        summary_input.schema.fields[0].dtype,
        SummaryFamilyType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
            GroupingStrategy::default()
        )
    );

    let SummaryExpr::KeepPreAsap(kept_leaf) = &child.expr else {
        panic!("expected KeepPreAsap leaf, got {:?}", child.expr);
    };
    assert!(matches!(kept_leaf.as_ref(), QueryExpr::Scan { .. }));
    assert!(
        child
            .schema
            .fields
            .iter()
            .all(|f| matches!(f.dtype, SummaryFamilyType::Plain(_))),
        "logical edges carry only plain columns"
    );
}

/// `SELECT COUNT(DISTINCT service) FROM metrics` at ε = 0.01 lowers to
/// `AggIntent::Cardinality` (`sql_lowering.rs::count_distinct_is_cardinality`)
/// — unlike `Quantile`/`Count`/`TopK`, its preferred candidate is HLL, not
/// KLL/CMS (`replacement::summary_candidates`), so this exercises a
/// distinct branch of the sketch-vs-exact decision than the quantile test
/// above.
#[tokio::test]
async fn sql_count_distinct_binds_hll_sketch_over_named_column() {
    let pre_asap = lower(
        "SELECT COUNT(DISTINCT service) FROM metrics",
        AccuracyTarget::Epsilon(0.01),
    )
    .await;
    let agg = inner_aggregate(&pre_asap);
    let root = realize(agg).expect("binding failed");

    let SummaryExpr::SummaryEstimate {
        summary_input,
        query,
    } = &root.expr
    else {
        panic!("expected SummaryEstimate root, got {:?}", root.expr);
    };
    assert!(matches!(query, SketchQuery::Cardinality));
    assert_eq!(
        root.schema.fields[0].dtype,
        SummaryFamilyType::Plain(DataType::Int64),
        "COUNT(DISTINCT …) reads back out as an integer count"
    );

    let SummaryExpr::SummaryAgg {
        family,
        col,
        reduction,
        ..
    } = &summary_input.expr
    else {
        panic!("expected SummaryAgg, got {:?}", summary_input.expr);
    };
    assert_eq!(
        family,
        &SummaryFamilyType::Sketch(
            SketchKind::new(SketchAlgorithm::Hll, SketchParams::Hll { precision: 14 }),
            GroupingStrategy::default()
        )
    );
    assert_eq!(
        col,
        &ColumnRef::Qualified {
            table: "metrics".into(),
            name: "service".into(),
        }
    );
    assert_eq!(reduction, &Reduction::by(vec![]));
}

/// An exact workload binds zero sketches: `SUM(bytes) GROUP BY service` at
/// `AccuracyTarget::Exact` still gets its mergeable exact accumulator, and
/// `AVG(bytes)` (non-mergeable) stays a whole logical subtree untouched. SQL
/// counterpart of `promql_to_post_asap.rs`'s
/// `promql_exact_workload_binds_accumulators_not_sketches`.
#[tokio::test]
async fn sql_exact_workload_binds_accumulators_not_sketches() {
    let pre_asap = lower(
        "SELECT service, SUM(bytes) FROM metrics GROUP BY service",
        AccuracyTarget::Exact,
    )
    .await;
    let agg = inner_aggregate(&pre_asap);
    let root = realize(agg).expect("binding failed");
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
        &Reduction::by(vec![1]),
        "service is col 1 in [ts, service, latency, bytes]"
    );
    assert_eq!(
        dtype(&root.schema, "service"),
        &SummaryFamilyType::Plain(DataType::Utf8),
        "group keys pass through verbatim"
    );

    let pre_asap = lower("SELECT AVG(bytes) FROM metrics", AccuracyTarget::Exact).await;
    let agg = inner_aggregate(&pre_asap);
    let root = realize(agg).expect("binding failed");
    assert!(
        matches!(root.expr, SummaryExpr::KeepPreAsap(_)),
        "avg has no mergeable accumulator — stays logical"
    );
}
