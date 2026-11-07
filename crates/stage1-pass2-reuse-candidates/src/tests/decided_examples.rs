//! The examples decided on 2026-11-07, written as tests before their
//! implementation (failing-test-first). Each test's doc comment states the
//! decision and the change that makes it pass; a test that already passes
//! is a regression guard. Like the other Pass 2 tests, each builds its
//! input memo through the `Memo` methods with the candidates Pass 1 would
//! add (no other stage crate is called) and asserts only on the decided
//! structure: the memo's groups and candidates, the logical edges and
//! Pass 2's pruned list.
use std::collections::BTreeSet;

use asap_types::ir::operator::operator_properties::TimeShift;
use asap_types::ir::ProjectItem;
use asap_types::stages::CandidateError;
use asap_types::workload::TimestampMs;

use super::*;

// ── Fixtures ──────────────────────────────────────────────────────────────

/// A query repeating every `interval_ms` at phase 0 (`FixedIntervalAt`)
/// with `accuracy`.
fn entry_at_phase_0(interval_ms: u32, accuracy: AccuracyTarget) -> QueryWorkloadEntry {
    QueryWorkloadEntry {
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
            interval: RepetitionInterval(interval_ms),
            evaluation_phase: TimestampMs(0),
        }),
        ..entry(interval_ms, accuracy)
    }
}

/// `child offset <seconds>`.
fn offset(memo: &mut Memo, child: MemoGroupId, seconds: i64) -> MemoGroupId {
    group(
        memo,
        Operator::NonASAP(NonASAPOp::TimeShift {
            shift: TimeShift {
                offset_ms: seconds * 1000,
                at: None,
            },
            child,
        }),
    )
}

/// An exact accumulator state of `kind` over `window` grouped by
/// `reduction`, read by a `FinalizeExactAccumulator` group (the query
/// root): what Pass 1 offers for `sum_over_time` (per series) or for a
/// `sum by (keys)` of `count_over_time` (the count by keys).
fn exact_state(
    memo: &mut Memo,
    window: MemoGroupId,
    kind: ExactKind,
    reduction: Reduction,
) -> (MemoGroupId, MemoGroupId) {
    let state = group(memo, build(window, FieldDataType::exact(kind), reduction));
    let finalized = group(
        memo,
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: state }),
    );
    (state, finalized)
}

/// The readouts Pass 1 offers one quantile group, per family.
struct QuantileReadouts {
    /// The `Aggregate` group `quantile_over_time(q, window)` per series.
    result: MemoGroupId,
    /// Per family: its state group and the readout candidate of `result`.
    by_family: Vec<(SketchAlgorithm, MemoGroupId, CandidateForMemoGroupId)>,
}

/// `quantile_over_time(q, window)` per series as Pass 1 leaves it: the
/// aggregate group with its exact candidate and, per family, a state group
/// (one build per listed θ), a readout group over it, and a candidate of
/// the aggregate group reading it (renamed by a `Project` when the column
/// names differ, as Pass 1's `add_output_candidate` does).
fn quantile_over_time_readouts(
    memo: &mut Memo,
    window: MemoGroupId,
    q: f64,
    families: &[SketchAlgorithm],
) -> QuantileReadouts {
    let result = group(
        memo,
        Operator::NonASAP(NonASAPOp::Aggregate {
            child: window,
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Quantile {
                col: None,
                q,
                accuracy: AccuracyTarget::max_error(0.05),
            }],
            output_names: vec![String::new()],
            filters: vec![None],
            having: None,
        }),
    );
    let mut by_family = Vec::new();
    for &algorithm in families {
        let state = sketch_state(memo, window, algorithm, Reduction::PerEntity);
        let readout = estimate(memo, state, SketchStatistic::Quantile { q });
        let renamed = {
            let to = &memo.groups()[result.0 as usize].group_properties.schema;
            Operator::NonASAP(NonASAPOp::Project {
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
            })
        };
        let direct = Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: state,
            query: SketchStatistic::Quantile { q },
        });
        let id = match try_candidate(memo, result, direct) {
            Ok(id) => id,
            Err(CandidateError::OutputDiffersFromGroup { .. }) => {
                try_candidate(memo, result, renamed).expect("the renamed readout")
            }
            Err(error) => panic!("{error}"),
        };
        by_family.push((algorithm, state, id));
    }
    QuantileReadouts { result, by_family }
}

/// Adds `operator` (no θ, no window) to `group`, as Pass 1 does.
fn try_candidate(
    memo: &mut Memo,
    group: MemoGroupId,
    operator: Operator<MemoGroupId>,
) -> Result<CandidateForMemoGroupId, CandidateError> {
    let expression = CandidateExpression { operator };
    let logical_properties = LogicalProperties::new(&expression, None, None).unwrap();
    let input_requirements =
        vec![LogicalRequirements::new(None); expression.operator.children().len()];
    memo.add_candidate(
        group,
        CandidateForMemoGroup {
            expression,
            logical_properties,
            input_requirements,
            fused_groups: Vec::new(),
            origin: OriginOfCandidateForMemoGroup::Pass1,
        },
    )
}

/// The tumbling-window widths that serve state group `state`, in either
/// representation, so the tests hold before and after E2's design (c):
/// a candidate with a tumbling layout (today), or a candidate
/// `SummaryMerge` over `TimeRange`/`TimeShift` groups over a
/// `SummaryTumblingWindowChain` group (design (c)).
fn tumbling_widths_serving(memo: &Memo, state: MemoGroupId) -> BTreeSet<u64> {
    let mut widths = BTreeSet::new();
    for candidate in candidates_of(memo, state) {
        if let Some(WindowLayout::Tumbling { width_ms }) =
            candidate.logical_properties.window_layout()
        {
            widths.insert(*width_ms);
        }
        if let Operator::ASAP(ASAPOp::SummaryMerge { children, .. }) =
            &candidate.expression.operator
        {
            for &child in children {
                if let Some(width_ms) = chain_width_below(memo, child) {
                    widths.insert(width_ms);
                }
            }
        }
    }
    widths
}

/// The width of the tumbling-window chain `group` reads through
/// `TimeRange` and `TimeShift` groups, if any.
fn chain_width_below(memo: &Memo, mut group: MemoGroupId) -> Option<u64> {
    loop {
        match &memo.groups()[group.0 as usize].expression {
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => return Some(*tumbling_window_width_ms),
            Operator::NonASAP(
                NonASAPOp::TimeRange { child, .. } | NonASAPOp::TimeShift { child, .. },
            ) => group = *child,
            _ => return None,
        }
    }
}

/// The groups whose expression matches `pattern`.
fn groups_where(memo: &Memo, pattern: impl Fn(&Operator<MemoGroupId>) -> bool) -> Vec<MemoGroupId> {
    memo.groups()
        .iter()
        .enumerate()
        .filter(|(_, group)| pattern(&group.expression))
        .map(|(index, _)| MemoGroupId(index as u32))
        .collect()
}

/// The group whose expression is `operator`, if the memo has one.
fn group_of(memo: &Memo, operator: &Operator<MemoGroupId>) -> Option<MemoGroupId> {
    groups_where(memo, |expression| expression == operator)
        .first()
        .copied()
}

/// Pass 2's pruned list: each candidate it removed, from which group, and
/// the `Debug` name of its `PruneReason` (Stage 2's reasons plus the new
/// `BelowFloor`, `MetricRule` and `NoConsumer`).
///
/// Interface used by these tests, to be added: a field
/// `LogicalASAPMemoCandidates::pruned: Vec<PrunedCandidate<CandidateForMemoGroup>>`
/// (the removed candidate itself, since its index no longer exists in the
/// memo). This body becomes
/// `logical.pruned.iter().map(|p| (p.group, p.candidate.clone(), format!("{:?}", p.reason))).collect()`.
fn pass2_pruned(
    logical: &LogicalASAPMemoCandidates,
) -> Vec<(MemoGroupId, CandidateForMemoGroup, String)> {
    let _ = logical;
    todo!("LogicalASAPMemoCandidates::pruned is not there yet (dead-candidate elimination)")
}

fn sketch_algorithm(family: &FieldDataType) -> Option<SketchAlgorithm> {
    match family {
        FieldDataType::Sketch(algorithm, _) => Some(*algorithm),
        _ => None,
    }
}

/// The sketch family of a state group (`SummaryAgg`) or chain group.
fn state_family(memo: &Memo, group: MemoGroupId) -> Option<SketchAlgorithm> {
    match &memo.groups()[group.0 as usize].expression {
        Operator::ASAP(
            ASAPOp::SummaryAgg { family, .. } | ASAPOp::SummaryTumblingWindowChain { family, .. },
        ) => sketch_algorithm(family),
        _ => None,
    }
}

// ── (1) Rule order and reach (E3) ─────────────────────────────────────────

/// Decided 2026-11-07 (E3, rule order): Pass 2 runs rules 2, 4, 5 and 6,
/// then the reach along candidate edges, then rule 1, so rule 1's widths
/// come from every query that reaches a state through a candidate.
///
/// A `sum by (job, instance) (count_over_time(m[5m]))` every 60 s; B
/// `sum by (job) (count_over_time(m[5m]))` every 90 s (both at phase 0).
/// B's state rolls up A's (rule 6), so both reach A's state, whose widths
/// are the gcd closure of {60 s, 90 s} with 300 s: 30 s and 60 s. The
/// 30 s chain serves A and B's rollup (90 s is a multiple of 30 s).
///
/// Fails until the E3 rule-order change (rule 1 after the reach): today
/// rule 1 runs first and sees only A's 60 s grid.
#[test]
fn e3_rule_one_runs_after_reach_so_a_rolled_up_query_adds_its_grid() {
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
    let query_a = memo.add_query(QueryRoot::Operator(a));
    let query_b = memo.add_query(QueryRoot::Operator(b));
    let exact = AccuracyTarget::exact();
    let logical = pass2(
        memo,
        &[
            entry_at_phase_0(60_000, exact.clone()),
            entry_at_phase_0(90_000, exact),
        ],
    );
    let memo = &logical.memo;
    // B's state rolls up A's.
    assert!(
        candidates_of(memo, by_job).iter().any(|c| {
            c.origin == OriginOfCandidateForMemoGroup::Rollup
                && c.expression.operator.children() == vec![&by_job_instance]
        }),
        "B's state reads A's through a rollup"
    );
    // Both queries reach A's state.
    let reached_by = &memo.groups()[by_job_instance.0 as usize].queries;
    assert!(
        reached_by.contains(&query_a) && reached_by.contains(&query_b),
        "A's state is reached by A and B: {reached_by:?}"
    );
    // Rule 1 saw both grids: 30 s (B's rollup and A) and 60 s (A).
    let widths = tumbling_widths_serving(memo, by_job_instance);
    assert!(
        widths.contains(&30_000) && widths.contains(&60_000),
        "widths serving A's state: {widths:?}"
    );
}

// ── (2) Offset in the gcd (E3) ────────────────────────────────────────────

/// Decided 2026-11-07 (E3, offsets): a window read at an `offset` takes the
/// offset into the gcd, since its windows' edges move by it.
///
/// `sum_over_time(m[5m] offset 45s)` every 60 s (phase 0): widths divide
/// 60 s, 300 s and 45 s, so 15 s only.
///
/// Fails until the E3 offset-in-gcd change: today the widths are the gcd
/// of the grid and the window only (60 s).
#[test]
fn e3_an_offset_enters_the_gcd_of_the_widths() {
    let mut memo = Memo::default();
    let input = scan(&mut memo, "m", &["job"]);
    let shifted = offset(&mut memo, input, 45);
    let window = range(&mut memo, shifted, 300);
    let (state, root) = exact_state(&mut memo, window, ExactKind::Sum, Reduction::PerEntity);
    memo.add_query(QueryRoot::Operator(root));
    let logical = pass2(memo, &[entry_at_phase_0(60_000, AccuracyTarget::exact())]);
    assert_eq!(
        tumbling_widths_serving(&logical.memo, state),
        BTreeSet::from([15_000])
    );
}

// ── (3) Dead-candidate elimination and Pass 2's pruned list ──────────────

/// Decided 2026-11-07 (approved order: dead-candidate elimination, then
/// windowed builds for the survivors, with a Pass 2 pruned list).
///
/// `quantile_over_time(0.99, m[5m]) / quantile_over_time(0.5, m[5m])`,
/// relative error 0.05, every 60 s; Pass 1 offers KLL and DDSketch to both
/// quantiles. The ratio reads no rank error, so every KLL readout is
/// pruned ("metric rule") and so is every KLL build (nothing reads them).
/// The ratio's floor is DDSketch alpha 0.02, so the builds with alpha 0.1
/// and 0.05 are pruned ("below floor"). Every pruned candidate is in
/// Pass 2's pruned list with its reason, and rule 1 adds windowed builds
/// only for the surviving builds.
///
/// Fails until dead-candidate elimination and the pruned list
/// (`LogicalASAPMemoCandidates::pruned`, see [`pass2_pruned`]) are added:
/// today every candidate stays in the memo.
#[test]
fn dead_candidates_are_pruned_and_listed_before_windowed_builds() {
    let mut memo = Memo::default();
    let input = scan(&mut memo, "m", &["job"]);
    let window = range(&mut memo, input, 300);
    let families = [SketchAlgorithm::Kll, SketchAlgorithm::DDSketch];
    let p99 = quantile_over_time_readouts(&mut memo, window, 0.99, &families);
    let p50 = quantile_over_time_readouts(&mut memo, window, 0.5, &families);
    let ratio = arithmetic(&mut memo, ArithmeticOpKind::Div, p99.result, p50.result);
    memo.add_query(QueryRoot::Operator(ratio));
    let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    let logical = pass2(memo, &[entry_at_phase_0(60_000, target)]);
    let memo = &logical.memo;
    let kll_state = p99.by_family[0].1;
    let reads_kll = |c: &CandidateForMemoGroup| {
        c.expression.operator.children().iter().any(|&&child| {
            child == kll_state
                || matches!(
                    &memo.groups()[child.0 as usize].expression,
                    Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. })
                        if *summary_input == kll_state
                )
        })
    };
    // No KLL build or readout remains.
    for (index, group) in memo.groups().iter().enumerate() {
        let id = MemoGroupId(index as u32);
        let kll_group = state_family(memo, id) == Some(SketchAlgorithm::Kll);
        for c in &group.candidates {
            assert!(
                !(kll_group || reads_kll(c)),
                "G{index} keeps a KLL candidate: {:?}",
                c.expression.operator
            );
        }
    }
    // Only DDSketch builds with alpha ≤ 0.02 remain, windowed ones
    // included (rule 1 runs on the survivors).
    for (index, _) in memo.groups().iter().enumerate() {
        let id = MemoGroupId(index as u32);
        if state_family(memo, id) != Some(SketchAlgorithm::DDSketch) {
            continue;
        }
        for c in candidates_of(memo, id) {
            if let Some(SketchParams::DDSketch { alpha }) = c.logical_properties.parameters() {
                assert!(*alpha <= 0.02, "G{index} keeps DDSketch alpha {alpha}");
            }
        }
    }
    // Every pruned candidate is listed with its reason.
    let pruned = pass2_pruned(&logical);
    let reason_of = |pattern: &dyn Fn(MemoGroupId, &CandidateForMemoGroup) -> bool| {
        pruned
            .iter()
            .filter(|(group, c, _)| pattern(*group, c))
            .map(|(_, _, reason)| reason.as_str())
            .collect::<Vec<_>>()
    };
    for quantile in [&p99, &p50] {
        let kll_readout = reason_of(&|group, c| group == quantile.result && reads_kll(c));
        assert_eq!(
            kll_readout,
            vec!["MetricRule"],
            "KLL readout of G{}",
            quantile.result.0
        );
    }
    let kll_builds = reason_of(&|group, _| group == kll_state);
    assert_eq!(kll_builds.len(), parameter_list(SketchAlgorithm::Kll).len());
    assert!(kll_builds
        .iter()
        .all(|reason| matches!(*reason, "MetricRule" | "NoConsumer")));
    let ddsketch_state = p99.by_family[1].1;
    let mut weak: Vec<f64> = pruned
        .iter()
        .filter(|(group, _, reason)| *group == ddsketch_state && reason == "BelowFloor")
        .filter_map(|(_, c, _)| match c.logical_properties.parameters() {
            Some(SketchParams::DDSketch { alpha }) => Some(*alpha),
            _ => None,
        })
        .collect();
    weak.sort_by(f64::total_cmp);
    assert_eq!(weak, vec![0.05, 0.1]);
}

// ── (4) One range-free chain group across ranges and offsets (E2) ─────────

/// Decided 2026-11-07 (E2 option B, design (c)): a tumbling-window chain is
/// a group of its own, `SummaryTumblingWindowChain` over the rows with a
/// width and no range, offset or window count, so every range and offset
/// of one rows-and-width reads the same chain group; a range state reads
/// it through `TimeRange` (and `TimeShift`) groups and a `SummaryMerge`.
///
/// A `sum_over_time(m[5m])`, B `sum_over_time(m[5m] offset 1h)`, C
/// `sum_over_time(m[1h])`, each every 60 s (phase 0). Expected: one chain
/// group Gw of width 60 s over the scan of m; the groups `TimeShift(1h,
/// Gw)`, `TimeRange(5m, Gw)`, `TimeRange(5m, TimeShift(1h, Gw))` and
/// `TimeRange(1h, Gw)` (hash-consed); each state group has a
/// `SummaryMerge` candidate over its `TimeRange` group, which
/// `add_candidate` accepted (equal coverage). No candidate carries a window
/// layout (`LogicalProperties` keeps only θ).
///
/// Not asserted (open): the chain's output kind and schema (C1), its
/// executor and realizer handling (C6), Stage 3's retention.
///
/// Fails until the E2 chain-group change: today rule 1 adds windowed
/// builds (candidates with a layout) and no chain group.
#[test]
fn e2_one_range_free_chain_group_serves_every_range_and_offset() {
    let mut memo = Memo::default();
    let input = scan(&mut memo, "m", &["job"]);
    let five_minutes = range(&mut memo, input, 300);
    let shifted_input = offset(&mut memo, input, 3600);
    let five_minutes_an_hour_ago = range(&mut memo, shifted_input, 300);
    let one_hour = range(&mut memo, input, 3600);
    let mut states = Vec::new();
    for window in [five_minutes, five_minutes_an_hour_ago, one_hour] {
        let (state, root) = exact_state(&mut memo, window, ExactKind::Sum, Reduction::PerEntity);
        memo.add_query(QueryRoot::Operator(root));
        states.push(state);
    }
    let exact = AccuracyTarget::exact();
    let logical = pass2(memo, &vec![entry_at_phase_0(60_000, exact); 3]);
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
                Operator::ASAP(ASAPOp::SummaryMerge { children, .. }) if *children == vec![range]
            )),
            "G{} reads G{} through a SummaryMerge",
            state.0,
            range.0
        );
    }
    for (index, group) in memo.groups().iter().enumerate() {
        for c in &group.candidates {
            assert!(
                c.logical_properties.window_layout().is_none(),
                "G{index}: a candidate with a window layout"
            );
        }
    }
}

// ── (5) Constant error contracts (E1) ─────────────────────────────────────

/// Decided 2026-11-07 (E1, PR 4): a `Filter` whose predicate reads only
/// key or label columns keeps or drops whole rows, so a value's error
/// passes through it unchanged (β = 1 in every metric).
///
/// `quantile_over_time(0.99, latency[5m])` behind a key-only filter
/// (`job = "api"`), relative error 0.05: the DDSketch readout keeps an
/// admissible edge into the filter. (The contract alone is tested in
/// `asap_types::stages`.)
///
/// Fails until E1's constant contracts are in `error_propagation` (a
/// filter has no contract today, so no approximate input is admissible).
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
    let filtered = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::Filter {
            pred: Predicate(ScalarExpr::Compare {
                left: Box::new(ScalarExpr::Column(job)),
                op: CompareOpKind::Eq,
                right: Box::new(ScalarExpr::Literal(ScalarValue::Utf8("api".into()))),
                semantics: ExprSemantics::Promql,
            }),
            child: p99.result,
        }),
    );
    memo.add_query(QueryRoot::Operator(filtered));
    let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    let logical = pass2(memo, &[entry_at_phase_0(60_000, target)]);
    let ddsketch_readout = p99.by_family[0].2;
    let edges = edges_into(&logical, own_candidate(filtered), 0);
    assert!(
        edges.contains(&(ddsketch_readout, true)),
        "edges into the filter: {edges:?}"
    );
}

// ── (6) H2: avg over relative errors stays exact-only ─────────────────────

/// Decided 2026-11-07 (H2, documented behaviour): `avg` has an absolute
/// contract only; a relative bound on a mean needs the sign facts PR 8
/// brings. So `avg(quantile_over_time(0.99, m[5m]))` with a relative
/// target has no admissible edge from any sketch readout into the `avg`:
/// it reads the exact quantile only.
///
/// Passes today: a regression guard until PR 8 changes it on purpose.
#[test]
fn h2_avg_of_readouts_under_a_relative_target_reads_exact_only() {
    let mut memo = Memo::default();
    let input = scan(&mut memo, "m", &["job"]);
    let window = range(&mut memo, input, 300);
    let families = [SketchAlgorithm::Kll, SketchAlgorithm::DDSketch];
    let p99 = quantile_over_time_readouts(&mut memo, window, 0.99, &families);
    let avg = group(
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
    memo.add_query(QueryRoot::Operator(avg));
    let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    let logical = pass2(memo, &[entry_at_phase_0(60_000, target)]);
    let edges = edges_into(&logical, own_candidate(avg), 0);
    for (_, _, readout) in &p99.by_family {
        assert!(
            !edges.contains(&(*readout, true)),
            "a sketch readout is admissible into avg: {edges:?}"
        );
    }
    assert!(
        edges.contains(&(own_candidate(p99.result), true)),
        "the exact quantile stays admissible: {edges:?}"
    );
}
