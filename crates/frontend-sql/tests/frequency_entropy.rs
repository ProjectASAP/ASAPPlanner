//! SQL entropy recognition keeps natural-log units and the original relational path.
use asap_aware_mapping::{
    replacement::{Replacement, ReplacementStrategy, TargetSubDAG},
    SemanticEquivalentRewriteStrategy,
};
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::{
    ir::{NonASAPOp, OperatorNode},
    pre_asap::{AggIntent, DataType, Field, Schema},
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
// The proposal's natural-log idiom adds an entropy intent and preserves output schema.
#[tokio::test]
async fn recognizes_sql_entropy_in_nats() {
    let root = lower_sql("SELECT -SUM(p * LN(p)) AS entropy FROM (SELECT COUNT(*) * 1.0 / SUM(COUNT(*)) OVER () AS p FROM flows GROUP BY src_ip) f", &catalog(false), AccuracyTarget::Exact).await.unwrap();
    let replacements = SemanticEquivalentRewriteStrategy.replacements(&TargetSubDAG::new(&root));
    let rewritten = replacements
        .iter()
        .find_map(|r| match &r.replacement {
            Replacement::SubDAG(node) if has_entropy(node) => Some(node),
            _ => None,
        })
        .expect("entropy candidate");
    assert_eq!(root.schema, rewritten.schema);
    assert!(!has_entropy(&root));
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
        let candidates = SemanticEquivalentRewriteStrategy.replacements(&TargetSubDAG::new(&root));
        assert!(!candidates.iter().any(|r| matches!(&r.replacement, Replacement::SubDAG(n) if has_entropy(n))));
    }
}

// Default search retains both the original exact SQL graph and the recognized entropy graph.
#[tokio::test]
async fn entropy_search_preserves_relational_alternative() {
    use asap_aware_mapping::replacement::{default_strategies, search_workload_with_targets};
    let root = lower_sql("SELECT -SUM(p*LN(p)) FROM (SELECT COUNT(*)*1.0/SUM(COUNT(*)) OVER () AS p FROM flows GROUP BY src_ip) f", &catalog(false), AccuracyTarget::Exact).await.unwrap();
    let space = search_workload_with_targets(
        vec![(0, root, Some(AccuracyTarget::Exact))],
        &default_strategies(),
        &asap_aware_mapping::accuracy::DefaultAccuracyModel,
    );
    let inventory = space.enumerate_candidate_dags(1000).unwrap();
    assert!(inventory
        .candidates
        .iter()
        .any(|candidate| has_entropy(&candidate[0].1)));
    assert!(inventory
        .candidates
        .iter()
        .any(|candidate| !has_entropy(&candidate[0].1)));
}

// Entropy receives the source query budget while its empty-population guard stays exact.
#[tokio::test]
async fn entropy_accuracy_and_population_guard_are_separate() {
    let target = AccuracyTarget::EpsilonDelta {
        epsilon: 0.05,
        delta: 0.01,
    };
    let root = lower_sql("SELECT -SUM(p*LN(p)) FROM (SELECT COUNT(*)*1.0/SUM(COUNT(*)) OVER () AS p FROM flows GROUP BY src_ip) f", &catalog(false), target.clone()).await.unwrap();
    let candidates = SemanticEquivalentRewriteStrategy.replacements(&TargetSubDAG::new(&root));
    let Replacement::SubDAG(node) = &candidates[0].replacement else {
        panic!("rewrite");
    };
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
