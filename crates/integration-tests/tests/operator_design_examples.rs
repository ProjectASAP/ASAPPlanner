//! #511 examples: source text → unified dag → summary rewrite → flat export.
use crate::physical_common;
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
    let wire = physical_common::compile_physical_asap_dag(&root).unwrap();
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
    let wire = physical_common::compile_physical_asap_dag(&rewritten).unwrap();
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
        let wire = physical_common::compile_physical_asap_dag(&root).unwrap();
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
    let wire = physical_common::compile_physical_asap_dag(&root).unwrap();
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

/// A real query batch replaces each SUM by its exact accumulator state,
/// retains two result roots, and executes both selected plans. Pass 2
/// shares the identical state, and building the shared sub-DAG once per
/// evaluation for both queries (`MatAtQuery`, on the state or on its
/// finalized sum above it) is the cheapest plan, so both roots read one
/// state. No replacement dag is constructed by the test.
#[tokio::test]
async fn batch_planning_replaces_summary_operators() {
    use asap_executor::values::Value;
    use asap_plan_selection::pass::PlanningModels;
    use asap_planner::{e2e_plan, FrontendInput, UserInput};
    use asap_types::workload::*;
    let queries = [
        "SELECT SUM(bytes) + 1 AS result FROM requests",
        "SELECT SUM(bytes) * 2 AS result FROM requests",
    ];
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
            query_batch: Some(
                queries
                    .iter()
                    .map(|query| BatchEntry {
                        query: Query((*query).into()),
                        requirements: QueryRequirements {
                            accuracy: AccuracyTarget::exact(),
                            ..Default::default()
                        },
                        predictability: Predictability::Unknown,
                        invocations: 2,
                        execute_at: None,
                        range_evaluation: None,
                    })
                    .collect(),
            ),
            repeating_queries: None,
        },
        // `requests` at rest, holding a million rows.
        data_workload: Some(DataWorkload {
            arrival: DataArrival::AtRest,
            sources: [(
                "requests".to_string(),
                asap_types::workload::SourceFacts {
                    rows_held: Some(1_000_000),
                    ..Default::default()
                },
            )]
            .into(),
        }),
    };
    let catalog = SqlCatalog::new().with_table(
        "requests",
        Schema::new(vec![Field::plain("bytes", DataType::Float64, false)]),
    );
    let output = e2e_plan(UserInput::new(
        &workload,
        FrontendInput::Sql { catalog: &catalog },
        PlanningModels::builtin(),
    ))
    .await
    .unwrap();
    assert_eq!(output.entry_indices(), [0, 1]);
    assert_eq!(output.roots().len(), 2);
    let states: Vec<_> = output
        .operators()
        .into_iter()
        .filter(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. })))
        .collect();
    let [state] = &states[..] else {
        panic!("one SUM state for both queries: {states:?}")
    };
    assert!(output.operators().into_iter().any(|n| {
        matches!(
            n.materialization,
            asap_types::physical::Materialization::MatAtQuery(_)
        ) && OperatorNode::reachable(&n)
            .iter()
            .any(|m| Rc::ptr_eq(m, state))
    }));
    for (plan, expected) in output.plans.iter().zip([31.0, 60.0]) {
        let root = &plan.root;
        root.validate_structure().unwrap();
        assert!(OperatorNode::reachable(root)
            .iter()
            .any(|n| Rc::ptr_eq(n, state)));
        // The table `requests` bound to its two rows.
        let rows = physical_common::execute_raw_rows(
            root,
            vec![vec![Value::Float64(10.0)], vec![Value::Float64(20.0)]],
        );
        assert_eq!(rows.len(), 1);
        assert!(
            matches!(rows[0][0], Value::Float64(v) if v == expected),
            "{:?}",
            rows
        );
    }
    // The batch exports as one physical DAG: a root per query, the one SUM
    // state, and the shared node the selected plan builds once.
    let workload_dag = output.physical_asap_dag().unwrap();
    assert_eq!(workload_dag.roots.len(), 2);
    assert_ne!(workload_dag.roots[0], workload_dag.roots[1]);
    let states: Vec<_> = workload_dag
        .nodes
        .iter()
        .filter(|n| {
            matches!(
                n.payload,
                asap_types::ir::physical_export::PhysicalASAPOperatorPayload::ASAP(
                    asap_types::ir::ASAPOp::SummaryAgg { .. }
                )
            )
        })
        .collect();
    let [state] = &states[..] else {
        panic!("one SUM state: {states:?}")
    };
    // The node built once is the state or a node above it.
    let reaches_state = |from| {
        let mut stack = vec![from];
        let mut seen = std::collections::HashSet::new();
        while let Some(id) = stack.pop() {
            if id == state.id {
                return true;
            }
            if seen.insert(id) {
                stack.extend(
                    workload_dag
                        .edges
                        .iter()
                        .filter(|e| e.consumer == id)
                        .map(|e| e.producer),
                );
            }
        }
        false
    };
    assert!(workload_dag.nodes.iter().any(|n| matches!(
        n.materialization,
        asap_types::physical::Materialization::MatAtQuery(_)
    ) && reaches_state(n.id)));
}
