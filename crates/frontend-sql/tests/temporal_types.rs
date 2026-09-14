use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::{
    pre_asap::schema::{Column, DataType, Schema},
    types::AccuracyTarget,
};
fn catalog() -> SqlCatalog {
    SqlCatalog::new().with_table(
        "t",
        Schema::new(vec![Column::new("d", DataType::Date, false)]),
    )
}
// Unsupported fixed-duration results fail lowering instead of acquiring a float schema.
#[tokio::test]
async fn temporal_subtraction_rejects_unrepresentable_duration() {
    for dtype in [DataType::Date, DataType::Timestamp] {
        let catalog =
            SqlCatalog::new().with_table("t", Schema::new(vec![Column::new("d", dtype, false)]));
        let error = lower_sql(
            "SELECT d - d AS elapsed FROM t",
            &catalog,
            AccuracyTarget::Exact,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("unsupported duration type"),
            "{error}"
        );
    }
}
// Date/interval arithmetic continues to preserve dates, including explicit interval casts.
#[tokio::test]
async fn date_shifts_keep_their_type() {
    for query in [
        "SELECT d + INTERVAL '30' DAY AS shifted FROM t",
        "SELECT d - CAST('1 day' AS INTERVAL) AS shifted FROM t",
    ] {
        let node = lower_sql(query, &catalog(), AccuracyTarget::Exact)
            .await
            .unwrap();
        assert_eq!(
            node.output_schema().unwrap().columns[0].dtype,
            DataType::Date
        );
    }
}
// Interval literals and explicit interval casts must both cross the Arrow bridge.
#[tokio::test]
async fn interval_cast_lowers_like_interval_literal() {
    for query in [
        "SELECT INTERVAL '1' DAY AS duration FROM t",
        "SELECT CAST('1 day' AS INTERVAL) AS duration FROM t",
    ] {
        let node = lower_sql(query, &catalog(), AccuracyTarget::Exact)
            .await
            .unwrap();
        assert_eq!(
            node.output_schema().unwrap().columns[0].dtype,
            DataType::Interval
        );
    }
}
