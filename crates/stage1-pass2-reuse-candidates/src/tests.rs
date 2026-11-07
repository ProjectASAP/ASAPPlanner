//! Pass 2's tests. Each builds its input memo through
//! `Memo::find_or_add_group`, `Memo::add_candidate` and `Memo::add_query`,
//! with the candidates Pass 1 would add, and reads what the rules and the
//! edge construction add; no other stage crate is called.
use std::time::Duration;

use asap_types::ir::operator::operator_properties::{BinaryOpKind, Reduction, Source};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::{ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::schema_support::with_promql_series_identity;
use asap_types::ir::schema::{
    DataType, ExactKind, Field, FieldDataType, GroupingStrategy, Schema, SketchAlgorithm,
    SketchParams, SketchStatistic, SummaryInputExpr, SummaryUpdate,
};
use asap_types::ir::{
    ASAPOp, BinaryOperator, ExprSemantics, NonASAPOp, Operator, OperatorNode, Predicate, QueryRoot,
    ScalarExpr, TimeRangeKind,
};
use asap_types::stages::{
    parameter_list, CandidateExpression, CandidateForMemoGroup, CandidateForMemoGroupId,
    LogicalASAPMemoCandidates, LogicalProperties, LogicalRequirements, Memo, MemoGroupId,
    OriginOfCandidateForMemoGroup, WindowLayout,
};
use asap_types::workload::{
    Predictability, Query, QueryRecurrence, QueryRequirements, QueryWorkloadEntry, RepeatedDemand,
    RepetitionInterval,
};

use crate::generate_logical_asap_memo_candidates;
use crate::logical_edges::{merged_tumbling_windows_guarantee, OutputError};
use crate::memo::window_candidates::tumbling_window_widths;
use crate::parameter_floors::{strongest, weakest};

// ── Fixtures ──────────────────────────────────────────────────────────────

/// Adds `operator` with `parameters` to `group`, as Pass 1 does.
fn candidate(
    memo: &mut Memo,
    group: MemoGroupId,
    operator: Operator<MemoGroupId>,
    parameters: Option<SketchParams>,
) -> CandidateForMemoGroupId {
    let expression = CandidateExpression { operator };
    let logical_properties = LogicalProperties::new(&expression, parameters, None).unwrap();
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
    .unwrap()
}

/// The group of `operator`, with `operator` as its exact candidate.
fn group(memo: &mut Memo, operator: Operator<MemoGroupId>) -> MemoGroupId {
    let group = memo.find_or_add_group(operator.clone()).unwrap();
    candidate(memo, group, operator, None);
    group
}

/// `[ts, value, labels...]`, the series identity realized as the planner
/// realizes it before Stage 0.
fn series_schema(labels: &[&str]) -> Schema {
    let mut fields = vec![
        Field::plain("ts", DataType::Timestamp, false),
        Field::plain("value", DataType::Float64, false),
    ];
    fields.extend(
        labels
            .iter()
            .map(|n| Field::plain(*n, DataType::Utf8, true)),
    );
    let open = OperatorNode::new_shared(scan_operator(
        "m",
        Schema::with_time_index(fields, 0, vec![]),
    ))
    .unwrap();
    with_promql_series_identity(&open).unwrap().schema.clone()
}

fn scan_operator<C>(metric: &str, schema: Schema) -> Operator<C> {
    Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: metric.into(),
        },
        predicates: vec![],
        schema,
    })
}

/// A time-series scan of `metric` listing `labels`.
fn scan(memo: &mut Memo, metric: &str, labels: &[&str]) -> MemoGroupId {
    let schema = series_schema(labels);
    group(memo, scan_operator(metric, schema))
}

/// `child[seconds]`.
fn range(memo: &mut Memo, child: MemoGroupId, seconds: u64) -> MemoGroupId {
    group(
        memo,
        Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(seconds),
            kind: TimeRangeKind::Range,
            child,
        }),
    )
}

/// A summary build of `algorithm` over the sample values of `child`,
/// grouped by `reduction`.
fn build(child: MemoGroupId, family: FieldDataType, reduction: Reduction) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        family,
        input: SummaryUpdate {
            item: None,
            weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
            weight_domain: Default::default(),
        },
        reduction,
        grouping: GroupingStrategy::default(),
        filter: None,
    })
}

fn sketch(algorithm: SketchAlgorithm) -> FieldDataType {
    FieldDataType::Sketch(algorithm, GroupingStrategy::default())
}

/// The state group of a sketch build, one candidate per listed θ.
fn sketch_state(
    memo: &mut Memo,
    child: MemoGroupId,
    algorithm: SketchAlgorithm,
    reduction: Reduction,
) -> MemoGroupId {
    let operator = build(child, sketch(algorithm), reduction);
    let state = memo.find_or_add_group(operator.clone()).unwrap();
    for parameters in parameter_list(algorithm) {
        candidate(memo, state, operator.clone(), Some(parameters.clone()));
    }
    state
}

/// A readout group of `statistic` over `state`.
fn estimate(memo: &mut Memo, state: MemoGroupId, statistic: SketchStatistic) -> MemoGroupId {
    group(
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
    group(
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

/// A query repeating every `interval_ms` at phase 0 with `accuracy`.
fn entry(interval_ms: u32, accuracy: AccuracyTarget) -> QueryWorkloadEntry {
    QueryWorkloadEntry {
        query: Query("q".into()),
        requirements: QueryRequirements {
            accuracy,
            max_latency_ms: None,
        },
        predictability: Predictability::Predictable { known_at: None },
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
            interval_ms,
        ))),
        range_evaluation: None,
    }
}

fn pass2(memo: Memo, entries: &[QueryWorkloadEntry]) -> LogicalASAPMemoCandidates {
    generate_logical_asap_memo_candidates(memo, entries).unwrap()
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

// ── Rule 1: tumbling layouts ──────────────────────────────────────────────

/// A 5m window's widths are the gcds of the window with the evaluation
/// grids and their gcd closure: 120 s gives 60 s; 90 s gives 30 s; both
/// queries add their gcd 30 s once; no grid gives none.
#[test]
fn tumbling_window_widths_are_the_gcd_closure_dividing_the_window() {
    assert_eq!(tumbling_window_widths(300_000, [120_000]), vec![60_000]);
    assert_eq!(tumbling_window_widths(300_000, [90_000]), vec![30_000]);
    assert_eq!(tumbling_window_widths(300_000, [300_000]), vec![300_000]);
    assert_eq!(
        tumbling_window_widths(300_000, [120_000, 90_000]),
        vec![30_000, 60_000]
    );
    assert!(tumbling_window_widths(300_000, []).is_empty());
}

/// Every build of a mergeable state over a range gets, per width, the same
/// build with a tumbling layout: one per listed θ. Every consumer of the
/// state group sees them; no consumer is copied.
#[test]
fn a_mergeable_state_gets_one_windowed_build_per_listed_parameters_and_width() {
    let mut memo = Memo::default();
    let (_, state, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 300);
    memo.add_query(QueryRoot::Operator(readout));
    let logical = pass2(memo, &[entry(60_000, AccuracyTarget::max_error(0.1))]);
    let windowed: Vec<_> = candidates_of(&logical.memo, state)
        .iter()
        .filter(|c| c.logical_properties.window_layout().is_some())
        .collect();
    let listed = parameter_list(SketchAlgorithm::Kll).len();
    assert_eq!(windowed.len(), listed);
    assert!(windowed.iter().all(|c| {
        c.logical_properties.window_layout() == Some(&WindowLayout::Tumbling { width_ms: 60_000 })
            && c.origin == OriginOfCandidateForMemoGroup::TumblingWindows
    }));
    // The readout is one candidate, and reads every build of the state.
    assert_eq!(candidates_of(&logical.memo, readout).len(), 1);
    assert_eq!(
        edges_into(&logical, own_candidate(readout), 0).len(),
        2 * listed
    );
}

/// No layout for a family without a merge (an instant rate), nor for a
/// window read at an `@` anchor.
#[test]
fn no_tumbling_layout_without_a_merge_or_at_an_anchor() {
    use asap_types::ir::operator::operator_properties::{AtModifier, TimeShift};
    let mut memo = Memo::default();
    let input = scan(&mut memo, "m", &["job"]);
    let window = range(&mut memo, input, 300);
    let irate = group(
        &mut memo,
        build(
            window,
            FieldDataType::exact(ExactKind::IRate),
            Reduction::PerEntity,
        ),
    );
    let anchored = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::TimeShift {
            shift: TimeShift {
                offset_ms: 0,
                at: Some(AtModifier::Start),
            },
            child: input,
        }),
    );
    let anchored_window = range(&mut memo, anchored, 300);
    let kll = sketch_state(
        &mut memo,
        anchored_window,
        SketchAlgorithm::Kll,
        Reduction::PerEntity,
    );
    let finalized = group(
        &mut memo,
        Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: irate }),
    );
    let readout = estimate(&mut memo, kll, SketchStatistic::Quantile { q: 0.5 });
    memo.add_query(QueryRoot::Operator(finalized));
    memo.add_query(QueryRoot::Operator(readout));
    let target = AccuracyTarget::max_error(0.1);
    let logical = pass2(
        memo,
        &[entry(60_000, target.clone()), entry(60_000, target)],
    );
    for state in [irate, kll] {
        assert!(candidates_of(&logical.memo, state)
            .iter()
            .all(|c| c.logical_properties.window_layout().is_none()));
    }
}

// ── Rule 2: scan widening ─────────────────────────────────────────────────

/// Scans of one metric listing different labels get the union scan (each
/// new label before the series identity), and each original scan, and
/// each group above it that passes labels on, reads it through a
/// projection of its own columns.
#[test]
fn scans_of_one_metric_with_other_labels_read_one_widened_scan() {
    let mut memo = Memo::default();
    let by_job = scan(&mut memo, "m", &["job"]);
    let by_host = scan(&mut memo, "m", &["host", "job"]);
    let job_window = range(&mut memo, by_job, 60);
    let host_window = range(&mut memo, by_host, 60);
    memo.add_query(QueryRoot::Operator(job_window));
    memo.add_query(QueryRoot::Operator(host_window));
    let target = AccuracyTarget::exact();
    let logical = pass2(
        memo,
        &[entry(60_000, target.clone()), entry(60_000, target)],
    );
    let memo = &logical.memo;
    let reads_widened = |group: MemoGroupId| -> Option<MemoGroupId> {
        candidates_of(memo, group)
            .iter()
            .find_map(|c| match (&c.origin, &c.expression.operator) {
                (
                    OriginOfCandidateForMemoGroup::WidenedScans,
                    Operator::NonASAP(NonASAPOp::Project { child, .. }),
                ) => Some(*child),
                _ => None,
            })
    };
    let wide_job = reads_widened(by_job).expect("the narrow scan reads the widened one");
    let wide_host = reads_widened(by_host).expect("both scans read it");
    assert_eq!(wide_job, wide_host);
    let names: Vec<&str> = memo.groups()[wide_job.0 as usize]
        .group_properties
        .schema
        .fields
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["ts", "value", "job", "host", "$promql_series_identity"]
    );
    // The time ranges above them read one widened time range.
    assert_eq!(reads_widened(job_window), reads_widened(host_window));
    assert!(reads_widened(job_window).is_some());
}

/// Two queries whose quantile builds differ only in the labels their scans
/// list read one widened build: each readout gets a projection of the one
/// widened readout.
#[test]
fn builds_over_scans_listing_other_labels_share_one_widened_build() {
    let mut memo = Memo::default();
    let mut readouts = Vec::new();
    for labels in [&["job"][..], &["host", "job"][..]] {
        let input = scan(&mut memo, "m", labels);
        let window = range(&mut memo, input, 60);
        let state = sketch_state(
            &mut memo,
            window,
            SketchAlgorithm::Kll,
            Reduction::PerEntity,
        );
        let readout = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.9 });
        memo.add_query(QueryRoot::Operator(readout));
        readouts.push(readout);
    }
    let target = AccuracyTarget::max_error(0.1);
    let logical = pass2(
        memo,
        &[entry(60_000, target.clone()), entry(60_000, target)],
    );
    let widened: Vec<MemoGroupId> = readouts
        .iter()
        .map(|&readout| {
            candidates_of(&logical.memo, readout)
                .iter()
                .find_map(|c| match &c.expression.operator {
                    Operator::NonASAP(NonASAPOp::Project { child, .. })
                        if c.origin == OriginOfCandidateForMemoGroup::WidenedScans =>
                    {
                        Some(*child)
                    }
                    _ => None,
                })
                .expect("a projection of the widened readout")
        })
        .collect();
    assert_eq!(widened[0], widened[1]);
    let Operator::ASAP(ASAPOp::SummaryEstimate { summary_input, .. }) =
        &logical.memo.groups()[widened[0].0 as usize].expression
    else {
        panic!("the widened readout");
    };
    // The widened state has the original builds' θ and layouts: every
    // listed θ, without and with rule 1's 60 s tumbling windows.
    assert_eq!(
        candidates_of(&logical.memo, *summary_input).len(),
        2 * parameter_list(SketchAlgorithm::Kll).len()
    );
}

// ── Rule 4: top-k reuse ───────────────────────────────────────────────────

/// An exact top-3 reads the top-5 of the same input and partition; the
/// top-5 reads nothing new. Only the exact top-5 gives it an edge.
#[test]
fn a_smaller_topk_reads_the_largest_topk_of_the_same_input() {
    let mut memo = Memo::default();
    let input = scan(&mut memo, "m", &["job"]);
    let topk = |memo: &mut Memo, k| {
        group(
            memo,
            Operator::NonASAP(NonASAPOp::Aggregate {
                child: input,
                reduction: Reduction::by(vec![]),
                measures: vec![AggIntent::TopK {
                    k,
                    accuracy: AccuracyTarget::exact(),
                }],
                output_names: vec!["topk".into()],
                filters: vec![None],
                having: None,
            }),
        )
    };
    let top3 = topk(&mut memo, 3);
    let top5 = topk(&mut memo, 5);
    memo.add_query(QueryRoot::Operator(top3));
    memo.add_query(QueryRoot::Operator(top5));
    let exact = AccuracyTarget::exact();
    let logical = pass2(memo, &[entry(60_000, exact.clone()), entry(60_000, exact)]);
    let reuse: Vec<_> = candidates_of(&logical.memo, top3)
        .iter()
        .enumerate()
        .filter(|(_, c)| c.origin == OriginOfCandidateForMemoGroup::TopKReuse)
        .collect();
    assert_eq!(reuse.len(), 1);
    assert_eq!(reuse[0].1.expression.operator.children(), vec![&top5]);
    assert!(candidates_of(&logical.memo, top5)
        .iter()
        .all(|c| c.origin != OriginOfCandidateForMemoGroup::TopKReuse));
    let edges = edges_into(
        &logical,
        CandidateForMemoGroupId {
            group: top3,
            index_in_group: reuse[0].0,
        },
        0,
    );
    assert_eq!(edges, vec![(own_candidate(top5), true)]);
}

// ── Rule 5: finer aggregate ───────────────────────────────────────────────

fn table_scan(memo: &mut Memo) -> MemoGroupId {
    group(
        memo,
        Operator::NonASAP(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: "flows".into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![
                Field::plain("src", DataType::Utf8, false),
                Field::plain("dst", DataType::Utf8, false),
                Field::plain("bytes", DataType::Float64, false),
            ]),
        }),
    )
}

fn sql_sum(memo: &mut Memo, child: MemoGroupId, keys: Vec<usize>) -> MemoGroupId {
    group(
        memo,
        Operator::NonASAP(NonASAPOp::Aggregate {
            child,
            reduction: Reduction::by(keys),
            measures: vec![AggIntent::Sum { col: Some(2) }],
            output_names: vec!["total".into()],
            filters: vec![None],
            having: None,
        }),
    )
}

/// `sum(bytes) by src` reads `sum(bytes) by (src, dst)`, summing its rows
/// into its groups; the finer one reads nothing from the coarser one.
#[test]
fn a_coarse_sql_sum_rolls_up_a_finer_sum() {
    let mut memo = Memo::default();
    let rows = table_scan(&mut memo);
    let coarse = sql_sum(&mut memo, rows, vec![0]);
    let fine = sql_sum(&mut memo, rows, vec![0, 1]);
    memo.add_query(QueryRoot::Operator(coarse));
    memo.add_query(QueryRoot::Operator(fine));
    let exact = AccuracyTarget::exact();
    let logical = pass2(memo, &[entry(60_000, exact.clone()), entry(60_000, exact)]);
    let rollups: Vec<_> = candidates_of(&logical.memo, coarse)
        .iter()
        .filter(|c| c.origin == OriginOfCandidateForMemoGroup::Rollup)
        .collect();
    assert_eq!(rollups.len(), 1);
    let Operator::NonASAP(NonASAPOp::Aggregate {
        child,
        reduction,
        measures,
        ..
    }) = &rollups[0].expression.operator
    else {
        panic!("{:?}", rollups[0].expression.operator);
    };
    assert_eq!(*child, fine);
    assert_eq!(*reduction, Reduction::by(vec![0]));
    assert_eq!(*measures, vec![AggIntent::Sum { col: Some(2) }]);
    assert!(candidates_of(&logical.memo, fine)
        .iter()
        .all(|c| c.origin != OriginOfCandidateForMemoGroup::Rollup));
}

/// `… WHERE dst = 'a' GROUP BY src` reads the finer groups through a
/// filter on the finer key; the key is pinned, so each query group is one
/// finer row, read with `min`.
#[test]
fn a_filtered_sql_sum_reads_the_finer_rows_through_a_compensating_filter() {
    let mut memo = Memo::default();
    let rows = table_scan(&mut memo);
    let fine = sql_sum(&mut memo, rows, vec![0, 1]);
    let filtered = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::Filter {
            pred: Predicate(ScalarExpr::Compare {
                left: Box::new(ScalarExpr::Column(1)),
                op: CompareOpKind::Eq,
                right: Box::new(ScalarExpr::Literal(ScalarValue::Utf8("a".into()))),
                semantics: ExprSemantics::Sql,
            }),
            child: rows,
        }),
    );
    let coarse = sql_sum(&mut memo, filtered, vec![0]);
    memo.add_query(QueryRoot::Operator(coarse));
    memo.add_query(QueryRoot::Operator(fine));
    let exact = AccuracyTarget::exact();
    let logical = pass2(memo, &[entry(60_000, exact.clone()), entry(60_000, exact)]);
    let rollup = candidates_of(&logical.memo, coarse)
        .iter()
        .find(|c| c.origin == OriginOfCandidateForMemoGroup::Rollup)
        .expect("the filtered sum reads the finer one");
    let Operator::NonASAP(NonASAPOp::Aggregate {
        child, measures, ..
    }) = &rollup.expression.operator
    else {
        panic!();
    };
    assert_eq!(*measures, vec![AggIntent::Min { col: Some(2) }]);
    let Operator::NonASAP(NonASAPOp::Filter { child: below, .. }) =
        &logical.memo.groups()[child.0 as usize].expression
    else {
        panic!("a compensating filter group");
    };
    assert_eq!(*below, fine);
}

// ── Rule 6: rollup ────────────────────────────────────────────────────────

/// A KLL state grouped by `job` reads the one grouped by `(host, job)` over
/// the same rows, merged into its groups (`group_by` the finer state's
/// `job` column); the finer one reads nothing from it.
#[test]
fn a_coarse_sketch_state_merges_a_finer_state() {
    let mut memo = Memo::default();
    let input = scan(&mut memo, "m", &["host", "job"]);
    let window = range(&mut memo, input, 60);
    let coarse = sketch_state(
        &mut memo,
        window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![3]),
    );
    let fine = sketch_state(
        &mut memo,
        window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![2, 3]),
    );
    let coarse_readout = estimate(&mut memo, coarse, SketchStatistic::Quantile { q: 0.9 });
    let fine_readout = estimate(&mut memo, fine, SketchStatistic::Quantile { q: 0.9 });
    memo.add_query(QueryRoot::Operator(coarse_readout));
    memo.add_query(QueryRoot::Operator(fine_readout));
    let target = AccuracyTarget::max_error(0.1);
    let logical = pass2(
        memo,
        &[entry(60_000, target.clone()), entry(60_000, target)],
    );
    let merges: Vec<_> = candidates_of(&logical.memo, coarse)
        .iter()
        .filter(|c| c.origin == OriginOfCandidateForMemoGroup::Rollup)
        .collect();
    assert_eq!(merges.len(), 1, "{merges:?}");
    let Operator::ASAP(ASAPOp::SummaryMerge {
        children,
        group_by,
        newest_tumbling_windows: None,
    }) = &merges[0].expression.operator
    else {
        panic!("{:?}", merges[0].expression.operator);
    };
    assert_eq!(*children, vec![fine]);
    assert_eq!(*group_by, Some(vec![1]));
    assert!(candidates_of(&logical.memo, fine)
        .iter()
        .all(|c| c.origin != OriginOfCandidateForMemoGroup::Rollup));
}

/// The finer state's windowed builds (rule 1) serve the coarse state
/// through its rollup: one tumbling-window chain for both queries, without
/// a window rollup rule.
#[test]
fn a_coarse_state_reads_the_finer_states_windowed_builds() {
    let mut memo = Memo::default();
    let input = scan(&mut memo, "m", &["host", "job"]);
    let window = range(&mut memo, input, 300);
    let coarse = sketch_state(
        &mut memo,
        window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![3]),
    );
    let fine = sketch_state(
        &mut memo,
        window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![2, 3]),
    );
    for state in [coarse, fine] {
        let readout = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.9 });
        memo.add_query(QueryRoot::Operator(readout));
    }
    let target = AccuracyTarget::max_error(0.1);
    let logical = pass2(
        memo,
        &[entry(60_000, target.clone()), entry(60_000, target)],
    );
    let merge = candidates_of(&logical.memo, coarse)
        .iter()
        .position(|c| c.origin == OriginOfCandidateForMemoGroup::Rollup)
        .expect("the rollup");
    let windowed = edges_into(
        &logical,
        CandidateForMemoGroupId {
            group: coarse,
            index_in_group: merge,
        },
        0,
    )
    .into_iter()
    .filter(|&(producer, _)| {
        logical
            .memo
            .candidate(producer)
            .logical_properties
            .window_layout()
            .is_some()
    })
    .count();
    assert_eq!(windowed, parameter_list(SketchAlgorithm::Kll).len());
}

/// A merge of finer states takes the floor of the readouts of the coarse
/// state: the merged state has the finer build's θ.
#[test]
fn a_rollup_merge_takes_its_readouts_floor() {
    let mut memo = Memo::default();
    let input = scan(&mut memo, "m", &["host", "job"]);
    let window = range(&mut memo, input, 60);
    let coarse = sketch_state(
        &mut memo,
        window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![3]),
    );
    let fine = sketch_state(
        &mut memo,
        window,
        SketchAlgorithm::Kll,
        Reduction::by(vec![2, 3]),
    );
    let readout = estimate(&mut memo, coarse, SketchStatistic::Quantile { q: 0.9 });
    let fine_readout = estimate(&mut memo, fine, SketchStatistic::Quantile { q: 0.9 });
    memo.add_query(QueryRoot::Operator(readout));
    memo.add_query(QueryRoot::Operator(fine_readout));
    let target = AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Rank);
    let logical = pass2(
        memo,
        &[entry(60_000, target.clone()), entry(60_000, target)],
    );
    let readout_floor = candidates_of(&logical.memo, readout)[0].input_requirements[0]
        .parameter_floor_for_required_accuracy()
        .cloned();
    assert!(readout_floor.is_some());
    let merge = candidates_of(&logical.memo, coarse)
        .iter()
        .find(|c| c.origin == OriginOfCandidateForMemoGroup::Rollup)
        .expect("the rollup");
    assert_eq!(
        merge.input_requirements[0]
            .parameter_floor_for_required_accuracy()
            .cloned(),
        readout_floor
    );
}

/// Reading merged tumbling windows keeps one build's guarantee when the
/// family's merged readout does (a KLL quantile), and has none when it
/// does not (a heap's top-k).
#[test]
fn merged_windows_keep_a_guarantee_only_where_the_family_merges_it() {
    let kll = sketch(SketchAlgorithm::Kll);
    let quantile = SketchStatistic::Quantile { q: 0.9 };
    let k = parameter_list(SketchAlgorithm::Kll).first();
    assert_eq!(
        merged_tumbling_windows_guarantee(&kll, k, true, &quantile),
        OutputError::Bound(AccuracyMetric::Rank)
    );
    assert_eq!(
        merged_tumbling_windows_guarantee(&kll, k, false, &quantile),
        OutputError::Bound(AccuracyMetric::Rank)
    );
    let heap = sketch(SketchAlgorithm::CmsWithHeap);
    assert_eq!(
        merged_tumbling_windows_guarantee(
            &heap,
            parameter_list(SketchAlgorithm::CmsWithHeap).first(),
            true,
            &SketchStatistic::TopK { k: 3 }
        ),
        OutputError::WithoutGuarantee
    );
}

// ── Edges, metric rule, caps ──────────────────────────────────────────────

/// Every producer of an input's group is an edge into it, rejected ones
/// included: each consumer input lists every candidate of its group.
#[test]
fn every_candidate_of_an_input_group_is_an_edge() {
    let mut memo = Memo::default();
    let (_, state, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 60);
    memo.add_query(QueryRoot::Operator(readout));
    let logical = pass2(memo, &[]);
    let edges = edges_into(&logical, own_candidate(readout), 0);
    assert_eq!(edges.len(), candidates_of(&logical.memo, state).len());
    // Without a target no input is capped: every edge satisfies.
    assert!(edges.iter().all(|&(_, satisfies)| satisfies));
}

/// A rank target 0.01 caps the KLL readout's input at 0.01: its floor is
/// the weakest listed k within it, the weaker builds' edges fail and the
/// stronger ones' hold.
#[test]
fn a_query_target_caps_the_readout_input_with_a_parameter_floor() {
    let mut memo = Memo::default();
    let (_, state, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 60);
    memo.add_query(QueryRoot::Operator(readout));
    let target = AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Rank);
    let logical = pass2(memo, &[entry(60_000, target)]);
    let floor = candidates_of(&logical.memo, readout)[0].input_requirements[0]
        .parameter_floor_for_required_accuracy()
        .cloned()
        .expect("a floor");
    let SketchParams::Kll { k: floor_k, .. } = floor else {
        panic!("{floor:?}");
    };
    for (producer, satisfies) in edges_into(&logical, own_candidate(readout), 0) {
        let Some(SketchParams::Kll { k, .. }) = logical
            .memo
            .candidate(producer)
            .logical_properties
            .parameters()
        else {
            panic!();
        };
        assert_eq!(satisfies, *k >= floor_k, "k = {k}");
    }
    assert!(edges_into(&logical, own_candidate(readout), 0)
        .iter()
        .any(|&(_, satisfies)| !satisfies));
    assert!(producer_count(&logical, state) > 0);
}

fn producer_count(logical: &LogicalASAPMemoCandidates, group: MemoGroupId) -> usize {
    candidates_of(&logical.memo, group).len()
}

/// The metric rule: a relative-error ratio reads a DDSketch readout
/// (relative error) but not a KLL one (rank error), and always an exact
/// value.
#[test]
fn a_ratio_reads_no_rank_error() {
    let mut memo = Memo::default();
    let (_, _, kll) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.99, 60);
    let (_, _, ddsketch) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.5, 60);
    let ratio = arithmetic(&mut memo, ArithmeticOpKind::Div, kll, ddsketch);
    memo.add_query(QueryRoot::Operator(ratio));
    let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    let logical = pass2(memo, &[entry(60_000, target)]);
    assert_eq!(
        edges_into(&logical, own_candidate(ratio), 0),
        vec![(own_candidate(kll), false)]
    );
    assert_eq!(
        edges_into(&logical, own_candidate(ratio), 1),
        vec![(own_candidate(ddsketch), true)]
    );
}

/// A ratio passes its target down through `1 / (1 - b)` (and caps its
/// denominator at `b`): with a relative target 0.05 both readouts' inputs
/// need `alpha ≤ 0.045`, so the weakest listed alpha within it.
#[test]
fn a_ratio_divides_its_target_by_its_propagation_factor() {
    let mut memo = Memo::default();
    let (_, _, numerator) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.99, 60);
    let input = scan(&mut memo, "m", &["job"]);
    let window = range(&mut memo, input, 120);
    let state = sketch_state(
        &mut memo,
        window,
        SketchAlgorithm::DDSketch,
        Reduction::PerEntity,
    );
    let denominator = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.5 });
    let ratio = arithmetic(&mut memo, ArithmeticOpKind::Div, numerator, denominator);
    memo.add_query(QueryRoot::Operator(ratio));
    let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    let logical = pass2(memo, &[entry(60_000, target)]);
    for readout in [numerator, denominator] {
        assert_eq!(
            candidates_of(&logical.memo, readout)[0].input_requirements[0]
                .parameter_floor_for_required_accuracy(),
            Some(&SketchParams::DDSketch { alpha: 0.02 }),
        );
    }
}

/// A query whose target names another metric than the readout's sets no
/// floor: the cap would compare errors in two metrics.
#[test]
fn a_target_in_another_metric_sets_no_floor() {
    let mut memo = Memo::default();
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 60);
    memo.add_query(QueryRoot::Operator(readout));
    let target = AccuracyTarget::max_error(0.001).accuracy_metric(AccuracyMetric::Relative);
    let logical = pass2(memo, &[entry(60_000, target)]);
    assert!(
        candidates_of(&logical.memo, readout)[0].input_requirements[0]
            .parameter_floor_for_required_accuracy()
            .is_none()
    );
}

/// Two queries reading one readout: the larger cap wins, since either may
/// use it. A query reading it through a sum, which carries no rank error,
/// gives it no cap.
#[test]
fn the_loosest_query_sets_a_shared_inputs_cap() {
    let strict = AccuracyTarget::max_error(0.001).accuracy_metric(AccuracyMetric::Rank);
    let loose = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Rank);
    let floor = |targets: &[&AccuracyTarget], through_sum: bool| {
        let mut memo = Memo::default();
        let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 60);
        memo.add_query(QueryRoot::Operator(readout));
        if targets.len() > 1 {
            let root = if through_sum {
                arithmetic(&mut memo, ArithmeticOpKind::Add, readout, readout)
            } else {
                readout
            };
            memo.add_query(QueryRoot::Operator(root));
        }
        let entries: Vec<_> = targets
            .iter()
            .map(|t| entry(60_000, (*t).clone()))
            .collect();
        let logical = pass2(memo, &entries);
        candidates_of(&logical.memo, readout)[0].input_requirements[0]
            .parameter_floor_for_required_accuracy()
            .cloned()
    };
    assert_ne!(floor(&[&strict], false), floor(&[&loose], false));
    assert_eq!(floor(&[&strict, &loose], false), floor(&[&loose], false));
    assert_eq!(floor(&[&strict, &loose], true), floor(&[&strict], false));
}

/// A top-k readout's heap floor (heap size ≥ k) stays when a cap tightens
/// the width: the floor satisfies both.
#[test]
fn floors_combine_per_parameter() {
    let heap = SketchParams::CmsWithHeap {
        width: 100,
        depth: 3,
        heap_size: 50,
    };
    let wide = SketchParams::CmsWithHeap {
        width: 400,
        depth: 5,
        heap_size: 10,
    };
    assert_eq!(
        strongest(&heap, &wide),
        Some(SketchParams::CmsWithHeap {
            width: 400,
            depth: 5,
            heap_size: 50
        })
    );
    assert_eq!(
        weakest(&heap, &wide),
        Some(SketchParams::CmsWithHeap {
            width: 100,
            depth: 3,
            heap_size: 10
        })
    );
    assert_eq!(
        strongest(
            &SketchParams::DDSketch { alpha: 0.01 },
            &SketchParams::DDSketch { alpha: 0.05 }
        ),
        Some(SketchParams::DDSketch { alpha: 0.01 })
    );
    assert_eq!(
        strongest(&heap, &SketchParams::DDSketch { alpha: 0.01 }),
        None
    );
}
