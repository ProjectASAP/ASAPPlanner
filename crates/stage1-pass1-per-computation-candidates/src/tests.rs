//! Pass 1's tests. Each builds its input memo through `Memo::find_or_add_group`
//! and `Memo::add_query`, as Stage 0 does, and reads the candidates
//! [`pass1_memo`](crate::pass1_memo) adds; no other stage crate is called.
use std::fmt::Write as _;
use std::time::Duration;

use asap_types::ir::operator::arguments::{BinaryOpKind, Reduction, Source};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::{ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema, PROMQL_SERIES_IDENTITY};
use asap_types::ir::{
    ASAPOp, BinaryOperator, ExprSemantics, NonASAPOp, Operator, Predicate, ScalarExpr,
    TimeRangeKind,
};
use asap_types::stages::{CandidateForMemoGroup, LogicalRequirements, Memo, MemoGroupId};
use asap_types::summary_type::{parameter_list, SummaryTypeCapability};
use asap_types::summary_type::{
    ExactKind, SketchAlgorithm, SketchParams, SketchStatistic, SummaryInputExpr, SummaryUpdate,
    WeightDomain,
};

use crate::candidates::{add_measure_way, key_only_having, Pending, SingleMeasure};
use crate::computation_targets::{
    computation_candidates_for_intent, computation_targets, HowToComputeAggregateMeasure,
};
use crate::summary_realization::{
    realize_keyed_additive_summary_input, summarised_input, summary_candidates,
};
use crate::test_fixtures::aggregate_group;
use crate::{pass1_memo, PerComputationCandidateError};
use asap_types::test_support::memo_builders::{group, query, scan_group};
use asap_types::test_support::memo_fixtures::scan;
use asap_types::workload::DataWorkload;

/// A compact printout of every group and its candidates: the expression's
/// operator, then per candidate its operator, input groups, θ and fused
/// groups.
fn memo_printout(memo: &Memo) -> String {
    let mut out = String::new();
    for (index, group) in memo.groups().iter().enumerate() {
        let _ = writeln!(out, "G{index}: {}", label(&group.expression));
        for (position, candidate) in group.candidates.iter().enumerate() {
            let inputs: Vec<String> = candidate
                .expression
                .operator
                .children()
                .iter()
                .map(|g| format!("G{}", g.0))
                .collect();
            let mut line = format!(
                "  [{position}] {} inputs=[{}]",
                label(&candidate.expression.operator),
                inputs.join(", ")
            );
            if let Some(parameters) = candidate.logical_properties.parameters() {
                let _ = write!(line, " θ={parameters:?}");
            }
            if !candidate.fused_groups.is_empty() {
                let _ = write!(line, " fused_groups={:?}", candidate.fused_groups);
            }
            let _ = writeln!(out, "{line}");
        }
    }
    out
}

fn label(operator: &Operator<MemoGroupId>) -> String {
    match operator {
        Operator::ASAP(ASAPOp::SummaryAgg { summary_type, .. }) => {
            format!("SummaryAgg[{summary_type:?}]")
        }
        Operator::ASAP(ASAPOp::SummaryEstimate { query, .. }) => {
            format!("SummaryEstimate[{query:?}]")
        }
        Operator::NonASAP(NonASAPOp::Aggregate { measures, .. }) => {
            let names: Vec<String> = measures
                .iter()
                .map(|m| {
                    format!("{m:?}")
                        .split([' ', '{'])
                        .next()
                        .unwrap_or("")
                        .to_string()
                })
                .collect();
            format!("Aggregate[{}]", names.join(", "))
        }
        operator => operator.kind_name().to_string(),
    }
}

fn candidates(memo: &Memo, group: MemoGroupId) -> &[CandidateForMemoGroup] {
    &memo.groups()[group.0 as usize].candidates
}

/// The groups of `memo` whose expression matches `pattern`.
fn groups_where(memo: &Memo, pattern: impl Fn(&Operator<MemoGroupId>) -> bool) -> Vec<MemoGroupId> {
    memo.groups()
        .iter()
        .enumerate()
        .filter(|(_, group)| pattern(&group.expression))
        .map(|(index, _)| MemoGroupId(index as u32))
        .collect()
}

/// The state group of `algorithm` in `memo`, and its build's update.
fn state_group(memo: &Memo, algorithm: SketchAlgorithm) -> (MemoGroupId, SummaryUpdate) {
    let found = groups_where(
        memo,
        |expression| matches!(expression, Operator::ASAP(ASAPOp::SummaryAgg { summary_type: FieldDataType::Sketch(a), .. }) if *a == algorithm),
    );
    let [state] = found[..] else {
        panic!(
            "one {algorithm:?} state group: {found:?}\n{}",
            memo_printout(memo)
        )
    };
    let Operator::ASAP(ASAPOp::SummaryAgg { input, .. }) =
        &memo.groups()[state.0 as usize].expression
    else {
        unreachable!()
    };
    (state, input.clone())
}

fn quantile(q: f64) -> AggIntent {
    AggIntent::Quantile { col: None, q }
}

fn sum() -> AggIntent {
    AggIntent::Sum { col: None }
}

fn topk(k: usize) -> AggIntent {
    AggIntent::TopK { k }
}

/// `metric[5m]` (`Range`) or `metric` (`Instant`, which a cross-series
/// aggregate reads): a selector over `metric{labels}`.
fn selector(memo: &mut Memo, kind: TimeRangeKind, metric: &str, labels: &[&str]) -> MemoGroupId {
    let scan = scan_group(memo, metric, labels, vec![]);
    group(
        memo,
        Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(300),
            kind,
            child: scan,
        }),
    )
}

/// `column op literal` over an aggregate's output.
fn compare(column: usize, op: CompareOpKind, value: ScalarValue) -> Predicate<MemoGroupId> {
    Predicate(ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(column)),
        op,
        right: Box::new(ScalarExpr::Literal(value)),
        semantics: ExprSemantics::Promql,
    })
}

/// `Filter(pred)` over `child`, added as Stage 0 adds a consumer `Filter`.
fn filter(memo: &mut Memo, child: MemoGroupId, pred: Predicate<MemoGroupId>) -> MemoGroupId {
    group(memo, Operator::NonASAP(NonASAPOp::Filter { pred, child }))
}

// ── Running example (concepts doc, section 1) ──────────────────────────────

/// The doc's running example: query A reads p95 of `latency`, query B
/// reads p99 / p50 of it. KLL state group holds one build per θ of KLL's
/// list, DDSketch's state group likewise; the p95, p99 and p50 groups each
/// read either state through their own readout groups. The metric rule
/// (B's relative ratio cannot read KLL's rank error) drops edges in Pass 2,
/// so Pass 1 offers both algorithms to every quantile group.
#[test]
fn running_example_gives_one_build_per_theta_and_separate_readouts() {
    let mut memo = Memo::new(&DataWorkload::default());
    let g1 = selector(&mut memo, TimeRangeKind::Instant, "latency", &[]);
    let g3 = aggregate_group(
        &mut memo,
        g1,
        Reduction::by(vec![]),
        vec![quantile(0.95)],
        None,
    );
    let g4 = aggregate_group(
        &mut memo,
        g1,
        Reduction::by(vec![]),
        vec![quantile(0.99)],
        None,
    );
    let g5 = aggregate_group(
        &mut memo,
        g1,
        Reduction::by(vec![]),
        vec![quantile(0.5)],
        None,
    );
    let g6 = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::BinaryOp {
            operator: BinaryOperator {
                checked_relative_division: false,
                checked_finite_division: false,
                kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Div),
                vector_match: None,
            },
            return_bool: false,
            lhs: g4,
            rhs: g5,
        }),
    );
    query(&mut memo, g3, None);
    query(&mut memo, g6, None);
    let memo = pass1_memo(memo, None).unwrap();
    println!("{}", memo_printout(&memo));
    let (kll, _) = state_group(&memo, SketchAlgorithm::Kll);
    let (ddsketch, _) = state_group(&memo, SketchAlgorithm::DDSketch);
    for (state, algorithm) in [
        (kll, SketchAlgorithm::Kll),
        (ddsketch, SketchAlgorithm::DDSketch),
    ] {
        let thetas: Vec<SketchParams> = candidates(&memo, state)
            .iter()
            .map(|candidate| candidate.logical_properties.parameters().unwrap().clone())
            .collect();
        assert_eq!(thetas, parameter_list(algorithm));
        // θ is a property, never an operator argument: every build has the
        // group's own expression.
        assert!(candidates(&memo, state).iter().all(|candidate| {
            candidate.expression.operator == memo.groups()[state.0 as usize].expression
        }));
    }
    // Each quantile group: its exact aggregate and one readout per algorithm,
    // each through a readout group of its own over the state group.
    for (result, q) in [(g3, 0.95), (g4, 0.99), (g5, 0.5)] {
        let readouts: Vec<MemoGroupId> = candidates(&memo, result)[1..]
            .iter()
            .map(|candidate| *candidate.expression.operator.children()[0])
            .collect();
        assert_eq!(readouts.len(), 2, "G{}", result.0);
        for (readout, state) in readouts.iter().zip([kll, ddsketch]) {
            assert_eq!(
                memo.groups()[readout.0 as usize].expression,
                Operator::ASAP(ASAPOp::SummaryEstimate {
                    summary_input: state,
                    query: SketchStatistic::Quantile { q },
                })
            );
            assert!(candidates(&memo, *readout)[0]
                .logical_properties
                .parameters()
                .is_none());
        }
    }
    // The ratio's group keeps its one candidate, its own expression.
    assert_eq!(candidates(&memo, g6).len(), 1);
}

// ── Series identity ────────────────────────────────────────────────────────

/// `quantile_over_time(0.9, latency[5m])` over series whose identity is
/// realized as a column (as the planner does before Stage 0): each readout
/// candidate outputs the per-series aggregate's exact schema, the
/// `$promql_series_identity` column included, so every readout keeps the
/// series it answers for.
#[test]
fn per_series_readouts_keep_the_series_identity() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Range, "latency", &["job"]);
    let per_series = aggregate_group(
        &mut memo,
        rows,
        Reduction::PerEntity,
        vec![quantile(0.9)],
        None,
    );
    query(&mut memo, per_series, None);
    let memo = pass1_memo(memo, None).unwrap();
    let output = &memo.groups()[per_series.0 as usize].group_properties.schema;
    assert!(output.closed && output.has_promql_series_identity());
    let readouts = &candidates(&memo, per_series)[1..];
    assert_eq!(readouts.len(), 2, "{}", memo_printout(&memo));
    for readout in readouts {
        let Operator::NonASAP(NonASAPOp::Project { child, cols, .. }) =
            &readout.expression.operator
        else {
            panic!("a renaming Project: {:?}", readout.expression.operator)
        };
        let estimate = &memo.groups()[child.0 as usize].group_properties.schema;
        assert!(estimate.has_promql_series_identity());
        let identity = output
            .fields
            .iter()
            .position(|field| field.name == PROMQL_SERIES_IDENTITY)
            .unwrap();
        let read = estimate
            .fields
            .iter()
            .position(|field| field.name == PROMQL_SERIES_IDENTITY)
            .unwrap();
        assert_eq!(cols[identity].expr, ScalarExpr::Column(read));
    }
}

/// Without a realized identity the per-series aggregate's schema is open:
/// its rows carry labels it does not list. A renaming `Project` would drop
/// them, so no readout is offered; the group keeps its own expression.
#[test]
fn an_open_per_series_aggregate_offers_no_renamed_readout() {
    let mut memo = Memo::new(&DataWorkload::default());
    let scan = group(
        &mut memo,
        scan("latency", &[("job", DataType::Utf8)], vec![]),
    );
    let rows = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(300),
            kind: TimeRangeKind::Range,
            child: scan,
        }),
    );
    let per_series = aggregate_group(
        &mut memo,
        rows,
        Reduction::PerEntity,
        vec![quantile(0.9)],
        None,
    );
    query(&mut memo, per_series, None);
    let memo = pass1_memo(memo, None).unwrap();
    assert!(
        !memo.groups()[per_series.0 as usize]
            .group_properties
            .schema
            .closed
    );
    assert_eq!(candidates(&memo, per_series).len(), 1);
}

// ── Target discovery ───────────────────────────────────────────────────────

/// Each measure of a multi-measure aggregate is its own target; a measure
/// whose only realization is its exact accumulator is still a target.
#[test]
fn multi_measure_aggregate_has_one_target_per_measure() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let both = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![2]),
        vec![sum(), quantile(0.9)],
        None,
    );
    query(&mut memo, both, None);
    let targets = computation_targets(&memo, None).unwrap();
    let measures: Vec<(MemoGroupId, usize)> = targets
        .iter()
        .map(|target| (target.aggregate_group, target.measure_index))
        .collect();
    assert_eq!(measures, vec![(both, 0), (both, 1)]);
}

/// An aggregate with no realization besides exact execution (no
/// accumulator, no summary: `stddev`) is not a target.
#[test]
fn aggregate_without_realization_is_not_a_target() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &[]);
    let stddev = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![]),
        vec![AggIntent::StdDev {
            col: None,
            population: true,
        }],
        None,
    );
    query(&mut memo, stddev, None);
    assert!(computation_targets(&memo, None).unwrap().is_empty());
    let memo = pass1_memo(memo, None).unwrap();
    assert_eq!(candidates(&memo, stddev).len(), 1);
}

/// An aggregate several queries read, or a scalar root reads twice, is one
/// target per measure: Pass 1 walks the groups.
#[test]
fn an_aggregate_read_by_several_roots_is_one_target() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &[]);
    let p = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![]),
        vec![quantile(0.9)],
        None,
    );
    query(&mut memo, p, None);
    query(&mut memo, p, None);
    assert_eq!(computation_targets(&memo, None).unwrap().len(), 1);
}

/// A memo whose groups already hold ASAP operators fails at the stage
/// boundary.
#[test]
fn asap_input_is_rejected() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &[]);
    let state = group(
        &mut memo,
        Operator::ASAP(ASAPOp::SummaryAgg {
            child: rows,
            summary_type: FieldDataType::exact(ExactKind::Sum),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
        }),
    );
    query(&mut memo, state, None);
    assert!(matches!(
        pass1_memo(memo, None),
        Err(PerComputationCandidateError::ContainsAsap)
    ));
}

/// The summaries the deployment does not support are not offered.
#[test]
fn unsupported_summaries_are_not_offered() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Range, "m", &[]);
    let p = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![]),
        vec![quantile(0.9)],
        None,
    );
    query(&mut memo, p, None);
    let memo = pass1_memo(memo, Some(&[])).unwrap();
    assert_eq!(candidates(&memo, p).len(), 1);
    assert!(groups_where(&memo, |e| matches!(e, Operator::ASAP(_))).is_empty());
}

// ── Candidate composition ──────────────────────────────────────────────────

/// Every group has its own expression as a candidate, with no θ: plain
/// exact execution.
#[test]
fn every_group_offers_its_own_expression() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let s = aggregate_group(&mut memo, rows, Reduction::by(vec![2]), vec![sum()], None);
    query(&mut memo, s, None);
    let stage0 = memo.groups().len();
    let memo = pass1_memo(memo, None).unwrap();
    for index in 0..stage0 {
        let group = &memo.groups()[index];
        let own = &group.candidates[0];
        assert_eq!(own.expression.operator, group.expression);
        assert!(own.logical_properties.parameters().is_none());
    }
}

/// An exact accumulator is a state group (`SummaryAgg` of the exact
/// summary type, no θ) read by a `FinalizeExactAccumulator` group, which the
/// aggregate's group reads through a renaming `Project`.
#[test]
fn exact_accumulator_is_a_state_group_and_a_finalize() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let s = aggregate_group(&mut memo, rows, Reduction::by(vec![2]), vec![sum()], None);
    query(&mut memo, s, None);
    let memo = pass1_memo(memo, None).unwrap();
    let states = groups_where(&memo, |e| {
        matches!(
            e,
            Operator::ASAP(ASAPOp::SummaryAgg {
                summary_type: FieldDataType::ExactAggregate(ExactKind::Sum, _),
                ..
            })
        )
    });
    let [state] = states[..] else {
        panic!("{}", memo_printout(&memo))
    };
    assert_eq!(candidates(&memo, state).len(), 1);
    assert!(candidates(&memo, state)[0]
        .logical_properties
        .parameters()
        .is_none());
    // The aggregate reads it through a `FinalizeExactAccumulator`, renamed
    // to the aggregate's columns when its names differ.
    let finalize = Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child: state });
    let finalize_groups = groups_where(&memo, |e| *e == finalize);
    assert!(candidates(&memo, s).iter().any(|c| {
        c.expression.operator == finalize
            || c.expression
                .operator
                .children()
                .iter()
                .any(|g| finalize_groups.contains(g))
    }));
}

/// A multi-measure aggregate's candidate joins the measures' own groups
/// (one single-measure aggregate each, with its own candidates) on the
/// group keys.
#[test]
fn multi_measure_candidate_joins_single_measure_groups() {
    let mut memo = Memo::new(&DataWorkload::default());
    // A SQL table `flows(src Utf8, bytes Float64)`.
    let rows = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::Scan {
            source: Source::Table {
                table_ref: "flows".into(),
            },
            predicates: vec![],
            schema: Schema::new(vec![
                Field::plain("src", DataType::Utf8, false),
                Field::plain("bytes", DataType::Float64, false),
            ]),
        }),
    );
    let both = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![0]),
        vec![
            AggIntent::Sum { col: Some(1) },
            AggIntent::Quantile {
                col: Some(1),
                q: 0.9,
            },
        ],
        None,
    );
    query(&mut memo, both, None);
    let memo = pass1_memo(memo, None).unwrap();
    let single = groups_where(
        &memo,
        |e| matches!(e, Operator::NonASAP(NonASAPOp::Aggregate { measures, .. }) if measures.len() == 1),
    );
    assert_eq!(single.len(), 2, "{}", memo_printout(&memo));
    // The quantile measure's group has its exact aggregate and two readouts.
    assert!(single.iter().any(|g| candidates(&memo, *g).len() == 3));
    let joins = groups_where(&memo, |e| {
        matches!(e, Operator::NonASAP(NonASAPOp::Join { .. }))
    });
    assert_eq!(joins.len(), 1);
    assert_eq!(candidates(&memo, both).len(), 2);
}

/// Pass 1 sets no input requirement, a top-k readout's included: each
/// candidate has one empty requirement per input group, and Pass 2 sets
/// the readouts' required accuracy (a top-k's `heap_size` ≥ k among it).
#[test]
fn pass1_sets_no_input_requirement() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Range, "m", &["job"]);
    let per_series = aggregate_group(&mut memo, rows, Reduction::PerEntity, vec![sum()], None);
    let by_job = aggregate_group(
        &mut memo,
        per_series,
        Reduction::by(vec![2]),
        vec![sum()],
        None,
    );
    let ranked = aggregate_group(
        &mut memo,
        by_job,
        Reduction::by(vec![]),
        vec![topk(15)],
        None,
    );
    let samples = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let p = aggregate_group(
        &mut memo,
        samples,
        Reduction::by(vec![]),
        vec![quantile(0.9)],
        None,
    );
    query(&mut memo, ranked, None);
    query(&mut memo, p, None);
    let memo = pass1_memo(memo, None).unwrap();
    // The top-k readout is offered, and has no input requirement either.
    assert!(!groups_where(&memo, |e| matches!(
        e,
        Operator::ASAP(ASAPOp::SummaryEstimate {
            query: SketchStatistic::TopK { k: 15 },
            ..
        })
    ))
    .is_empty());
    for group in memo.groups() {
        for candidate in &group.candidates {
            assert_eq!(
                candidate.input_requirements,
                vec![
                    LogicalRequirements::default();
                    candidate.expression.operator.children().len()
                ]
            );
        }
    }
}

// ── HAVING placement ───────────────────────────────────────────────────────

/// The aggregate's input with a key-only predicate pushed below it: a
/// `Filter` group over `rows`.
fn pushed_below(memo: &Memo, rows: MemoGroupId) -> Vec<MemoGroupId> {
    groups_where(
        memo,
        |e| matches!(e, Operator::NonASAP(NonASAPOp::Filter { child, .. }) if *child == rows),
    )
}

/// A key-only `HAVING` (a consumer `Filter` over the group keys) gets
/// candidates in the `Filter`'s group that push it below the aggregate: a
/// `Filter` on the input rows, which the states read. The aggregate's own
/// group keeps its unfiltered candidates for its other consumers.
#[test]
fn key_only_having_is_pushed_below_into_the_filter_group() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let by_job = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![2]),
        vec![quantile(0.9)],
        None,
    );
    let having = filter(
        &mut memo,
        by_job,
        compare(0, CompareOpKind::Eq, ScalarValue::Utf8("a".into())),
    );
    query(&mut memo, having, None);
    query(&mut memo, by_job, None);
    let memo = pass1_memo(memo, None).unwrap();
    let below = pushed_below(&memo, rows);
    assert_eq!(below.len(), 1, "{}", memo_printout(&memo));
    // The Filter group: its own expression, then one readout per algorithm
    // over states built from the filtered rows.
    assert_eq!(candidates(&memo, having).len(), 3);
    assert_eq!(candidates(&memo, by_job).len(), 3);
}

/// A `HAVING` over an aggregate value stays above the output: nothing is
/// pushed below, and the `Filter` group keeps its own expression only.
#[test]
fn value_having_stays_above() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let by_job = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![2]),
        vec![quantile(0.9)],
        None,
    );
    let having = filter(
        &mut memo,
        by_job,
        compare(1, CompareOpKind::Gt, ScalarValue::Float64(10.0)),
    );
    query(&mut memo, having, None);
    let memo = pass1_memo(memo, None).unwrap();
    assert!(pushed_below(&memo, rows).is_empty());
    assert_eq!(candidates(&memo, having).len(), 1);
}

/// A global aggregate's `HAVING` stays above even when it is constant: the
/// one output row exists with no input row.
#[test]
fn global_aggregate_having_stays_above() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &[]);
    let p = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![]),
        vec![quantile(0.9)],
        None,
    );
    let having = filter(
        &mut memo,
        p,
        Predicate(ScalarExpr::Literal(ScalarValue::Boolean(false))),
    );
    query(&mut memo, having, None);
    let memo = pass1_memo(memo, None).unwrap();
    assert!(pushed_below(&memo, rows).is_empty());
}

/// Which `HAVING` conjuncts of a grouped aggregate (`by (job)`) are pushed
/// below it: a key conjunct, and a column-free one built of literals and
/// comparison and boolean operators (`1 = 0`); not a column-free one with a
/// function call, which may be volatile (`random() < 0.5`, built in the IR
/// directly: the frontends and schema derivation reject `random`), nor
/// one over the aggregate's value, nor any of a global aggregate's.
#[test]
fn key_only_having_pushes_keys_and_constants_but_no_function_call() {
    let literal = |value| Box::new(ScalarExpr::Literal(value));
    let one_equals_zero = ScalarExpr::Compare {
        left: literal(ScalarValue::Int64(1)),
        op: CompareOpKind::Eq,
        right: literal(ScalarValue::Int64(0)),
        semantics: ExprSemantics::Sql,
    };
    let random_below_half = ScalarExpr::Compare {
        left: Box::new(ScalarExpr::FunctionCall {
            name: "random".into(),
            args: vec![],
        }),
        op: CompareOpKind::Lt,
        right: literal(ScalarValue::Float64(0.5)),
        semantics: ExprSemantics::Sql,
    };
    let job_is_a = compare(0, CompareOpKind::Eq, ScalarValue::Utf8("a".into())).0;
    let value_above_ten = compare(1, CompareOpKind::Gt, ScalarValue::Float64(10.0)).0;
    let by_job = Reduction::by(vec![2]);
    assert!(key_only_having(&by_job, &job_is_a));
    assert!(key_only_having(&by_job, &one_equals_zero));
    assert!(key_only_having(
        &by_job,
        &ScalarExpr::Not(Box::new(one_equals_zero.clone()))
    ));
    assert!(!key_only_having(&by_job, &random_below_half));
    assert!(!key_only_having(&by_job, &value_above_ten));
    let global = Reduction::by(vec![]);
    assert!(!key_only_having(&global, &one_equals_zero));
}

/// A grouped aggregate's constant `HAVING 1 = 0` is pushed below it.
#[test]
fn a_constant_having_of_a_grouped_aggregate_is_pushed_below() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let by_job = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![2]),
        vec![quantile(0.9)],
        None,
    );
    let having = filter(
        &mut memo,
        by_job,
        Predicate(ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Literal(ScalarValue::Int64(1))),
            op: CompareOpKind::Eq,
            right: Box::new(ScalarExpr::Literal(ScalarValue::Int64(0))),
            semantics: ExprSemantics::Sql,
        }),
    );
    query(&mut memo, having, None);
    let memo = pass1_memo(memo, None).unwrap();
    assert_eq!(
        pushed_below(&memo, rows).len(),
        1,
        "{}",
        memo_printout(&memo)
    );
    assert_eq!(candidates(&memo, having).len(), 3);
}

/// The aggregate's own value `having` is a `Filter` above each readout:
/// the aggregate group's summary candidates are `Filter`s over a readout.
#[test]
fn own_value_having_filters_each_readout() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let by_job = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![2]),
        vec![quantile(0.9)],
        Some(compare(1, CompareOpKind::Gt, ScalarValue::Float64(10.0))),
    );
    query(&mut memo, by_job, None);
    let memo = pass1_memo(memo, None).unwrap();
    let kinds: Vec<&str> = candidates(&memo, by_job)
        .iter()
        .map(|c| c.expression.operator.kind_name())
        .collect();
    assert_eq!(kinds, ["Aggregate", "Filter", "Filter"]);
}

// ── Fused heaps, whole-expression top-k, ranked items, raw-data signs ──────

/// `topk(5, sum by (job) (sum_over_time(m[5m])))`: the top-k gets heaps
/// over the range's raw data samples, keyed by `job` (the ranked item),
/// weighted by the sample value with the open requirement that the samples
/// of `m` are non-negative (`RawDataSamplesOfMetric`). Each heap build
/// lists the absorbed sums' groups in `fused_groups`, and the top-k's own
/// heaps over its one-row-per-item input are pruned.
#[test]
fn whole_expression_topk_heaps_read_the_raw_samples() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Range, "m", &["job"]);
    let per_series = aggregate_group(&mut memo, rows, Reduction::PerEntity, vec![sum()], None);
    let by_job = aggregate_group(
        &mut memo,
        per_series,
        Reduction::by(vec![2]),
        vec![sum()],
        None,
    );
    let ranked = aggregate_group(
        &mut memo,
        by_job,
        Reduction::by(vec![]),
        vec![topk(5)],
        None,
    );
    query(&mut memo, ranked, None);
    let memo = pass1_memo(memo, None).unwrap();
    for algorithm in [
        SketchAlgorithm::CmsWithHeap,
        SketchAlgorithm::CountSketchWithHeap,
    ] {
        let (state, update) = state_group(&memo, algorithm);
        assert_eq!(
            *memo.groups()[state.0 as usize].expression.children()[0],
            rows
        );
        assert_eq!(
            update.item,
            Some(SummaryInputExpr::Column(ColumnRef::Named("job".into())))
        );
        assert_eq!(
            update.weight_domain,
            WeightDomain::RawDataSamplesOfMetric { metric: "m".into() }
        );
        for build in candidates(&memo, state) {
            let mut fused = build.fused_groups.clone();
            fused.sort();
            assert_eq!(fused, vec![per_series, by_job]);
        }
        assert_eq!(
            candidates(&memo, state).len(),
            parameter_list(algorithm).len()
        );
    }
}

/// `topk(5, sum by (job) (sum_over_time(m[5m])))`: the whole-expression
/// heap's candidates, built through the readout and output path every
/// summary way takes. The top-k group gets one `Project` per heap
/// algorithm, ranking the estimate read from that heap's state; only the
/// state's builds list the absorbed groups.
#[test]
fn whole_expression_topk_heap_candidates() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Range, "m", &["job"]);
    let per_series = aggregate_group(&mut memo, rows, Reduction::PerEntity, vec![sum()], None);
    let by_job = aggregate_group(
        &mut memo,
        per_series,
        Reduction::by(vec![2]),
        vec![sum()],
        None,
    );
    let ranked = aggregate_group(
        &mut memo,
        by_job,
        Reduction::by(vec![]),
        vec![topk(5)],
        None,
    );
    query(&mut memo, ranked, None);
    let memo = pass1_memo(memo, None).unwrap();
    // The top-k group's own expression, then one ranked `Project` per heap.
    let ranked_candidates = candidates(&memo, ranked);
    let kinds: Vec<&str> = ranked_candidates
        .iter()
        .map(|c| c.expression.operator.kind_name())
        .collect();
    assert_eq!(
        kinds,
        ["Aggregate", "Project", "Project"],
        "{}",
        memo_printout(&memo)
    );
    for (project, algorithm) in ranked_candidates[1..].iter().zip([
        SketchAlgorithm::CmsWithHeap,
        SketchAlgorithm::CountSketchWithHeap,
    ]) {
        assert!(project.fused_groups.is_empty());
        let estimate = *project.expression.operator.children()[0];
        let [estimate_candidate] = candidates(&memo, estimate) else {
            panic!("one readout candidate: {}", memo_printout(&memo));
        };
        assert!(matches!(
            estimate_candidate.expression.operator,
            Operator::ASAP(ASAPOp::SummaryEstimate { .. })
        ));
        assert!(estimate_candidate.fused_groups.is_empty());
        let (state, _) = state_group(&memo, algorithm);
        assert_eq!(*estimate_candidate.expression.operator.children()[0], state);
        assert_eq!(
            candidates(&memo, state).len(),
            parameter_list(algorithm).len()
        );
        for build in candidates(&memo, state) {
            assert_eq!(build.fused_groups, [by_job, per_series]);
        }
    }
    // Scan, range, the two sums, the top-k, the sums' two accumulator
    // states, and per heap its state and readout: no other group.
    assert_eq!(memo.groups().len(), 11, "{}", memo_printout(&memo));
}

/// A top-k over a `count` ranks the counted items with unit weight, proven
/// non-negative.
#[test]
fn count_ranked_topk_counts_with_unit_weight() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let counts = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![2]),
        vec![AggIntent::Count],
        None,
    );
    let ranked = aggregate_group(
        &mut memo,
        counts,
        Reduction::by(vec![]),
        vec![topk(5)],
        None,
    );
    query(&mut memo, ranked, None);
    let memo = pass1_memo(memo, None).unwrap();
    let (_, update) = state_group(&memo, SketchAlgorithm::CmsWithHeap);
    assert_eq!(update.weight, SummaryInputExpr::Constant(1.0));
    assert!(matches!(
        update.weight_domain,
        WeightDomain::NonNegative { .. }
    ));
}

/// The ranked item over several group keys is their tuple; the top-k's own
/// partition keys partition the heap and stay out of the tuple.
#[test]
fn ranked_item_is_the_keys_outside_the_partition() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Range, "m", &["job", "host"]);
    let per_series = aggregate_group(&mut memo, rows, Reduction::PerEntity, vec![sum()], None);
    let by_both = aggregate_group(
        &mut memo,
        per_series,
        Reduction::by(vec![2, 3]),
        vec![sum()],
        None,
    );
    let ranked = aggregate_group(
        &mut memo,
        by_both,
        Reduction::by(vec![0]),
        vec![topk(5)],
        None,
    );
    query(&mut memo, ranked, None);
    let memo = pass1_memo(memo, None).unwrap();
    let (_, update) = state_group(&memo, SketchAlgorithm::CountSketchWithHeap);
    assert_eq!(
        update.item,
        Some(SummaryInputExpr::Column(ColumnRef::Named("host".into())))
    );
}

// ── Nested distributive merges ─────────────────────────────────────────────

/// `sum by (job) (sum_over_time(m[5m]))`, and likewise `min` of `min` and
/// `max` of `max`, gets a merged build: one exact accumulator of that kind
/// over the raw samples grouped by `job`, a candidate of the outer
/// aggregate's own accumulator state group, listing the inner aggregate's
/// group in `fused_groups`.
#[test]
fn nested_distributive_aggregate_merges_into_the_outer_state_group() {
    for (intent, kind) in [
        (sum(), ExactKind::Sum),
        (AggIntent::Min { col: None }, ExactKind::Min),
        (AggIntent::Max { col: None }, ExactKind::Max),
    ] {
        let mut memo = Memo::new(&DataWorkload::default());
        let rows = selector(&mut memo, TimeRangeKind::Range, "m", &["job"]);
        let per_series = aggregate_group(
            &mut memo,
            rows,
            Reduction::PerEntity,
            vec![intent.clone()],
            None,
        );
        let by_job = aggregate_group(
            &mut memo,
            per_series,
            Reduction::by(vec![2]),
            vec![intent],
            None,
        );
        query(&mut memo, by_job, None);
        let memo = pass1_memo(memo, None).unwrap();
        let outer_state = groups_where(
            &memo,
            |e| matches!(e, Operator::ASAP(ASAPOp::SummaryAgg { child, summary_type: FieldDataType::ExactAggregate(state, _), .. }) if *child == per_series && *state == kind),
        );
        let [outer_state] = outer_state[..] else {
            panic!("{kind:?}: {}", memo_printout(&memo))
        };
        let builds = candidates(&memo, outer_state);
        assert_eq!(builds.len(), 2, "{kind:?}: {}", memo_printout(&memo));
        let merged = &builds[1];
        assert_eq!(merged.expression.operator.children(), vec![&rows]);
        assert_eq!(merged.fused_groups, vec![per_series]);
    }
}

/// The merge needs the same distributive aggregate twice: a `sum` over a
/// `max`, or a `min` over a `max`, gets none.
#[test]
fn nested_merge_needs_the_same_distributive_aggregate() {
    for outer in [sum(), AggIntent::Min { col: None }] {
        let mut memo = Memo::new(&DataWorkload::default());
        let rows = selector(&mut memo, TimeRangeKind::Range, "m", &["job"]);
        let per_series = aggregate_group(
            &mut memo,
            rows,
            Reduction::PerEntity,
            vec![AggIntent::Max { col: None }],
            None,
        );
        let by_job = aggregate_group(
            &mut memo,
            per_series,
            Reduction::by(vec![2]),
            vec![outer],
            None,
        );
        query(&mut memo, by_job, None);
        let memo = pass1_memo(memo, None).unwrap();
        assert!(memo
            .groups()
            .iter()
            .flat_map(|g| &g.candidates)
            .all(|c| c.fused_groups.is_empty()));
    }
}

// ── Stream norms (update weights) ──────────────────────────────────────────

/// A Count-Min count adds one per row (bounded by the row count); a sum's
/// state reads the raw value column (bounded by the source's norm).
#[test]
fn update_weights_follow_the_measure() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let counts = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![2]),
        vec![AggIntent::Count],
        None,
    );
    query(&mut memo, counts, None);
    let memo = pass1_memo(memo, None).unwrap();
    let (_, update) = state_group(&memo, SketchAlgorithm::Cms);
    assert_eq!(update.weight, SummaryInputExpr::Constant(1.0));
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let sums = aggregate_group(&mut memo, rows, Reduction::by(vec![2]), vec![sum()], None);
    query(&mut memo, sums, None);
    let memo = pass1_memo(memo, None).unwrap();
    let state = groups_where(
        &memo,
        |e| matches!(e, Operator::ASAP(ASAPOp::SummaryAgg { input, .. }) if input.weight == SummaryInputExpr::Column(ColumnRef::SampleValue)),
    );
    assert_eq!(state.len(), 1);
}

// ── Intents ────────────────────────────────────────────────────────────────

fn algorithms(choices: &[HowToComputeAggregateMeasure]) -> Vec<SketchAlgorithm> {
    choices
        .iter()
        .filter_map(|choice| match choice {
            HowToComputeAggregateMeasure::Summary(algorithm) => Some(*algorithm),
            _ => None,
        })
        .collect()
}

/// Example 2 preserves specialized distinct summaries and the universal
/// alternative; plain exact execution is the group's own expression.
#[test]
fn cardinality_keeps_specialized_and_universal_summaries() {
    let choices = computation_candidates_for_intent(&AggIntent::Cardinality { cols: vec![0] });
    assert!(choices
        .iter()
        .all(|choice| matches!(choice, HowToComputeAggregateMeasure::Summary(_))));
    assert_eq!(
        algorithms(&choices),
        vec![
            SketchAlgorithm::Hll,
            SketchAlgorithm::Kmv,
            SketchAlgorithm::UnivMon
        ]
    );
    let tuple = computation_candidates_for_intent(&AggIntent::Cardinality { cols: vec![0, 1] });
    assert!(!algorithms(&tuple).contains(&SketchAlgorithm::UnivMon));
}

/// Frequency moments offer a universal sketch without certification.
#[test]
fn frequency_statistics_keep_universal_choices() {
    for intent in [
        AggIntent::FrequencyL2 { col: Some(0) },
        AggIntent::FrequencyEntropy { col: Some(0) },
    ] {
        let choices = computation_candidates_for_intent(&intent);
        assert_eq!(
            choices,
            [HowToComputeAggregateMeasure::Summary(
                SketchAlgorithm::UnivMon
            )]
        );
    }
}

/// TopK keeps both declared heap substrates without choosing an implementation.
#[test]
fn topk_keeps_both_specialized_heap_choices() {
    let choices = computation_candidates_for_intent(&topk(10));
    assert_eq!(
        algorithms(&choices),
        vec![
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap
        ]
    );
}

/// Approximate count lists its exact accumulator first, beside the
/// summaries (the plain aggregate is the group's own expression).
#[test]
fn approximate_count_lists_its_accumulator_and_summaries() {
    let choices = computation_candidates_for_intent(&AggIntent::Count);
    assert_eq!(
        choices[0],
        HowToComputeAggregateMeasure::ExactAccumulator(vec![ExactKind::Count])
    );
    assert!(choices.contains(&HowToComputeAggregateMeasure::Summary(
        SketchAlgorithm::UnivMon
    )));
}

/// Count Sketch's signed counters give no total, so Pass 1 offers no Count
/// Sketch for a count.
#[test]
fn a_count_offers_no_count_sketch() {
    assert!(
        !computation_candidates_for_intent(&AggIntent::Count).contains(
            &HowToComputeAggregateMeasure::Summary(SketchAlgorithm::CountSketch)
        )
    );
}

// ── Summary realization ──────────────────────────────────────────────────────

/// A quantile outside [0, 1] is answered -Inf or +Inf: it gets no summary,
/// so no way besides the plain exact aggregate.
#[test]
fn a_quantile_outside_the_unit_interval_offers_no_summary() {
    let choices = computation_candidates_for_intent(&AggIntent::Quantile { col: None, q: 1.5 });
    assert!(choices.is_empty());
}

/// A heap sketch consumes every raw row, so the keyed input rule refuses
/// a TopK over a filtered count.
#[test]
fn keyed_heap_input_refuses_a_filtered_count() {
    let mut memo = Memo::new(&DataWorkload::default());
    let rows = scan_group(&mut memo, "m", &["job"], vec![]);
    let mut count_by_job = |filter: Option<Predicate<MemoGroupId>>| {
        group(
            &mut memo,
            Operator::NonASAP(NonASAPOp::Aggregate {
                reduction: Reduction::by(vec![2]),
                measures: vec![AggIntent::Count],
                output_names: vec![String::new()],
                filters: vec![filter],
                having: None,
                child: rows,
            }),
        )
    };
    let unfiltered = count_by_job(None);
    let value_above_one = Predicate(ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(1)),
        op: CompareOpKind::Gt,
        right: Box::new(ScalarExpr::Literal(ScalarValue::Float64(1.0))),
        semantics: ExprSemantics::Promql,
    });
    let filtered = count_by_job(Some(value_above_one));
    let output = Reduction::Reduce(asap_types::ir::operator::arguments::GroupKeys::none());
    assert!(realize_keyed_additive_summary_input(&memo, &output, unfiltered).is_some());
    assert!(realize_keyed_additive_summary_input(&memo, &output, filtered).is_none());
}

/// A summary reads the intent's own input columns: one column by name,
/// none as the sample value, several as one hashed tuple.
#[test]
fn summarised_input_reads_the_intents_columns() {
    // Columns [ts=0, value=1, job=2].
    let Operator::NonASAP(NonASAPOp::Scan { schema, .. }) =
        scan("m", &[("job", DataType::Utf8)], vec![])
    else {
        unreachable!("a scan")
    };
    let column = |name: &str| SummaryInputExpr::Column(ColumnRef::Named(name.into()));
    let cases = [
        (vec![2], column("job")),
        (vec![1], column("value")),
        (vec![], SummaryInputExpr::Column(ColumnRef::SampleValue)),
        (
            vec![1, 2],
            SummaryInputExpr::Tuple(vec![column("value"), column("job")]),
        ),
    ];
    for (cols, want) in cases {
        let intent = AggIntent::Cardinality { cols: cols.clone() };
        assert_eq!(
            summarised_input(&intent, &schema),
            Some(want),
            "wrong summarised input for cols={cols:?}"
        );
    }
}

/// Each approximable intent is offered the algorithms it was offered when
/// the list was written by hand, in the same order; a non-sketch intent
/// is offered none.
#[test]
fn summary_candidates_keep_the_hand_written_lists() {
    use SketchAlgorithm as S;
    let cardinality = |cols: Vec<usize>| AggIntent::Cardinality { cols };
    let cases = [
        (
            AggIntent::Quantile { col: None, q: 0.99 },
            vec![S::Kll, S::DDSketch],
        ),
        (cardinality(vec![]), vec![S::Hll, S::Kmv, S::UnivMon]),
        (cardinality(vec![1]), vec![S::Hll, S::Kmv, S::UnivMon]),
        (cardinality(vec![1, 2]), vec![S::Hll, S::Kmv]),
        (AggIntent::FrequencyL2 { col: None }, vec![S::UnivMon]),
        (AggIntent::FrequencyEntropy { col: None }, vec![S::UnivMon]),
        (
            AggIntent::TopK { k: 10 },
            vec![S::CmsWithHeap, S::CountSketchWithHeap],
        ),
        (AggIntent::Count, vec![S::Cms, S::UnivMon]),
        (AggIntent::Sum { col: None }, vec![]),
    ];
    for (intent, want) in cases {
        assert_eq!(summary_candidates(&intent), want, "{intent:?}");
    }
}

/// An algorithm is offered for an intent exactly when its capability table
/// row answers the intent's statistic: Count-Min's row has no top-k readout,
/// so a top-k is offered only the heap variants, whose rows have one.
#[test]
fn an_algorithm_without_the_readout_is_not_offered() {
    let top_k = AggIntent::TopK { k: 10 };
    let statistic = SketchStatistic::TopK { k: 10 };
    let answers = |algorithm| {
        SummaryTypeCapability::of(&FieldDataType::Sketch(algorithm)).answers(&statistic)
    };
    assert!(!answers(SketchAlgorithm::Cms));
    assert!(answers(SketchAlgorithm::CmsWithHeap));
    let offered = summary_candidates(&top_k);
    assert!(!offered.contains(&SketchAlgorithm::Cms));
    assert!(offered.iter().all(|algorithm| answers(*algorithm)));
}

// ── Unsupported ways add nothing ─────────────────────────────────────────────

/// Offers `measure` by `way` to `result_group`, expecting `Unsupported`, and
/// checks the memo is unchanged: its groups, each group's candidates and
/// its hash-consing index (all in the memo's `Debug`).
fn assert_unsupported_way_adds_nothing(
    memo: &mut Memo,
    measure: &SingleMeasure<'_>,
    way: HowToComputeAggregateMeasure,
    result_group: MemoGroupId,
) {
    let groups = memo.groups().len();
    let before = format!("{memo:?}");
    let added = add_measure_way(memo, Pending::new(memo), measure, &way, result_group, None);
    assert!(
        matches!(added, Err(PerComputationCandidateError::Unsupported(_))),
        "{added:?}"
    );
    assert_eq!(memo.groups().len(), groups);
    assert_eq!(format!("{memo:?}"), before);
}

/// A heap top-k whose estimate lacks a column of the result group (it
/// partitions `host` rows; the group ranks `job`): `ranked_as_topk` finds no
/// `job` in the pending estimate, so neither the heap's state group nor its
/// estimate group is added.
#[test]
fn a_heap_estimate_without_the_groups_columns_adds_nothing() {
    let mut memo = Memo::new(&DataWorkload::default());
    let by_job = scan_group(&mut memo, "m", &["job"], vec![]);
    let by_host = scan_group(&mut memo, "n", &["host"], vec![]);
    let ranked = aggregate_group(
        &mut memo,
        by_job,
        Reduction::by(vec![2]),
        vec![topk(5)],
        None,
    );
    let reduction = Reduction::by(vec![2]);
    let intent = topk(5);
    let measure = SingleMeasure {
        input: by_host,
        unfiltered_input: by_host,
        reduction: &reduction,
        intent: &intent,
        filter: None,
        raw_data_samples_of: None,
    };
    assert_unsupported_way_adds_nothing(
        &mut memo,
        &measure,
        HowToComputeAggregateMeasure::Summary(SketchAlgorithm::CountSketchWithHeap),
        ranked,
    );
}

/// A readout of an open per-series aggregate would need a renaming
/// `Project`, which drops the labels an open schema does not list: the
/// KLL state and readout groups are not added.
#[test]
fn a_readout_an_open_group_cannot_rename_adds_nothing() {
    let mut memo = Memo::new(&DataWorkload::default());
    let scanned = group(
        &mut memo,
        scan("latency", &[("job", DataType::Utf8)], vec![]),
    );
    let rows = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(300),
            kind: TimeRangeKind::Range,
            child: scanned,
        }),
    );
    let intent = quantile(0.9);
    let per_series = aggregate_group(
        &mut memo,
        rows,
        Reduction::PerEntity,
        vec![intent.clone()],
        None,
    );
    let measure = SingleMeasure {
        input: rows,
        unfiltered_input: rows,
        reduction: &Reduction::PerEntity,
        intent: &intent,
        filter: None,
        raw_data_samples_of: None,
    };
    assert_unsupported_way_adds_nothing(
        &mut memo,
        &measure,
        HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Kll),
        per_series,
    );
}
