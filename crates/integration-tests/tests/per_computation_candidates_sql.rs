//! SQL `HAVING` → Pass 1 per-computation candidates: where each `HAVING` is applied.

use asap_aware_mapping::per_computation_candidates::{
    generate_per_computation_candidates, ComputationTarget, HavingPlacement,
};
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::ir::QueryRoot;
use asap_types::pre_asap::schema::{DataType, Field, Schema};
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

/// Whether `node` reads a quantile sketch (KLL or DDSketch) anywhere below it.
fn reads_quantile_sketch(node: &std::rc::Rc<asap_types::ir::OperatorNode>) -> bool {
    use asap_types::post_asap::SketchAlgorithm;
    asap_types::ir::OperatorNode::reachable(node)
        .iter()
        .any(|node| {
            matches!(
                node.asap(),
                Some(asap_types::ir::ASAPOp::SummaryAgg {
                    family: asap_types::post_asap::FieldDataType::Sketch(kind, _),
                    ..
                }) if matches!(kind.algorithm(), SketchAlgorithm::Kll | SketchAlgorithm::DDSketch)
            )
        })
}

/// A measure filter that reads an approximate value decides which rows
/// count by an estimate, so an estimate near its threshold can flip the
/// result arbitrarily: the output has no guarantee (#669), whether the
/// measure counts rows or folds the estimate itself.
#[tokio::test]
async fn a_measure_filter_over_an_estimate_has_no_guarantee() {
    let catalog = SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![
            Field::plain("src", DataType::Utf8, false),
            Field::plain("bytes", DataType::Float64, false),
        ]),
    );
    let accuracy = AccuracyTarget::Epsilon(0.01);
    for sql in [
        "SELECT count(*) FILTER (WHERE p > 5) FROM \
         (SELECT src, approx_percentile_cont(bytes, 0.99) AS p FROM flows GROUP BY src) s",
        "SELECT max(p) FILTER (WHERE p < 100) FROM \
         (SELECT src, approx_percentile_cont(bytes, 0.99) AS p FROM flows GROUP BY src) s",
    ] {
        let root = lower_sql(sql, &catalog, accuracy.clone())
            .await
            .unwrap_or_else(|e| panic!("lower failed for {sql:?}: {e}"));
        let candidates: Vec<_> =
            asap_integration_tests::planning::logical_candidates(root, accuracy.clone())
                .into_iter()
                .filter(reads_quantile_sketch)
                .collect();
        assert!(!candidates.is_empty(), "{sql}");
        for candidate in candidates {
            assert_eq!(candidate.guarantee, None, "{sql}");
        }
    }
}
