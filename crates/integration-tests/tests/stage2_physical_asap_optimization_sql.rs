//! SQL heavy-hitter queries through Stage 1 (every Pass 1 choice composed)
//! and physical ASAP-aware optimization: exact top-k sorts by the ranked
//! measure, descending, whatever the group keys are called or typed, and no
//! valid candidate is pruned.

use asap_aware_mapping::per_computation_candidates::{
    compose_logical_candidate, generate_per_computation_candidates, ComputationCandidate,
};
use asap_aware_mapping::stage2_physical_asap_optimization::optimize_physical_asap;
use asap_frontend_sql::{lower_sql, SqlCatalog};
use asap_types::ir::{NonASAPOp, OperatorNode, QueryRoot, ScalarExpr};
use asap_types::pre_asap::schema::{DataType, Field, Schema};
use asap_types::pre_asap::AggIntent;
use asap_types::types::AccuracyTarget;

fn approximate() -> AccuracyTarget {
    AccuracyTarget::epsilon_delta(0.01, 0.001)
}

fn names(node: &OperatorNode) -> Vec<String> {
    node.schema.fields.iter().map(|f| f.name.clone()).collect()
}

/// The `(sort column name, ascending)` of an exact top-k's `Sort`, if `root`
/// is one (a heavy-hitter summary candidate has none).
fn exact_topk_sort(root: &OperatorNode) -> Option<(String, bool)> {
    let NonASAPOp::Project { child: limit, .. } = root.non_asap()? else {
        return None;
    };
    let NonASAPOp::Limit {
        n: Some(5),
        child: sort,
        ..
    } = limit.non_asap()?
    else {
        return None;
    };
    let NonASAPOp::Sort { keys, .. } = sort.expect_non_asap() else {
        panic!("sort below limit")
    };
    let [key] = keys.as_slice() else {
        panic!("one sort key")
    };
    let ScalarExpr::Column(column) = key.expr else {
        panic!("sort by a column")
    };
    Some((sort.schema.fields[column].name.clone(), key.ascending))
}

/// Every Pass 1 choice of `sql` under `accuracy`, composed and physically
/// optimized: each must succeed, every exact top-k must sort by `ranked`
/// descending, and every root must output `root_names`. Returns how many
/// candidates had an exact top-k.
async fn check_every_choice(
    sql: &str,
    accuracy: AccuracyTarget,
    ranked: &str,
    root_names: &[&str],
) -> usize {
    let catalog = SqlCatalog::new().with_table(
        "metrics",
        Schema::new(vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("value", DataType::Float64, false),
            Field::plain("host", DataType::Utf8, false),
            Field::plain("port", DataType::Int64, false),
        ]),
    );
    let root = lower_sql(sql, &catalog, accuracy).await.unwrap();
    let NonASAPOp::Aggregate { measures, .. } = root.expect_non_asap() else {
        panic!("{sql}: heavy-hitter aggregate root")
    };
    assert!(
        matches!(measures.as_slice(), [AggIntent::TopK { k: 5, .. }]),
        "{sql}"
    );
    let inventory =
        generate_per_computation_candidates(vec![(0, QueryRoot::Operator(root))]).unwrap();
    // The cartesian product of every target's candidates.
    let mut choices = vec![vec![]];
    for target in &inventory.targets {
        choices = choices
            .into_iter()
            .flat_map(|prefix: Vec<usize>| {
                (0..target.choices.candidates.len()).map(move |i| {
                    let mut choice = prefix.clone();
                    choice.push(i);
                    choice
                })
            })
            .collect();
    }
    let mut exact = 0;
    for choice in choices {
        let composed = match compose_logical_candidate(&inventory, &choice) {
            Ok(composed) => composed,
            // Stage 1 cannot yet compose a heavy-hitter summary over some
            // keys (an integer or several key columns); Stage 2 never sees
            // that candidate. Every other choice must compose.
            Err(e) => {
                let summarized_topk = inventory.targets.iter().zip(&choice).any(|(t, &i)| {
                    let NonASAPOp::Aggregate { measures, .. } = t.target.expect_non_asap() else {
                        return false;
                    };
                    matches!(measures[t.measure], AggIntent::TopK { .. })
                        && t.choices.candidates[i] != ComputationCandidate::Exact
                });
                assert!(summarized_topk, "{sql}, choice {choice:?}: {e}");
                continue;
            }
        };
        let roots: Vec<_> = composed.into_iter().map(|(_, root)| root).collect();
        let candidate = optimize_physical_asap("L1", &roots)
            .unwrap_or_else(|e| panic!("{sql}, choice {choice:?} pruned: {e}"));
        let QueryRoot::Operator(physical) = &candidate.roots[0] else {
            panic!("operator root")
        };
        assert_eq!(names(physical), root_names, "{sql}, choice {choice:?}");
        if let Some(sort) = exact_topk_sort(physical) {
            assert_eq!(
                sort,
                (ranked.to_string(), false),
                "{sql}, choice {choice:?}"
            );
            exact += 1;
        }
    }
    exact
}

/// Each query under an exact and an approximate target; every exact top-k
/// sorts by the ranked measure.
async fn check(sql: &str, ranked: &str, root_names: &[&str]) {
    for accuracy in [AccuracyTarget::exact(), approximate()] {
        let exact = check_every_choice(sql, accuracy.clone(), ranked, root_names).await;
        assert!(
            exact > 0,
            "{sql} under {accuracy:?}: an exact top-k candidate"
        );
    }
}

/// Grouping by a column named `value` still ranks by the count.
#[tokio::test]
async fn group_key_named_value_ranks_by_count() {
    check(
        "SELECT value, COUNT(*) AS c FROM metrics GROUP BY value ORDER BY c DESC LIMIT 5",
        "count(Int64(1))",
        &["topk_5"],
    )
    .await;
}

/// An integer group key leaves no `value` column; the count is still ranked.
#[tokio::test]
async fn integer_group_key_ranks_by_count() {
    check(
        "SELECT port, COUNT(*) AS c FROM metrics GROUP BY port ORDER BY c DESC LIMIT 5",
        "count(Int64(1))",
        &["topk_5"],
    )
    .await;
}

/// Two group keys, one of them an integer.
#[tokio::test]
async fn multi_key_ranks_by_count() {
    check(
        "SELECT host, port, COUNT(*) AS c FROM metrics GROUP BY host, port \
         ORDER BY c DESC LIMIT 5",
        "count(Int64(1))",
        &["topk_5"],
    )
    .await;
}

/// A SUM ranking sorts by the sum.
#[tokio::test]
async fn sum_ranking_ranks_by_sum() {
    check(
        "SELECT port, SUM(value) AS s FROM metrics GROUP BY port ORDER BY s DESC LIMIT 5",
        "sum(metrics.value)",
        &["topk_5"],
    )
    .await;
}
