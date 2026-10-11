//! Query string → planned summary state → `coverage()`, for the examples of
//! the ASAP primitive schema design doc (#573 §4.2.2). Each query is lowered,
//! searched and selected with the default cost model, and the quantile's KLL
//! `SummaryAgg` in the selected DAG is checked.

use std::ops::Bound;
use std::rc::Rc;

use asap_aware_mapping::plan_selection::candidate_selection::global_selection;
use asap_aware_mapping::DefaultCostModel;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_integration_tests::fixtures::lower_promql;
use asap_logical_optimizer::search_workload;
use asap_types::ir::properties::summary_coverage::{ColumnIdentity, Constraint, SelectionBox};
use asap_types::ir::scalar::ScalarValue;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema, SketchAlgorithm};
use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode, Predicate, ScalarExpr};
use asap_types::types::AccuracyTarget;

fn accuracy() -> AccuracyTarget {
    AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.01,
    }
}

fn plan(pre: Rc<OperatorNode>) -> Rc<OperatorNode> {
    let space = search_workload(vec![("query", pre)]);
    global_selection(&space, &DefaultCostModel)
        .assemble_selected_dag(&space.roots[0].1)
        .expect("materialization failed")
        .expect("root must be discovered")
}

/// The single KLL `SummaryAgg` of the selected DAG.
fn kll_state(root: &Rc<OperatorNode>) -> Rc<OperatorNode> {
    let states: Vec<_> = OperatorNode::reachable(root)
        .into_iter()
        .filter(|node| {
            matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. })
                if kind.algorithm() == &SketchAlgorithm::Kll)
        })
        .collect();
    assert_eq!(states.len(), 1, "expected one KLL state in {root:#?}");
    states[0].clone()
}

async fn sql_state(sql: &str) -> Rc<OperatorNode> {
    let catalog = SqlCatalog::new().with_table(
        "t",
        Schema::new(vec![
            Field::plain("job", DataType::Utf8, false),
            Field::plain("region", DataType::Utf8, false),
            Field::plain("latency", DataType::Float64, false),
        ]),
    );
    let pre = lower_sql(sql, &catalog, accuracy())
        .await
        .unwrap_or_else(|e| panic!("lower failed for {sql:?}: {e}"));
    kll_state(&plan(pre))
}

fn promql_state(query: &str) -> Rc<OperatorNode> {
    kll_state(&plan(
        lower_promql(query, accuracy()).expect("lowering failed"),
    ))
}

fn column(table: Option<&str>, name: &str) -> ColumnIdentity {
    ColumnIdentity {
        table: table.map(str::to_string),
        name: name.to_string(),
    }
}

fn utf8(value: &str) -> ScalarValue {
    ScalarValue::Utf8(value.to_string())
}

/// The child of the definition's `SummaryAgg`.
fn definition_child(state: &OperatorNode) -> Rc<OperatorNode> {
    state.coverage().unwrap().definition.children()[0].clone()
}

#[tokio::test]
async fn sql_where_value_set_moves_to_selection() {
    let state =
        sql_state("SELECT approx_percentile_cont(latency, 0.99) FROM t WHERE region = 'us'").await;
    let coverage = state.coverage().unwrap();
    assert_eq!(
        coverage.selection,
        vec![SelectionBox {
            columns: [(
                column(Some("t"), "region"),
                Constraint::In(vec![utf8("us")])
            )]
            .into(),
            relative_time: None,
        }]
    );
    let child = definition_child(&state);
    assert!(
        matches!(child.non_asap(), Some(NonASAPOp::Scan { predicates, .. }) if predicates.is_empty())
    );
}

#[tokio::test]
async fn sql_where_range_moves_to_selection() {
    let state =
        sql_state("SELECT approx_percentile_cont(latency, 0.99) FROM t WHERE latency < 100").await;
    let constraint = &state.coverage().unwrap().selection[0].columns[&column(Some("t"), "latency")];
    assert!(
        matches!(
            constraint,
            Constraint::Interval {
                lower: Bound::Unbounded,
                upper: Bound::Excluded(ScalarValue::Float64(100.0)),
            }
        ),
        "{constraint:?}"
    );
}

/// The worked example without its `FILTER` clause: the rename carries
/// `region = 'us'` into the selection as `r`, and the expression condition
/// stays in the definition's scan.
#[tokio::test]
async fn sql_renamed_column_moves_and_expression_stays() {
    let state = sql_state(
        "SELECT job, approx_percentile_cont(latency, 0.99) \
         FROM (SELECT job, region AS r, latency FROM t \
               WHERE region = 'us' AND latency * 2 > 10) \
         GROUP BY job",
    )
    .await;
    assert_eq!(
        state.coverage().unwrap().selection,
        vec![SelectionBox {
            columns: [(column(None, "r"), Constraint::In(vec![utf8("us")]))].into(),
            relative_time: None,
        }]
    );
    let project = definition_child(&state);
    assert!(matches!(
        project.non_asap(),
        Some(NonASAPOp::Project { .. })
    ));
    let scan = project.children()[0].clone();
    let Some(NonASAPOp::Scan { predicates, .. }) = scan.non_asap() else {
        panic!("expected a scan, got {scan:#?}");
    };
    assert_eq!(predicates.len(), 1, "{predicates:#?}");
    assert!(
        matches!(&predicates[0], Predicate(ScalarExpr::Compare { left, .. })
            if matches!(**left, ScalarExpr::Arithmetic { .. })),
        "the expression condition should stay: {predicates:#?}"
    );
}

#[test]
fn promql_offset_window_is_relative_time() {
    let state = promql_state("quantile_over_time(0.99, m[1m] offset 2m)");
    assert_eq!(
        state.coverage().unwrap().selection,
        vec![SelectionBox {
            columns: Default::default(),
            relative_time: Some((Bound::Excluded(-180_000), Bound::Included(-120_000))),
        }]
    );
    assert!(matches!(
        definition_child(&state).non_asap(),
        Some(NonASAPOp::Scan { .. })
    ));
}

/// The label matcher is below `rate`, which the walk does not pass.
#[test]
fn promql_matcher_below_rate_stays_in_definition() {
    let state = promql_state("quantile(0.99, rate(m{job=\"api\"}[5m]))");
    assert_eq!(
        state.coverage().unwrap().selection,
        vec![SelectionBox::default()]
    );
}

/// A PromQL comparison lowers to a `Filter` above `rate`, so it picks the
/// rate outputs the state reads.
#[test]
fn promql_comparison_above_rate_moves_to_selection() {
    let state = promql_state("quantile(0.99, rate(m[5m]) > 0)");
    assert_eq!(
        state.coverage().unwrap().selection,
        vec![SelectionBox {
            columns: [(
                column(None, "value"),
                Constraint::Interval {
                    lower: Bound::Excluded(ScalarValue::Float64(0.0)),
                    upper: Bound::Unbounded,
                },
            )]
            .into(),
            relative_time: None,
        }]
    );
}
