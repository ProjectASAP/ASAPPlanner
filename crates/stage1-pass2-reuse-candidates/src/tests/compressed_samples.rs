//! Compressed-sample chains and the `DecodeSamplesInRange` candidates of
//! raw-range groups (design doc `compressed-sample-windows.md`, sections
//! 2.3 and 3.1; tests P1–P5).
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use asap_types::ir::operator::arguments::{GroupKeys, Reduction, Source};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, TimeRangeKind};
use asap_types::stages::{
    CandidateForMemoGroupId, Memo, MemoGroupId, OriginOfCandidateForMemoGroup, RootPruneReason,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::test_fixtures::*;
use asap_types::test_support::memo_builders::*;
use asap_types::test_support::memo_fixtures::{aggregate, range, shift};

/// The ladder widths a compressed-samples chain is offered at when its
/// decode reads the unsealed window (section 3.1): 1 min, 5 min, 15 min.
const LADDER_MS: [u64; 3] = [60_000, 300_000, 900_000];

/// One `DecodeSamplesInRange` candidate of a raw-range group.
#[derive(Debug)]
struct Decode {
    candidate: CandidateForMemoGroupId,
    /// The compressed-samples chain group it reads, under any shifts.
    chain: MemoGroupId,
    range: Duration,
    kind: TimeRangeKind,
    labels: Vec<String>,
    reads_unsealed_window: bool,
    origin: OriginOfCandidateForMemoGroup,
}

/// The group `group` reads through `TimeShift` groups.
fn below_shifts(memo: &Memo, mut group: MemoGroupId) -> MemoGroupId {
    while let Operator::NonASAP(NonASAPOp::TimeShift { child, .. }) =
        &memo.groups()[group.0 as usize].expression
    {
        group = *child;
    }
    group
}

/// The width of `group` when it is a compressed-samples chain group.
fn compressed_chain_width(memo: &Memo, group: MemoGroupId) -> Option<u64> {
    match &memo.groups()[group.0 as usize].expression {
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
            summary_type: FieldDataType::CompressedSamples,
            tumbling_window_width_ms,
            ..
        }) => Some(*tumbling_window_width_ms),
        _ => None,
    }
}

/// Every compressed-samples chain group of `memo`.
fn compressed_chains(memo: &Memo) -> Vec<MemoGroupId> {
    groups_where(memo, |expression| {
        matches!(
            expression,
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                summary_type: FieldDataType::CompressedSamples,
                ..
            })
        )
    })
}

/// The metric the chain group `chain` is built over, and the columns of
/// the scan it reads.
fn chain_scan(memo: &Memo, chain: MemoGroupId) -> (String, Vec<String>) {
    let Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { child, .. }) =
        &memo.groups()[chain.0 as usize].expression
    else {
        panic!("not a chain: {chain:?}");
    };
    let Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries { metric },
        schema,
        ..
    }) = &memo.groups()[child.0 as usize].expression
    else {
        panic!("a compressed chain reads a series scan");
    };
    (
        metric.clone(),
        schema
            .fields
            .iter()
            .map(|field| field.name.clone())
            .collect(),
    )
}

/// The `DecodeSamplesInRange` candidates of `group`.
fn decodes_of(memo: &Memo, group: MemoGroupId) -> Vec<Decode> {
    candidates_of(memo, group)
        .iter()
        .enumerate()
        .filter_map(
            |(index_in_group, candidate)| match &candidate.expression.operator {
                Operator::ASAP(ASAPOp::DecodeSamplesInRange {
                    child,
                    range,
                    kind,
                    labels,
                    reads_unsealed_window,
                }) => Some(Decode {
                    candidate: CandidateForMemoGroupId {
                        group,
                        index_in_group,
                    },
                    chain: below_shifts(memo, *child),
                    range: *range,
                    kind: *kind,
                    labels: labels.iter().map(|field| field.name.clone()).collect(),
                    reads_unsealed_window: *reads_unsealed_window,
                    origin: candidate.origin.clone(),
                }),
                _ => None,
            },
        )
        .collect()
}

/// A per-series exact `measure` over `metric[seconds]`, the scan
/// projecting `labels`, as a query root: the range group and the root.
fn over_time(
    memo: &mut Memo,
    metric: &str,
    labels: &[&str],
    measure: AggIntent,
    seconds: i64,
) -> (MemoGroupId, MemoGroupId) {
    let input = scan_group(memo, metric, labels, vec![]);
    let window = group(memo, range(seconds * 1000, input));
    let root = group(memo, aggregate(Reduction::PerEntity, measure, window));
    query(memo, root, None);
    (window, root)
}

/// The typical-dashboard shape of section 1 (per-series parts only): the
/// gauge `data` read over 5 min to 1 h by scans projecting `job`,
/// `region`, and `region, service`, and the counter `data_total` by scans
/// projecting `job` and `region`, refreshed every 10–30 s. Returns the
/// range groups with their metric and labels, and the workload entries.
fn typical_dashboard(
    memo: &mut Memo,
) -> (
    Vec<(MemoGroupId, &'static str, Vec<String>)>,
    Vec<QueryWorkloadEntry>,
) {
    let readers: [(&str, &[&str], AggIntent, i64, u32); 9] = [
        ("data", &["job"], AggIntent::Avg { col: None }, 300, 10_000),
        ("data", &["job"], AggIntent::Max { col: None }, 900, 15_000),
        (
            "data",
            &["job"],
            AggIntent::Quantile { col: None, q: 0.99 },
            900,
            20_000,
        ),
        (
            "data",
            &["region", "service"],
            AggIntent::Avg { col: None },
            1_800,
            30_000,
        ),
        (
            "data",
            &["region"],
            AggIntent::Max { col: None },
            3_600,
            30_000,
        ),
        (
            "data",
            &["region"],
            AggIntent::Quantile { col: None, q: 0.5 },
            3_600,
            30_000,
        ),
        ("data_total", &["job"], AggIntent::Rate, 300, 10_000),
        (
            "data_total",
            &["region"],
            AggIntent::Increase,
            3_600,
            30_000,
        ),
        ("data_total", &["job"], AggIntent::Increase, 900, 20_000),
    ];
    let mut windows = Vec::new();
    let mut entries = Vec::new();
    for (metric, labels, measure, seconds, interval_ms) in readers {
        let (window, _) = over_time(memo, metric, labels, measure, seconds);
        windows.push((
            window,
            metric,
            labels.iter().map(|label| label.to_string()).collect(),
        ));
        entries.push(entry(interval_ms));
    }
    (windows, entries)
}

/// P1. Raw data not retained (no query-time scan is a candidate here): each
/// raw-range group of the typical-dashboard shape gains exactly one
/// unsealed-read decode per ladder width, reading a chain group keyed by
/// series identity only, so the three `data` scans that differ in their
/// projected labels share one chain per width: 2 metrics × 3 widths. The
/// decode carries the group's range and labels. Fails if Pass 2 adds no
/// decode, keys the chain by the projected labels (5 chains per width), or
/// drops a width.
#[test]
fn raw_range_groups_get_decode_candidates() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (windows, entries) = typical_dashboard(&mut memo);
    let logical = pass2(memo, &entries);
    let memo = &logical.memo;

    let mut ladder_chains: BTreeMap<(String, u64), MemoGroupId> = BTreeMap::new();
    for (window, metric, labels) in &windows {
        let Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) =
            &memo.groups()[window.0 as usize].expression
        else {
            panic!("not a range group");
        };
        let unsealed: Vec<Decode> = decodes_of(memo, *window)
            .into_iter()
            .filter(|decode| decode.reads_unsealed_window)
            .collect();
        let widths: Vec<u64> = unsealed
            .iter()
            .map(|decode| compressed_chain_width(memo, decode.chain).expect("a compressed chain"))
            .collect();
        assert_eq!(
            widths.iter().copied().collect::<BTreeSet<_>>(),
            BTreeSet::from(LADDER_MS),
            "{metric} {labels:?}"
        );
        assert_eq!(widths.len(), LADDER_MS.len(), "one decode per width");
        for decode in unsealed {
            assert_eq!(decode.range, *range);
            assert_eq!(decode.kind, TimeRangeKind::Range);
            assert_eq!(&decode.labels, labels);
            assert_eq!(
                decode.origin,
                OriginOfCandidateForMemoGroup::CompressedSampleWindows
            );
            let (chain_metric, columns) = chain_scan(memo, decode.chain);
            assert_eq!(chain_metric, *metric);
            assert_eq!(
                columns,
                ["ts", "value", "$promql_series_identity"],
                "keyed by series identity only"
            );
            let width = compressed_chain_width(memo, decode.chain).unwrap();
            let previous = ladder_chains.insert((metric.to_string(), width), decode.chain);
            if let Some(previous) = previous {
                assert_eq!(previous, decode.chain, "one chain per metric and width");
            }
        }
    }
    assert_eq!(ladder_chains.len(), 2 * LADDER_MS.len());
}

/// P2. `m[5m] offset 7s` every 10 s: the 5 min chain's windows end neither
/// on the 10 s grid nor at the offset, yet the unsealed-read decode over it
/// (through the shift) keeps every edge and is not pruned, since it trims
/// both edges and reads the open window. Fails if the grid rule
/// (`ChainWidthDoesNotDivideEvaluationGrid`) is applied to it.
#[test]
fn unsealed_decode_edges_ignore_the_evaluation_grid() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan_group(&mut memo, "m", &["job"], vec![]);
    let shifted = group(&mut memo, shift(7_000, None, input));
    let window = group(&mut memo, range(300_000, shifted));
    let root = group(
        &mut memo,
        aggregate(Reduction::PerEntity, AggIntent::Max { col: None }, window),
    );
    query(&mut memo, root, None);
    let logical = pass2(memo, &[entry(10_000)]);
    let memo = &logical.memo;

    let five_minutes: Vec<Decode> = decodes_of(memo, window)
        .into_iter()
        .filter(|decode| {
            decode.reads_unsealed_window
                && compressed_chain_width(memo, decode.chain) == Some(300_000)
        })
        .collect();
    assert_eq!(five_minutes.len(), 1);
    let decode = &five_minutes[0];
    assert!(pruned_entry(&logical, decode.candidate).is_none());
    let into_decode: Vec<_> = logical
        .edges
        .iter()
        .filter(|edge| edge.consumer_candidate_id == decode.candidate)
        .collect();
    assert!(!into_decode.is_empty());
    assert!(into_decode
        .iter()
        .all(|edge| edge.satisfies_logical_requirement));
    assert!(!logical.failing_logical_edges.iter().any(|(edge, reason)| {
        edge.consumer_candidate_id == decode.candidate
            && matches!(
                reason,
                RootPruneReason::ChainWidthDoesNotDivideEvaluationGrid { .. }
            )
    }));
}

/// P3. A SQL table with no (key, time) unique key is not a series source:
/// its raw-range group gets no decode and no compressed chain is built.
/// Passes once the vocabulary exists; it guards the series-source rule
/// (a wrong rule would encode rows of unrelated entities into one block).
#[test]
fn no_decode_for_non_series_sources() {
    let mut memo = Memo::new(&DataWorkload::default());
    let table = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: "events".into(),
            },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("latency", DataType::Float64, false),
                    Field::plain("user_id", DataType::Utf8, false),
                ],
                0,
                vec![],
            ),
        }),
    );
    let window = group(&mut memo, range(300_000, table));
    let root = group(
        &mut memo,
        aggregate(
            Reduction::Reduce(GroupKeys::by(vec![])),
            AggIntent::Max { col: None },
            window,
        ),
    );
    query(&mut memo, root, None);
    let logical = pass2(memo, &[entry(10_000)]);
    assert!(decodes_of(&logical.memo, window).is_empty());
    assert!(compressed_chains(&logical.memo).is_empty());
}

/// P4. Every candidate that reads a compressed-samples chain group
/// (directly or through shifts) is a `DecodeSamplesInRange`: no merge, no
/// range selection, no cover and no wider level reads it (section 3.2).
/// Fails if `add_chain_groups`, the rollup or a level rule treats the new
/// state as an ordinary mergeable summary.
#[test]
fn compressed_chains_are_read_only_by_decodes() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, entries) = typical_dashboard(&mut memo);
    let logical = pass2(memo, &entries);
    let memo = &logical.memo;
    let chains: BTreeSet<MemoGroupId> = compressed_chains(memo).into_iter().collect();
    assert!(!chains.is_empty());
    for (group, memo_group) in memo.groups().iter().enumerate() {
        let group = MemoGroupId(group as u32);
        if matches!(
            memo_group.expression,
            Operator::NonASAP(NonASAPOp::TimeShift { .. })
        ) {
            continue;
        }
        for candidate in &memo_group.candidates {
            let reads_a_chain = candidate
                .expression
                .operator
                .children()
                .into_iter()
                .any(|child| chains.contains(&below_shifts(memo, *child)));
            if reads_a_chain {
                assert!(
                    matches!(
                        candidate.expression.operator,
                        Operator::ASAP(ASAPOp::DecodeSamplesInRange { .. })
                    ),
                    "{group:?}: {:?}",
                    candidate.expression.operator
                );
            }
        }
    }
}

/// P5. The sealed-read decodes of each raw-range group read chains at the
/// widths `add_chain_groups` would use for it — the gcd closure of its
/// readers' period, range and offsets — and an edge from such a chain
/// fails with `ChainWidthDoesNotDivideEvaluationGrid` exactly when the
/// width does not divide the consuming group's grid. `m[30s]` read every
/// 15 s and every 10 s: widths {5 s, 10 s, 15 s}; only the 5 s decode keeps
/// its edges. Fails if sealed decodes are missing (a deployment without
/// unsealed-window reads would have no compressed candidate), use ladder
/// widths, or skip the grid rule.
#[test]
fn sealed_decodes_use_aligned_widths() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan_group(&mut memo, "m", &["job"], vec![]);
    let window = group(&mut memo, range(30_000, input));
    let root = group(
        &mut memo,
        aggregate(Reduction::PerEntity, AggIntent::Max { col: None }, window),
    );
    query(&mut memo, root, None);
    query(&mut memo, root, None);
    let logical = pass2(memo, &[entry(15_000), entry(10_000)]);
    let memo = &logical.memo;

    let sealed: Vec<Decode> = decodes_of(memo, window)
        .into_iter()
        .filter(|decode| !decode.reads_unsealed_window)
        .collect();
    let widths: BTreeSet<u64> = sealed
        .iter()
        .map(|decode| compressed_chain_width(memo, decode.chain).expect("a compressed chain"))
        .collect();
    assert_eq!(widths, BTreeSet::from([5_000, 10_000, 15_000]));
    for decode in &sealed {
        let width = compressed_chain_width(memo, decode.chain).unwrap();
        let off_grid = logical.failing_logical_edges.iter().any(|(edge, reason)| {
            edge.consumer_candidate_id == decode.candidate
                && matches!(
                    reason,
                    RootPruneReason::ChainWidthDoesNotDivideEvaluationGrid {
                        evaluation_period_ms: Some(5_000),
                        ..
                    }
                )
        });
        assert_eq!(off_grid, width != 5_000, "{width} ms");
        assert_eq!(
            pruned_entry(&logical, decode.candidate).is_none(),
            width == 5_000,
            "{width} ms"
        );
    }
}
