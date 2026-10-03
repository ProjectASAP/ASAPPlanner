//! SQL L2 recognition survives wire compilation and executes the exact native fallback.
mod physical_common;
use asap_aware_mapping::{
    replacement::{Replacement, ReplacementStrategy, TargetSubDAG},
    SemanticEquivalentRewriteStrategy,
};
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_physical_operators::values::Value;
use asap_types::{
    ir::NonASAPOp,
    pre_asap::{DataType, Field, Schema},
    types::AccuracyTarget,
};

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
        let original = physical_common::execute_raw_rows(&root, rows.clone());
        let actual = physical_common::execute_raw_rows(rewritten, rows);
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
