//! End-to-end SQL query-string → post-ASAP IR pin (issue #191).
//!
//! The SQL counterpart of `promql_to_post_asap.rs`: drives SQL text —
//! `lower_sql` (text → non-ASAP `OperatorNode` tree) → the planner stages'
//! logical candidates (→ a tree with ASAP operators, see [`realize`]
//! below) — and pins the resulting sketch-vs-exact-accumulator shape node by
//! node, the way `promql_to_post_asap.rs` does for PromQL.
//!
//! ## A structural wrinkle PromQL doesn't have
//!
//! `lower_promql` returns a *bare* `NonASAPOp::Aggregate` for a top-level
//! aggregation (`sum by (job) (m)`, `quantile(0.99, …)`), so a summary can
//! bind it directly at the DAG root. `lower_sql` never does: DataFusion's
//! planner always wraps even a single, unaliased aggregate in an identity
//! `Project` (confirmed below), so a SQL DAG's *root* is normally `Project {
//! child: Aggregate { .. } }`. Final materialization retains that projection
//! as a query-time non-ASAP node and independently plans its child, keeping
//! both SELECT-list semantics and the summary-bound aggregate visible.

use std::rc::Rc;

use asap_frontend_sql::{lower_sql, lower_sql_dialect, SqlCatalog};
use asap_integration_tests::physical_export::physical_asap_dag;
use asap_integration_tests::planning::logical_candidates;
use asap_types::ir::operator::operator_properties::Reduction;
use asap_types::ir::physical_export::{EdgeRole, PhysicalASAPNodeId, PhysicalASAPOperatorPayload};
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::scalar::expr_ir::ColumnRef;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::schema::{
    ExactKind, ExactParams, FieldDataType, GroupingStrategy, SketchAlgorithm, SketchKind,
    SketchParams, SketchStatistic, SummaryUpdate,
};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, Predicate, ScalarExpr};
use asap_types::workload::SqlDialect;

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
/// rewritten aggregate's output names with a `Project`, and SQL wraps every
/// query in one.
fn below_projects(mut node: &Rc<OperatorNode>) -> &Rc<OperatorNode> {
    while let Some(NonASAPOp::Project { child, .. }) = node.non_asap() {
        node = child;
    }
    node
}

/// The first `SummaryAgg` reached from `node` through unary nodes, estimates
/// and finalized accumulators.
fn first_summary_agg(node: &Rc<OperatorNode>) -> &Rc<OperatorNode> {
    match &node.operator {
        Operator::ASAP(ASAPOp::SummaryAgg { .. }) => node,
        Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => {
            first_summary_agg(summary_input)
        }
        _ => first_summary_agg(
            unary_child(node).unwrap_or_else(|| panic!("no SummaryAgg below {:?}", node.operator)),
        ),
    }
}

/// The single input of a unary non-ASAP node (Project, Filter, Sort, ...) or
/// of a `FinalizeExactAccumulator`; `None` for anything else.
fn unary_child(node: &OperatorNode) -> Option<&Rc<OperatorNode>> {
    match &node.operator {
        Operator::NonASAP(op) => match op.children().as_slice() {
            [child] => Some(*child),
            _ => None,
        },
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }) => Some(child),
        Operator::ASAP(_) => None,
    }
}

/// A sub-DAG kept as plain (non-ASAP) work: no ASAP operator anywhere below.
fn is_kept_non_asap(node: &OperatorNode) -> bool {
    node.non_asap().is_some() && !node.contains_asap()
}

/// Mirror a scalar-only predicate (no operator references) to its wire form.
fn wire_pred(pred: &Predicate) -> Predicate<PhysicalASAPNodeId> {
    Predicate(pred.0.map_operator_refs(&mut |_| -> PhysicalASAPNodeId {
        panic!("fixture predicate references no operator")
    }))
}

fn col(name: &str, dtype: DataType) -> Field {
    Field::plain(name, dtype, false)
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

async fn lower(sql: &str, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    lower_sql(sql, &catalog(), accuracy)
        .await
        .unwrap_or_else(|e| panic!("lower failed for {sql:?}: {e}"))
}

#[tokio::test]
async fn clickhouse_temporal_sql_reuses_rate_and_increase_physical_summaries() {
    for (function, expected) in [
        (
            "asap_rate",
            FieldDataType::ExactAggregate(ExactKind::Rate, ExactParams::Rate),
        ),
        (
            "asap_increase",
            FieldDataType::ExactAggregate(ExactKind::Increase, ExactParams::Increase),
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
            AccuracyTarget::exact(),
        )
        .await
        .expect("explicit temporal SQL must lower");
        let planned = realize(&pre_asap, AccuracyTarget::exact());
        let physical = first_summary_agg(&planned);
        let Operator::ASAP(ASAPOp::SummaryAgg {
            family,
            reduction,
            child,
            ..
        }) = &physical.operator
        else {
            panic!("expected a shared SummaryAgg, got {:?}", physical.operator);
        };
        assert_eq!(family, &expected);
        assert_eq!(reduction, &Reduction::PerEntity);
        assert!(
            is_kept_non_asap(child),
            "expected a retained temporal SQL input, got {:?}",
            child.operator
        );
        assert!(
            matches!(child.non_asap(), Some(NonASAPOp::TimeRange { range, child, .. })
            if *range == std::time::Duration::from_secs(300)
                && matches!(child.non_asap(), Some(NonASAPOp::Project { .. })))
        );
    }
}

#[tokio::test]
async fn clickhouse_outer_sum_recursively_binds_inner_temporal_aggregate() {
    for (function, window_ms) in [
        ("asap_rate", 300_000),
        ("asap_rate", 3_600_000),
        ("asap_increase", 300_000),
    ] {
        let sql = format!(
            "SELECT sum(v) AS value FROM (\
             SELECT service, {function}(latency, ts, {window_ms}) AS v \
             FROM metrics GROUP BY service)"
        );
        let pre_asap = lower_sql_dialect(
            &sql,
            &catalog(),
            SqlDialect::ClickhouseSQL,
            AccuracyTarget::exact(),
        )
        .await
        .expect("nested temporal SQL must lower");
        let root = realize(&pre_asap, AccuracyTarget::exact());

        fn has_temporal_summary(node: &OperatorNode) -> bool {
            match &node.operator {
                Operator::ASAP(ASAPOp::SummaryAgg {
                    family: FieldDataType::ExactAggregate(ExactKind::Rate | ExactKind::Increase, _),
                    ..
                }) => true,
                Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) => {
                    has_temporal_summary(summary_input)
                }
                Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) => has_temporal_summary(child),
                _ => unary_child(node).is_some_and(|child| has_temporal_summary(child)),
            }
        }
        assert!(
            has_temporal_summary(&root),
            "inner {function} was hidden: {root:?}"
        );
        // The outer sum is an exact Sum state over the finalized per-series
        // values, so the export keeps both summaries.
        let Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) = &first_summary_agg(&root).operator
        else {
            unreachable!()
        };
        assert_eq!(
            family,
            &FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum)
        );
        let dag = physical_asap_dag(&root);
        assert_eq!(
            dag.nodes
                .iter()
                .filter(|node| matches!(
                    node.payload,
                    PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg { .. })
                ))
                .count(),
            2
        );
    }
}

#[tokio::test]
async fn sql_full_query_retains_project_and_binds_inner_aggregate() {
    let pre_asap = lower(
        "SELECT approx_percentile_cont(latency, 0.99) AS p99 FROM metrics",
        AccuracyTarget::max_error(0.01),
    )
    .await;
    let Some(NonASAPOp::Project {
        cols: expected_cols,
        qualifier: expected_qualifier,
        ..
    }) = pre_asap.non_asap()
    else {
        panic!("sanity: a SQL root is a Project, unlike lower_promql's bare Aggregate");
    };
    let root = realize(&pre_asap, AccuracyTarget::max_error(0.01));
    let Some(NonASAPOp::Project {
        child,
        cols,
        qualifier,
    }) = root.non_asap()
    else {
        panic!("expected retained Project root, got {:?}", root.operator);
    };
    assert_eq!(cols, expected_cols, "projection expressions and aliases");
    assert_eq!(qualifier, expected_qualifier, "projection qualifier");
    assert_eq!(root.schema.fields[0].name, "p99", "project output schema");
    assert_eq!(
        root.schema.fields[0].dtype,
        FieldDataType::Plain(DataType::Float64)
    );
    assert!(
        matches!(
            below_projects(child).operator,
            Operator::ASAP(ASAPOp::SummaryEstimate { .. })
        ),
        "the Aggregate under Project must be summary-bound"
    );
}

/// A relational join remains a read-time node while both derived-table
/// aggregates are independently selected as physical summaries.
#[tokio::test]
async fn sql_join_recursively_binds_both_temporal_aggregate_children() {
    let pre_asap = lower_sql_dialect(
        "SELECT a.service, a.v / b.v AS ratio FROM \
         (SELECT service, asap_rate(latency, ts, 300000) AS v FROM metrics WHERE service='errors' GROUP BY service) a \
         INNER JOIN \
         (SELECT service, asap_rate(latency, ts, 300000) AS v FROM metrics WHERE service='requests' GROUP BY service) b \
         ON b.service=a.service",
        &catalog(),
        SqlDialect::ClickhouseSQL,
        AccuracyTarget::exact(),
    )
    .await
    .expect("two-subquery rate ratio must lower");
    let root = realize(&pre_asap, AccuracyTarget::exact());
    let Some(NonASAPOp::Project {
        child: join, cols, ..
    }) = root.non_asap()
    else {
        panic!(
            "expected Project above relational join, got {:?}",
            root.operator
        );
    };
    assert!(matches!(
        &cols[1].expr,
        ScalarExpr::Arithmetic {
            op: asap_types::ir::scalar::ArithmeticOpKind::Div,
            ..
        }
    ));
    let Some(NonASAPOp::Join {
        left,
        right,
        kind,
        pred,
    }) = join.non_asap()
    else {
        panic!(
            "expected read-time relational join, got {:?}",
            join.operator
        );
    };
    assert_eq!(kind, &asap_types::ir::operator::JoinKind::Inner);
    assert!(matches!(
        &pred.0,
        ScalarExpr::Compare {
            left,
            op: asap_types::ir::scalar::CompareOpKind::Eq,
            right,
            ..
        } if matches!(
            (left.as_ref(), right.as_ref()),
            (ScalarExpr::Column(0), ScalarExpr::Column(2))
                | (ScalarExpr::Column(2), ScalarExpr::Column(0))
        )
    ));
    assert_eq!(
        join.schema
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>(),
        vec!["service", "v", "service", "v"]
    );
    for child in [left, right] {
        let Some(NonASAPOp::Project {
            child: aggregate, ..
        }) = child.non_asap()
        else {
            panic!(
                "derived table Project was not retained: {:?}",
                child.operator
            );
        };
        let Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: aggregate }) =
            &aggregate.operator
        else {
            panic!("derived table Project must consume finalized exact values");
        };
        assert!(matches!(
            aggregate.operator,
            Operator::ASAP(ASAPOp::SummaryAgg {
                family: FieldDataType::ExactAggregate(ExactKind::Rate, ExactParams::Rate),
                ..
            })
        ));
    }
    assert!(join
        .guarantee
        .as_ref()
        .is_some_and(|value| value.is_exact()));
    let dag = physical_asap_dag(&root);
    let join_id = dag
        .nodes
        .iter()
        .find(|node| {
            matches!(
                node.payload,
                PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Join { .. })
            )
        })
        .expect("relational join node")
        .id;
    let roles = dag
        .edges
        .iter()
        .filter(|edge| edge.consumer == join_id)
        .map(|edge| edge.role)
        .collect::<Vec<_>>();
    assert_eq!(roles, vec![EdgeRole::Left, EdgeRole::Right]);
}

#[tokio::test]
async fn sql_relational_parents_retain_summary_bound_aggregate() {
    let pre_asap = lower(
        "SELECT t.service, t.p FROM \
         (SELECT service, approx_percentile_cont(latency, 0.9) AS p \
          FROM metrics GROUP BY service) t \
         WHERE t.p > 100 ORDER BY t.p DESC LIMIT 5",
        AccuracyTarget::max_error(0.01),
    )
    .await;
    let root = realize(&pre_asap, AccuracyTarget::max_error(0.01));

    let mut node = root.as_ref();
    let mut saw_project = false;
    let mut saw_filter = false;
    let mut saw_sort = false;
    let mut saw_limit = false;
    loop {
        if let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &node.operator {
            assert!(matches!(
                summary_input.operator,
                Operator::ASAP(ASAPOp::SummaryAgg { .. })
            ));
            break;
        }
        match node.non_asap() {
            Some(NonASAPOp::Project { .. }) => saw_project = true,
            Some(NonASAPOp::Filter { .. }) => saw_filter = true,
            Some(NonASAPOp::Sort { .. }) => saw_sort = true,
            Some(NonASAPOp::Limit { n, offset, .. }) => {
                assert_eq!((*n, *offset), (Some(5), 0));
                saw_limit = true;
            }
            _ => {}
        }
        node = unary_child(node).unwrap_or_else(|| {
            panic!(
                "expected relational parents over SummaryEstimate, got {:?}",
                node.operator
            )
        });
    }
    assert!(saw_project && saw_filter && saw_sort && saw_limit);
}

/// A post-aggregate predicate is read-time work, while a source predicate is
/// part of the rows that populate the maintained summary. Neither predicate
/// may be dropped or moved across the aggregation boundary.
#[tokio::test]
async fn sql_filter_keeps_read_predicate_and_summary_population_selection() {
    let pre_asap = lower(
        "SELECT t.service, t.p FROM \
         (SELECT service, approx_percentile_cont(latency, 0.9) AS p \
          FROM metrics WHERE service = 'api' GROUP BY service) t \
         WHERE t.p > 100",
        AccuracyTarget::max_error(0.01),
    )
    .await;
    let expected_read_predicate = {
        let mut node = &pre_asap;
        loop {
            match node.non_asap() {
                Some(NonASAPOp::Filter { pred, .. }) => break pred.clone(),
                Some(
                    NonASAPOp::Project { child, .. }
                    | NonASAPOp::Sort { child, .. }
                    | NonASAPOp::Limit { child, .. },
                ) => node = child,
                _ => panic!(
                    "expected a Filter above the aggregate, got {:?}",
                    node.operator
                ),
            }
        }
    };
    let expected_source_predicates = {
        let mut node = &pre_asap;
        loop {
            match node.non_asap() {
                Some(NonASAPOp::Scan { predicates, .. }) => break predicates.clone(),
                Some(
                    NonASAPOp::Project { child, .. }
                    | NonASAPOp::Filter { child, .. }
                    | NonASAPOp::Aggregate { child, .. }
                    | NonASAPOp::Sort { child, .. }
                    | NonASAPOp::Limit { child, .. },
                ) => node = child,
                _ => panic!(
                    "expected a unary SQL plan over Scan, got {:?}",
                    node.operator
                ),
            }
        }
    };
    assert_eq!(expected_source_predicates.len(), 1, "fixture source WHERE");

    let root = realize(&pre_asap, AccuracyTarget::max_error(0.01));

    let mut node = root.as_ref();
    let mut retained_read_predicate = None;
    loop {
        if let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) = &node.operator {
            let Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) = &summary_input.operator else {
                panic!("expected SummaryAgg below SummaryEstimate");
            };
            assert!(
                is_kept_non_asap(child),
                "expected raw summary population below SummaryAgg"
            );
            let Some(NonASAPOp::Scan { predicates, .. }) = child.non_asap() else {
                panic!("expected source selection to remain a Scan");
            };
            assert_eq!(predicates, &expected_source_predicates);
            break;
        }
        if let Some(NonASAPOp::Filter { pred, .. }) = node.non_asap() {
            retained_read_predicate = Some(pred.clone());
        }
        node = unary_child(node).unwrap_or_else(|| {
            panic!(
                "expected read-time operations over a summary, got {:?}",
                node.operator
            )
        });
    }
    assert_eq!(retained_read_predicate, Some(expected_read_predicate));

    let dag = physical_asap_dag(&root);
    let expected_wire = wire_pred(retained_read_predicate.as_ref().unwrap());
    assert!(dag.nodes.iter().any(|node| matches!(
        &node.payload,
        PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Filter { pred, .. }) if *pred == expected_wire
    )));
}

#[tokio::test]
async fn sql_quantile_binds_kll_sketch_over_named_column() {
    let pre_asap = lower(
        "SELECT approx_percentile_cont(latency, 0.99) FROM metrics",
        AccuracyTarget::max_error(0.01),
    )
    .await;
    let planned = realize(&pre_asap, AccuracyTarget::max_error(0.01));
    let root = below_projects(&planned);

    let Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input,
        query,
    }) = &root.operator
    else {
        panic!("expected SummaryEstimate root, got {:?}", root.operator);
    };
    assert!(matches!(query, SketchStatistic::Quantile { q } if *q == 0.99));
    assert_eq!(
        root.schema.fields.len(),
        1,
        "no GROUP BY — a single output column"
    );
    assert_eq!(
        root.schema.fields[0].dtype,
        FieldDataType::Plain(DataType::Float64),
        "the summary-state type must not propagate past the estimate"
    );

    let Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        family,
        input,
        reduction,
        ..
    }) = &summary_input.operator
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
        FieldDataType::Sketch(
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

    assert!(
        is_kept_non_asap(child),
        "expected a kept non-ASAP leaf, got {:?}",
        child.operator
    );
    assert!(matches!(child.non_asap(), Some(NonASAPOp::Scan { .. })));
    assert!(
        child
            .schema
            .fields
            .iter()
            .all(|f| matches!(f.dtype, FieldDataType::Plain(_))),
        "logical edges carry only plain columns"
    );
}

/// `SELECT COUNT(DISTINCT service) FROM metrics` at ε = 0.01 lowers to
/// `AggIntent::Cardinality` (`sql_lowering.rs::count_distinct_is_cardinality`)
/// — unlike `Quantile`/`Count`/`TopK`, its preferred candidate is HLL, not
/// KLL/CMS (`realization::summary_candidates`), so this exercises a
/// distinct branch of the sketch-vs-exact decision than the quantile test
/// above. HLL is sized from ε and the default δ = 0.01 (#694): `m ≥
/// 1/(2ε²δ)` reaches precision 18, the largest.
#[tokio::test]
async fn sql_count_distinct_with_epsilon_binds_hll_over_named_column() {
    let pre_asap = lower(
        "SELECT COUNT(DISTINCT service) FROM metrics",
        AccuracyTarget::max_error(0.01),
    )
    .await;
    let planned = realize(&pre_asap, AccuracyTarget::max_error(0.01));
    let root = below_projects(&planned);

    let Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input,
        query,
    }) = &root.operator
    else {
        panic!("expected SummaryEstimate root, got {:?}", root.operator);
    };
    assert!(matches!(query, SketchStatistic::Cardinality));
    assert_eq!(
        root.schema.fields[0].dtype,
        FieldDataType::Plain(DataType::Int64),
        "COUNT(DISTINCT …) reads back out as an integer count"
    );

    let Operator::ASAP(ASAPOp::SummaryAgg {
        family,
        input,
        reduction,
        ..
    }) = &summary_input.operator
    else {
        panic!("expected SummaryAgg, got {:?}", summary_input.operator);
    };
    assert_eq!(
        family,
        &FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::Hll,
                SketchParams::Hll {
                    precision: 18,
                    relative_error: Some(0.01)
                }
            ),
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

#[tokio::test]
async fn map_projection_export_keeps_the_map_over_an_avg_of_exact_states() {
    let pre = lower_sql_dialect(
        "SELECT map('job', t.service) AS labels, t.avg_bytes FROM (SELECT service, AVG(bytes) AS avg_bytes FROM metrics GROUP BY service) t WHERE t.avg_bytes > 100",
        &catalog(), SqlDialect::ClickhouseSQL, AccuracyTarget::exact(),
    ).await.unwrap();
    let root = realize(&pre, AccuracyTarget::exact());
    let dag = physical_asap_dag(&root);
    assert!(dag.nodes.iter().any(|node| matches!(&node.payload,
        PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Project { cols, .. })
        if cols.iter().any(|item| matches!(&item.expr, ScalarExpr::FunctionCall { name, .. } if name == "map"))
    )));
    // The AVG under the map projection is built from exact Sum and Count
    // states, divided at read time.
    let families: Vec<_> = dag
        .nodes
        .iter()
        .filter_map(|node| match &node.payload {
            PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg { family, .. }) => Some(family),
            _ => None,
        })
        .collect();
    assert_eq!(
        families,
        vec![
            &FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            &FieldDataType::ExactAggregate(ExactKind::Count, ExactParams::Count),
        ]
    );
}
