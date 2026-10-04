//! The SQL frontend names the natural-log entropy idiom as a `FrequencyEntropy` intent.
use asap_frontend_common::resolve_root;
use asap_frontend_sql::{lower_sql, SqlCatalog, SqlLowerer};
use asap_types::{
    ir::operator::AggIntent,
    ir::schema::{DataType, Field, Schema},
    ir::{NonASAPOp, OperatorNode},
    types::AccuracyTarget,
};
fn catalog(nullable: bool) -> SqlCatalog {
    SqlCatalog::new().with_table(
        "flows",
        Schema::new(vec![Field::plain("src_ip", DataType::Utf8, nullable)]),
    )
}
fn has_entropy(node: &OperatorNode) -> bool {
    if let Some(NonASAPOp::Aggregate { measures, .. }) = node.non_asap() {
        if measures
            .iter()
            .any(|m| matches!(m, AggIntent::FrequencyEntropy { .. }))
        {
            return true;
        }
    }
    node.children().iter().any(|child| has_entropy(child))
}
// The proposal's natural-log idiom lowers to an entropy intent and preserves output schema.
#[tokio::test]
async fn recognizes_sql_entropy_in_nats() {
    let sql = "SELECT -SUM(p * LN(p)) AS entropy FROM (SELECT COUNT(*) * 1.0 / SUM(COUNT(*)) OVER () AS p FROM flows GROUP BY src_ip) f";
    let rewritten = lower_sql(sql, &catalog(false), AccuracyTarget::Exact)
        .await
        .unwrap();
    let original = resolve_root(
        &SqlLowerer::new(&catalog(false))
            .lower(sql, &AccuracyTarget::Exact)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(has_entropy(&rewritten));
    assert_eq!(original.schema, rewritten.schema);
    assert!(!has_entropy(&original));
}

// Changes to units, normalization, window coverage or the counted population are not entropy rewrites.
#[tokio::test]
async fn declines_non_equivalent_entropy_shapes() {
    for (sql, nullable) in [
        ("SELECT -SUM(p*LN(p)) FROM (SELECT COUNT(*)*2.0/SUM(COUNT(*)) OVER () AS p FROM flows GROUP BY src_ip) f", false),
        ("SELECT -SUM(p*LOG2(p)) FROM (SELECT COUNT(*)*1.0/SUM(COUNT(*)) OVER () AS p FROM flows GROUP BY src_ip) f", false),
        ("SELECT -SUM(p*LN(p)) FROM (SELECT COUNT(*)*1.0/SUM(COUNT(*)) OVER (PARTITION BY src_ip) AS p FROM flows GROUP BY src_ip) f", false),
        ("SELECT -SUM(p*LN(p)) FROM (SELECT COUNT(*)*1.0/SUM(COUNT(*)) OVER (ORDER BY src_ip) AS p FROM flows GROUP BY src_ip) f", false),
        ("SELECT -SUM(p*LN(p)) FROM (SELECT COUNT(*)*1.0/SUM(COUNT(*)) OVER () AS p FROM flows GROUP BY src_ip HAVING COUNT(*) > 1) f", false),
        ("SELECT -SUM(p*LN(p)) FROM (SELECT COUNT(*)*1.0/SUM(COUNT(*)) OVER () AS p FROM flows GROUP BY src_ip) f", true),
    ] {
        let root = lower_sql(sql, &catalog(nullable), AccuracyTarget::Exact).await.unwrap();
        assert!(!has_entropy(&root));
    }
}

// Entropy receives the source query budget while its empty-population guard stays exact.
#[tokio::test]
async fn entropy_accuracy_and_population_guard_are_separate() {
    let target = AccuracyTarget::EpsilonDelta {
        epsilon: 0.05,
        delta: 0.01,
    };
    let node = lower_sql("SELECT -SUM(p*LN(p)) FROM (SELECT COUNT(*)*1.0/SUM(COUNT(*)) OVER () AS p FROM flows GROUP BY src_ip) f", &catalog(false), target.clone()).await.unwrap();
    let NonASAPOp::Project { child, .. } = node.expect_non_asap() else {
        panic!("project");
    };
    let NonASAPOp::Join { left, right, .. } = child.expect_non_asap() else {
        panic!("guarded statistic");
    };
    let NonASAPOp::Aggregate { measures, .. } = left.expect_non_asap() else {
        panic!("entropy");
    };
    assert!(
        matches!(&measures[0], AggIntent::FrequencyEntropy { accuracy, .. } if *accuracy == target)
    );
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
