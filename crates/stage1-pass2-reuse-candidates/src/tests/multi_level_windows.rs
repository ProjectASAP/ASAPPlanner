//! Step 5, wider chain levels and covers (design doc
//! `multi-level-window-chains.md`).
use std::collections::BTreeSet;

use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::stages::{Memo, MemoGroupId};
use asap_types::summary_type::{ExactKind, SketchAlgorithm};
use asap_types::workload::DataWorkload;

use crate::test_fixtures::*;
use asap_types::test_support::memo_builders::*;
use asap_types::test_support::memo_fixtures::range;

/// The widths of the chain groups of `memo`.
fn chain_widths(memo: &Memo) -> BTreeSet<u64> {
    memo.groups()
        .iter()
        .filter(|group| !is_compressed_samples_chain(&group.expression))
        .filter_map(|group| group.expression.chain_tumbling_window_width_ms())
        .collect()
}

/// Whether `expression` is a compressed-samples chain: Pass 2 offers those
/// for every raw range, at their own widths, and never levels them.
fn is_compressed_samples_chain(expression: &Operator<MemoGroupId>) -> bool {
    matches!(
        expression,
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            summary_type: FieldDataType::CompressedSamples,
            ..
        })
    )
}

/// The widths of the levels each `TimeRangeCover` that state group `state`
/// merges over reads, widest first.
fn covers_merged_by(memo: &Memo, state: MemoGroupId) -> BTreeSet<Vec<u64>> {
    candidates_of(memo, state)
        .iter()
        .filter_map(|candidate| match &candidate.expression.operator {
            Operator::ASAP(ASAPOp::SummaryMerge {
                children,
                group_by: None,
            }) => match &memo.groups()[children[0].0 as usize].expression {
                Operator::NonASAP(NonASAPOp::TimeRangeCover { levels, .. }) => Some(
                    levels
                        .iter()
                        .map(|level| {
                            memo.groups()[level.0 as usize]
                                .expression
                                .chain_tumbling_window_width_ms()
                                .unwrap()
                        })
                        .collect(),
                ),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// T3. `quantile_over_time(0.99, m[1h])` every 15 s: chain groups at 15 s,
/// 1 min, 5 min and 15 min, not 1 h (2 · 1 h > 1 h). Each wider level is
/// built only from the next narrower one, a `SummaryMergeWindows` per θ,
/// never from the rows; the state group merges over two covers: over the
/// first wider level and over all of them, widest first.
#[test]
fn a_long_range_gets_wider_levels() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, state, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.99, 3_600);
    query(&mut memo, readout, None);
    let logical = pass2(memo, &[entry(15_000)]);
    let memo = &logical.memo;
    assert_eq!(
        chain_widths(memo),
        BTreeSet::from([15_000, 60_000, 300_000, 900_000])
    );
    for group in memo.groups() {
        if is_compressed_samples_chain(&group.expression) {
            continue;
        }
        let Some(width) = group.expression.chain_tumbling_window_width_ms() else {
            continue;
        };
        if width == 15_000 {
            continue;
        }
        assert!(!group.candidates.is_empty(), "{width} ms");
        for candidate in &group.candidates {
            let Operator::ASAP(ASAPOp::SummaryMergeWindows {
                child,
                tumbling_window_width_ms,
            }) = &candidate.expression.operator
            else {
                panic!("a wider level is built from a narrower one: {candidate:?}");
            };
            assert_eq!(*tumbling_window_width_ms, width);
            let narrower = memo.groups()[child.0 as usize]
                .expression
                .chain_tumbling_window_width_ms()
                .unwrap();
            let expected = match width {
                60_000 => 15_000,
                300_000 => 60_000,
                _ => 300_000,
            };
            assert_eq!(narrower, expected);
        }
    }
    assert_eq!(
        covers_merged_by(memo, state),
        BTreeSet::from([vec![60_000, 15_000], vec![900_000, 300_000, 60_000, 15_000],])
    );
}

/// T4. `m[1m]` every 15 s: no width of the ladder fits twice in the range,
/// so no wider level and no cover; the chain is today's 15 s one.
#[test]
fn a_short_range_gets_no_wider_level() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, state, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.99, 60);
    query(&mut memo, readout, None);
    let logical = pass2(memo, &[entry(15_000)]);
    assert_eq!(chain_widths(&logical.memo), BTreeSet::from([15_000]));
    assert!(covers_merged_by(&logical.memo, state).is_empty());
}

/// T5. A state whose summary does not merge (`irate`) gets no chain, so no
/// wider level, over a range of an hour.
#[test]
fn wider_levels_need_a_merging_summary() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan_group(&mut memo, "m", &["job"], vec![]);
    let window = group(&mut memo, range(3_600_000, input));
    let (_, root) = exact_state(&mut memo, window, ExactKind::IRate, Reduction::PerEntity);
    query(&mut memo, root, None);
    let logical = pass2(memo, &[entry(15_000)]);
    assert!(chain_widths(&logical.memo).is_empty());
}
