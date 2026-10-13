//! SQL `HAVING` → Pass 1 local candidates: where each `HAVING` is applied.

use asap_aware_mapping::logical_candidates::{
    enumerate_local_logical_candidates, HavingPlacement, LocalLogicalTarget,
};
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::ir::QueryRoot;
use asap_types::pre_asap::schema::{DataType, Field, Schema};
use asap_types::types::AccuracyTarget;

async fn targets(sql: &str) -> Vec<LocalLogicalTarget> {
    let catalog = SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![
            Field::plain("src", DataType::Utf8, false),
            Field::plain("bytes", DataType::Float64, false),
        ]),
    );
    let accuracy = AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.01,
    };
    let root = lower_sql(sql, &catalog, accuracy)
        .await
        .unwrap_or_else(|e| panic!("lower failed for {sql:?}: {e}"));
    enumerate_local_logical_candidates(vec![(0, QueryRoot::Operator(root))])
        .unwrap()
        .targets
}

fn placements(targets: &[LocalLogicalTarget]) -> Vec<HavingPlacement> {
    targets
        .iter()
        .flat_map(|target| target.having.iter().map(|entry| entry.placement))
        .collect()
}

#[tokio::test]
async fn key_only_having_is_pushed_below() {
    let targets = targets("SELECT src, count(*) FROM flows GROUP BY src HAVING src = 'a'").await;
    assert_eq!(placements(&targets), vec![HavingPlacement::BelowAggregate]);
}

#[tokio::test]
async fn value_having_stays_above() {
    let targets =
        targets("SELECT src, count(*) FROM flows GROUP BY src HAVING count(*) > 10").await;
    assert_eq!(placements(&targets), vec![HavingPlacement::AboveOutput]);
}

#[tokio::test]
async fn global_constant_having_stays_above() {
    let targets = targets("SELECT count(*) FROM flows HAVING false").await;
    assert_eq!(placements(&targets), vec![HavingPlacement::AboveOutput]);
}

#[tokio::test]
async fn mixed_key_and_value_having_stays_above() {
    let targets =
        targets("SELECT src, count(*) FROM flows GROUP BY src HAVING src = 'a' AND count(*) > 10")
            .await;
    assert_eq!(placements(&targets), vec![HavingPlacement::AboveOutput]);
}
