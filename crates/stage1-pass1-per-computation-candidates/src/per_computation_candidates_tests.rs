//! Pass 1's tests. Each builds its input memo through `Memo::find_or_add_group`
//! and `Memo::add_query`, as Stage 0 does, and reads the candidates
//! [`pass1_memo`](crate::pass1_memo) adds; no other stage crate is called.
use std::fmt::Write as _;
use std::time::Duration;

use asap_types::ir::operator::operator_properties::{BinaryOpKind, Reduction, Source};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::{ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::{
    DataType, ExactKind, Field, FieldDataType, Schema, SketchAlgorithm, SketchParams,
    SketchStatistic, SummaryInputExpr, SummaryUpdate, WeightDomain, PROMQL_SERIES_IDENTITY,
};
use asap_types::ir::{
    ASAPOp, BinaryOperator, ExprSemantics, NonASAPOp, Operator, Predicate, ScalarExpr,
    TimeRangeKind,
};
use asap_types::stages::{
    parameter_list, CandidateForMemoGroup, HowToComputeAggregateMeasure, Memo, MemoGroupId,
};

use crate::pass1_memo;
use crate::per_computation_candidates::{
    computation_candidates_for_intent, computation_targets, PerComputationCandidateError,
};
use crate::test_fixtures::{
    aggregate_group, group, metric_schema, query, scan_group, scan_operator,
};

/// A compact printout of every group and its candidates: the expression's
/// operator, then per candidate its operator, input groups, θ and input
/// floors.
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
            for (input, requirement) in candidate.input_requirements.iter().enumerate() {
                if let Some(floor) = requirement.parameter_floor_for_required_accuracy() {
                    let _ = write!(line, " floor[{input}]={floor:?}");
                }
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
        Operator::ASAP(ASAPOp::SummaryAgg { family, .. }) => format!("SummaryAgg[{family:?}]"),
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

/// The state group of `family` in `memo`, and its build's update.
fn state_group(memo: &Memo, family: SketchAlgorithm) -> (MemoGroupId, SummaryUpdate) {
    let found = groups_where(
        memo,
        |expression| matches!(expression, Operator::ASAP(ASAPOp::SummaryAgg { family: FieldDataType::Sketch(f, _), .. }) if *f == family),
    );
    let [state] = found[..] else {
        panic!(
            "one {family:?} state group: {found:?}\n{}",
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

fn approximate() -> AccuracyTarget {
    AccuracyTarget::max_error(0.05).failure_probability(0.01)
}

fn quantile(q: f64) -> AggIntent {
    AggIntent::Quantile {
        col: None,
        q,
        accuracy: approximate(),
    }
}

fn sum() -> AggIntent {
    AggIntent::Sum { col: None }
}

fn count() -> AggIntent {
    AggIntent::Count {
        accuracy: approximate(),
    }
}

fn topk(k: usize) -> AggIntent {
    AggIntent::TopK {
        k,
        accuracy: approximate(),
    }
}

/// `metric[5m]` (`Range`) or `metric` (`Instant`, which a cross-series
/// aggregate reads): a selector over `metric{labels}`.
fn selector(memo: &mut Memo, kind: TimeRangeKind, metric: &str, labels: &[&str]) -> MemoGroupId {
    let scan = scan_group(memo, metric, labels);
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
/// (B's relative ratio cannot read KLL's rank error) drops edges in PR 4,
/// so Pass 1 offers both families to every quantile group.
#[test]
fn running_example_gives_one_build_per_theta_and_separate_readouts() {
    let mut memo = Memo::default();
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
    query(&mut memo, g3);
    query(&mut memo, g6);
    let memo = pass1_memo(memo, None).unwrap();
    println!("{}", memo_printout(&memo));
    let (kll, _) = state_group(&memo, SketchAlgorithm::Kll);
    let (ddsketch, _) = state_group(&memo, SketchAlgorithm::DDSketch);
    for (state, family) in [
        (kll, SketchAlgorithm::Kll),
        (ddsketch, SketchAlgorithm::DDSketch),
    ] {
        let thetas: Vec<SketchParams> = candidates(&memo, state)
            .iter()
            .map(|candidate| candidate.logical_properties.parameters().unwrap().clone())
            .collect();
        assert_eq!(thetas, parameter_list(family));
        // θ is a property, never an operator argument: every build has the
        // group's own expression.
        assert!(candidates(&memo, state).iter().all(|candidate| {
            candidate.expression.operator == memo.groups()[state.0 as usize].expression
        }));
    }
    // Each quantile group: its exact aggregate and one readout per family,
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
    let mut memo = Memo::default();
    let rows = selector(&mut memo, TimeRangeKind::Range, "latency", &["job"]);
    let per_series = aggregate_group(
        &mut memo,
        rows,
        Reduction::PerEntity,
        vec![quantile(0.9)],
        None,
    );
    query(&mut memo, per_series);
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
    let mut memo = Memo::default();
    let scan = group(
        &mut memo,
        scan_operator("latency", metric_schema(&["job"], vec![])),
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
    query(&mut memo, per_series);
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
    let mut memo = Memo::default();
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let both = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![2]),
        vec![sum(), quantile(0.9)],
        None,
    );
    query(&mut memo, both);
    let targets = computation_targets(&memo, None).unwrap();
    let measures: Vec<(MemoGroupId, usize)> = targets
        .iter()
        .map(|target| (target.aggregate_group, target.measure_index))
        .collect();
    assert_eq!(measures, vec![(both, 0), (both, 1)]);
}

/// An aggregate with no realization besides exact execution is not a target.
#[test]
fn aggregate_without_realization_is_not_a_target() {
    let mut memo = Memo::default();
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &[]);
    let quantile_exact = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![]),
        vec![AggIntent::Quantile {
            col: None,
            q: 0.5,
            accuracy: AccuracyTarget::exact(),
        }],
        None,
    );
    query(&mut memo, quantile_exact);
    let memo = pass1_memo(memo, None).unwrap();
    assert_eq!(candidates(&memo, quantile_exact).len(), 1);
}

/// An aggregate several queries read, or a scalar root reads twice, is one
/// target per measure: Pass 1 walks the groups.
#[test]
fn an_aggregate_read_by_several_roots_is_one_target() {
    let mut memo = Memo::default();
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &[]);
    let p = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![]),
        vec![quantile(0.9)],
        None,
    );
    query(&mut memo, p);
    query(&mut memo, p);
    assert_eq!(computation_targets(&memo, None).unwrap().len(), 1);
}

/// A memo whose groups already hold ASAP operators, and an approximate
/// accuracy target outside (0, 1), fail at the stage boundary.
#[test]
fn asap_input_and_invalid_accuracy_are_rejected() {
    let mut memo = Memo::default();
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &[]);
    let state = group(
        &mut memo,
        Operator::ASAP(ASAPOp::SummaryAgg {
            child: rows,
            family: FieldDataType::exact(ExactKind::Sum),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            grouping: Default::default(),
            filter: None,
        }),
    );
    query(&mut memo, state);
    assert!(matches!(
        pass1_memo(memo, None),
        Err(PerComputationCandidateError::ContainsAsap)
    ));
    let mut memo = Memo::default();
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &[]);
    let invalid = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![]),
        vec![AggIntent::Quantile {
            col: None,
            q: 0.5,
            accuracy: AccuracyTarget::max_error(1.5),
        }],
        None,
    );
    query(&mut memo, invalid);
    assert!(matches!(
        pass1_memo(memo, None),
        Err(PerComputationCandidateError::InvalidAccuracy { .. })
    ));
}

/// The summaries the deployment does not support are not offered.
#[test]
fn unsupported_summaries_are_not_offered() {
    let mut memo = Memo::default();
    let rows = selector(&mut memo, TimeRangeKind::Range, "m", &[]);
    let p = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![]),
        vec![quantile(0.9)],
        None,
    );
    query(&mut memo, p);
    let memo = pass1_memo(memo, Some(&[])).unwrap();
    assert_eq!(candidates(&memo, p).len(), 1);
    assert!(groups_where(&memo, |e| matches!(e, Operator::ASAP(_))).is_empty());
}

// ── Candidate composition ──────────────────────────────────────────────────

/// Every group has its own expression as a candidate, with no θ and no
/// floor: plain exact execution.
#[test]
fn every_group_offers_its_own_expression() {
    let mut memo = Memo::default();
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let s = aggregate_group(&mut memo, rows, Reduction::by(vec![2]), vec![sum()], None);
    query(&mut memo, s);
    let stage0 = memo.groups().len();
    let memo = pass1_memo(memo, None).unwrap();
    for index in 0..stage0 {
        let group = &memo.groups()[index];
        let own = &group.candidates[0];
        assert_eq!(own.expression.operator, group.expression);
        assert!(own.logical_properties.parameters().is_none());
        assert!(own
            .input_requirements
            .iter()
            .all(|r| r.parameter_floor_for_required_accuracy().is_none()));
    }
}

/// An exact accumulator is a state group (`SummaryAgg` of the exact
/// family, no θ) read by a `FinalizeExactAccumulator` group, which the
/// aggregate's group reads through a renaming `Project`.
#[test]
fn exact_accumulator_is_a_state_group_and_a_finalize() {
    let mut memo = Memo::default();
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let s = aggregate_group(&mut memo, rows, Reduction::by(vec![2]), vec![sum()], None);
    query(&mut memo, s);
    let memo = pass1_memo(memo, None).unwrap();
    let states = groups_where(&memo, |e| {
        matches!(
            e,
            Operator::ASAP(ASAPOp::SummaryAgg {
                family: FieldDataType::ExactAggregate(ExactKind::Sum, _),
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
    let mut memo = Memo::default();
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
                accuracy: approximate(),
            },
        ],
        None,
    );
    query(&mut memo, both);
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

/// Pass 1's candidates leave every input's floor empty except a top-k
/// readout's (the heap size it needs): the consumers of state groups
/// propagate error linearly, so no validity cap applies.
#[test]
fn only_a_topk_readout_has_a_parameter_floor() {
    let mut memo = Memo::default();
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
    query(&mut memo, ranked);
    query(&mut memo, p);
    let memo = pass1_memo(memo, None).unwrap();
    for (index, group) in memo.groups().iter().enumerate() {
        for candidate in &group.candidates {
            let floors: Vec<_> = candidate
                .input_requirements
                .iter()
                .filter_map(|r| r.parameter_floor_for_required_accuracy())
                .collect();
            match &candidate.expression.operator {
                Operator::ASAP(ASAPOp::SummaryEstimate {
                    query: SketchStatistic::TopK { .. },
                    ..
                }) => {
                    let [floor] = floors[..] else {
                        panic!("G{index}: {floors:?}")
                    };
                    assert!(matches!(
                        floor,
                        SketchParams::CmsWithHeap { heap_size: 15, .. }
                            | SketchParams::CountSketchWithHeap { heap_size: 15, .. }
                    ));
                }
                _ => assert!(floors.is_empty(), "G{index}"),
            }
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
    let mut memo = Memo::default();
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
    query(&mut memo, having);
    query(&mut memo, by_job);
    let memo = pass1_memo(memo, None).unwrap();
    let below = pushed_below(&memo, rows);
    assert_eq!(below.len(), 1, "{}", memo_printout(&memo));
    // The Filter group: its own expression, then one readout per family
    // over states built from the filtered rows.
    assert_eq!(candidates(&memo, having).len(), 3);
    assert_eq!(candidates(&memo, by_job).len(), 3);
}

/// A `HAVING` over an aggregate value stays above the output: nothing is
/// pushed below, and the `Filter` group keeps its own expression only.
#[test]
fn value_having_stays_above() {
    let mut memo = Memo::default();
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
    query(&mut memo, having);
    let memo = pass1_memo(memo, None).unwrap();
    assert!(pushed_below(&memo, rows).is_empty());
    assert_eq!(candidates(&memo, having).len(), 1);
}

/// A global aggregate's `HAVING` stays above even when it is constant: the
/// one output row exists with no input row.
#[test]
fn global_aggregate_having_stays_above() {
    let mut memo = Memo::default();
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
    query(&mut memo, having);
    let memo = pass1_memo(memo, None).unwrap();
    assert!(pushed_below(&memo, rows).is_empty());
}

/// The aggregate's own value `having` is a `Filter` above each readout:
/// the aggregate group's summary candidates are `Filter`s over a readout.
#[test]
fn own_value_having_filters_each_readout() {
    let mut memo = Memo::default();
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let by_job = aggregate_group(
        &mut memo,
        rows,
        Reduction::by(vec![2]),
        vec![quantile(0.9)],
        Some(compare(1, CompareOpKind::Gt, ScalarValue::Float64(10.0))),
    );
    query(&mut memo, by_job);
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
    let mut memo = Memo::default();
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
    query(&mut memo, ranked);
    let memo = pass1_memo(memo, None).unwrap();
    for family in [
        SketchAlgorithm::CmsWithHeap,
        SketchAlgorithm::CountSketchWithHeap,
    ] {
        let (state, update) = state_group(&memo, family);
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
        assert_eq!(candidates(&memo, state).len(), parameter_list(family).len());
    }
}

/// A top-k over a `count` ranks the counted items with unit weight, proven
/// non-negative.
#[test]
fn count_ranked_topk_counts_with_unit_weight() {
    let mut memo = Memo::default();
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let counts = aggregate_group(&mut memo, rows, Reduction::by(vec![2]), vec![count()], None);
    let ranked = aggregate_group(
        &mut memo,
        counts,
        Reduction::by(vec![]),
        vec![topk(5)],
        None,
    );
    query(&mut memo, ranked);
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
    let mut memo = Memo::default();
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
    query(&mut memo, ranked);
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
        let mut memo = Memo::default();
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
        query(&mut memo, by_job);
        let memo = pass1_memo(memo, None).unwrap();
        let outer_state = groups_where(
            &memo,
            |e| matches!(e, Operator::ASAP(ASAPOp::SummaryAgg { child, family: FieldDataType::ExactAggregate(state, _), .. }) if *child == per_series && *state == kind),
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
        let mut memo = Memo::default();
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
        query(&mut memo, by_job);
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
    let mut memo = Memo::default();
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let counts = aggregate_group(&mut memo, rows, Reduction::by(vec![2]), vec![count()], None);
    query(&mut memo, counts);
    let memo = pass1_memo(memo, None).unwrap();
    let (_, update) = state_group(&memo, SketchAlgorithm::Cms);
    assert_eq!(update.weight, SummaryInputExpr::Constant(1.0));
    let mut memo = Memo::default();
    let rows = selector(&mut memo, TimeRangeKind::Instant, "m", &["job"]);
    let sums = aggregate_group(&mut memo, rows, Reduction::by(vec![2]), vec![sum()], None);
    query(&mut memo, sums);
    let memo = pass1_memo(memo, None).unwrap();
    let state = groups_where(
        &memo,
        |e| matches!(e, Operator::ASAP(ASAPOp::SummaryAgg { input, .. }) if input.weight == SummaryInputExpr::Column(ColumnRef::SampleValue)),
    );
    assert_eq!(state.len(), 1);
}

// ── Intents ────────────────────────────────────────────────────────────────

fn candidates_for(intent: AggIntent) -> Vec<HowToComputeAggregateMeasure> {
    computation_candidates_for_intent(&intent)
        .unwrap()
        .candidates
}

fn algorithms(choices: &[HowToComputeAggregateMeasure]) -> Vec<SketchAlgorithm> {
    choices
        .iter()
        .filter_map(|choice| match choice {
            HowToComputeAggregateMeasure::Summary(algorithm) => Some(*algorithm),
            _ => None,
        })
        .collect()
}

/// Example 2 preserves specialized distinct summaries and the universal alternative.
#[test]
fn cardinality_keeps_exact_specialized_and_universal_candidates() {
    let choices = candidates_for(AggIntent::Cardinality {
        cols: vec![0],
        accuracy: approximate(),
    });
    assert_eq!(choices[0], HowToComputeAggregateMeasure::Exact);
    assert_eq!(
        algorithms(&choices),
        vec![
            SketchAlgorithm::Hll,
            SketchAlgorithm::Kmv,
            SketchAlgorithm::UnivMon
        ]
    );
    let tuple = candidates_for(AggIntent::Cardinality {
        cols: vec![0, 1],
        accuracy: approximate(),
    });
    assert!(!algorithms(&tuple).contains(&SketchAlgorithm::UnivMon));
}

/// Frequency moments retain exact execution and a universal sketch without certification.
#[test]
fn frequency_statistics_keep_universal_choices() {
    for intent in [
        AggIntent::FrequencyL2 {
            col: Some(0),
            accuracy: approximate(),
        },
        AggIntent::FrequencyEntropy {
            col: Some(0),
            accuracy: approximate(),
        },
    ] {
        let choices = candidates_for(intent);
        assert_eq!(choices[0], HowToComputeAggregateMeasure::Exact);
        assert_eq!(algorithms(&choices), vec![SketchAlgorithm::UnivMon]);
    }
}

/// TopK keeps both declared heap substrates without choosing an implementation.
#[test]
fn topk_keeps_both_specialized_heap_choices() {
    let choices = candidates_for(topk(10));
    assert_eq!(choices[0], HowToComputeAggregateMeasure::Exact);
    assert_eq!(
        algorithms(&choices),
        vec![
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap
        ]
    );
}

/// Summary candidates are unsized, so they do not depend on the requested error.
#[test]
fn summary_candidates_are_unsized() {
    let tight = candidates_for(AggIntent::Count {
        accuracy: AccuracyTarget::max_error(0.0001).failure_probability(0.0001),
    });
    assert_eq!(candidates_for(count()), tight);
}

/// Approximate count lists one exact option, its accumulator, beside the
/// summaries (the plain aggregate is the group's own expression).
#[test]
fn approximate_count_lists_its_accumulator_and_summaries() {
    let choices = candidates_for(count());
    assert!(!choices.contains(&HowToComputeAggregateMeasure::Exact));
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
        !candidates_for(count()).contains(&HowToComputeAggregateMeasure::Summary(
            SketchAlgorithm::CountSketch
        ))
    );
}
