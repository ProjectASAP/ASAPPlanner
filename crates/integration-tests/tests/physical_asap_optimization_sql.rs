//! SQL heavy-hitter queries through physical ASAP-aware optimization: exact
//! top-k sorts by the ranked count, whatever the group key is.

use asap_aware_mapping::physical_asap_optimization::optimize_physical_asap;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::ir::{NonASAPOp, QueryRoot, ScalarExpr};
use asap_types::pre_asap::schema::{DataType, Field, Schema};
use asap_types::types::AccuracyTarget;

/// The root's output column names and its `(sort column name, ascending)`.
async fn exact_topk(sql: &str) -> (Vec<String>, (String, bool)) {
    let catalog = SqlCatalog::new().with_table(
        "metrics",
        Schema::new(vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("value", DataType::Float64, false),
            Field::plain("port", DataType::Int64, false),
        ]),
    );
    let root = lower_sql(sql, &catalog, AccuracyTarget::Exact)
        .await
        .unwrap();
    let candidate = optimize_physical_asap("L1", &[QueryRoot::Operator(root)]).unwrap();
    let QueryRoot::Operator(limit) = &candidate.roots[0] else {
        panic!("operator root")
    };
    let NonASAPOp::Limit {
        n: Some(5),
        child: sort,
        ..
    } = limit.expect_non_asap()
    else {
        panic!("limit root")
    };
    let NonASAPOp::Sort { keys, .. } = sort.expect_non_asap() else {
        panic!("sort below limit")
    };
    let [key] = keys.as_slice() else {
        panic!("one sort key")
    };
    let ScalarExpr::Column(column) = key.expr else {
        panic!("sort by a column")
    };
    let names = limit.schema.fields.iter().map(|f| f.name.clone()).collect();
    (
        names,
        (sort.schema.fields[column].name.clone(), key.ascending),
    )
}

/// Grouping by a column named `value` still ranks by the count, descending.
#[tokio::test]
async fn group_key_named_value_ranks_by_count() {
    let (names, sort) = exact_topk(
        "SELECT value, COUNT(*) AS c FROM metrics GROUP BY value ORDER BY c DESC LIMIT 5",
    )
    .await;
    assert_eq!(names, ["value", "count(Int64(1))"]);
    assert_eq!(sort, ("count(Int64(1))".to_string(), false));
}

/// An integer group key leaves no `value` column; the count is still ranked.
#[tokio::test]
async fn integer_group_key_ranks_by_count() {
    let (names, sort) =
        exact_topk("SELECT port, COUNT(*) AS c FROM metrics GROUP BY port ORDER BY c DESC LIMIT 5")
            .await;
    assert_eq!(names, ["port", "count(Int64(1))"]);
    assert_eq!(sort, ("count(Int64(1))".to_string(), false));
}
