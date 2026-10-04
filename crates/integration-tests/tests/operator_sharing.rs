//! Acceptance tests for operator sharing (issue #468): one operator IR
//! before and after ASAP optimization, so non-ASAP operators sit both above
//! and below summary operators, can share inputs with them, and can carry
//! summaries below set operators.
//!
//! Each test drives SQL text through `lower_sql` → `search_workload` →
//! global selection → `assemble_selected_dag`, the pipeline
//! `sql_to_post_asap.rs` uses.

use std::rc::Rc;

use asap_aware_mapping::plan_selection::candidate_selection::global_selection;
use asap_aware_mapping::DefaultCostModel;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_integration_tests::post_asap::post_asap_dag;
use asap_logical_optimizer::search_workload;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode};
use asap_types::types::AccuracyTarget;

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

/// Lower `sql`, search, select with the default cost model and assemble the
/// selected post-ASAP DAG.
async fn plan(sql: &str, accuracy: AccuracyTarget) -> Rc<OperatorNode> {
    let pre = lower_sql(sql, &catalog(), accuracy)
        .await
        .unwrap_or_else(|e| panic!("lower failed for {sql:?}: {e}"));
    let space = search_workload(vec![("query", pre)]);
    let selection = global_selection(&space, &DefaultCostModel);
    selection
        .assemble_selected_dag(&space.roots[0].1)
        .expect("materialization failed")
        .expect("root must be discovered")
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

fn is_summary_evaluation(node: &OperatorNode) -> bool {
    matches!(
        node.operator,
        Operator::ASAP(
            ASAPOp::SummaryEstimate { .. }
                | ASAPOp::FinalizeExactAccumulator { .. }
                | ASAPOp::EvaluatePopulation { .. }
        )
    )
}

fn is_scan(node: &OperatorNode) -> bool {
    matches!(node.non_asap(), Some(NonASAPOp::Scan { .. }))
}

/// The first node reached through single-input non-ASAP operators below
/// `node` (inclusive) that is not one: where an operator chain meets a
/// summary or a multi-input operator.
fn through_unary_non_asap(node: &Rc<OperatorNode>) -> &Rc<OperatorNode> {
    match node.non_asap().map(|op| op.children()) {
        Some(children) if children.len() == 1 => through_unary_non_asap(children[0]),
        _ => node,
    }
}

// #468 problem 1: the Project above the summary evaluation and the Scan below
// it are both plain NonASAP nodes (no post-ASAP-only wrapper variant).
#[ignore = "planner chooses no summary here: Avg has no summary realization, so the Aggregate stays a logical pass-through"]
#[tokio::test]
async fn project_above_and_scan_below_a_summary_are_both_non_asap_nodes() {
    let root = plan(
        "WITH metric AS (SELECT avg(CASE WHEN l_quantity BETWEEN 1 AND 50 THEN 1.0 ELSE 0.0 END) \
         AS in_range FROM lineitem) SELECT in_range, in_range = 1.0 AS ok FROM metric",
        AccuracyTarget::Exact,
    )
    .await;
    // The root is the outer SELECT list: a NonASAP Project.
    assert!(
        matches!(root.operator, Operator::NonASAP(NonASAPOp::Project { .. })),
        "root must be the outer Project, got {:?}",
        root.operator
    );
    // A summary evaluation sits below the Project chain.
    let evaluation = through_unary_non_asap(&root);
    assert!(
        is_summary_evaluation(evaluation),
        "the Project chain must read a summary, got {:?}",
        evaluation.operator
    );
    // Below the summary the Scan is the same NonASAP operator a front end emits.
    let scans = find_all(evaluation, is_scan);
    assert_eq!(scans.len(), 1, "one lineitem Scan below the summary");
    assert!(!scans[0].is_asap());
    // The flat plan exports (time first, wire 6).
    post_asap_dag(&root);
}

// #468 problem 2: the exact aggregate and the sketch read one shared Scan
// (`Rc::ptr_eq`), not two copies.
#[ignore = "waits for the binding rule splitting multi-measure aggregates"]
#[tokio::test]
async fn exact_aggregate_and_sketch_share_one_scan() {
    let root = plan(
        "SELECT avg(l_extendedprice), approx_percentile_cont(l_discount, 0.99) FROM lineitem",
        AccuracyTarget::Epsilon(0.01),
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
async fn each_side_of_union_all_holds_a_summary_estimate() {
    let root = plan(
        "SELECT approx_distinct(l_partkey) FROM lineitem \
         UNION ALL SELECT approx_distinct(l_suppkey) FROM lineitem",
        AccuracyTarget::Epsilon(0.01),
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
    post_asap_dag(&root);
}
