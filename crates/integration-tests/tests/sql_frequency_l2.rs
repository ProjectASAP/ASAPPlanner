//! SQL L2 recognition survives wire compilation and executes the exact native fallback.
mod physical_common;
use asap_aware_mapping::{
    replacement::{Replacement, ReplacementStrategy, TargetSubDAG},
    SemanticEquivalentRewriteStrategy,
};
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_physical_operators::{
    runtime::Scope,
    values::{Batch, Value},
};
use asap_types::{
    ir::{
        export::{NonASAPOpKind, PostAsapOperatorPayload},
        NonASAPOp, OperatorNode,
    },
    pre_asap::{DataType, Field, Schema},
    types::AccuracyTarget,
};
use std::{collections::BTreeMap, rc::Rc, sync::Arc};

fn run(root: &Rc<OperatorNode>, rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    use asap_physical_operators::{
        physical_planner::bind_with_data_sources,
        runtime::{Limits, RunContext},
        sources::{DataSources, MemorySource},
    };
    use futures::{executor::block_on, StreamExt};
    let wire = physical_common::compile_post_asap_dag(root).unwrap();
    let scan = wire
        .nodes
        .iter()
        .find(|node| {
            matches!(
                node.payload,
                PostAsapOperatorPayload::Relational {
                    operator: NonASAPOpKind::Scan { .. }
                }
            )
        })
        .unwrap();
    let PostAsapOperatorPayload::Relational {
        operator: NonASAPOpKind::Scan { source, .. },
    } = &scan.payload
    else {
        unreachable!();
    };
    let input = Arc::new(scan.output_schema.clone());
    let batch = Batch::try_new(input.clone(), rows).unwrap();
    let mut sources = DataSources::default();
    sources
        .register(
            source.clone(),
            Arc::new(MemorySource::new(input, vec![batch]).unwrap()),
        )
        .unwrap();
    let root_id = u64::from(wire.root.0);
    let plan = bind_with_data_sources(&wire, BTreeMap::new(), &[root_id], &sources).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap();
    let result = block_on(async {
        let mut output = plan.execute(&[root_id], context.clone()).unwrap().remove(0);
        let mut rows = vec![];
        while let Some(batch) = output.next().await {
            rows.extend_from_slice(batch.unwrap().rows());
        }
        rows
    });
    assert_eq!(context.retained_bytes(), 0);
    result
}
// Original SQL and its logical alternative agree on filters and SQL's empty-input NULL.
#[tokio::test]
async fn sql_l2_original_and_rewrite_execute_equivalently() {
    let catalog = SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![
            Field::plain("src_ip", DataType::Utf8, false),
            Field::plain("keep", DataType::Bool, false),
        ]),
    );
    let root = lower_sql("SELECT SQRT(SUM(CAST(c AS DOUBLE)*CAST(c AS DOUBLE))) AS norm FROM (SELECT src_ip, COUNT(*) AS c FROM flows WHERE keep GROUP BY src_ip) f", &catalog, AccuracyTarget::Exact).await.unwrap();
    let replacements = SemanticEquivalentRewriteStrategy.replacements(&TargetSubDAG::new(&root));
    let Replacement::SubDAG(rewritten) = &replacements[0].replacement else {
        panic!("logical rewrite");
    };
    assert!(matches!(
        rewritten.non_asap(),
        Some(NonASAPOp::Project { .. })
    ));
    for rows in [
        vec![],
        vec![vec![Value::Utf8("discard".into()), Value::Bool(false)]],
        vec![
            vec![Value::Utf8("a".into()), Value::Bool(true)],
            vec![Value::Utf8("a".into()), Value::Bool(true)],
            vec![Value::Utf8("b".into()), Value::Bool(true)],
            vec![Value::Utf8("discard".into()), Value::Bool(false)],
        ],
    ] {
        let original = run(&root, rows.clone());
        let actual = run(rewritten, rows);
        if original.iter().any(|row| !matches!(row[0], Value::Null)) {
            assert!(
                matches!(original[0][0], Value::Float64(v) if (v - 5.0_f64.sqrt()).abs() < 1e-12)
            );
        }
        assert_eq!(original.len(), actual.len());
        for (expected, actual) in original.iter().zip(actual) {
            match (&expected[0], &actual[0]) {
                (Value::Null, Value::Null) => {}
                (Value::Float64(a), Value::Float64(b)) => assert!((a - b).abs() < 1e-12),
                other => panic!("mismatched SQL result: {other:?}"),
            }
        }
    }
}
