//! Pass 2's tests. Each builds its input memo through
//! `Memo::find_or_add_group`, `Memo::add_candidate` and `Memo::add_query`,
//! with the candidates Pass 1 would add, and reads what the rules, the
//! readout requirements, dead-candidate elimination and the edges give; no
//! other stage crate is called.
use std::collections::BTreeSet;
use std::time::Duration;

use asap_accuracy::NoMeasuredGuarantees;
use asap_types::ir::operator::operator_properties::{
    AtModifier, BinaryOpKind, Reduction, TimeShift,
};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::{ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::{
    ExactKind, FieldDataType, SketchAlgorithm, SketchParams, SketchStatistic, SummaryUpdate,
};
use asap_types::ir::{
    ASAPOp, BinaryOperator, ExprSemantics, NonASAPOp, Operator, Predicate, ProjectItem, QueryRoot,
    ScalarExpr, TimeRangeKind,
};
use asap_types::stages::{
    parameter_list, CandidateError, CandidateForMemoGroup, CandidateForMemoGroupId,
    LogicalASAPMemoCandidates, Memo, MemoGroupId, MemoQuery, OriginOfCandidateForMemoGroup,
    PruneCause, PrunedLogicalCandidate, RequiredAccuracy, RootPruneReason,
};
use asap_types::workload::{
    Predictability, Query, QueryRecurrence, QueryRequirements, QueryWorkloadEntry, RepeatedDemand,
    RepetitionInterval, TimestampMs,
};

use asap_types::test_support::{
    add_candidate, group_with_own_candidate, series_schema, time_series_scan,
};

use crate::generate_logical_asap_memo_candidates;

/// The examples decided on 2026-11-07.
mod decided_examples;

// ── Fixtures ──────────────────────────────────────────────────────────────

/// A time-series scan of `metric` listing `labels`.
fn scan(memo: &mut Memo, metric: &str, labels: &[&str]) -> MemoGroupId {
    group_with_own_candidate(
        memo,
        time_series_scan(metric, series_schema(labels), vec![]),
    )
}

/// `column = value`, PromQL semantics.
fn equals(column: usize, value: &str) -> ScalarExpr<MemoGroupId> {
    ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(column)),
        op: CompareOpKind::Eq,
        right: Box::new(ScalarExpr::Literal(ScalarValue::Utf8(value.into()))),
        semantics: ExprSemantics::Promql,
    }
}

/// A scan of `metric` listing `labels` whose label matchers are `matchers`.
fn scan_where(
    memo: &mut Memo,
    metric: &str,
    labels: &[&str],
    matchers: Vec<Predicate<MemoGroupId>>,
) -> MemoGroupId {
    group_with_own_candidate(
        memo,
        time_series_scan(metric, series_schema(labels), matchers),
    )
}

/// `child[seconds]`.
fn range(memo: &mut Memo, child: MemoGroupId, seconds: u64) -> MemoGroupId {
    group_with_own_candidate(
        memo,
        Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(seconds),
            kind: TimeRangeKind::Range,
            child,
        }),
    )
}

/// `child offset <seconds>`, or `child @ at`.
fn shift(memo: &mut Memo, child: MemoGroupId, seconds: i64, at: Option<AtModifier>) -> MemoGroupId {
    group_with_own_candidate(
        memo,
        Operator::NonASAP(NonASAPOp::TimeShift {
            shift: TimeShift {
                offset_ms: seconds * 1000,
                at,
            },
            child,
        }),
    )
}

/// A summary build of `summary_type` over the sample values of `child`,
/// grouped by `reduction`.
fn build(
    child: MemoGroupId,
    summary_type: FieldDataType,
    reduction: Reduction,
) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        summary_type,
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction,
        filter: None,
    })
}

/// The state group of a sketch build, one candidate per listed θ.
fn sketch_state(
    memo: &mut Memo,
    child: MemoGroupId,
    algorithm: SketchAlgorithm,
    reduction: Reduction,
) -> MemoGroupId {
    let operator = build(child, FieldDataType::Sketch(algorithm), reduction);
    let state = memo.find_or_add_group(operator.clone()).unwrap();
    for parameters in parameter_list(algorithm) {
        add_candidate(memo, state, operator.clone(), Some(parameters.clone())).unwrap();
    }
    state
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
    let state =
        group_with_own_candidate(memo, build(window, FieldDataType::exact(kind), reduction));
    let finalized = group_with_own_candidate(
        memo,
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: state }),
    );
    (state, finalized)
}

/// A readout group of `statistic` over `state`.
fn estimate(memo: &mut Memo, state: MemoGroupId, statistic: SketchStatistic) -> MemoGroupId {
    group_with_own_candidate(
        memo,
        Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: state,
            query: statistic,
        }),
    )
}

fn arithmetic(
    memo: &mut Memo,
    kind: ArithmeticOpKind,
    lhs: MemoGroupId,
    rhs: MemoGroupId,
) -> MemoGroupId {
    group_with_own_candidate(
        memo,
        Operator::NonASAP(NonASAPOp::BinaryOp {
            operator: BinaryOperator {
                checked_relative_division: false,
                checked_finite_division: false,
                kind: BinaryOpKind::Arithmetic(kind),
                vector_match: None,
            },
            return_bool: false,
            lhs,
            rhs,
        }),
    )
}

/// Query `root` with `target` (`None`: accuracy not enforced).
fn query(memo: &mut Memo, root: MemoGroupId, target: Option<AccuracyTarget>) -> usize {
    memo.add_query(MemoQuery {
        root: QueryRoot::Operator(root),
        accuracy_target: target,
        values_per_evaluation: Some(1),
    })
}

/// A query repeating every `interval_ms` at phase 0 (`FixedIntervalAt`).
fn entry(interval_ms: u32) -> QueryWorkloadEntry {
    QueryWorkloadEntry {
        query: Query("q".into()),
        requirements: QueryRequirements::default(),
        predictability: Predictability::Predictable { known_at: None },
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
            interval: RepetitionInterval(interval_ms),
            evaluation_phase: TimestampMs(0),
        }),
        range_evaluation: None,
    }
}

fn pass2(memo: Memo, entries: &[QueryWorkloadEntry]) -> LogicalASAPMemoCandidates {
    generate_logical_asap_memo_candidates(memo, entries, &NoMeasuredGuarantees).unwrap()
}

fn candidates_of(memo: &Memo, group: MemoGroupId) -> &[CandidateForMemoGroup] {
    &memo.groups()[group.0 as usize].candidates
}

/// The edges into input `input` of `consumer`: each producer and α^log.
fn edges_into(
    logical: &LogicalASAPMemoCandidates,
    consumer: CandidateForMemoGroupId,
    input: usize,
) -> Vec<(CandidateForMemoGroupId, bool)> {
    logical
        .edges
        .iter()
        .filter(|edge| {
            edge.consumer_candidate_id == consumer && edge.input_index_of_consumer == input
        })
        .map(|edge| {
            (
                edge.producer_candidate_id,
                edge.satisfies_logical_requirement,
            )
        })
        .collect()
}

fn own_candidate(group: MemoGroupId) -> CandidateForMemoGroupId {
    CandidateForMemoGroupId {
        group,
        index_in_group: 0,
    }
}

/// The pruned entry of `candidate`, if Pass 2 eliminated it.
fn pruned_entry(
    logical: &LogicalASAPMemoCandidates,
    candidate: CandidateForMemoGroupId,
) -> Option<&PrunedLogicalCandidate> {
    logical
        .pruned
        .iter()
        .find(|pruned| pruned.candidate == candidate)
}

/// The required accuracy of each readout candidate of `group` (its only
/// input's).
fn requirements_of(memo: &Memo, group: MemoGroupId) -> Vec<Option<RequiredAccuracy>> {
    candidates_of(memo, group)
        .iter()
        .filter(|c| {
            matches!(
                c.expression.operator,
                Operator::ASAP(ASAPOp::SummaryEstimate { .. })
            )
        })
        .map(|c| c.input_requirements[0].required_accuracy)
        .collect()
}

/// `quantile_over_time(q, m[seconds])` per series through a sketch state
/// of `algorithm`: the range group, the state group and the readout group.
fn quantile_over_time(
    memo: &mut Memo,
    algorithm: SketchAlgorithm,
    q: f64,
    seconds: u64,
) -> (MemoGroupId, MemoGroupId, MemoGroupId) {
    let input = scan(memo, "m", &["job"]);
    let window = range(memo, input, seconds);
    let state = sketch_state(memo, window, algorithm, Reduction::PerEntity);
    let readout = estimate(memo, state, SketchStatistic::Quantile { q });
    (window, state, readout)
}

/// The tumbling-window widths of the chains whose merges state group
/// `state` has as candidates.
fn chain_widths_serving(memo: &Memo, state: MemoGroupId) -> BTreeSet<u64> {
    candidates_of(memo, state)
        .iter()
        .filter_map(|c| match &c.expression.operator {
            Operator::ASAP(ASAPOp::SummaryMerge {
                children,
                group_by: None,
            }) => chain_width_below(memo, children[0]),
            _ => None,
        })
        .collect()
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

// ── Step 1: rollup from the finest common group ───────────────────────────

/// Concepts doc Example 1: F = `quantile by (job, host)(latency[5m])`, C =
/// `quantile by (host)(latency{job="api"}[5m])`. Equal definitions, the
/// narrowing `job ∈ {api}` on a key of F, {host} ⊆ {job, host}: C's state
/// gets `SummaryMerge(by host)(Filter(job = "api", F))`, and F is the
/// finest common group (nothing is synthesized).
#[test]
fn a_filtered_coarse_sketch_state_merges_the_finer_states_through_a_compensating_filter() {
    let mut memo = Memo::default();
    let labels = ["job", "host"];
    let all = scan(&mut memo, "latency", &labels);
    let api = scan_where(
        &mut memo,
        "latency",
        &labels,
        vec![Predicate(equals(2, "api"))],
    );
    let fine_window = range(&mut memo, all, 300);
    let coarse_window = range(&mut memo, api, 300);
    let fine = sketch_state(
        &mut memo,
        fine_window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![2, 3]),
    );
    let coarse = sketch_state(
        &mut memo,
        coarse_window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![3]),
    );
    let groups_before = memo.groups().len();
    for state in [fine, coarse] {
        let readout = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.99 });
        query(&mut memo, readout, None);
    }
    let logical = pass2(memo, &[entry(60_000), entry(60_000)]);
    let memo = &logical.memo;
    let filter = group_of(
        memo,
        &Operator::NonASAP(NonASAPOp::Filter {
            pred: Predicate(equals(0, "api")),
            child: fine,
        }),
    )
    .expect("Filter(job = \"api\", F)");
    assert!(candidates_of(memo, coarse).iter().any(|c| c.origin
        == OriginOfCandidateForMemoGroup::Rollup
        && c.expression.operator
            == Operator::ASAP(ASAPOp::SummaryMerge {
                children: vec![filter],
                group_by: Some(vec![1]),
            })));
    // F reads nothing from C, and no finest common group was added: the
    // memo grew by the readouts, the filter, and the chains only.
    assert!(candidates_of(memo, fine)
        .iter()
        .all(|c| c.origin != OriginOfCandidateForMemoGroup::Rollup));
    assert!(groups_where(memo, |e| matches!(
        e,
        Operator::ASAP(ASAPOp::SummaryAgg { .. })
    ))
    .iter()
    .all(|&g| g == fine || g == coarse || g.0 as usize >= groups_before));
}

/// Concepts doc Example 3: Q1 = `quantile by (host)(latency[5m])`, Q2 =
/// `quantile by (job)(latency{host="xxx"}[5m])`. Neither is finer, so the
/// finest common group G* = `SummaryAgg[KLL, by job, host]` over Q1's rows
/// is synthesized with one build per listed θ, Q1's state merges it by
/// host and Q2's merges `Filter(host = "xxx", G*)` by job.
#[test]
fn a_finest_common_group_no_query_asks_for_serves_both() {
    let mut memo = Memo::default();
    let labels = ["job", "host"];
    let all = scan(&mut memo, "latency", &labels);
    let xxx = scan_where(
        &mut memo,
        "latency",
        &labels,
        vec![Predicate(equals(3, "xxx"))],
    );
    let all_window = range(&mut memo, all, 300);
    let xxx_window = range(&mut memo, xxx, 300);
    let by_host = sketch_state(
        &mut memo,
        all_window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![3]),
    );
    let by_job = sketch_state(
        &mut memo,
        xxx_window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![2]),
    );
    for state in [by_host, by_job] {
        let readout = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.99 });
        query(&mut memo, readout, None);
    }
    let logical = pass2(memo, &[entry(60_000), entry(60_000)]);
    let memo = &logical.memo;
    let finest = group_of(
        memo,
        &build(
            all_window,
            FieldDataType::Sketch(SketchAlgorithm::Kll),
            Reduction::by(vec![2, 3]),
        ),
    )
    .expect("G*: KLL by job, host over Q1's rows");
    assert_eq!(
        candidates_of(memo, finest)
            .iter()
            .filter(|c| c.expression.operator == memo.groups()[finest.0 as usize].expression)
            .count(),
        parameter_list(SketchAlgorithm::Kll).len()
    );
    let merges = |state: MemoGroupId| -> Vec<Operator<MemoGroupId>> {
        candidates_of(memo, state)
            .iter()
            .filter(|c| c.origin == OriginOfCandidateForMemoGroup::Rollup)
            .map(|c| c.expression.operator.clone())
            .collect()
    };
    assert_eq!(
        merges(by_host),
        vec![Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![finest],
            group_by: Some(vec![1]),
        })]
    );
    let filter = group_of(
        memo,
        &Operator::NonASAP(NonASAPOp::Filter {
            pred: Predicate(equals(1, "xxx")),
            child: finest,
        }),
    )
    .expect("Filter(host = \"xxx\", G*)");
    assert_eq!(
        merges(by_job),
        vec![Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![filter],
            group_by: Some(vec![0]),
        })]
    );
}

/// A `quantile by (job)` over `status = "500"` rows does not roll up from
/// `quantile by (job, host)` over every row: `status` is no key of the
/// finer state, whose states mix both statuses.
#[test]
fn a_narrowing_on_a_column_that_is_no_key_gives_no_rollup() {
    let mut memo = Memo::default();
    let labels = ["job", "host", "status"];
    let all = scan(&mut memo, "latency", &labels);
    let errors = scan_where(
        &mut memo,
        "latency",
        &labels,
        vec![Predicate(equals(4, "500"))],
    );
    let all_window = range(&mut memo, all, 300);
    let errors_window = range(&mut memo, errors, 300);
    let fine = sketch_state(
        &mut memo,
        all_window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![2, 3]),
    );
    let coarse = sketch_state(
        &mut memo,
        errors_window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![2]),
    );
    for state in [fine, coarse] {
        let readout = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.99 });
        query(&mut memo, readout, None);
    }
    let logical = pass2(memo, &[entry(60_000), entry(60_000)]);
    assert!(candidates_of(&logical.memo, coarse).iter().all(|c| !c
        .expression
        .operator
        .children()
        .contains(&&fine)));
}

/// Concepts doc Example 2, exact values: `sum by (job)` rolls up the
/// finalized `sum by (job, instance)` of the same per-series rows with a
/// sum over its sum column.
#[test]
fn a_coarse_exact_sum_rolls_up_the_finer_sum() {
    let mut memo = Memo::default();
    let input = scan(&mut memo, "x", &["job", "instance"]);
    let window = range(&mut memo, input, 60);
    let rate = group_with_own_candidate(
        &mut memo,
        build(
            window,
            FieldDataType::exact(ExactKind::Rate),
            Reduction::PerEntity,
        ),
    );
    let rows = group_with_own_candidate(
        &mut memo,
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: rate }),
    );
    let sum_by = |memo: &mut Memo, keys: Vec<usize>, name: &str| {
        group_with_own_candidate(
            memo,
            Operator::NonASAP(NonASAPOp::Aggregate {
                child: rows,
                reduction: Reduction::by(keys),
                measures: vec![AggIntent::Sum { col: None }],
                output_names: vec![name.into()],
                filters: vec![None],
                having: None,
            }),
        )
    };
    let fine = sum_by(&mut memo, vec![2, 3], "value");
    let coarse = sum_by(&mut memo, vec![2], "value");
    query(&mut memo, fine, Some(AccuracyTarget::exact()));
    query(&mut memo, coarse, Some(AccuracyTarget::exact()));
    let logical = pass2(memo, &[entry(60_000), entry(60_000)]);
    let rollups: Vec<_> = candidates_of(&logical.memo, coarse)
        .iter()
        .filter(|c| c.origin == OriginOfCandidateForMemoGroup::Rollup)
        .map(|c| c.expression.operator.clone())
        .collect();
    assert_eq!(
        rollups,
        vec![Operator::NonASAP(NonASAPOp::Aggregate {
            child: fine,
            reduction: Reduction::by(vec![0]),
            measures: vec![AggIntent::Sum { col: Some(2) }],
            output_names: vec!["value".into()],
            filters: vec![None],
            having: None,
        })]
    );
}

// ── Step 2: top-k reuse ───────────────────────────────────────────────────

/// `topk(5, x)` reads `topk(10, x)`'s ranked rows: the 5 highest of the 10
/// highest are the 5 highest.
#[test]
fn a_smaller_topk_reads_the_largest_topk_of_the_same_input() {
    let mut memo = Memo::default();
    let input = scan(&mut memo, "x", &["job"]);
    let topk = |memo: &mut Memo, k: usize| {
        group_with_own_candidate(
            memo,
            Operator::NonASAP(NonASAPOp::Aggregate {
                child: input,
                reduction: Reduction::by(vec![]),
                measures: vec![AggIntent::TopK { k }],
                output_names: vec![String::new()],
                filters: vec![None],
                having: None,
            }),
        )
    };
    let top5 = topk(&mut memo, 5);
    let top10 = topk(&mut memo, 10);
    query(&mut memo, top5, Some(AccuracyTarget::exact()));
    query(&mut memo, top10, Some(AccuracyTarget::exact()));
    let logical = pass2(memo, &[entry(60_000), entry(60_000)]);
    let reuse: Vec<_> = candidates_of(&logical.memo, top5)
        .iter()
        .filter(|c| c.origin == OriginOfCandidateForMemoGroup::TopKReuse)
        .collect();
    assert_eq!(reuse.len(), 1);
    assert_eq!(reuse[0].expression.operator.children(), vec![&top10]);
    assert!(candidates_of(&logical.memo, top10)
        .iter()
        .all(|c| c.origin != OriginOfCandidateForMemoGroup::TopKReuse));
}

// ── Step 3: readout requirements ──────────────────────────────────────────

/// A single quantile query with rank error 0.01: its one readout gets that
/// requirement (λ = 1), and only the KLL builds whose rank error at δ = 1
/// is within 0.01 satisfy it.
#[test]
fn a_single_readout_requires_the_query_target() {
    let mut memo = Memo::default();
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

/// A whole-result failure probability is split over the values one
/// evaluation returns: δ = 0.01 over 100 series is 1e-4 per value.
#[test]
fn a_whole_result_failure_probability_is_split_per_value() {
    let mut memo = Memo::default();
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 300);
    memo.add_query(MemoQuery {
        root: QueryRoot::Operator(readout),
        accuracy_target: Some(AccuracyTarget::max_error(0.01).failure_probability(0.01)),
        values_per_evaluation: Some(100),
    });
    let logical = pass2(memo, &[entry(60_000)]);
    let [Some(required)] = requirements_of(&logical.memo, readout)[..] else {
        panic!("one requirement");
    };
    assert!((required.failure_probability - 1e-4).abs() < 1e-12);
}

/// A ratio reads no rank error: with a relative target, the KLL readout's
/// edge into the division fails the metric rule; a target naming the rank
/// metric gives the readouts no requirement in another metric.
#[test]
fn a_ratio_reads_no_rank_error() {
    let mut memo = Memo::default();
    let (_, _, p99) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.99, 300);
    let p50 = estimate(
        &mut memo,
        MemoGroupId(2),
        SketchStatistic::Quantile { q: 0.5 },
    );
    let ratio = arithmetic(&mut memo, ArithmeticOpKind::Div, p99, p50);
    let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    query(&mut memo, ratio, Some(target));
    let logical = pass2(memo, &[entry(60_000)]);
    assert_eq!(requirements_of(&logical.memo, p99), vec![None]);
    let pruned = pruned_entry(&logical, own_candidate(p99)).expect("pruned");
    assert!(matches!(
        pruned.causes[..],
        [PruneCause {
            root: RootPruneReason::MetricRule { input: 0, .. },
            ..
        }]
    ));
}

/// Without accuracy targets (accuracy not enforced) readouts get no
/// requirement and no metric rule applies: every edge is admissible.
#[test]
fn without_targets_every_edge_is_admissible() {
    let mut memo = Memo::default();
    let (_, _, p99) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.99, 300);
    let p50 = estimate(
        &mut memo,
        MemoGroupId(2),
        SketchStatistic::Quantile { q: 0.5 },
    );
    let ratio = arithmetic(&mut memo, ArithmeticOpKind::Div, p99, p50);
    query(&mut memo, ratio, None);
    let logical = pass2(memo, &[entry(60_000)]);
    assert!(logical.pruned.is_empty());
    assert!(logical
        .edges
        .iter()
        .all(|edge| edge.satisfies_logical_requirement));
}

/// A top-k heap smaller than k never serves the readout, whatever accuracy
/// is required: `topk(50)` reads only heaps of at least 50.
#[test]
fn a_heap_smaller_than_k_is_pruned() {
    let mut memo = Memo::default();
    let input = scan(&mut memo, "x", &["job"]);
    let window = range(&mut memo, input, 300);
    let state = sketch_state(
        &mut memo,
        window,
        SketchAlgorithm::CmsWithHeap,
        Reduction::by(vec![]),
    );
    let readout = estimate(&mut memo, state, SketchStatistic::TopK { k: 50 });
    query(&mut memo, readout, None);
    let logical = pass2(memo, &[entry(60_000)]);
    let mut small = 0;
    for pruned in logical.pruned.iter().filter(|p| p.candidate.group == state) {
        let Some(SketchParams::CmsWithHeap { heap_size, .. }) = logical
            .memo
            .candidate(pruned.candidate)
            .logical_properties
            .parameters()
        else {
            panic!("a heap build");
        };
        assert!(*heap_size < 50);
        assert!(matches!(
            pruned.causes[..],
            [PruneCause {
                root: RootPruneReason::HeapSmallerThanTopK { k: 50, .. },
                ..
            }]
        ));
        small += 1;
    }
    // Heaps of 10 and 20, with each width and depth.
    assert_eq!(small, 2 * 28);
}

// ── Step 5: chain groups ──────────────────────────────────────────────────

/// A mergeable state over a range gets one chain group per width with a
/// build per surviving θ; its readout reads the builds and the merge.
#[test]
fn a_mergeable_state_reads_a_chain_with_one_build_per_surviving_parameters() {
    let mut memo = Memo::default();
    let (_, state, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 300);
    query(&mut memo, readout, None);
    let logical = pass2(memo, &[entry(120_000)]);
    let memo = &logical.memo;
    assert_eq!(chain_widths_serving(memo, state), BTreeSet::from([60_000]));
    let chains = groups_where(memo, |e| {
        matches!(e, Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }))
    });
    let listed = parameter_list(SketchAlgorithm::Kll).len();
    assert_eq!(candidates_of(memo, chains[0]).len(), listed);
    assert_eq!(
        edges_into(&logical, own_candidate(readout), 0).len(),
        listed + 1
    );
}

/// No chain for a summary type whose states do not merge (an instant
/// rate), under `@ end()`, or for a query without a known grid.
#[test]
fn no_chain_without_a_merge_an_anchor_or_a_grid() {
    let mut memo = Memo::default();
    let input = scan(&mut memo, "m", &["job"]);
    let window = range(&mut memo, input, 300);
    let (irate, irate_root) =
        exact_state(&mut memo, window, ExactKind::IRate, Reduction::PerEntity);
    let anchored = shift(&mut memo, input, 0, Some(AtModifier::End));
    let anchored_window = range(&mut memo, anchored, 300);
    let (pinned, pinned_root) = exact_state(
        &mut memo,
        anchored_window,
        ExactKind::Sum,
        Reduction::PerEntity,
    );
    let other = scan(&mut memo, "n", &["job"]);
    let other_window = range(&mut memo, other, 300);
    let (no_grid, no_grid_root) = exact_state(
        &mut memo,
        other_window,
        ExactKind::Sum,
        Reduction::PerEntity,
    );
    for root in [irate_root, pinned_root, no_grid_root] {
        query(&mut memo, root, None);
    }
    let without_phase = QueryWorkloadEntry {
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
            60_000,
        ))),
        ..entry(60_000)
    };
    let logical = pass2(memo, &[entry(60_000), entry(60_000), without_phase]);
    for state in [irate, pinned, no_grid] {
        assert!(
            chain_widths_serving(&logical.memo, state).is_empty(),
            "G{}",
            state.0
        );
    }
    assert!(groups_where(&logical.memo, |e| matches!(
        e,
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
    ))
    .is_empty());
}

/// A state under `@ T` adds no width; it reads a chain another query's
/// width created when that width divides T.
#[test]
fn an_absolute_anchor_reuses_only_existing_chains() {
    let mut memo = Memo::default();
    let input = scan(&mut memo, "m", &["job"]);
    let window = range(&mut memo, input, 300);
    let (state, root) = exact_state(&mut memo, window, ExactKind::Sum, Reduction::PerEntity);
    query(&mut memo, root, None);
    let mut pinned = Vec::new();
    // T = …040 s lies on the 60 s grid; T = …010 s does not.
    for at_ms in [1_700_000_040_000, 1_700_000_010_000] {
        let at = shift(&mut memo, input, 0, Some(AtModifier::Timestamp(at_ms)));
        let at_window = range(&mut memo, at, 300);
        let (state, root) = exact_state(&mut memo, at_window, ExactKind::Sum, Reduction::PerEntity);
        query(&mut memo, root, None);
        pinned.push(state);
    }
    let logical = pass2(memo, &vec![entry(60_000); 3]);
    let widths = |state| chain_widths_serving(&logical.memo, state);
    assert_eq!(widths(state), BTreeSet::from([60_000]));
    assert_eq!(widths(pinned[0]), BTreeSet::from([60_000]));
    assert!(widths(pinned[1]).is_empty());
}

// ── Step 6: logical edges and evaluation grids ────────────────────────────

/// Every candidate of an input group is an edge of each consumer input.
#[test]
fn every_candidate_of_an_input_group_is_an_edge() {
    let mut memo = Memo::default();
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
    query(&mut memo, readout, None);
    let logical = pass2(memo, &[entry(300_000)]);
    let listed = parameter_list(SketchAlgorithm::DDSketch).len();
    // The builds and the merge over the 300 s chain.
    assert_eq!(
        edges_into(&logical, own_candidate(readout), 0).len(),
        listed + 1
    );
}

/// A group's evaluation grid is the gcd of its reaching queries' grids;
/// `None` when one has no known grid.
#[test]
fn a_groups_evaluation_grid_is_the_gcd_of_its_queries_grids() {
    let mut memo = Memo::default();
    let input = scan(&mut memo, "m", &["job"]);
    let window = range(&mut memo, input, 300);
    let (_, a) = exact_state(&mut memo, window, ExactKind::Sum, Reduction::PerEntity);
    let other = scan(&mut memo, "n", &["job"]);
    query(&mut memo, a, None);
    query(&mut memo, a, None);
    query(&mut memo, other, None);
    let without_phase = QueryWorkloadEntry {
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
            60_000,
        ))),
        ..entry(60_000)
    };
    let logical = pass2(memo, &[entry(60_000), entry(120_000), without_phase]);
    let grid = |group: MemoGroupId| logical.evaluation_grid_of_group[group.0 as usize];
    assert_eq!(grid(window).map(|g| g.period_ms), Some(60_000));
    assert_eq!(grid(other), None);
}
