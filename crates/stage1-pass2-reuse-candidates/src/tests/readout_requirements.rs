//! Step 3: readout requirements: what Pass 2 writes on the readouts and
//! prunes with the levels (the level math is tested in `asap_accuracy`).

use asap_types::accuracy::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::QueryRoot;
use asap_types::stages::{Memo, MemoQuery, RequiredAccuracy, RootPruneReason};
use asap_types::summary_type::{SketchAlgorithm, SketchParams};
use asap_types::workload::DataWorkload;

use crate::test_fixtures::*;
use asap_types::test_support::memo_builders::*;

/// A single quantile query with rank error 0.01: its one readout gets that
/// requirement (λ = 1), and only the KLL builds whose rank error at δ = 1
/// is within 0.01 satisfy it.
#[test]
fn a_single_readout_requires_the_query_target() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, state, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 300);
    query(&mut memo, readout, Some(AccuracyTarget::max_error(0.01)));
    let logical = pass2(memo, &[entry(60_000)]);
    assert_eq!(
        requirements_of(&logical.memo, readout),
        vec![Some(RequiredAccuracy {
            metric: AccuracyMetric::Rank,
            max_error: 0.01,
            failure_probability: 1.0,
        })]
    );
    let admitted: Vec<u32> = edges_into(&logical, own_candidate(readout), 0)
        .into_iter()
        .filter(|&(_, satisfies)| satisfies)
        .filter_map(|(producer, _)| {
            match logical
                .memo
                .candidate(producer)
                .logical_properties
                .parameters()
            {
                Some(SketchParams::Kll { k }) => Some(*k),
                _ => None,
            }
        })
        .collect();
    assert!(!admitted.is_empty());
    assert!(admitted.iter().all(|&k| k >= 269), "{admitted:?}");
    assert!(logical
        .pruned
        .iter()
        .filter(|p| p.candidate.group == state)
        .all(|p| p
            .causes
            .iter()
            .all(|cause| matches!(cause.root, RootPruneReason::BelowRequiredAccuracy { .. }))));
}

/// A whole-result failure probability with the
/// number of values per evaluation unknown gives no per-value one, so the
/// readouts the query reaches are pruned with `ValuesPerEvaluationUnknown`
/// instead of requiring δ = 0.
#[test]
fn a_whole_result_failure_probability_without_values_per_evaluation_prunes_the_readout() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 300);
    let query = memo.add_query(MemoQuery {
        root: QueryRoot::Operator(readout),
        accuracy_target: Some(AccuracyTarget::max_error(0.01).failure_probability(0.01)),
        number_of_values_per_query_evaluation: None,
    });
    let logical = pass2(memo, &[entry(60_000)]);
    let pruned = pruned_entry(&logical, own_candidate(readout)).expect("the readout is pruned");
    assert!(!pruned.causes.is_empty());
    for cause in &pruned.causes {
        assert!(
            matches!(
                cause.root,
                RootPruneReason::ValuesPerEvaluationUnknown { consumer, query: q }
                    if consumer == own_candidate(readout) && q == query
            ),
            "{cause:?}"
        );
    }
}

/// A readout another query gives a level is not pruned for a query whose
/// whole-result failure probability has no per-value one: Q0 (δ = 0.01,
/// values per evaluation unknown) and Q1 (no δ) read one KLL readout, and
/// the readout keeps Q1's level, live. Q0 reads its group, and the groups
/// below, only exactly: Q1's level says nothing about Q0's δ. Q1 may read
/// the readout and its state (only the rows below a build are exact for
/// both).
#[test]
fn a_readout_another_query_gives_a_level_survives_unknown_values_per_evaluation() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (window, state, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 300);
    let q0 = memo.add_query(MemoQuery {
        root: QueryRoot::Operator(readout),
        accuracy_target: Some(AccuracyTarget::max_error(0.01).failure_probability(0.01)),
        number_of_values_per_query_evaluation: None,
    });
    let q1 = query(&mut memo, readout, Some(AccuracyTarget::max_error(0.01)));
    let logical = pass2(memo, &[entry(60_000), entry(60_000)]);
    assert_eq!(
        requirements_of(&logical.memo, readout),
        vec![Some(RequiredAccuracy {
            metric: AccuracyMetric::Rank,
            max_error: 0.01,
            failure_probability: 1.0,
        })]
    );
    let pruned = pruned_entry(&logical, own_candidate(readout));
    assert!(pruned.is_none(), "{pruned:?}");
    let exact_only = &logical.exact_only_groups_of_query;
    for group in [readout, state, window] {
        assert!(exact_only[q0].contains(&group), "G{}", group.0);
    }
    for group in [readout, state] {
        assert!(!exact_only[q1].contains(&group), "G{}", group.0);
    }
}
