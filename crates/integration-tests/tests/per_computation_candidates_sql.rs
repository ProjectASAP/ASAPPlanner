//! SQL → Pass 1 per-computation candidates: where each `HAVING` is applied,
//! the guarantees composed over estimates, and what a Top-K heap sketch
//! ranks.

use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_logical_asap_optimization::pass1::per_computation_candidates::{
    compose_logical_candidate, generate_per_computation_candidates, ComputationTarget,
    HavingPlacement, HowToComputeAggregateMeasure,
};
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, Field, Schema};
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, SummaryInputExpr};
use asap_types::ir::{ASAPOp, OperatorNode, QueryRoot};

async fn targets(sql: &str) -> Vec<ComputationTarget> {
    let catalog = SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![
            Field::plain("src", DataType::Utf8, false),
            Field::plain("bytes", DataType::Float64, false),
        ]),
    );
    let accuracy = AccuracyTarget::max_error(0.01).failure_probability(0.01);
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
    use asap_types::ir::schema::SketchAlgorithm;
    asap_types::ir::OperatorNode::reachable(node)
        .iter()
        .any(|node| {
            matches!(
                node.asap(),
                Some(asap_types::ir::ASAPOp::SummaryAgg {
                    family: asap_types::ir::schema::FieldDataType::Sketch(kind, _),
                    ..
                }) if matches!(kind.algorithm(), SketchAlgorithm::Kll | SketchAlgorithm::DDSketch)
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

/// Whether `node` reads a DDSketch anywhere below it.
fn reads_ddsketch(node: &std::rc::Rc<asap_types::ir::OperatorNode>) -> bool {
    use asap_types::ir::schema::SketchAlgorithm;
    asap_types::ir::OperatorNode::reachable(node)
        .iter()
        .any(|node| {
            matches!(
                node.asap(),
                Some(asap_types::ir::ASAPOp::SummaryAgg {
                    family: asap_types::ir::schema::FieldDataType::Sketch(kind, _),
                    ..
                }) if matches!(kind.algorithm(), SketchAlgorithm::DDSketch)
            )
        })
}

/// A SQL fold over the groups of an approximate subquery counts rows the
/// series count of its source does not bound, so its δ names its own
/// row-count statistic (`group_row_count`), not the source's
/// `input_row_count`.
#[tokio::test]
async fn a_sql_fold_over_estimates_leaves_its_row_count_unbound() {
    let catalog = SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![
            Field::plain("src", DataType::Utf8, false),
            Field::plain("bytes", DataType::Float64, false),
        ]),
    );
    let accuracy = AccuracyTarget::max_error(0.01);
    // q=1 is an endpoint, so this isolates the fold's row-count statistic
    // without assuming a sign for SQL bytes or interpolating across zero.
    let sql = "SELECT max(p) FROM \
               (SELECT src, approx_percentile_cont(bytes, 1.0) AS p FROM flows GROUP BY src) s";
    let root = lower_sql(sql, &catalog, accuracy.clone()).await.unwrap();
    let candidates = vec![
        asap_integration_tests::planning::explicit_summary_candidate(
            root,
            SketchAlgorithm::DDSketch,
        ),
    ];
    assert!(candidates.iter().all(reads_ddsketch));
    assert!(!candidates.is_empty());
    for candidate in candidates {
        let g = candidate.guarantee.clone().expect("a relative max");
        let delta = format!("{:?}", g.spec.failure_probability);
        assert!(delta.contains("group_row_count"), "{delta}");
        assert!(!delta.contains("input_row_count"), "{delta}");
    }
}

/// The item and weight columns of every heap sketch build in each composed
/// candidate of `sql` over `flows(key: key_type, bytes)` that picks a heap
/// sketch for its top-k.
async fn heap_updates(key: &str, key_type: DataType, sql: &str) -> Vec<(String, String)> {
    heap_updates_over(
        vec![
            Field::plain(key, key_type, false),
            Field::plain("bytes", DataType::Float64, false),
        ],
        sql,
    )
    .await
}

/// [`heap_updates`] over `flows(fields)`.
async fn heap_updates_over(fields: Vec<Field>, sql: &str) -> Vec<(String, String)> {
    let catalog = SqlCatalog::new().with_table("flows", Schema::new(fields));
    let accuracy = AccuracyTarget::max_error(0.01).failure_probability(0.01);
    let root = lower_sql(sql, &catalog, accuracy).await.unwrap();
    let inventory =
        generate_per_computation_candidates(vec![(0, QueryRoot::Operator(root))]).unwrap();
    let name = |expr: &SummaryInputExpr| match expr {
        SummaryInputExpr::Column(ColumnRef::Named(name)) => name.clone(),
        SummaryInputExpr::Column(ColumnRef::Qualified { table, name }) => {
            assert_eq!(table, "flows");
            name.clone()
        }
        // The ranked value when it is the child's one sample value.
        SummaryInputExpr::Column(ColumnRef::SampleValue) => "<sample value>".into(),
        SummaryInputExpr::Tuple(items) => match items.as_slice() {
            [SummaryInputExpr::Column(ColumnRef::Named(name))] => name.clone(),
            [SummaryInputExpr::Column(ColumnRef::Qualified { table, name })] => {
                assert_eq!(table, "flows");
                name.clone()
            }
            other => panic!("one item column: {other:?}"),
        },
        SummaryInputExpr::Constant(value) => value.to_string(),
        other => panic!("a named column or constant: {other:?}"),
    };
    let mut updates = Vec::new();
    for (t, target) in inventory.targets.iter().enumerate() {
        for c in 0..target.choice_count() {
            let (candidate, _) = target.computation_for(c).unwrap();
            if !matches!(
                candidate,
                HowToComputeAggregateMeasure::Summary(
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
/// compose over raw rows, keyed by the key with unit weight.
#[tokio::test]
async fn topk_heap_over_an_int64_key_counts_raw_rows() {
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
            .all(|(item, weight)| item == "port" && weight == "1"),
        "{updates:?}"
    );
}

/// A key named `value` is not the ranked value: the heap sketches are
/// fed raw rows with unit weight.
#[tokio::test]
async fn topk_heap_over_a_key_named_value_counts_raw_rows() {
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
            .all(|(item, weight)| item == "value" && weight == "1"),
        "{updates:?}"
    );
}

/// SQL's per-group top-k idiom (`ROW_NUMBER() OVER (PARTITION BY city
/// ORDER BY COUNT(*) DESC) … WHERE rn <= 5`) is a heavy-hitter top-k, as
/// PromQL `topk by (city)` is: both heap sketches compose, keyed by the
/// host and fed raw rows with unit weight.
#[tokio::test]
async fn rownumber_per_group_topk_offers_heap_sketches() {
    let updates = heap_updates_over(
        vec![
            Field::plain("city", DataType::Utf8, false),
            Field::plain("host", DataType::Utf8, false),
        ],
        "SELECT city, host, c FROM (SELECT city, host, COUNT(*) AS c, ROW_NUMBER() OVER \
         (PARTITION BY city ORDER BY COUNT(*) DESC) AS rn FROM flows GROUP BY city, host) \
         WHERE rn <= 5",
    )
    .await;
    assert_eq!(updates.len(), 2, "{updates:?}");
    assert!(
        updates
            .iter()
            .all(|(item, weight)| item == "host" && weight == "1"),
        "{updates:?}"
    );
}
