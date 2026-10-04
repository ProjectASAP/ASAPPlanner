//! The SQL frontend names the frequency L2 idiom as a `FrequencyL2` intent.
use asap_frontend_common::resolve_root;
use asap_frontend_sql::{lower_sql, SqlCatalog, SqlLowerer};
use asap_types::{
    ir::operator::AggIntent,
    ir::schema::{DataType, Field, Schema},
    ir::{NonASAPOp, OperatorNode},
    types::AccuracyTarget,
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
            .lower(sql, &AccuracyTarget::Exact)
            .await
            .unwrap(),
    )
    .unwrap()
}
// Floating count products retain aliases, filters, type and empty-input nullability.
#[tokio::test]
async fn recognizes_float_frequency_l2() {
    for sql in [
        "SELECT SQRT(SUM(CAST(c AS DOUBLE) * CAST(c AS DOUBLE))) AS norm FROM (SELECT src_ip, COUNT(*) AS c FROM flows WHERE keep GROUP BY src_ip) f",
        "SELECT SQRT(SUM(c * c)) AS norm FROM (SELECT src_ip, CAST(COUNT(*) AS DOUBLE) AS c FROM flows GROUP BY src_ip) f",
    ] {
        let rewritten = lower_sql(sql, &catalog(false), AccuracyTarget::Exact).await.unwrap();
        let original = relational(sql, &catalog(false)).await;
        assert!(has_l2(&rewritten));
        assert_eq!(original.schema, rewritten.schema);
        assert!(!has_l2(&original));
    }
}
// Nearby SQL forms with different frequency, overflow or NULL semantics remain ordinary SQL.
#[tokio::test]
async fn declines_non_equivalent_frequency_shapes() {
    for (sql, nullable) in [
        ("SELECT SQRT(SUM(c*c)) FROM (SELECT src_ip, COUNT(*) AS c FROM flows GROUP BY src_ip) f", false),
        ("SELECT SQRT(SUM(CAST(c AS DOUBLE)*CAST(c AS DOUBLE))) FROM (SELECT src_ip, COUNT(*) AS c FROM flows GROUP BY src_ip) f", true),
        ("SELECT SQRT(SUM(c*c)) FROM (SELECT src_ip, CAST(COUNT(*) AS DOUBLE) AS c FROM flows GROUP BY src_ip HAVING COUNT(*) > 1) f", false),
        ("SELECT SQRT(SUM(c*c)) FROM (SELECT src_ip, CAST(SUM(CAST(keep AS BIGINT)) AS DOUBLE) AS c FROM flows GROUP BY src_ip) f", false),
    ] {
        let root = lower_sql(sql, &catalog(nullable), AccuracyTarget::Exact).await.unwrap();
        assert!(!has_l2(&root));
    }
}

// The new frequency intent carries the query's requested budget rather than an invented default.
#[tokio::test]
async fn frequency_l2_preserves_accuracy_target() {
    let target = AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.01,
    };
    let node = lower_sql("SELECT SQRT(SUM(c*c)) FROM (SELECT src_ip, CAST(COUNT(*) AS DOUBLE) AS c FROM flows GROUP BY src_ip) f", &catalog(false), target.clone()).await.unwrap();
    let NonASAPOp::Project { child, .. } = node.expect_non_asap() else {
        panic!("project");
    };
    let NonASAPOp::Join { left, .. } = child.expect_non_asap() else {
        panic!("guarded statistic");
    };
    let NonASAPOp::Aggregate { measures, .. } = left.expect_non_asap() else {
        panic!("aggregate");
    };
    assert!(matches!(&measures[0], AggIntent::FrequencyL2 { accuracy, .. } if *accuracy == target));
}

// Stage 1 offers the exact L2 reducer and summary alternatives for the recognized intent.
#[tokio::test]
async fn stage1_offers_exact_and_summary_l2_alternatives() {
    use asap_logical_optimizer::pass1::logical_candidates::enumerate_local_logical_candidates;
    use asap_logical_optimizer::pass1::replacement::Realization;
    use asap_types::ir::QueryRoot;
    let target = AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.01,
    };
    let root = lower_sql("SELECT SQRT(SUM(c*c)) FROM (SELECT src_ip, CAST(COUNT(*) AS DOUBLE) AS c FROM flows GROUP BY src_ip) f", &catalog(false), target).await.unwrap();
    let inventory = enumerate_local_logical_candidates(
        vec![(0, QueryRoot::Operator(root))],
        &Default::default(),
    )
    .unwrap();
    let l2 = inventory
        .targets
        .iter()
        .find(|t| has_l2(&t.target))
        .expect("L2 target");
    assert!(l2
        .alternatives
        .iter()
        .any(|a| matches!(a, Realization::PassThrough)));
    assert!(l2
        .alternatives
        .iter()
        .any(|a| matches!(a, Realization::Sketch(_))));
}

// An approximate L2 estimate must never decide whether SQL returns NULL.
#[tokio::test]
async fn frequency_empty_input_guard_uses_an_exact_count() {
    let target = AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.01,
    };
    let node = lower_sql("SELECT SQRT(SUM(c*c)) FROM (SELECT src_ip, CAST(COUNT(*) AS DOUBLE) AS c FROM flows GROUP BY src_ip) f", &catalog(false), target).await.unwrap();
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
        [AggIntent::Count {
            accuracy: AccuracyTarget::Exact
        }]
    ));
}
