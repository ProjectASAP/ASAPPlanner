//! The SQL frontend names the frequency L2 idiom as a `FrequencyL2` intent
//! (#509 Example 2's Q3, in its Float64 form).
use asap_frontend_common::resolve_root;
use asap_frontend_sql::{lower_sql, lower_sql_batch, SqlCatalog, SqlLowerer};
use asap_types::{
    ir::{NonASAPOp, OperatorNode},
    pre_asap::{AggIntent, DataType, Field, Schema},
    types::{AccuracyTarget, ErrorMetric},
};
use std::rc::Rc;

fn catalog(nullable: bool) -> SqlCatalog {
    SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![
            Field::plain("src_ip", DataType::Utf8, nullable),
            Field::plain("keep", DataType::Bool, false),
        ]),
    )
}

fn has_l2(node: &OperatorNode) -> bool {
    if let Some(NonASAPOp::Aggregate { measures, .. }) = node.non_asap() {
        if measures
            .iter()
            .any(|m| matches!(m, AggIntent::FrequencyL2 { .. }))
        {
            return true;
        }
    }
    node.children().iter().any(|child| has_l2(child))
}

/// The relational DAG the frontend lowers before frequency recognition.
async fn relational(sql: &str, catalog: &SqlCatalog) -> Rc<OperatorNode> {
    resolve_root(
        &SqlLowerer::new(catalog)
            .lower(sql, &AccuracyTarget::exact())
            .await
            .unwrap(),
    )
    .unwrap()
}

const CAST_PRODUCT: &str = "SELECT SQRT(SUM(CAST(c AS DOUBLE) * CAST(c AS DOUBLE))) AS norm FROM (SELECT src_ip, COUNT(*) AS c FROM flows WHERE keep GROUP BY src_ip) f";
const CAST_COUNT: &str = "SELECT SQRT(SUM(c * c)) AS norm FROM (SELECT src_ip, CAST(COUNT(*) AS DOUBLE) AS c FROM flows GROUP BY src_ip) f";

/// Float64 count products lower to an L2 intent that keeps the alias, the
/// filter, the type and the empty-input nullability of the original.
#[tokio::test]
async fn recognizes_float_frequency_l2() {
    for sql in [CAST_PRODUCT, CAST_COUNT] {
        let rewritten = lower_sql(sql, &catalog(false), AccuracyTarget::exact())
            .await
            .unwrap();
        let original = relational(sql, &catalog(false)).await;
        assert!(has_l2(&rewritten), "{sql}");
        assert_eq!(original.schema, rewritten.schema);
        assert!(!has_l2(&original));
    }
}

/// Nearby forms with other overflow, NULL or frequency semantics stay
/// relational: the Int64 product (SQL integer overflow is observable), a
/// nullable key, `HAVING`, and a sum that is not a unit count.
#[tokio::test]
async fn declines_non_equivalent_frequency_shapes() {
    for (sql, nullable) in [
        ("SELECT SQRT(SUM(c*c)) FROM (SELECT src_ip, COUNT(*) AS c FROM flows GROUP BY src_ip) f", false),
        ("SELECT SQRT(SUM(CAST(c AS DOUBLE)*CAST(c AS DOUBLE))) FROM (SELECT src_ip, COUNT(*) AS c FROM flows GROUP BY src_ip) f", true),
        ("SELECT SQRT(SUM(c*c)) FROM (SELECT src_ip, CAST(COUNT(*) AS DOUBLE) AS c FROM flows GROUP BY src_ip HAVING COUNT(*) > 1) f", false),
        ("SELECT SQRT(SUM(c*c)) FROM (SELECT src_ip, CAST(SUM(CAST(keep AS BIGINT)) AS DOUBLE) AS c FROM flows GROUP BY src_ip) f", false),
    ] {
        let root = lower_sql(sql, &catalog(nullable), AccuracyTarget::exact())
            .await
            .unwrap();
        assert!(!has_l2(&root), "{sql}");
    }
}

/// The L2 intent carries the query's accuracy target, not a default.
#[tokio::test]
async fn frequency_l2_preserves_accuracy_target() {
    let target = AccuracyTarget::epsilon_delta(0.01, 0.01);
    let node = lower_sql(CAST_COUNT, &catalog(false), target.clone())
        .await
        .unwrap();
    let NonASAPOp::Project { child, .. } = node.expect_non_asap() else {
        panic!("project");
    };
    let NonASAPOp::Join { left, .. } = child.expect_non_asap() else {
        panic!("guarded statistic");
    };
    let NonASAPOp::Aggregate { measures, .. } = left.expect_non_asap() else {
        panic!("aggregate");
    };
    assert!(
        matches!(&measures[0], AggIntent::FrequencyL2 { accuracy, .. } if *accuracy == target.target_in(ErrorMetric::Relative))
    );
}

/// An approximate L2 estimate never decides whether SQL returns NULL: the
/// empty-input guard is an exact count.
#[tokio::test]
async fn frequency_empty_input_guard_uses_an_exact_count() {
    let target = AccuracyTarget::epsilon_delta(0.01, 0.01);
    let node = lower_sql(CAST_COUNT, &catalog(false), target)
        .await
        .unwrap();
    let NonASAPOp::Project { child, .. } = node.expect_non_asap() else {
        panic!("project");
    };
    let NonASAPOp::Join { right, .. } = child.expect_non_asap() else {
        panic!("exact population guard");
    };
    let NonASAPOp::Aggregate { measures, .. } = right.expect_non_asap() else {
        panic!("count");
    };
    assert!(matches!(
        measures.as_slice(),
        [AggIntent::Count { accuracy }] if accuracy.is_exact()
    ));
}

/// The batch path recognizes the idiom as the single-query path does.
#[tokio::test]
async fn batch_lowering_recognizes_frequency_l2() {
    use asap_types::workload::{
        AccuracyRequirement, BatchEntry, LatencyRequirement, Predictability, Query, QueryLanguage,
        QueryRequirements, QueryWorkload, SqlDialect, TimeSelection,
    };
    let workload = QueryWorkload {
        language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
        query_batch: Some(vec![BatchEntry {
            query: Query(CAST_COUNT.into()),
            requirements: QueryRequirements {
                accuracy: AccuracyRequirement::Explicit(AccuracyTarget::exact()),
                response_latency: LatencyRequirement::Unspecified,
            },
            predictability: Predictability::Unknown,
            invocations: 1,
            execute_at: None,
            time_selection: TimeSelection::default(),
        }]),
        repeating_queries: None,
    };
    let roots = lower_sql_batch(&workload, &catalog(false)).await;
    assert!(has_l2(roots[0].as_ref().unwrap()));
}

/// Stage 1 offers the exact option and summary candidates for the
/// recognized L2 intent.
#[tokio::test]
async fn stage1_offers_exact_and_summary_l2_candidates() {
    use asap_aware_mapping::per_computation_candidates::{
        generate_per_computation_candidates, ComputationCandidate,
    };
    use asap_types::ir::QueryRoot;
    let target = AccuracyTarget::epsilon_delta(0.01, 0.01);
    let root = lower_sql(CAST_COUNT, &catalog(false), target)
        .await
        .unwrap();
    let inventory =
        generate_per_computation_candidates(vec![(0, QueryRoot::Operator(root))]).unwrap();
    let l2 = inventory
        .targets
        .iter()
        .find(|t| has_l2(&t.target))
        .expect("L2 target");
    let candidates = &l2.choices.candidates;
    assert!(
        candidates.iter().any(|c| matches!(
            c,
            ComputationCandidate::Exact | ComputationCandidate::ExactAccumulator { .. }
        )),
        "{candidates:?}"
    );
    assert!(
        candidates
            .iter()
            .any(|c| matches!(c, ComputationCandidate::Summary(_))),
        "{candidates:?}"
    );
}

/// #509 Example 2 over `flows(ts, src_ip)` with its one-minute window: Q2
/// lowers to entropy and the Float64 Q3 to L2; the design's literal Int64
/// Q3 stays relational.
#[tokio::test]
async fn example2_queries_lower_to_frequency_intents() {
    let catalog = SqlCatalog::new().with_table(
        "flows",
        Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("src_ip", DataType::Utf8, false),
            ],
            0,
            vec![],
        ),
    );
    const WINDOW: &str = "WHERE ts >= now() - INTERVAL '1 minute'";
    let q2 = format!("SELECT -SUM(p * LN(p)) FROM (SELECT COUNT(*) * 1.0 / SUM(COUNT(*)) OVER () AS p FROM flows {WINDOW} GROUP BY src_ip)");
    let q3_float = format!("SELECT SQRT(SUM(CAST(c AS DOUBLE) * CAST(c AS DOUBLE))) FROM (SELECT src_ip, COUNT(*) AS c FROM flows {WINDOW} GROUP BY src_ip)");
    let q3_int = format!("SELECT SQRT(SUM(c * c)) FROM (SELECT src_ip, COUNT(*) AS c FROM flows {WINDOW} GROUP BY src_ip)");
    let lower = |sql: String| {
        let catalog = &catalog;
        async move {
            lower_sql(&sql, catalog, AccuracyTarget::exact())
                .await
                .unwrap()
        }
    };
    let q2 = lower(q2).await;
    assert!(
        q2.children().iter().any(|join| join.children().iter().any(
            |aggregate| matches!(aggregate.non_asap(), Some(NonASAPOp::Aggregate { measures, .. })
                if matches!(measures[..], [AggIntent::FrequencyEntropy { col: Some(1), .. }]))
        )),
        "{q2:#?}"
    );
    let q3_float = lower(q3_float).await;
    assert!(has_l2(&q3_float));
    assert!(!has_l2(&*lower(q3_int).await));
}
