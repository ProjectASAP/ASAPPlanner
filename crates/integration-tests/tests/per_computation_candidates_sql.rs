//! SQL → Pass 1 per-computation candidates: where each `HAVING` is applied,
//! the guarantees composed over estimates, and what a Top-K heap sketch
//! ranks.

use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_stage1_pass1_per_computation_candidates::test_support::generate_per_computation_candidates;
use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::QueryRoot;
use asap_types::stages::{ComputationTarget, HavingPlacement};

async fn targets(sql: &str) -> Vec<ComputationTarget> {
    let catalog = SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![
            Field::plain("src", DataType::Utf8, false),
            Field::plain("bytes", DataType::Float64, false),
        ]),
    );
    let root = lower_sql(sql, &catalog)
        .await
        .unwrap_or_else(|e| panic!("lower failed for {sql:?}: {e}"));
    generate_per_computation_candidates(
        &asap_stage0_memo::memo_of_queries(
            &[QueryRoot::Operator(root)],
            &[None],
            &Default::default(),
        )
        .unwrap(),
    )
    .unwrap()
    .targets
}

fn placements(targets: &[ComputationTarget]) -> Vec<HavingPlacement> {
    targets
        .iter()
        .flat_map(|target| target.having_filters.iter().map(|entry| entry.placement))
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
    use asap_types::summary_type::SketchAlgorithm;
    asap_types::ir::OperatorNode::reachable(node)
        .iter()
        .any(|node| {
            matches!(
                node.asap(),
                Some(asap_types::ir::ASAPOp::SummaryAgg {
                    summary_type: asap_types::ir::schema::FieldDataType::Sketch(kind),
                    ..
                }) if matches!(kind, SketchAlgorithm::Kll | SketchAlgorithm::DDSketch)
            )
        })
}

/// A measure filter or a grouping key that reads an approximate value
/// decides which rows count, or which group a row joins, by an estimate, so
/// an estimate near a threshold or a group boundary can change the result
/// arbitrarily: the output has no guarantee (#669), whether the measure
/// counts rows or folds the estimate itself. So does a count of the
/// estimates' distinct values, of non-null estimates, or a group key
/// computed from an estimate.
#[tokio::test]
async fn a_filter_or_group_key_over_an_estimate_has_no_guarantee() {
    let catalog = SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![
            Field::plain("src", DataType::Utf8, false),
            Field::plain("bytes", DataType::Float64, false),
        ]),
    );
    let accuracy = AccuracyTarget::max_error(0.01);
    for sql in [
        "SELECT count(*) FILTER (WHERE p > 5) FROM \
         (SELECT src, approx_percentile_cont(bytes, 0.99) AS p FROM flows GROUP BY src) s",
        "SELECT max(p) FILTER (WHERE p < 100) FROM \
         (SELECT src, approx_percentile_cont(bytes, 0.99) AS p FROM flows GROUP BY src) s",
        "SELECT p, count(*) FROM \
         (SELECT src, approx_percentile_cont(bytes, 0.99) AS p FROM flows GROUP BY src) s \
         GROUP BY p",
        "SELECT p, max(p) FROM \
         (SELECT src, approx_percentile_cont(bytes, 0.99) AS p FROM flows GROUP BY src) s \
         GROUP BY p",
        "SELECT p, sum(p) FROM \
         (SELECT src, approx_percentile_cont(bytes, 0.99) AS p FROM flows GROUP BY src) s \
         GROUP BY p",
        "SELECT p > 5, count(*) FROM \
         (SELECT src, approx_percentile_cont(bytes, 0.99) AS p FROM flows GROUP BY src) s \
         GROUP BY p > 5",
        "SELECT count(DISTINCT p) FROM \
         (SELECT src, approx_percentile_cont(bytes, 0.99) AS p FROM flows GROUP BY src) s",
        "SELECT count(p) FROM \
         (SELECT src, approx_percentile_cont(bytes, 0.99) AS p FROM flows GROUP BY src) s",
    ] {
        let root = lower_sql(sql, &catalog)
            .await
            .unwrap_or_else(|e| panic!("lower failed for {sql:?}: {e}"));
        // No guarantee meets the target, so Pass 1 prunes the sketch
        // unless every output is accepted.
        let candidates: Vec<_> =
            asap_integration_tests::planning::logical_candidates_ignoring_guarantees(
                root,
                accuracy.clone(),
            )
            .into_iter()
            .filter(reads_quantile_sketch)
            .collect();
        assert!(!candidates.is_empty(), "{sql}");
        for candidate in candidates {
            assert_eq!(candidate.guarantee, None, "{sql}");
        }
    }
}
