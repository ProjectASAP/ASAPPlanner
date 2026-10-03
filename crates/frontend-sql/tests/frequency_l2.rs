//! SQL frequency idioms expose logical candidates without replacing the exact SQL DAG.
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
// Floating count products retain aliases, filters, type and empty-input nullability.
#[tokio::test]
async fn recognizes_float_frequency_l2_as_an_additional_candidate() {
    for sql in [
        "SELECT SQRT(SUM(CAST(c AS DOUBLE) * CAST(c AS DOUBLE))) AS norm FROM (SELECT src_ip, COUNT(*) AS c FROM flows WHERE keep GROUP BY src_ip) f",
        "SELECT SQRT(SUM(c * c)) AS norm FROM (SELECT src_ip, CAST(COUNT(*) AS DOUBLE) AS c FROM flows GROUP BY src_ip) f",
    ] {
        let root = lower_sql(sql, &catalog(false), AccuracyTarget::Exact).await.unwrap();
        let replacements = SemanticEquivalentRewriteStrategy.replacements(&TargetSubDAG::new(&root));
        let rewritten = replacements.iter().find_map(|r| match &r.replacement { Replacement::SubDAG(node) if has_l2(node) => Some(node), _ => None }).expect("frequency L2 candidate");
        assert_eq!(root.schema, rewritten.schema);
        assert!(!has_l2(&root));
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
        let candidates = SemanticEquivalentRewriteStrategy.replacements(&TargetSubDAG::new(&root));
        assert!(!candidates.iter().any(|r| matches!(&r.replacement, Replacement::SubDAG(n) if has_l2(n))));
    }
}

// The new frequency intent carries the query's requested budget rather than an invented default.
#[tokio::test]
async fn frequency_l2_preserves_accuracy_target() {
    let target = AccuracyTarget::EpsilonDelta {
        epsilon: 0.01,
        delta: 0.01,
    };
    let root = lower_sql("SELECT SQRT(SUM(c*c)) FROM (SELECT src_ip, CAST(COUNT(*) AS DOUBLE) AS c FROM flows GROUP BY src_ip) f", &catalog(false), target.clone()).await.unwrap();
    let candidates = SemanticEquivalentRewriteStrategy.replacements(&TargetSubDAG::new(&root));
    let Replacement::SubDAG(node) = &candidates[0].replacement else {
        panic!("rewrite");
    };
    let NonASAPOp::Project { child, .. } = node.expect_non_asap() else {
        panic!("project");
    };
    let NonASAPOp::Aggregate { measures, .. } = child.expect_non_asap() else {
        panic!("aggregate");
    };
    assert!(matches!(&measures[0], AggIntent::FrequencyL2 { accuracy, .. } if *accuracy == target));
}

// The default search discovers the rewrite and keeps an original relational alternative.
#[tokio::test]
async fn default_search_keeps_exact_sql_and_frequency_alternatives() {
    use asap_aware_mapping::replacement::{default_strategies, search_workload_with_targets};
    let root = lower_sql("SELECT SQRT(SUM(c*c)) FROM (SELECT src_ip, CAST(COUNT(*) AS DOUBLE) AS c FROM flows GROUP BY src_ip) f", &catalog(false), AccuracyTarget::Exact).await.unwrap();
    let space = search_workload_with_targets(
        vec![(0, root, Some(AccuracyTarget::Exact))],
        &default_strategies(),
        &asap_aware_mapping::accuracy::DefaultAccuracyModel,
    );
    let inventory = space.enumerate_candidate_dags(1000).unwrap();
    assert!(inventory
        .candidates
        .iter()
        .any(|candidate| has_l2(&candidate[0].1)));
    assert!(inventory
        .candidates
        .iter()
        .any(|candidate| !has_l2(&candidate[0].1)));
}
