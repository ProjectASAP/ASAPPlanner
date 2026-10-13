//! Query string → planned summary state → `coverage()`, for the examples of
//! the ASAP primitive schema design doc (#573 §4.2.2). Each query is lowered
//! and searched, and the quantile's KLL `SummaryAgg` candidate in the search
//! space is checked: what its `definition` keeps and what its `selection`
//! takes, row by row of §4.2.2's "More examples". The tests read the
//! candidate list, not a cost model's selection, so they do not depend on
//! which candidate a cost model prefers.

use std::ops::Bound;
use std::rc::Rc;

use asap_aware_mapping::replacement::Replacement;
use asap_aware_mapping::search_workload;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_integration_tests::fixtures::lower_promql;
use asap_types::ir::summary_coverage::{ColumnIdentity, Constraint, SelectionBox};
use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode, Predicate, ScalarExpr};
use asap_types::post_asap::{FieldDataType, SketchAlgorithm};
use asap_types::pre_asap::expr_ir::ScalarValue;
use asap_types::pre_asap::schema::{DataType, Field, Schema};
use asap_types::types::AccuracyTarget;

fn accuracy() -> AccuracyTarget {
    AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.01,
    }
}

/// The single KLL `SummaryAgg` among the search space's candidates for `pre`.
fn kll_state(pre: Rc<OperatorNode>) -> Rc<OperatorNode> {
    let space = search_workload(vec![("query", pre)]).unwrap();
    let mut states: Vec<Rc<OperatorNode>> = Vec::new();
    for group in space.target_subdag_candidates() {
        for candidate in &group.candidates {
            let Replacement::SubDAG(dag) = &candidate.replacement else {
                continue;
            };
            for node in OperatorNode::reachable(dag) {
                let is_kll = matches!(node.asap(), Some(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. })
                    if kind.algorithm() == &SketchAlgorithm::Kll);
                if is_kll && !states.iter().any(|s| Rc::ptr_eq(s, &node)) {
                    states.push(node);
                }
            }
        }
    }
    assert_eq!(
        states.len(),
        1,
        "expected one KLL candidate state: {states:#?}"
    );
    states.pop().unwrap()
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
    kll_state(pre)
}

fn promql_state(query: &str) -> Rc<OperatorNode> {
    kll_state(lower_promql(query, accuracy()).expect("lowering failed"))
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

/// The definition's `SummaryAgg` carries no filter of its own.
fn assert_definition_unfiltered(state: &OperatorNode) {
    let definition = state.coverage().unwrap().definition;
    assert!(
        matches!(
            definition.asap(),
            Some(ASAPOp::SummaryAgg { filter: None, .. })
        ),
        "{definition:#?}"
    );
}

/// The definition keeps the whole sub-DAG below the state: nothing moved
/// into the selection, which is the one unrestricted box.
fn assert_nothing_selected(state: &OperatorNode) {
    assert_eq!(
        state.coverage().unwrap().selection,
        vec![SelectionBox::default()]
    );
    assert!(Rc::ptr_eq(&definition_child(state), state.children()[0]));
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
    assert_definition_unfiltered(&state);
    let child = definition_child(&state);
    assert!(
        matches!(child.non_asap(), Some(NonASAPOp::Scan { predicates, .. }) if predicates.is_empty())
    );
}

#[tokio::test]
async fn sql_where_range_moves_to_selection() {
    let state =
        sql_state("SELECT approx_percentile_cont(latency, 0.99) FROM t WHERE latency < 100").await;
    assert_eq!(
        state.coverage().unwrap().selection,
        vec![SelectionBox {
            columns: [(
                column(Some("t"), "latency"),
                Constraint::Interval {
                    lower: Bound::Unbounded,
                    upper: Bound::Excluded(ScalarValue::Float64(100.0)),
                }
            )]
            .into(),
            relative_time: None,
        }]
    );
    assert_definition_unfiltered(&state);
    let child = definition_child(&state);
    assert!(
        matches!(child.non_asap(), Some(NonASAPOp::Scan { predicates, .. }) if predicates.is_empty())
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
    assert_definition_unfiltered(&state);
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

/// The label matcher is below `rate`, which the walk does not pass: the
/// definition keeps all of it, the matcher included.
#[test]
fn promql_matcher_below_rate_stays_in_definition() {
    let state = promql_state("quantile(0.99, rate(m{job=\"api\"}[5m]))");
    assert_nothing_selected(&state);
    let scans: Vec<_> = OperatorNode::reachable(&definition_child(&state))
        .into_iter()
        .filter_map(|node| match node.non_asap() {
            Some(NonASAPOp::Scan { predicates, .. }) => Some(predicates.len()),
            _ => None,
        })
        .collect();
    assert_eq!(scans, vec![1], "the matcher stays in the scan");
}

/// A PromQL comparison lowers to a `Filter` above `rate`, so it picks the
/// rate outputs the state reads; the definition keeps the `rate` below it.
#[test]
fn promql_comparison_above_rate_moves_to_selection() {
    let state = promql_state("quantile(0.99, rate(m[5m]) > 0)");
    let filter = state.children()[0];
    assert!(matches!(filter.non_asap(), Some(NonASAPOp::Filter { .. })));
    assert!(Rc::ptr_eq(&definition_child(&state), filter.children()[0]));
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

/// A vector/vector comparison is a `BinaryOp`, which the walk does not enter.
#[test]
fn promql_vector_comparison_stays_in_definition() {
    let state = promql_state("quantile(0.99, rate(a[5m]) > rate(b[5m]))");
    assert_nothing_selected(&state);
    assert!(matches!(
        definition_child(&state).non_asap(),
        Some(NonASAPOp::BinaryOp { .. })
    ));
}

/// A `bool` comparison lowers to a `Project` (values become 0 or 1), which
/// drops no row, so it stays in the definition.
#[test]
fn promql_bool_comparison_stays_in_definition() {
    let state = promql_state("quantile(0.99, rate(m[5m]) > bool 0)");
    assert_nothing_selected(&state);
    assert!(matches!(
        definition_child(&state).non_asap(),
        Some(NonASAPOp::Project { .. })
    ));
}
