//! #511 examples: source text → unified dag → summary rewrite → flat export.

use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::schema::{
    ExactKind, ExactParams, FieldDataType, GroupingStrategy, SummaryUpdate,
};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, ScalarExpr};
use std::rc::Rc;

fn catalog() -> SqlCatalog {
    SqlCatalog::new()
        .with_table(
            "requests",
            Schema::new(vec![
                Field::plain("bytes", DataType::Int64, true),
                Field::plain("status", DataType::Int64, false),
            ]),
        )
        .with_table(
            "lineitem",
            Schema::new(vec![Field::plain("l_quantity", DataType::Int64, false)]),
        )
}

/// The SQL scalar example keeps column scopes and a Boolean row predicate.
#[tokio::test]
async fn sql_filter_projection_example() {
    let root = lower_sql(
        "SELECT l_quantity * 2 AS q2 FROM lineitem WHERE l_quantity > 10",
        &catalog(),
        AccuracyTarget::exact(),
    )
    .await
    .unwrap();
    root.validate_structure().unwrap();
    assert_eq!(
        root.schema.fields[0],
        Field::plain("q2", DataType::Int64, false)
    );
    assert!(
        matches!(root.expect_non_asap(),NonASAPOp::Project { cols,.. } if matches!(cols[0].expr,ScalarExpr::Arithmetic { .. }))
    );
    let wire = asap_types::test_support::physical_asap_dag_at_query_time(&root).unwrap();
    wire.validate().unwrap();
    assert_eq!(wire.nodes.len(), OperatorNode::reachable(&root).len());
}

/// SUM's evaluation preserves integer type and SQL NULL behavior across the rewrite.
#[tokio::test]
async fn sql_sum_projection_before_and_after_summary_rewrite() {
    let root = lower_sql(
        "SELECT SUM(bytes) + 1 AS total_bytes FROM requests WHERE status = 200",
        &catalog(),
        AccuracyTarget::exact(),
    )
    .await
    .unwrap();
    root.validate_structure().unwrap();
    assert_eq!(
        root.schema.fields[0],
        Field::plain("total_bytes", DataType::Int64, true)
    );
    fn rewrite(node: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        if let Some(NonASAPOp::Aggregate {
            child,
            reduction,
            measures,
            ..
        }) = node.non_asap()
        {
            let [AggIntent::Sum { col: Some(column) }] = measures.as_slice() else {
                panic!()
            };
            let state = Rc::new(
                OperatorNode::new(Operator::ASAP(ASAPOp::SummaryAgg {
                    child: Rc::clone(child),
                    family: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
                    input: SummaryUpdate::column(ColumnRef::Named(
                        child.schema.fields[*column].name.clone(),
                    )),
                    reduction: reduction.clone(),
                    grouping: GroupingStrategy::default(),
                    filter: None,
                }))
                .unwrap(),
            );
            let finalize = std::rc::Rc::new(
                OperatorNode::with_schema(
                    asap_types::ir::Operator::ASAP(ASAPOp::FinalizeExactAccumulator {
                        child: state,
                    }),
                    node.schema.clone(),
                )
                .with_guarantee(None),
            );
            return finalize;
        }
        Rc::new(node.with_new_children(rewrite).unwrap())
    }
    let rewritten = rewrite(&root);
    rewritten.validate_structure().unwrap();
    assert_eq!(rewritten.schema, root.schema);
    let dag = OperatorNode::reachable(&rewritten);
    assert!(dag
        .iter()
        .any(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. }))));
    assert!(dag
        .iter()
        .any(|n| matches!(n.asap(), Some(ASAPOp::FinalizeExactAccumulator { .. }))));
    let wire = asap_types::test_support::physical_asap_dag_at_query_time(&rewritten).unwrap();
    wire.validate().unwrap();
    assert_eq!(wire.nodes.len(), dag.len());
}

/// Scalar subqueries survive normalization with shared, visible producers.
#[tokio::test]
async fn sql_scalar_subquery_retains_its_cardinality_contract() {
    for query in [
        "SELECT (SELECT bytes FROM requests) AS v FROM lineitem",
        "SELECT l_quantity NOT IN (SELECT bytes FROM requests) AS present FROM lineitem",
    ] {
        let root = lower_sql(query, &catalog(), AccuracyTarget::exact())
            .await
            .unwrap();
        root.validate_structure().unwrap();
        assert!(root.children().len() > 1);
        let wire = asap_types::test_support::physical_asap_dag_at_query_time(&root).unwrap();
        assert!(wire
            .edges
            .iter()
            .any(|e| e.role == asap_types::ir::physical_export::EdgeRole::ScalarRef));
    }
}

/// Execute the SQL SUM example for nonempty, empty and all-NULL populations.
#[tokio::test]
async fn sql_sum_example_executes_with_sql_null_semantics() {
    use asap_executor::{
        physical_planner::compile,
        runtime::{Limits, RunContext, Scope},
        sources::InMemoryDataStore,
        values::{Batch, Value},
    };
    use futures::StreamExt;
    use std::sync::Arc;
    let root = lower_sql(
        "SELECT SUM(bytes) + 1 AS total_bytes FROM requests WHERE status = 200",
        &catalog(),
        AccuracyTarget::exact(),
    )
    .await
    .unwrap();
    let logical_scan = OperatorNode::reachable(&root)
        .into_iter()
        .find(|node| matches!(node.non_asap(), Some(NonASAPOp::Scan { .. })))
        .unwrap();
    let NonASAPOp::Scan { source, .. } = logical_scan.expect_non_asap() else {
        panic!()
    };
    let wire = asap_types::test_support::physical_asap_dag_at_query_time(&root).unwrap();
    let scan = wire
        .nodes
        .iter()
        .find(|node| {
            matches!(
                &node.payload,
                asap_types::ir::physical_export::PhysicalASAPOperatorPayload::NonASAP(
                    asap_types::ir::NonASAPOp::Scan { .. }
                )
            )
        })
        .unwrap();
    let schema = Arc::new(scan.output_schema.clone());
    let plan = compile(&wire, &[], &[wire.roots[0] as u64]).unwrap();
    for (rows, expected) in [
        (
            vec![
                vec![Value::Int64(10), Value::Int64(200)],
                vec![Value::Int64(20), Value::Int64(500)],
                vec![Value::Null, Value::Int64(200)],
            ],
            Value::Int64(11),
        ),
        (vec![], Value::Null),
        (vec![vec![Value::Null, Value::Int64(200)]], Value::Null),
    ] {
        let store = InMemoryDataStore::default()
            .with_raw_data(
                source.clone(),
                schema.clone(),
                vec![Batch::try_new(schema.clone(), rows).unwrap()],
            )
            .unwrap();
        let bound = plan.instantiate(Arc::new(store)).unwrap();
        let mut stream = bound
            .execute(
                plan.roots(),
                RunContext::new(
                    Scope::Query {
                        evaluation_time_ms: 300_000,
                    },
                    Limits::default(),
                )
                .unwrap(),
            )
            .unwrap()
            .remove(0);
        let mut rows = vec![];
        while let Some(batch) = stream.next().await {
            rows.extend(batch.unwrap().to_rows().iter().cloned());
        }
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 1);
        match (&rows[0][0], expected) {
            (Value::Null, Value::Null) => {}
            (Value::Int64(actual), Value::Int64(expected)) => assert_eq!(*actual, expected),
            other => panic!("{other:?}"),
        }
    }
}

/// Empty window frames and filtered groups can yield NULL even on non-NULL input.
#[tokio::test]
async fn sql_window_and_filtered_aggregate_types() {
    for query in [
        "SELECT SUM(l_quantity) OVER (ORDER BY l_quantity ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING) AS s FROM lineitem",
        "SELECT MIN(l_quantity) OVER (ORDER BY l_quantity ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING) AS s FROM lineitem",
        "SELECT SUM(l_quantity) FILTER (WHERE l_quantity < 0) AS s FROM lineitem GROUP BY l_quantity",
    ] {
        let root = lower_sql(query, &catalog(), AccuracyTarget::exact()).await.unwrap();
        root.validate_structure().unwrap();
        assert_eq!(root.schema.fields[0], Field::plain("s", DataType::Int64, true), "{query}");
    }
}
