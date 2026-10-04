//! #511 examples: source text → unified dag → summary rewrite → flat export.
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::summary_coverage::{CoverageRegion, SummaryCoverage};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::schema::{
    ExactKind, ExactParams, FieldDataType, GroupingStrategy, SummaryUpdate,
};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, ScalarExpr};
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
                .unwrap()
                // Whole-source coverage, as the planner declares it today (#570).
                .with_coverage(SummaryCoverage {
                    source: asap_types::ir::operator::Source::Table {
                        table_ref: "requests".into(),
                    },
                    regions: vec![CoverageRegion {
                        time_ms: None,
                        population: Default::default(),
                    }],
                })
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
        Rc::new(node.map_children(rewrite).unwrap())
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
        let wire = physical_common::compile_physical_asap_dag(&root).unwrap();
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
    let wire = physical_common::compile_physical_asap_dag(&root).unwrap();
    let scan = wire
        .nodes
        .iter()
        .find(|node| {
            matches!(
                &node.payload,
                asap_types::ir::export::PhysicalASAPOperatorPayload::Relational {
                    operator: asap_types::ir::export::NonASAPOpKind::Scan { .. }
                }
            )
        })
        .unwrap();
    let schema = Arc::new(scan.output_schema.clone());
    let plan = compile(
        &wire,
        BTreeMap::from([(u64::from(scan.id.0), InputContract::bounded(schema.clone()))]),
        &[u64::from(wire.roots[0].0)],
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

/// Empty window frames and filtered groups can yield NULL even on non-NULL input.
#[tokio::test]
async fn sql_window_and_filtered_aggregate_types() {
    for query in [
        "SELECT SUM(l_quantity) OVER (ORDER BY l_quantity ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING) AS s FROM lineitem",
        "SELECT MIN(l_quantity) OVER (ORDER BY l_quantity ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING) AS s FROM lineitem",
        "SELECT SUM(l_quantity) FILTER (WHERE l_quantity < 0) AS s FROM lineitem GROUP BY l_quantity",
    ] {
        let root = lower_sql(query, &catalog(), AccuracyTarget::Exact).await.unwrap();
        root.validate_structure().unwrap();
        assert_eq!(root.schema.fields[0], Field::plain("s", DataType::Int64, true), "{query}");
    }
}

/// A real query batch retains two result roots and executes both selected
/// plans. Stage 3 prices a query-time SUM state above the raw SUM it would
/// replace, so each plan aggregates its scan directly. No replacement dag is
/// constructed by the test.
#[tokio::test]
async fn batch_planning_selects_and_executes_each_plan() {
    use asap_aware_mapping::pass::PlanningModels;
    use asap_physical_operators::{
        physical_planner::{compile, InputContract},
        runtime::Scope,
        values::{Batch, Value},
    };
    use asap_planner::{e2e_plan, FrontendInput, UserInput};
    use asap_types::workload::*;
    use std::{collections::BTreeMap, sync::Arc};
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
                            accuracy: AccuracyRequirement::Explicit(AccuracyTarget::Exact),
                            ..Default::default()
                        },
                        predictability: Predictability::Unknown,
                        invocations: 2,
                        execute_at: None,
                        time_selection: TimeSelection::default(),
                    })
                    .collect(),
            ),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::AtRest,
            ..Default::default()
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
    assert!(states.is_empty(), "Stage 3 selects the raw SUM");
    for (plan, expected) in output.plans.iter().zip([31.0, 60.0]) {
        let root = &plan.root;
        root.validate_structure().unwrap();
        let wire = physical_common::compile_physical_asap_dag(root).unwrap();
        let scan = wire
            .nodes
            .iter()
            .find(|n| {
                matches!(
                    n.payload,
                    asap_types::ir::export::PhysicalASAPOperatorPayload::Relational {
                        operator: asap_types::ir::export::NonASAPOpKind::Scan { .. }
                    }
                )
            })
            .unwrap();
        let schema = Arc::new(scan.output_schema.clone());
        let program = compile(
            &wire,
            BTreeMap::from([(u64::from(scan.id.0), InputContract::bounded(schema.clone()))]),
            &[u64::from(wire.roots[0].0)],
        )
        .unwrap();
        let result = physical_common::execute(
            &program,
            BTreeMap::from([(
                u64::from(scan.id.0),
                Batch::try_new(
                    schema,
                    vec![vec![Value::Float64(10.0)], vec![Value::Float64(20.0)]],
                )
                .unwrap(),
            )]),
            Scope::Query {
                evaluation_time_ms: 0,
                revision: 1,
            },
        );
        let rows: Vec<_> = result[0].iter().flat_map(|batch| batch.rows()).collect();
        assert_eq!(rows.len(), 1);
        assert!(
            matches!(rows[0][0], Value::Float64(v) if v == expected),
            "{:?}",
            rows
        );
    }
    // The batch exports as one physical DAG: a root per query, no state.
    let workload_dag = output.execution_timed_dag().unwrap();
    assert_eq!(workload_dag.roots.len(), 2);
    assert_ne!(workload_dag.roots[0], workload_dag.roots[1]);
    assert_eq!(
        workload_dag
            .nodes
            .iter()
            .filter(|n| matches!(
                n.payload,
                asap_types::ir::export::PhysicalASAPOperatorPayload::SummaryAgg { .. }
            ))
            .count(),
        0
    );
}
