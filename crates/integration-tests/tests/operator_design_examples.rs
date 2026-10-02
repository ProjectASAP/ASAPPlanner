//! #511 examples: source text → unified graph → summary rewrite → flat export.
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, ScalarExpr};
use asap_types::post_asap::{
    ExactKind, ExactParams, FieldDataType, GroupingStrategy, SummaryUpdate,
};
use asap_types::pre_asap::{AggIntent, ColumnRef, DataType, Field, Schema};
use asap_types::types::AccuracyTarget;
use std::rc::Rc;
mod physical_common;

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
        AccuracyTarget::Exact,
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
    let wire = physical_common::compile_post_asap_dag(&root).unwrap();
    wire.validate().unwrap();
    assert_eq!(wire.nodes.len(), OperatorNode::reachable(&root).len());
}

/// SUM's readout preserves integer type and SQL NULL behavior across the rewrite.
#[tokio::test]
async fn sql_sum_projection_before_and_after_summary_rewrite() {
    let root = lower_sql(
        "SELECT SUM(bytes) + 1 AS total_bytes FROM requests WHERE status = 200",
        &catalog(),
        AccuracyTarget::Exact,
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
            let finalize = OperatorNode::asap_node(
                ASAPOp::FinalizeExactAccumulator { child: state },
                node.schema.clone(),
                None,
            );
            return finalize;
        }
        Rc::new(node.map_children(rewrite).unwrap())
    }
    let rewritten = rewrite(&root);
    rewritten.validate_structure().unwrap();
    assert_eq!(rewritten.schema, root.schema);
    let graph = OperatorNode::reachable(&rewritten);
    assert!(graph
        .iter()
        .any(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. }))));
    assert!(graph
        .iter()
        .any(|n| matches!(n.asap(), Some(ASAPOp::FinalizeExactAccumulator { .. }))));
    let wire = physical_common::compile_post_asap_dag(&rewritten).unwrap();
    wire.validate().unwrap();
    assert_eq!(wire.nodes.len(), graph.len());
    let json = serde_json::to_string(&wire).unwrap();
    assert!(!json.contains("KeepPreAsap") && !json.contains("ScalarBridge"));
}

/// Scalar subqueries survive normalization with shared, visible producers.
#[tokio::test]
async fn sql_scalar_subquery_retains_its_cardinality_contract() {
    for query in [
        "SELECT (SELECT bytes FROM requests) AS v FROM lineitem",
        "SELECT l_quantity NOT IN (SELECT bytes FROM requests) AS present FROM lineitem",
    ] {
        let root = lower_sql(query, &catalog(), AccuracyTarget::Exact)
            .await
            .unwrap();
        root.validate_structure().unwrap();
        assert!(root.children().len() > 1);
        let wire = physical_common::compile_post_asap_dag(&root).unwrap();
        assert!(wire
            .edges
            .iter()
            .any(|e| e.role == asap_types::ir::export::EdgeRole::ScalarRef));
    }
}

/// Execute the SQL SUM example for nonempty, empty and all-NULL populations.
#[tokio::test]
async fn sql_sum_example_executes_with_sql_null_semantics() {
    use asap_physical_operators::{
        physical_planner::{compile, InputContract, Source},
        runtime::{Limits, RunContext, Scope},
        sources::{DataSources, MemorySource},
        values::{Batch, Value},
    };
    use futures::StreamExt;
    use std::{collections::BTreeMap, sync::Arc};
    let root = lower_sql(
        "SELECT SUM(bytes) + 1 AS total_bytes FROM requests WHERE status = 200",
        &catalog(),
        AccuracyTarget::Exact,
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
    let wire = physical_common::compile_post_asap_dag(&root).unwrap();
    let scan = wire
        .nodes
        .iter()
        .find(|node| {
            matches!(
                &node.payload,
                asap_types::ir::export::PostAsapOperatorPayload::Relational {
                    operator: asap_types::ir::export::NonASAPOpKind::Scan { .. }
                }
            )
        })
        .unwrap();
    let schema = Arc::new(scan.output_schema.clone());
    let plan = compile(
        &wire,
        BTreeMap::from([(u64::from(scan.id.0), InputContract::bounded(schema.clone()))]),
        &[u64::from(wire.root.0)],
    )
    .unwrap();
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
        let mut sources = DataSources::default();
        sources
            .register(
                source.clone(),
                Arc::new(
                    MemorySource::new(
                        schema.clone(),
                        vec![Batch::try_new(schema.clone(), rows).unwrap()],
                    )
                    .unwrap(),
                ),
            )
            .unwrap();
        let bound = plan
            .instantiate(BTreeMap::from([(
                u64::from(scan.id.0),
                Box::new(sources.bind(&logical_scan).unwrap()) as Source<'_>,
            )]))
            .unwrap();
        let mut stream = bound
            .execute(
                plan.roots(),
                RunContext::new(
                    Scope::Query {
                        evaluation_time_ms: 300_000,
                        revision: 1,
                    },
                    Limits::default(),
                )
                .unwrap(),
            )
            .unwrap()
            .remove(0);
        let mut rows = vec![];
        while let Some(batch) = stream.next().await {
            rows.extend(batch.unwrap().rows().iter().cloned());
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
