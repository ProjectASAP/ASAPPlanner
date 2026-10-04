//! SQL L2 recognition survives physical compilation and executes the exact native reducer.
mod physical_common;
use asap_executor::values::Value;
use asap_frontend_common::resolve_root;
use asap_frontend_sql::{lower_sql, SqlCatalog, SqlLowerer};
use asap_types::{
    ir::schema::{DataType, Field, Schema},
    ir::NonASAPOp,
    types::AccuracyTarget,
};

// Original SQL and its recognized form agree on filters and SQL's empty-input NULL.
#[tokio::test]
async fn sql_l2_original_and_rewrite_execute_equivalently() {
    let catalog = SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![
            Field::plain("src_ip", DataType::Utf8, false),
            Field::plain("keep", DataType::Bool, false),
        ]),
    );
    let sql = "SELECT SQRT(SUM(CAST(c AS DOUBLE)*CAST(c AS DOUBLE))) AS norm FROM (SELECT src_ip, COUNT(*) AS c FROM flows WHERE keep GROUP BY src_ip) f";
    let root = resolve_root(
        &SqlLowerer::new(&catalog)
            .lower(sql, &AccuracyTarget::Exact)
            .await
            .unwrap(),
    )
    .unwrap();
    let rewritten = lower_sql(sql, &catalog, AccuracyTarget::Exact)
        .await
        .unwrap();
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
        let actual = physical_common::execute_raw_rows(&rewritten, rows);
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
