//! SQL → Pass 1 per-computation candidates: where each `HAVING` is applied,
//! and what a Top-K heap sketch ranks.

use asap_aware_mapping::per_computation_candidates::{
    compose_logical_candidate, generate_per_computation_candidates, ComputationCandidate,
    ComputationTarget, HavingPlacement,
};
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};
use asap_types::post_asap::{FieldDataType, SketchAlgorithm, SummaryInputExpr};
use asap_types::pre_asap::schema::{DataType, Field, Schema};
use asap_types::pre_asap::ColumnRef;
use asap_types::types::AccuracyTarget;

async fn targets(sql: &str) -> Vec<ComputationTarget> {
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
    generate_per_computation_candidates(vec![(0, QueryRoot::Operator(root))])
        .unwrap()
        .targets
}

fn placements(targets: &[ComputationTarget]) -> Vec<HavingPlacement> {
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

/// The item and weight columns of every heap sketch build in each composed
/// candidate of `sql` over `flows(key: key_type, bytes)` that picks a heap
/// sketch for its top-k.
async fn heap_updates(key: &str, key_type: DataType, sql: &str) -> Vec<(String, String)> {
    let catalog = SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![
            Field::plain(key, key_type, false),
            Field::plain("bytes", DataType::Float64, false),
        ]),
    );
    let accuracy = AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.01,
    };
    let root = lower_sql(sql, &catalog, accuracy).await.unwrap();
    let inventory =
        generate_per_computation_candidates(vec![(0, QueryRoot::Operator(root))]).unwrap();
    let name = |expr: &SummaryInputExpr| match expr {
        SummaryInputExpr::Column(ColumnRef::Named(name)) => name.clone(),
        SummaryInputExpr::Tuple(items) => match items.as_slice() {
            [SummaryInputExpr::Column(ColumnRef::Named(name))] => name.clone(),
            other => panic!("one item column: {other:?}"),
        },
        other => panic!("a named column: {other:?}"),
    };
    let mut updates = Vec::new();
    for (t, target) in inventory.targets.iter().enumerate() {
        for (c, candidate) in target.choices.candidates.iter().enumerate() {
            if !matches!(
                candidate,
                ComputationCandidate::Summary(
                    SketchAlgorithm::CmsWithHeap | SketchAlgorithm::CountSketchWithHeap
                )
            ) {
                continue;
            }
            let mut choice = vec![0; inventory.targets.len()];
            choice[t] = c;
            let composed = compose_logical_candidate(&inventory, &choice)
                .unwrap_or_else(|e| panic!("{candidate:?} composes: {e}"));
            let QueryRoot::Operator(root) = &composed[0].1 else {
                panic!("operator root")
            };
            for node in OperatorNode::reachable(root) {
                if let Some(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(..),
                    input,
                    ..
                }) = node.asap()
                {
                    updates.push((name(input.item.as_ref().unwrap()), name(&input.weight)));
                }
            }
        }
    }
    updates
}

const COUNT_RANKED_TOPK: &str =
    "SELECT KEY, count(*) FROM flows GROUP BY KEY ORDER BY count(*) DESC LIMIT 10";

/// An Int64 key is not mistaken for the ranked value: both heap sketches
/// compose, keyed by the key and weighted by the count.
#[tokio::test]
async fn topk_heap_over_an_int64_key_is_weighted_by_the_count() {
    let updates = heap_updates(
        "port",
        DataType::Int64,
        &COUNT_RANKED_TOPK.replace("KEY", "port"),
    )
    .await;
    assert_eq!(updates.len(), 2, "{updates:?}");
    assert!(
        updates
            .iter()
            .all(|(item, weight)| item == "port" && weight.starts_with("count(")),
        "{updates:?}"
    );
}

/// A key named `value` is not the ranked value: the heap sketches are
/// weighted by the count, not by the key.
#[tokio::test]
async fn topk_heap_over_a_key_named_value_is_weighted_by_the_count() {
    let updates = heap_updates(
        "value",
        DataType::Utf8,
        &COUNT_RANKED_TOPK.replace("KEY", "value"),
    )
    .await;
    assert_eq!(updates.len(), 2, "{updates:?}");
    assert!(
        updates
            .iter()
            .all(|(item, weight)| item == "value" && weight.starts_with("count(")),
        "{updates:?}"
    );
}
