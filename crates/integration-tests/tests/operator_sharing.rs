//! Acceptance tests for operator sharing (issue #468): one operator IR
//! before and after ASAP optimization, so non-ASAP operators sit both above
//! and below summary operators, can share inputs with them, and can carry
//! summaries below set operators.
//!
//! Each test drives SQL text through `lower_sql` and the plan the planner
//! pipeline selects (`run_stage1_to_stage3`).

use std::rc::Rc;

use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_integration_tests::physical_export::physical_asap_dag;
use asap_integration_tests::planning::selected_dag;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, OperatorNode};

fn col(name: &str, dtype: DataType) -> Field {
    Field::plain(name, dtype, false)
}

/// TPC-H `lineitem`, as `frontend-sql/tests/data_quality_check/tpch_deequ.rs`
/// declares it (DECIMAL columns as `Float64`, no time index, no keys).
fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "lineitem",
        Schema::new(vec![
            col("l_orderkey", DataType::Int64),
            col("l_partkey", DataType::Int64),
            col("l_suppkey", DataType::Int64),
            col("l_linenumber", DataType::Int64),
            col("l_quantity", DataType::Float64),
            col("l_extendedprice", DataType::Float64),
            col("l_discount", DataType::Float64),
            col("l_tax", DataType::Float64),
            col("l_returnflag", DataType::Utf8),
            col("l_linestatus", DataType::Utf8),
            col("l_shipdate", DataType::Date),
            col("l_commitdate", DataType::Date),
            col("l_receiptdate", DataType::Date),
            col("l_shipinstruct", DataType::Utf8),
            col("l_shipmode", DataType::Utf8),
            col("l_comment", DataType::Utf8),
        ]),
    )
}

/// Lower `sql` and take the logical DAG the planner pipeline selects.
async fn plan(sql: &str, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    let pre = lower_sql(sql, &catalog(), accuracy.clone())
        .await
        .unwrap_or_else(|e| panic!("lower failed for {sql:?}: {e}"));
    selected_dag(pre, accuracy)
}

/// Every unique node reachable from `root` whose operator matches `pred`.
fn find_all(
    root: &Rc<OperatorNode>,
    pred: impl Fn(&OperatorNode) -> bool,
) -> Vec<Rc<OperatorNode>> {
    OperatorNode::reachable(root)
        .into_iter()
        .filter(|node| pred(node))
        .collect()
}

fn is_scan(node: &OperatorNode) -> bool {
    matches!(node.non_asap(), Some(NonASAPOp::Scan { .. }))
}

// #468 problem 2: the exact aggregate and the sketch read one shared Scan
// (`Rc::ptr_eq`), not two copies.
#[ignore = "waits for the binding rule splitting multi-measure aggregates"]
#[tokio::test]
async fn exact_aggregate_and_sketch_share_one_scan() {
    let root = plan(
        "SELECT avg(l_extendedprice), approx_percentile_cont(l_discount, 0.99) FROM lineitem",
        AccuracyTarget::max_error(0.01),
    )
    .await;
    let exact = find_all(&root, |node| {
        matches!(node.non_asap(), Some(NonASAPOp::Aggregate { .. }))
    });
    let sketch = find_all(&root, |node| {
        matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. }))
    });
    assert_eq!(exact.len(), 1, "one exact Aggregate for avg: {root:?}");
    assert_eq!(
        sketch.len(),
        1,
        "one sketch SummaryAgg for the percentile: {root:?}"
    );
    let scan_under = |node: &Rc<OperatorNode>| {
        let scans = find_all(node, is_scan);
        assert_eq!(scans.len(), 1, "one Scan under {:?}", node.operator);
        Rc::clone(&scans[0])
    };
    assert!(
        Rc::ptr_eq(&scan_under(&exact[0]), &scan_under(&sketch[0])),
        "the exact aggregate and the sketch must read one shared Scan"
    );
}

// #468 problem 3: a summary can sit below a set operator — each side of the
// UNION ALL holds its own SummaryEstimate.
#[tokio::test]
#[ignore = "plan selection picks the exact plan: a query-time summary on each side never costs less than the exact aggregate until Pass 2 shares it (#580)"]
async fn each_side_of_union_all_holds_a_summary_estimate() {
    let root = plan(
        "SELECT approx_distinct(l_partkey) FROM lineitem \
         UNION ALL SELECT approx_distinct(l_suppkey) FROM lineitem",
        AccuracyTarget::max_error(0.01),
    )
    .await;
    // The SQL front end lowers UNION ALL to `SetOp { all: true }`.
    let Some(NonASAPOp::SetOp {
        all: true,
        left,
        right,
        ..
    }) = root.non_asap()
    else {
        panic!("root must be the UNION ALL SetOp, got {:?}", root.operator)
    };
    for side in [left, right] {
        let estimates = find_all(side, |node| {
            matches!(node.asap(), Some(ASAPOp::SummaryEstimate { .. }))
        });
        assert!(
            !estimates.is_empty(),
            "UNION ALL side has no SummaryEstimate: {:?}",
            side.operator
        );
    }
    physical_asap_dag(&root);
}
