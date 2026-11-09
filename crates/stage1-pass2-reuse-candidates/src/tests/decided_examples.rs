//! The examples decided on 2026-11-07 (rule order, offsets in the gcd,
//! dead-candidate elimination with its causes, one range-free chain group,
//! constant error contracts, the mean of quantile readouts), each asserting
//! only the decided structure: the memo's groups and candidates, the
//! logical edges and Pass 2's pruned list.
use std::collections::BTreeMap;

use asap_types::ir::operator::operator_properties::SourceColumn;
use asap_types::stages::ValueSign;

use super::*;

/// The readouts Pass 1 offers one quantile group, per summary type.
struct QuantileReadouts {
    /// The `Aggregate` group `quantile_over_time(q, window)` per series.
    result: MemoGroupId,
    /// Per algorithm: its state group and the readout candidate of `result`.
    by_algorithm: Vec<(SketchAlgorithm, MemoGroupId, CandidateForMemoGroupId)>,
}

/// `quantile_over_time(q, window)` per series as Pass 1 leaves it: the
/// aggregate group with its exact candidate and, per algorithm, a state
/// group (one build per listed θ) and a candidate of the aggregate group
/// reading it (through a renaming `Project` over a readout group when the
/// column names differ, as Pass 1's `add_output_candidate` does).
fn quantile_over_time_readouts(
    memo: &mut Memo,
    window: MemoGroupId,
    q: f64,
    algorithms: &[SketchAlgorithm],
) -> QuantileReadouts {
    let result = group_with_own_candidate(
        memo,
        Operator::NonASAP(NonASAPOp::Aggregate {
            child: window,
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Quantile { col: None, q }],
            output_names: vec![String::new()],
            filters: vec![None],
            having: None,
        }),
    );
    let mut by_algorithm = Vec::new();
    for &algorithm in algorithms {
        let state = sketch_state(memo, window, algorithm, Reduction::PerEntity);
        let direct = Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: state,
            query: SketchStatistic::Quantile { q },
        });
        let id = match add_candidate(memo, result, direct, None) {
            Ok(id) => id,
            Err(CandidateError::OutputDiffersFromGroup { .. }) => {
                let readout = estimate(memo, state, SketchStatistic::Quantile { q });
                let to = &memo.groups()[result.0 as usize].group_properties.schema;
                let renamed = Operator::NonASAP(NonASAPOp::Project {
                    cols: to
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(column, field)| ProjectItem {
                            alias: Some(field.name.clone()),
                            expr: ScalarExpr::Column(column),
                        })
                        .collect(),
                    child: readout,
                });
                add_candidate(memo, result, renamed, None).unwrap()
            }
            Err(error) => panic!("{error}"),
        };
        by_algorithm.push((algorithm, state, id));
    }
    QuantileReadouts {
        result,
        by_algorithm,
    }
}

/// The readout of `state` a candidate `id` of a quantile group is or reads.
fn readout_of(memo: &Memo, id: CandidateForMemoGroupId) -> CandidateForMemoGroupId {
    match &memo.candidate(id).expression.operator {
        Operator::NonASAP(NonASAPOp::Project { child, .. }) => own_candidate(*child),
        _ => id,
    }
}

// ── (1) Rule order and reach (E3) ─────────────────────────────────────────

/// Decided 2026-11-07 (E3, rule order): chain groups come after the rollup
/// rule and the reach along candidate edges, so a state's widths come from
/// every query that reaches it through a candidate.
///
/// A `sum by (job, instance) (count_over_time(m[5m]))` every 60 s; B
/// `sum by (job) (count_over_time(m[5m]))` every 90 s (both at phase 0).
/// B's state rolls up A's, so both reach A's state, whose widths are the
/// gcd closure of {60 s, 90 s} with 300 s: 30 s and 60 s.
#[test]
fn e3_chain_groups_run_after_reach_so_a_rolled_up_query_adds_its_grid() {
    let mut memo = Memo::default();
    let input = scan(&mut memo, "m", &["job", "instance"]);
    let window = range(&mut memo, input, 300);
    let (by_job_instance, a) = exact_state(
        &mut memo,
        window,
        ExactKind::Count,
        Reduction::by(vec![2, 3]),
    );
    let (by_job, b) = exact_state(&mut memo, window, ExactKind::Count, Reduction::by(vec![2]));
    let query_a = query(&mut memo, a, Some(AccuracyTarget::exact()));
    let query_b = query(&mut memo, b, Some(AccuracyTarget::exact()));
    let logical = pass2(memo, &[entry(60_000), entry(90_000)]);
    let memo = &logical.memo;
    assert!(
        candidates_of(memo, by_job).iter().any(|c| {
            c.origin == OriginOfCandidateForMemoGroup::Rollup
                && c.expression.operator.children() == vec![&by_job_instance]
        }),
        "B's state reads A's through a rollup"
    );
    let reached_by = memo.groups()[by_job_instance.0 as usize].reached_by_queries();
    assert!(
        reached_by.contains(&query_a) && reached_by.contains(&query_b),
        "A's state is reached by A and B: {reached_by:?}"
    );
    assert_eq!(
        chain_widths_serving(memo, by_job_instance),
        BTreeSet::from([30_000, 60_000])
    );
    assert_eq!(chain_widths_serving(memo, by_job), BTreeSet::from([30_000]));
}

// ── (2) Offset in the gcd (E3) ────────────────────────────────────────────

/// Decided 2026-11-07 (E3, offsets): a window read at an `offset` takes the
/// offset into the gcd, since its windows' edges move by it.
/// `sum_over_time(m[5m] offset 45s)` every 60 s (phase 0): widths divide
/// 60 s, 300 s and 45 s, so 15 s only.
#[test]
fn e3_an_offset_enters_the_gcd_of_the_widths() {
    let mut memo = Memo::default();
    let input = scan(&mut memo, "m", &["job"]);
    let shifted = shift(&mut memo, input, 45, None);
    let window = range(&mut memo, shifted, 300);
    let (state, root) = exact_state(&mut memo, window, ExactKind::Sum, Reduction::PerEntity);
    query(&mut memo, root, Some(AccuracyTarget::exact()));
    let logical = pass2(memo, &[entry(60_000)]);
    assert_eq!(
        chain_widths_serving(&logical.memo, state),
        BTreeSet::from([15_000])
    );
}

// ── (3) Dead-candidate elimination and Pass 2's pruned list ──────────────

/// Decided 2026-11-07, concepts doc example "ratio of quantiles":
/// `quantile_over_time(0.99, m[5m]) / quantile_over_time(0.5, m[5m])`,
/// relative error 0.05, every 60 s; Pass 1 offers KLL and DDSketch to both
/// quantiles. The ratio carries only relative error, so each KLL readout is
/// pruned by the metric rule and every KLL build through it. The readouts
/// of DDSketch get the levels 0.02 … 0.001 (cap 0.045), so the builds with
/// alpha 0.1 and 0.05 miss every level they could serve. Chain groups are
/// added only for the surviving builds. `m`'s values are declared
/// non-negative, the sign DDSketch's guarantee needs.
#[test]
fn dead_candidates_are_pruned_and_listed_before_chain_groups() {
    let (logical, p99, p50) = pass2_of_ratio_of_quantiles(ValueSign::NonNegative);
    let memo = &logical.memo;
    let (kll_state, ddsketch_state) = (p99.by_algorithm[0].1, p99.by_algorithm[1].1);
    // Each KLL readout: the metric rule at the ratio, directly.
    for quantile in [&p99, &p50] {
        let kll_readout = quantile.by_algorithm[0].2;
        let pruned = pruned_entry(&logical, kll_readout).expect("the KLL readout is pruned");
        assert!(pruned.causes.iter().all(|cause| matches!(
            &cause.root,
            RootPruneReason::MetricRule { producer_metric: AccuracyMetric::Rank, consumer_carries, .. }
                if consumer_carries == &vec![AccuracyMetric::Relative]
        )));
    }
    // Every KLL build: through a pruned readout to the metric rule.
    for index_in_group in 0..parameter_list(SketchAlgorithm::Kll).len() {
        let build = CandidateForMemoGroupId {
            group: kll_state,
            index_in_group,
        };
        let pruned = pruned_entry(&logical, build).expect("every KLL build is pruned");
        assert!(!pruned.causes.is_empty());
        assert!(pruned.causes.iter().all(|cause| {
            !cause.through_pruned_consumers.is_empty()
                && matches!(cause.root, RootPruneReason::MetricRule { .. })
        }));
    }
    // DDSketch: alpha 0.1 and 0.05 pruned below every required accuracy;
    // the levels of the readouts are 0.02 … 0.001.
    let pruned_alphas: Vec<f64> = logical
        .pruned
        .iter()
        .filter(|pruned| pruned.candidate.group == ddsketch_state)
        .map(|pruned| {
            assert!(pruned
                .causes
                .iter()
                .all(|cause| cause.through_pruned_consumers.is_empty()
                    && matches!(cause.root, RootPruneReason::BelowRequiredAccuracy { .. })));
            match memo
                .candidate(pruned.candidate)
                .logical_properties
                .parameters()
            {
                Some(SketchParams::DDSketch { alpha }) => *alpha,
                other => panic!("{other:?}"),
            }
        })
        .collect();
    assert_eq!(pruned_alphas, vec![0.1, 0.05]);
    let readout_group = readout_of(memo, p99.by_algorithm[1].2).group;
    let mut levels: Vec<f64> = requirements_of(memo, readout_group)
        .into_iter()
        .map(|required| required.expect("a DDSketch readout's level").max_error)
        .collect();
    levels.sort_by(f64::total_cmp);
    assert_eq!(levels, vec![0.001, 0.002, 0.005, 0.01, 0.02]);
    // Chain builds exist only for the five surviving alphas.
    let chains = groups_where(memo, |e| {
        matches!(e, Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { summary_type, .. })
            if *summary_type == FieldDataType::Sketch(SketchAlgorithm::DDSketch))
    });
    assert_eq!(chains.len(), 1);
    assert_eq!(candidates_of(memo, chains[0]).len(), 5);
    assert!(groups_where(memo, |e| matches!(
        e,
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { summary_type, .. })
            if *summary_type == FieldDataType::Sketch(SketchAlgorithm::Kll)
    ))
    .is_empty());
    // No logical edge names a pruned candidate.
    let pruned: BTreeSet<_> = logical.pruned.iter().map(|p| p.candidate).collect();
    assert!(logical
        .edges
        .iter()
        .all(|edge| !pruned.contains(&edge.producer_candidate_id)
            && !pruned.contains(&edge.consumer_candidate_id)));
    let _ = kll_state;
}

/// The ratio of quantiles of [`dead_candidates_are_pruned_and_listed_before_chain_groups`]
/// after Pass 2, with `m`'s values declared of sign `sign`, and its p99
/// and p50.
fn pass2_of_ratio_of_quantiles(
    sign: ValueSign,
) -> (
    LogicalASAPMemoCandidates,
    QuantileReadouts,
    QuantileReadouts,
) {
    let mut memo = Memo::new(BTreeMap::from([(
        SourceColumn {
            source: "m".into(),
            column: "value".into(),
        },
        sign,
    )]));
    let input = scan(&mut memo, "m", &["job"]);
    let window = range(&mut memo, input, 300);
    let algorithms = [SketchAlgorithm::Kll, SketchAlgorithm::DDSketch];
    let p99 = quantile_over_time_readouts(&mut memo, window, 0.99, &algorithms);
    let p50 = quantile_over_time_readouts(&mut memo, window, 0.5, &algorithms);
    let ratio = arithmetic(&mut memo, ArithmeticOpKind::Div, p99.result, p50.result);
    let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    query(&mut memo, ratio, Some(target));
    (pass2(memo, &[entry(60_000)]), p99, p50)
}

/// Decided 2026-11-08 (sign conditions): DDSketch's relative error on a
/// quantile needs values of one sign. Over values of unknown sign no
/// DDSketch build meets any readout's requirement.
#[test]
fn ddsketch_quantiles_over_values_of_unknown_sign_have_no_guarantee() {
    let (logical, p99, _) = pass2_of_ratio_of_quantiles(ValueSign::Unknown);
    let ddsketch_state = p99.by_algorithm[1].1;
    for index_in_group in 0..parameter_list(SketchAlgorithm::DDSketch).len() {
        let build = CandidateForMemoGroupId {
            group: ddsketch_state,
            index_in_group,
        };
        let pruned = pruned_entry(&logical, build).expect("every DDSketch build is pruned");
        assert!(pruned
            .causes
            .iter()
            .all(|cause| matches!(cause.root, RootPruneReason::BelowRequiredAccuracy { .. })));
    }
}

// ── (4) One range-free chain group across ranges and offsets (E2) ─────────

/// Decided 2026-11-07 (E2, design (c)): a tumbling-window chain is a group
/// of its own, `SummaryTumblingWindowChain` over the rows with a width and
/// no range, offset or window count, so every range and offset of one rows
/// and width reads the same chain group through `TimeRange` (and
/// `TimeShift`) groups and a `SummaryMerge`.
///
/// A `sum_over_time(m[5m])`, B `sum_over_time(m[5m] offset 1h)`, C
/// `sum_over_time(m[1h])`, each every 60 s (phase 0): one chain group Gw of
/// width 60 s over the scan of m; the groups `TimeShift(1h, Gw)`,
/// `TimeRange(5m, Gw)`, `TimeRange(5m, TimeShift(1h, Gw))` and
/// `TimeRange(1h, Gw)`; each state group has a `SummaryMerge` candidate
/// over its `TimeRange` group.
#[test]
fn e2_one_range_free_chain_group_serves_every_range_and_offset() {
    let mut memo = Memo::default();
    let input = scan(&mut memo, "m", &["job"]);
    let five_minutes = range(&mut memo, input, 300);
    let shifted_input = shift(&mut memo, input, 3600, None);
    let five_minutes_an_hour_ago = range(&mut memo, shifted_input, 300);
    let one_hour = range(&mut memo, input, 3600);
    let mut states = Vec::new();
    for window in [five_minutes, five_minutes_an_hour_ago, one_hour] {
        let (state, root) = exact_state(&mut memo, window, ExactKind::Sum, Reduction::PerEntity);
        query(&mut memo, root, Some(AccuracyTarget::exact()));
        states.push(state);
    }
    let logical = pass2(memo, &vec![entry(60_000); 3]);
    let memo = &logical.memo;
    let chains = groups_where(memo, |e| {
        matches!(e, Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }))
    });
    let [chain] = chains[..] else {
        panic!("one chain group, found {chains:?}");
    };
    let Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child,
        tumbling_window_width_ms,
        ..
    }) = &memo.groups()[chain.0 as usize].expression
    else {
        unreachable!();
    };
    assert_eq!(*tumbling_window_width_ms, 60_000);
    assert_eq!(
        *child, input,
        "the chain reads the scan: no range or shift below it"
    );
    let range_over = |range_s: u64, child: MemoGroupId| {
        group_of(
            memo,
            &Operator::NonASAP(NonASAPOp::TimeRange {
                range: Duration::from_secs(range_s),
                kind: TimeRangeKind::Range,
                child,
            }),
        )
    };
    let shifted_chain = group_of(
        memo,
        &Operator::NonASAP(NonASAPOp::TimeShift {
            shift: TimeShift {
                offset_ms: 3_600_000,
                at: None,
            },
            child: chain,
        }),
    )
    .expect("TimeShift(1h, Gw)");
    let ranges = [
        range_over(300, chain).expect("TimeRange(5m, Gw)"),
        range_over(300, shifted_chain).expect("TimeRange(5m, TimeShift(1h, Gw))"),
        range_over(3600, chain).expect("TimeRange(1h, Gw)"),
    ];
    for (state, range) in states.iter().zip(ranges) {
        assert!(
            candidates_of(memo, *state).iter().any(|c| matches!(
                &c.expression.operator,
                Operator::ASAP(ASAPOp::SummaryMerge { children, group_by: None })
                    if *children == vec![range]
            )),
            "G{} reads G{} through a SummaryMerge",
            state.0,
            range.0
        );
    }
}

// ── (5) Constant error contracts (E1) ─────────────────────────────────────

/// Decided 2026-11-07 (E1): a `Filter` whose predicate reads only label
/// columns keeps or drops whole rows, so a value's error passes through it
/// unchanged. `quantile_over_time(0.99, latency[5m])` behind `job = "api"`,
/// relative error 0.05: the DDSketch readout keeps an admissible edge into
/// the filter. (The contract alone is tested in `asap_types::stages`.)
#[test]
fn e1_a_key_only_filter_keeps_the_ddsketch_readout_admissible() {
    let mut memo = Memo::default();
    let input = scan(&mut memo, "latency", &["job"]);
    let window = range(&mut memo, input, 300);
    let p99 = quantile_over_time_readouts(&mut memo, window, 0.99, &[SketchAlgorithm::DDSketch]);
    let job = memo.groups()[p99.result.0 as usize]
        .group_properties
        .schema
        .fields
        .iter()
        .position(|field| field.name == "job")
        .expect("the result keeps the job label");
    let filtered = group_with_own_candidate(
        &mut memo,
        Operator::NonASAP(NonASAPOp::Filter {
            pred: Predicate(equals(job, "api")),
            child: p99.result,
        }),
    );
    let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    query(&mut memo, filtered, Some(target));
    let logical = pass2(memo, &[entry(60_000)]);
    let ddsketch_readout = p99.by_algorithm[0].2;
    let edges = edges_into(&logical, own_candidate(filtered), 0);
    assert!(
        edges.contains(&(ddsketch_readout, true)),
        "edges into the filter: {edges:?}"
    );
}

// ── (6) H2: avg over relative errors needs values of one sign ──────────

/// Decided 2026-11-07 (H2) and 2026-11-08 (sign facts):
/// `avg(quantile_over_time(0.99, m[5m]))` with a relative target. A mean
/// of values with relative errors is off by at most the largest of them
/// only when the values have one sign. With `m`'s values of unknown sign
/// the `avg` has no relative contract, so no sketch readout feeds it and it
/// reads the exact quantile only; with `m` declared non-negative the
/// DDSketch readout (relative error) feeds it, the KLL readout (rank) does
/// not.
#[test]
fn h2_avg_of_readouts_under_a_relative_target_needs_values_of_one_sign() {
    for sign in [ValueSign::Unknown, ValueSign::NonNegative] {
        let mut memo = Memo::new(BTreeMap::from([(
            SourceColumn {
                source: "m".into(),
                column: "value".into(),
            },
            sign,
        )]));
        let input = scan(&mut memo, "m", &["job"]);
        let window = range(&mut memo, input, 300);
        let algorithms = [SketchAlgorithm::Kll, SketchAlgorithm::DDSketch];
        let p99 = quantile_over_time_readouts(&mut memo, window, 0.99, &algorithms);
        let avg = group_with_own_candidate(
            &mut memo,
            Operator::NonASAP(NonASAPOp::Aggregate {
                child: p99.result,
                reduction: Reduction::by(vec![]),
                measures: vec![AggIntent::Avg { col: None }],
                output_names: vec!["avg".into()],
                filters: vec![None],
                having: None,
            }),
        );
        let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
        query(&mut memo, avg, Some(target));
        let logical = pass2(memo, &[entry(60_000)]);
        let edges = edges_into(&logical, own_candidate(avg), 0);
        let (kll_readout, ddsketch_readout) = (p99.by_algorithm[0].2, p99.by_algorithm[1].2);
        assert!(!edges.contains(&(kll_readout, true)), "{sign:?}: {edges:?}");
        assert_eq!(
            edges.contains(&(ddsketch_readout, true)),
            sign == ValueSign::NonNegative,
            "{sign:?}: {edges:?}"
        );
        assert!(
            edges.contains(&(own_candidate(p99.result), true)),
            "the exact quantile stays admissible: {edges:?}"
        );
    }
}
