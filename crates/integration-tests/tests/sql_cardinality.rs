//! The design's SQL distinct query executes its exact native fallback.
mod physical_common;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_physical_operators::values::Value;
use asap_types::{
    pre_asap::{DataType, Field, Schema},
    types::AccuracyTarget,
};

// COUNT(DISTINCT src_ip) skips NULL and preserves WHERE, Utf8 identities and zero on no input.
#[tokio::test]
async fn sql_distinct_executes_through_raw_scan_and_native_binding() {
    let catalog = SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![
            Field::plain("src_ip", DataType::Utf8, true),
            Field::plain("keep", DataType::Bool, false),
        ]),
    );
    let root = lower_sql(
        "SELECT COUNT(DISTINCT src_ip) AS sources FROM flows WHERE keep",
        &catalog,
        AccuracyTarget::Exact,
    )
    .await
    .unwrap();
    for (rows, expected) in [
        (vec![], 0),
        (vec![vec![Value::Null, Value::Bool(true)]], 0),
        (
            vec![
                vec![Value::Utf8("a".into()), Value::Bool(true)],
                vec![Value::Utf8("a".into()), Value::Bool(true)],
                vec![Value::Utf8("b".into()), Value::Bool(true)],
                vec![Value::Utf8("discard".into()), Value::Bool(false)],
                vec![Value::Null, Value::Bool(true)],
            ],
            2,
        ),
    ] {
        let result = physical_common::execute_raw_rows(&root, rows);
        assert!(
            matches!(result.as_slice(), [row] if matches!(row.as_slice(), [Value::Int64(v)] if *v == expected))
        );
    }
}
