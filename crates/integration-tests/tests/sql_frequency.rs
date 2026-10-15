//! The SQL entropy and L2 idioms, recognized by the SQL frontend, execute
//! natively and exactly: entropy in nats (SQL's `LN`), L2 of the
//! per-source counts, and NULL on empty input, decided by an exact count.
mod physical_common;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_physical_operators::values::Value;
use asap_types::{
    pre_asap::{DataType, Field, Schema},
    types::AccuracyTarget,
};

const ENTROPY: &str = "SELECT -SUM(p * LN(p)) FROM (\
    SELECT COUNT(*) * 1.0 / SUM(COUNT(*)) OVER () AS p FROM flows GROUP BY src_ip)";
const L2: &str = "SELECT SQRT(SUM(CAST(c AS DOUBLE) * CAST(c AS DOUBLE))) FROM (\
    SELECT src_ip, COUNT(*) AS c FROM flows GROUP BY src_ip)";

fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![Field::plain("src_ip", DataType::Utf8, false)]),
    )
}

fn rows(sources: &[&str]) -> Vec<Vec<Value>> {
    sources
        .iter()
        .map(|s| vec![Value::Utf8((*s).into())])
        .collect()
}

async fn run(sql: &str, sources: &[&str]) -> Option<f64> {
    let root = lower_sql(sql, &catalog(), AccuracyTarget::Exact)
        .await
        .unwrap();
    let result = physical_common::execute_raw_rows(&root, rows(sources));
    match result.as_slice() {
        [row] => match row.as_slice() {
            [Value::Float64(v)] => Some(*v),
            [Value::Null] => None,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// Counts 3 and 1 over four rows: entropy −0.75 ln 0.75 − 0.25 ln 0.25
/// nats; one source gives −0.0; no rows give NULL.
#[tokio::test]
async fn recognized_sql_entropy_executes_in_nats() {
    let expected = -0.75f64 * 0.75f64.ln() - 0.25 * 0.25f64.ln();
    let got = run(ENTROPY, &["a", "a", "a", "b"]).await.unwrap();
    assert!((got - expected).abs() < 1e-12, "{got} vs {expected}");
    let single = run(ENTROPY, &["a", "a"]).await.unwrap();
    assert_eq!(single, 0.0);
    assert!(single.is_sign_negative(), "SQL's -SUM(1 * LN 1) is -0.0");
    assert_eq!(run(ENTROPY, &[]).await, None);
}

/// Counts 2 and 1: L2 √5; no rows give NULL.
#[tokio::test]
async fn recognized_sql_l2_executes_exactly() {
    let got = run(L2, &["a", "a", "b"]).await.unwrap();
    assert!((got - 5f64.sqrt()).abs() < 1e-12, "{got}");
    assert_eq!(run(L2, &[]).await, None);
}
