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
//! `Project` (confirmed below), so a SQL tree's *root* is normally `Project {
//! child: Aggregate { .. } }`. Final materialization retains that projection
//! as a query-time value operation and independently plans its child, keeping
//! both SELECT-list semantics and the summary-bound aggregate visible.

use std::rc::Rc;

use asap_aware_mapping::replacement::{keep_pre_asap, ImplementError};
use asap_aware_mapping::{
    search_workload, DefaultCostModel, Replacement, ReplacementStrategy, ReplacementSubDAG,
    SketchAlgorithmStrategy, TargetSubDAG,
};
use asap_frontend_sql::{lower_sql, lower_sql_dialect, SqlCatalog};
use asap_types::post_asap::{
    compile_executable_dag, ExactKind, ExactParams, ExecutableOperatorPayload, GroupingStrategy,
    SketchAlgorithm, SketchKind, SketchParams, SketchQuery, SummaryExpr, SummaryFamilyType,
    SummaryNode, SummarySchema, SummaryUpdate, ValueOperation,
};
use asap_types::pre_asap::expr_ir::ColumnRef;
use asap_types::pre_asap::query_expr::{QueryExpr, Reduction};
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::types::AccuracyTarget;
use asap_types::workload::SqlDialect;

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
            vec![vec![0, 1]],
        ),
    )
}

async fn lower(sql: &str, accuracy: AccuracyTarget) -> QueryExpr {
    lower_sql(sql, &catalog(), accuracy)
        .await
        .unwrap_or_else(|e| panic!("lower failed for {sql:?}: {e}"))
}

#[tokio::test]
async fn clickhouse_temporal_sql_reuses_rate_and_increase_physical_summaries() {
    for (function, expected) in [
        (
            "asap_rate",
            SummaryFamilyType::ExactAggregate(ExactKind::Rate, ExactParams::Rate),
        ),
        (
            "asap_increase",
            SummaryFamilyType::ExactAggregate(ExactKind::Increase, ExactParams::Increase),
        ),
    ] {
        let sql = format!(
            "SELECT service, {function}(latency, ts, 300000) AS v \
             FROM metrics WHERE bytes > 0 GROUP BY service"
        );
        let pre_asap = lower_sql_dialect(
            &sql,
            &catalog(),
            SqlDialect::ClickhouseSQL,
            AccuracyTarget::Exact,
        )
        .await
        .expect("explicit temporal SQL must lower");
        let physical =
            realize(inner_aggregate(&pre_asap)).expect("temporal reducer must be planned");
        let SummaryExpr::SummaryAgg {
            family,
            reduction,
            child,
            ..
        } = &physical.expr
        else {
            panic!("expected a shared SummaryAgg, got {:?}", physical.expr);
        };
        assert_eq!(family, &expected);
        assert_eq!(reduction, &Reduction::PerEntity);
        let SummaryExpr::KeepPreAsap(raw) = &child.expr else {
            panic!(
                "expected a retained temporal SQL input, got {:?}",
                child.expr
            );
        };
        assert!(matches!(raw.as_ref(), QueryExpr::TimeRange { range, child }
            if *range == std::time::Duration::from_secs(300)
                && matches!(child.as_ref(), QueryExpr::Project { .. })));
    }
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

/// A complete SQL frontend result retains its projection and recursively
/// materializes the selected aggregate beneath it.
#[tokio::test]
async fn sql_full_query_retains_project_and_binds_inner_aggregate() {
    let pre_asap = lower(
        "SELECT approx_percentile_cont(latency, 0.99) AS p99 FROM metrics",
        AccuracyTarget::Epsilon(0.01),
    )
    .await;
    assert!(
        matches!(pre_asap, QueryExpr::Project { .. }),
        "sanity: a SQL root is a Project, unlike lower_promql's bare Aggregate"
    );
    let pre_asap = Rc::new(pre_asap);
    let space = search_workload(vec![("query", Rc::clone(&pre_asap))]);
    let selection = space.global_selection(&DefaultCostModel);
    let root = selection
        .materialize(&space.roots[0].1)
        .expect("materialization failed")
        .expect("root must be discovered");
    let QueryExpr::Project {
        cols: expected_cols,
        qualifier: expected_qualifier,
        ..
    } = pre_asap.as_ref()
    else {
        unreachable!()
    };
    let SummaryExpr::ValueOperation {
        child,
        operation: asap_types::post_asap::ValueOperation::Project { cols, qualifier },
        ..
    } = &root.expr
    else {
        panic!("expected retained Project root, got {:?}", root.expr);
    };
    assert_eq!(cols, expected_cols, "projection expressions and aliases");
    assert_eq!(qualifier, expected_qualifier, "projection qualifier");
    assert_eq!(root.schema.fields[0].name, "p99", "project output schema");
    assert_eq!(
        root.schema.fields[0].dtype,
        SummaryFamilyType::Plain(DataType::Float64)
    );
    assert!(
        matches!(child.expr, SummaryExpr::SummaryEstimate { .. }),
        "the Aggregate under Project must be summary-bound"
    );
}

/// Relational parents emitted around a derived-table aggregate remain
/// explicit read-time nodes while the aggregate is summary-bound.
#[tokio::test]
async fn sql_relational_parents_retain_summary_bound_aggregate() {
    let pre_asap = Rc::new(
        lower(
            "SELECT t.service, t.p FROM \
             (SELECT service, approx_percentile_cont(latency, 0.9) AS p \
              FROM metrics GROUP BY service) t \
             WHERE t.p > 100 ORDER BY t.p DESC LIMIT 5",
            AccuracyTarget::Epsilon(0.01),
        )
        .await,
    );
    let space = search_workload(vec![("query", Rc::clone(&pre_asap))]);
    let selection = space.global_selection(&DefaultCostModel);
    let root = selection
        .materialize(&space.roots[0].1)
        .expect("materialization failed")
        .expect("root must be discovered");

    let mut node = root.as_ref();
    let mut saw_project = false;
    let mut saw_filter = false;
    let mut saw_sort = false;
    let mut saw_limit = false;
    loop {
        match &node.expr {
            SummaryExpr::ValueOperation {
                child, operation, ..
            } => {
                match operation {
                    asap_types::post_asap::ValueOperation::Project { .. } => saw_project = true,
                    asap_types::post_asap::ValueOperation::Filter { .. } => saw_filter = true,
                    asap_types::post_asap::ValueOperation::Sort { .. } => saw_sort = true,
                    asap_types::post_asap::ValueOperation::Limit { n, offset } => {
                        assert_eq!((*n, *offset), (5, 0));
                        saw_limit = true;
                    }
                    _ => {}
                }
                node = child;
            }
            SummaryExpr::SummaryEstimate { summary_input, .. } => {
                assert!(matches!(summary_input.expr, SummaryExpr::SummaryAgg { .. }));
                break;
            }
            other => panic!("expected relational parents over SummaryEstimate, got {other:?}"),
        }
    }
    assert!(saw_project && saw_filter && saw_sort && saw_limit);
}

/// A post-aggregate predicate is read-time work, while a source predicate is
/// part of the rows that populate the maintained summary. Neither predicate
/// may be dropped or moved across the aggregation boundary.
#[tokio::test]
async fn sql_filter_keeps_read_predicate_and_summary_population_selection() {
    let pre_asap = Rc::new(
        lower(
            "SELECT t.service, t.p FROM \
             (SELECT service, approx_percentile_cont(latency, 0.9) AS p \
              FROM metrics WHERE service = 'api' GROUP BY service) t \
             WHERE t.p > 100",
            AccuracyTarget::Epsilon(0.01),
        )
        .await,
    );
    let expected_read_predicate = {
        let mut node = pre_asap.as_ref();
        loop {
            match node {
                QueryExpr::Filter { pred, .. } => break pred.clone(),
                QueryExpr::Project { child, .. }
                | QueryExpr::Sort { child, .. }
                | QueryExpr::Limit { child, .. } => node = child,
                other => panic!("expected a Filter above the aggregate, got {other:?}"),
            }
        }
    };
    let expected_source_predicates = {
        let mut node = pre_asap.as_ref();
        loop {
            match node {
                QueryExpr::Scan { predicates, .. } => break predicates.clone(),
                QueryExpr::Project { child, .. }
                | QueryExpr::Filter { child, .. }
                | QueryExpr::Aggregate { child, .. }
                | QueryExpr::Sort { child, .. }
                | QueryExpr::Limit { child, .. } => node = child,
                other => panic!("expected a unary SQL plan over Scan, got {other:?}"),
            }
        }
    };
    assert_eq!(expected_source_predicates.len(), 1, "fixture source WHERE");

    let space = search_workload(vec![("query", Rc::clone(&pre_asap))]);
    let selection = space.global_selection(&DefaultCostModel);
    let root = selection
        .materialize(&space.roots[0].1)
        .expect("materialization failed")
        .expect("root must be discovered");

    let mut node = root.as_ref();
    let mut retained_read_predicate = None;
    loop {
        match &node.expr {
            SummaryExpr::ValueOperation {
                child,
                operation: ValueOperation::Filter { pred },
                ..
            } => {
                retained_read_predicate = Some(pred.clone());
                node = child;
            }
            SummaryExpr::ValueOperation { child, .. } => node = child,
            SummaryExpr::SummaryEstimate { summary_input, .. } => {
                let SummaryExpr::SummaryAgg { child, .. } = &summary_input.expr else {
                    panic!("expected SummaryAgg below SummaryEstimate");
                };
                let SummaryExpr::KeepPreAsap(raw_input) = &child.expr else {
                    panic!("expected raw summary population below SummaryAgg");
                };
                let QueryExpr::Scan { predicates, .. } = raw_input.as_ref() else {
                    panic!("expected source selection to remain a Scan");
                };
                assert_eq!(predicates, &expected_source_predicates);
                break;
            }
            other => panic!("expected read-time operations over a summary, got {other:?}"),
        }
    }
    assert_eq!(retained_read_predicate, Some(expected_read_predicate));

    let executable = compile_executable_dag(&root).expect("typed DAG compilation failed");
    assert!(executable.nodes.iter().any(|node| matches!(
        &node.payload,
        ExecutableOperatorPayload::Value {
            operation: ValueOperation::Filter { pred },
            ..
        } if pred == retained_read_predicate.as_ref().unwrap()
    )));
}

/// If the child has no legal summary implementation, retain only that child
/// as the fallback leaf and keep the supported Filter as an explicit local
/// read-time operation.
#[tokio::test]
async fn sql_filter_preserves_local_fallback_boundary_for_unsupported_child() {
    let pre_asap = Rc::new(
        lower(
            "SELECT t.service, t.avg_bytes FROM \
             (SELECT service, AVG(bytes) AS avg_bytes FROM metrics GROUP BY service) t \
             WHERE t.avg_bytes > 100",
            AccuracyTarget::Exact,
        )
        .await,
    );
    let space = search_workload(vec![("query", Rc::clone(&pre_asap))]);
    let selection = space.global_selection(&DefaultCostModel);
    let root = selection
        .materialize(&space.roots[0].1)
        .expect("materialization failed")
        .expect("root must be discovered");

    let mut node = root.as_ref();
    let mut saw_filter = false;
    loop {
        match &node.expr {
            SummaryExpr::ValueOperation {
                child, operation, ..
            } => {
                saw_filter |= matches!(operation, ValueOperation::Filter { .. });
                node = child;
            }
            SummaryExpr::KeepPreAsap(fallback) => {
                assert!(
                    matches!(fallback.as_ref(), QueryExpr::BinaryOp { .. }),
                    "AVG's unsupported rewritten child should be opaque, got {fallback:?}"
                );
                break;
            }
            other => panic!("expected local value operations over fallback child, got {other:?}"),
        }
    }
    assert!(saw_filter, "supported Filter must remain explicit");
}

/// `SELECT approx_percentile_cont(latency, 0.99) FROM metrics` at ε = 0.01,
/// with the wrapping `Project` stripped (see module docs):
///
/// ```text
/// SummaryEstimate { query: Quantile{0.99} }            → {…: Float64}
/// └─ SummaryAgg { Kll{k:269}, input: metrics.latency }  → {…: Sketch(Kll, {k:269})}
///    └─ KeepPreAsap(Scan)                                → {ts, service, latency, bytes}
/// ```
///
/// The SQL counterpart of `promql_to_post_asap.rs`'s
/// `promql_quantile_of_rate_binds_kll_over_rate_accumulator`: same intent
/// (`Quantile`), same 99%-confidence KLL sizing (k=269 from ε=0.01), but the summarised
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
    assert_eq!(
        input,
        &SummaryUpdate::column(ColumnRef::Qualified {
            table: "metrics".into(),
            name: "latency".into(),
        }),
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
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 269 }),
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
async fn sql_count_distinct_with_epsilon_binds_hll_rse_over_named_column() {
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
            SketchKind::new(SketchAlgorithm::Hll, SketchParams::Hll { precision: 14 }),
            GroupingStrategy::default()
        )
    );
    assert_eq!(
        input,
        &SummaryUpdate::column(ColumnRef::Qualified {
            table: "metrics".into(),
            name: "service".into(),
        })
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
