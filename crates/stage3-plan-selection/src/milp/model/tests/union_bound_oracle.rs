//! A brute-force check of constraint (K)'s failure-probability rows
//! (`accuracy-models.md`, "Which estimates a result depends on"): for
//! every plan the MILP accepts, the union-bound sum over the estimates the
//! covered output values depend on, maximized over every dataset the
//! declared facts allow, is at most the query's δ.
//!
//! It computes the union-bound worst case, `max_D Σ_{e ∈ S_q(D)} δ_e`, not
//! the true failure probability (which also depends on how the estimates'
//! failures correlate); that sum is what the rows must bound.
//!
//! Each shape is built over two metrics `m` and `b`, each declaring 6
//! series (labels `job` ∈ {0, 1} and `instance` ∈ {0, 1, 2}); a dataset is
//! a subset of each metric's 6 series (64 × 64 datasets). Each query's
//! selected path is evaluated over every dataset, every output value
//! carrying the estimates (readout copy, series) it depends on:
//! - a readout's value of a series depends on that estimate;
//! - an exact per-series value on none;
//! - a filter keeps `job = 0`;
//! - `or` copies the left value, else the right one; `and` and `unless`
//!   copy the left value where the right does (not) have the series;
//! - `+` one to one unions both sides' sets; `+ on() group_left` adds the
//!   right side's only value (no output unless it has exactly one);
//! - `max` and `avg by (job)` union their group's sets.
//!
//! Each readout group gets the level Pass 2's split gives each query
//! reaching it (`AccuracyTarget::failure_probability_per_readout_value`
//! over `asap_accuracy::readout_value_counts_of_queries`) and random
//! further copies, some looser and cheaper, so the solver is tempted by
//! copies given for another query. Queries are planned alone and in pairs
//! (a per-value query on a readout with a query over a shape reading it),
//! under the whole-result, per-value and record-share scopes.

use std::collections::{BTreeMap, BTreeSet};

use asap_accuracy::{readout_value_counts_of_queries, ErrorMetricsOfMemo};
use asap_types::workload::bounds::source_name;

use super::*;

/// A series of `m` or `b`: (job, instance).
type Series = (u8, u8);

/// An output value's key: a series, a job, or nothing (a global
/// reduction).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Series(Series),
    Job(u8),
    Nothing,
}

/// An estimate: the readout execution's physical candidate and the
/// series.
type Estimate = (usize, Series);

/// One evaluation's output: per key, the estimates the value depends on.
type Values = BTreeMap<Key, BTreeSet<Estimate>>;

const SERIES: [Series; 6] = [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2)];

/// A small deterministic generator (SplitMix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A PromQL set operator.
#[derive(Debug, Clone, Copy)]
enum SetKind {
    Or,
    And,
    Unless,
}

impl SetKind {
    fn operator(self) -> PromQLVectorSetOpKind {
        match self {
            Self::Or => PromQLVectorSetOpKind::Or,
            Self::And => PromQLVectorSetOpKind::And,
            Self::Unless => PromQLVectorSetOpKind::Unless,
        }
    }
}

/// The query shapes, as builders over a memo.
#[derive(Debug, Clone, Copy)]
enum Shape {
    Readout(&'static str),
    Exact(&'static str),
    Filter(&'static Shape),
    Max(&'static Shape),
    AvgByJob(&'static Shape),
    Set(SetKind, &'static Shape, &'static Shape),
    Add(&'static Shape, &'static Shape),
    /// `left + on() group_left max(right)`.
    AddMaxOfRight(&'static Shape, &'static Shape),
}

const RM: Shape = Shape::Readout("m");
const RB: Shape = Shape::Readout("b");
const EM: Shape = Shape::Exact("m");
const FILTER_RM: Shape = Shape::Filter(&RM);
const OR: Shape = Shape::Set(SetKind::Or, &RM, &RB);
const OR_SELF: Shape = Shape::Set(SetKind::Or, &RM, &FILTER_RM);
const AND: Shape = Shape::Set(SetKind::And, &RM, &RB);
const UNLESS: Shape = Shape::Set(SetKind::Unless, &RM, &RB);
const ADD: Shape = Shape::Add(&RM, &RB);
const ADD_EXACT: Shape = Shape::Add(&EM, &RB);
const GROUP_LEFT: Shape = Shape::AddMaxOfRight(&EM, &RB);
const GROUP_LEFT_READOUTS: Shape = Shape::AddMaxOfRight(&RM, &RB);

/// Every shape of depth at most 3 the oracle covers.
const SHAPES: &[Shape] = &[
    RM,
    FILTER_RM,
    Shape::Max(&RM),
    Shape::AvgByJob(&RM),
    OR,
    OR_SELF,
    AND,
    UNLESS,
    ADD,
    ADD_EXACT,
    GROUP_LEFT,
    GROUP_LEFT_READOUTS,
    Shape::Max(&OR),
    Shape::Max(&OR_SELF),
    Shape::AvgByJob(&OR),
    Shape::Max(&ADD),
    Shape::AvgByJob(&ADD_EXACT),
    Shape::Max(&AND),
    Shape::Filter(&OR),
    Shape::Set(SetKind::Or, &ADD, &RB),
    Shape::Add(&OR, &RB),
];

/// The column of label `label` in `group`'s schema.
fn column_of(memo: &Memo, group: MemoGroupId, label: &str) -> usize {
    memo.groups()[group.0 as usize]
        .group_properties
        .schema
        .fields
        .iter()
        .position(|field| field.name == label)
        .unwrap()
}

/// Builds `shape` in `memo`; the readout groups it adds are listed in
/// `readouts` (each with one candidate without a requirement, as Pass 1
/// leaves it).
fn build(memo: &mut Memo, shape: Shape, readouts: &mut BTreeSet<MemoGroupId>) -> MemoGroupId {
    let built = match shape {
        Shape::Readout(metric) => {
            let rows = scan_group(memo, metric, &["job", "instance"], vec![]);
            let window = range(memo, 60, rows);
            let state = sketch_state(
                memo,
                window,
                SketchAlgorithm::DDSketch,
                Reduction::PerEntity,
            );
            let operator = memo_fixtures::readout(SketchStatistic::Quantile { q: 0.9 }, state);
            let group = memo.find_or_add_group(operator.clone()).unwrap();
            if readouts.insert(group) {
                try_candidate(memo, group, operator, None).unwrap();
            }
            return group;
        }
        Shape::Exact(metric) => {
            let rows = scan_group(memo, metric, &["job", "instance"], vec![]);
            let window = range(memo, 60, rows);
            memo_fixtures::aggregate(Reduction::PerEntity, AggIntent::Max { col: None }, window)
        }
        Shape::Filter(child) => {
            let child = build(memo, *child, readouts);
            let job_0 = memo_fixtures::compare(
                column_of(memo, child, "job"),
                CompareOpKind::Eq,
                ScalarValue::Utf8("0".into()),
                ExprSemantics::Promql,
            );
            memo_fixtures::filter(job_0, child)
        }
        Shape::Max(child) => {
            let child = build(memo, *child, readouts);
            memo_fixtures::aggregate(Reduction::by(vec![]), AggIntent::Max { col: None }, child)
        }
        Shape::AvgByJob(child) => {
            let child = build(memo, *child, readouts);
            let job = column_of(memo, child, "job");
            memo_fixtures::aggregate(
                Reduction::by(vec![job]),
                AggIntent::Avg { col: None },
                child,
            )
        }
        Shape::Set(kind, left, right) => {
            let (left, right) = (build(memo, *left, readouts), build(memo, *right, readouts));
            memo_fixtures::set_operator(kind.operator(), left, right)
        }
        Shape::Add(left, right) => {
            let (left, right) = (build(memo, *left, readouts), build(memo, *right, readouts));
            arithmetic(ArithmeticOpKind::Add, left, right)
        }
        Shape::AddMaxOfRight(left, right) => {
            let left = build(memo, *left, readouts);
            let right = build(memo, Shape::Max(right), readouts);
            Operator::NonASAP(NonASAPOp::BinaryOp {
                operator: BinaryOperator {
                    checked_relative_division: false,
                    checked_finite_division: false,
                    kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Add),
                    vector_match: Some(VectorMatch {
                        kind: VectorMatchKind::On,
                        labels: vec![],
                        grouping: Some(VectorGrouping {
                            side: GroupSide::Left,
                            labels: vec![],
                        }),
                    }),
                },
                return_bool: false,
                lhs: left,
                rhs: right,
            })
        }
    };
    group(memo, built)
}

/// What one evaluation of execution `v` returns over `dataset` (the
/// series of each metric present), with `producer_of` the selected
/// producer per (consumer, input).
fn evaluate(
    physical: &PhysicalASAPMemoCandidates,
    producer_of: &BTreeMap<(usize, usize), usize>,
    v: usize,
    dataset: &BTreeMap<&str, Vec<Series>>,
) -> Values {
    let memo = &physical.logical_candidates.memo;
    let id = physical.physical_candidates[v].logical_candidate_id;
    let candidate = memo.candidate(id);
    let input = |i: usize| evaluate(physical, producer_of, producer_of[&(v, i)], dataset);
    let series_of_group = || {
        let source = memo.groups()[id.group.0 as usize]
            .group_properties
            .scanned_sources
            .iter()
            .next()
            .unwrap();
        dataset[source_name(source)].clone()
    };
    match &candidate.expression.operator {
        Operator::ASAP(ASAPOp::SummaryEstimate { .. }) => series_of_group()
            .into_iter()
            .map(|series| (Key::Series(series), [(v, series)].into()))
            .collect(),
        Operator::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            ..
        }) => series_of_group()
            .into_iter()
            .map(|series| (Key::Series(series), BTreeSet::new()))
            .collect(),
        Operator::NonASAP(NonASAPOp::Filter { .. }) => input(0)
            .into_iter()
            .filter(|(key, _)| matches!(key, Key::Series((0, _))))
            .collect(),
        Operator::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::Reduce(by),
            ..
        }) => {
            let mut out = Values::new();
            for (key, estimates) in input(0) {
                let group = match (by.keys().is_empty(), key) {
                    (true, _) => Key::Nothing,
                    (false, Key::Series((job, _)) | Key::Job(job)) => Key::Job(job),
                    (false, Key::Nothing) => Key::Nothing,
                };
                out.entry(group).or_default().extend(estimates);
            }
            out
        }
        Operator::NonASAP(NonASAPOp::BinaryOp {
            operator: BinaryOperator {
                kind, vector_match, ..
            },
            ..
        }) => {
            let (left, right) = (input(0), input(1));
            match (kind, vector_match) {
                (BinaryOpKind::Set(PromQLVectorSetOpKind::Or), _) => {
                    let mut out = right;
                    out.extend(left);
                    out
                }
                (BinaryOpKind::Set(PromQLVectorSetOpKind::And), _) => left
                    .into_iter()
                    .filter(|(key, _)| right.contains_key(key))
                    .collect(),
                (BinaryOpKind::Set(PromQLVectorSetOpKind::Unless), _) => left
                    .into_iter()
                    .filter(|(key, _)| !right.contains_key(key))
                    .collect(),
                (_, Some(_)) => {
                    // `on() group_left`: the right side's only value.
                    let mut only = right.into_values();
                    match (only.next(), only.next()) {
                        (Some(matched), None) => left
                            .into_iter()
                            .map(|(key, mut estimates)| {
                                estimates.extend(matched.iter().copied());
                                (key, estimates)
                            })
                            .collect(),
                        _ => Values::new(),
                    }
                }
                (_, None) => left
                    .into_iter()
                    .filter_map(|(key, mut estimates)| {
                        estimates.extend(right.get(&key)?.iter().copied());
                        Some((key, estimates))
                    })
                    .collect(),
            }
        }
        other => panic!("the oracle does not evaluate {other:?}"),
    }
}

/// The union-bound worst case of query `query`'s selected path at `v`
/// over every dataset: the largest summed δ of the estimates the values
/// its scope covers depend on.
fn worst_union_bound(
    physical: &PhysicalASAPMemoCandidates,
    producer_of: &BTreeMap<(usize, usize), usize>,
    v: usize,
    scope: AccuracyScope,
) -> f64 {
    let memo = &physical.logical_candidates.memo;
    let delta_of = |copy: usize| {
        let id = physical.physical_candidates[copy].logical_candidate_id;
        memo.candidate(id)
            .required_accuracy()
            .map_or(0.0, |required| required.failure_probability)
    };
    let mut worst: f64 = 0.0;
    for m_subset in 0u32..64 {
        for b_subset in 0u32..64 {
            let present = |subset: u32| -> Vec<Series> {
                (0..6)
                    .filter(|bit| subset & (1 << bit) != 0)
                    .map(|bit| SERIES[bit])
                    .collect()
            };
            let dataset: BTreeMap<&str, Vec<Series>> =
                [("m", present(m_subset)), ("b", present(b_subset))].into();
            let values: Vec<BTreeSet<Estimate>> = evaluate(physical, producer_of, v, &dataset)
                .into_values()
                .collect();
            let sum = |covered: &[&BTreeSet<Estimate>]| -> f64 {
                let union: BTreeSet<Estimate> =
                    covered.iter().flat_map(|set| set.iter().copied()).collect();
                union.iter().map(|&(copy, _)| delta_of(copy)).sum()
            };
            let covered = match scope {
                AccuracyScope::PerValue => values.iter().map(|set| sum(&[set])).fold(0.0, f64::max),
                AccuracyScope::WholeResult => sum(&values.iter().collect::<Vec<_>>()),
                AccuracyScope::GroupsWithRecordShareAtLeast(share) => {
                    // Which groups qualify depends on exact record counts:
                    // any `⌊1 / share⌋` of them may.
                    let at_most = (1.0 / share).floor() as usize;
                    (0u32..1 << values.len())
                        .filter(|chosen| chosen.count_ones() as usize <= at_most)
                        .map(|chosen| {
                            let subset: Vec<&BTreeSet<Estimate>> = values
                                .iter()
                                .enumerate()
                                .filter(|(index, _)| chosen & (1 << index) != 0)
                                .map(|(_, set)| set)
                                .collect();
                            sum(&subset)
                        })
                        .fold(0.0, f64::max)
                }
            };
            worst = worst.max(covered);
        }
    }
    worst
}

/// The most values one evaluation of `shape` returns over the datasets,
/// $n_q$ as Stage 0 would bound it (at least 1).
fn values_per_evaluation(shape: Shape) -> u64 {
    match shape {
        Shape::Readout(_) | Shape::Exact(_) | Shape::Filter(_) => 6,
        Shape::Max(_) => 1,
        Shape::AvgByJob(_) => 2,
        Shape::Set(..) | Shape::Add(..) | Shape::AddMaxOfRight(..) => 6,
    }
}

/// One planning case: the queries (shape, scope), with `delta` each (no
/// failure probability when `None`, which only checks that a plan
/// exists).
fn check(
    queries: &[(Shape, AccuracyScope)],
    delta: Option<f64>,
    rng: &mut Rng,
    report: &mut OracleReport,
) {
    let mut memo = memo_with_series(&[("m", 6), ("b", 6)]);
    let mut readouts = BTreeSet::new();
    let mut roots = Vec::new();
    for &(shape, scope) in queries {
        let root = build(&mut memo, shape, &mut readouts);
        roots.push(root);
        let mut target = AccuracyTarget::max_error(1.0)
            .accuracy_metric(AccuracyMetric::ABSOLUTE)
            .accuracy_scope(scope);
        if let Some(delta) = delta {
            target = target.failure_probability(delta);
        }
        add_query(&mut memo, root, target, Some(values_per_evaluation(shape)));
    }
    // Pass 2's level per query reaching each readout group, plus random
    // copies at δ from 1e-5 to 0.5.
    let order = memo.topological_order().unwrap();
    let counts =
        readout_value_counts_of_queries(&memo, &ErrorMetricsOfMemo::of(&memo, &order), &order);
    for &readout in &readouts {
        let mut levels: Vec<f64> = memo.groups()[readout.0 as usize]
            .reached_by_queries()
            .iter()
            .filter_map(|&query| {
                let memo_query = &memo.queries()[query];
                memo_query
                    .accuracy_target
                    .as_ref()?
                    .failure_probability_per_readout_value(
                        memo_query.number_of_values_per_query_evaluation,
                        counts[&query],
                    )
            })
            .collect();
        for _ in 0..2 {
            levels.push([1e-5, 1e-4, 1e-3, 5e-3, 0.01, 0.05, 0.5][rng.below(7) as usize]);
        }
        let pass1 = CandidateForMemoGroupId {
            group: readout,
            index_in_group: 0,
        };
        let unset = memo.candidate(pass1).clone();
        let requirement = |failure_probability| {
            vec![LogicalRequirements {
                required_accuracy: Some(RequiredAccuracy {
                    metric: AccuracyMetric::ABSOLUTE,
                    max_error: 0.01,
                    failure_probability,
                }),
            }]
        };
        memo.set_candidate_input_requirements(pass1, requirement(levels[0]));
        for &level in &levels[1..] {
            let mut copy = unset.clone();
            copy.input_requirements = requirement(level);
            let _ = memo.add_candidate(readout, copy);
        }
    }
    let physical = physical(memo.clone(), &every_candidate(&memo));
    // Random costs; a looser copy tends to be cheaper.
    let w: Vec<f64> = physical
        .physical_candidates
        .iter()
        .map(|candidate| {
            let looseness = memo
                .candidate(candidate.logical_candidate_id)
                .required_accuracy()
                .map_or(0.0, |required| required.failure_probability.log10() + 6.0);
            1.0 + rng.below(5) as f64 + 10.0 / (1.0 + looseness)
        })
        .collect();
    let entries = every_10s(queries.len());
    let solved = match solve(&physical, &entries, &w, &[], &[]).0 {
        Ok(solved) => solved,
        Err(SelectionMilpError::Infeasible) => {
            report.infeasible.push(format!("{queries:?}"));
            return;
        }
        Err(other) => panic!("{queries:?}: {other:?}"),
    };
    let (selectable, _) = selection_inputs(&physical, &entries, &w, &[], &[]);
    let position_of = |execution: &Execution| {
        selectable
            .executions()
            .iter()
            .position(|known| known == execution)
            .unwrap()
    };
    let candidate_of = |position: usize| selectable.executions()[position].candidate.0;
    let mut producer_of = BTreeMap::new();
    for edge in &solved.edges {
        let input = physical.edge(edge.edge).input_index_of_consumer;
        producer_of.insert(
            (edge.consumer.candidate.0, input),
            candidate_of(position_of(&edge.producer)),
        );
    }
    for (query, output) in solved.outputs.iter().enumerate() {
        let (_, scope) = queries[query];
        let worst = worst_union_bound(&physical, &producer_of, output.candidate.0, scope);
        report.accepted += 1;
        if worst > 0.01 * (1.0 + 1e-9) {
            report
                .violations
                .push(format!("{queries:?} query {query}: worst case {worst}"));
        }
    }
}

#[derive(Default)]
struct OracleReport {
    accepted: usize,
    infeasible: Vec<String>,
    violations: Vec<String>,
}

/// Every shape alone, under every scope, and paired with a per-value
/// query on `m`'s readout, each with three random draws of copies: every
/// accepted plan's union-bound worst case is within δ = 0.01. Every shape
/// planned alone that has a plan without a failure probability has one
/// with it (Pass 2's level for a query meets (K) for that query). A
/// filter or `avg by (job)` above `m or b` has none either way: no error
/// contract covers its label over two sources.
#[test]
fn every_accepted_plan_meets_its_failure_probability_on_every_dataset() {
    let mut rng = Rng(1);
    let mut report = OracleReport::default();
    let scopes = [
        AccuracyScope::WholeResult,
        AccuracyScope::PerValue,
        AccuracyScope::GroupsWithRecordShareAtLeast(0.5),
    ];
    let mut without_plan = Vec::new();
    for _ in 0..3 {
        for &shape in SHAPES {
            for scope in scopes {
                let mut alone = OracleReport::default();
                check(&[(shape, scope)], Some(0.01), &mut rng, &mut alone);
                if !alone.infeasible.is_empty() {
                    let mut without_delta = OracleReport::default();
                    check(&[(shape, scope)], None, &mut rng, &mut without_delta);
                    if without_delta.infeasible.is_empty() {
                        without_plan.extend(alone.infeasible);
                    }
                }
                report.accepted += alone.accepted;
                report.violations.extend(alone.violations);
            }
            check(
                &[
                    (RM, AccuracyScope::PerValue),
                    (shape, AccuracyScope::WholeResult),
                ],
                Some(0.01),
                &mut rng,
                &mut report,
            );
        }
    }
    assert!(report.violations.is_empty(), "{:#?}", report.violations);
    assert!(without_plan.is_empty(), "{without_plan:#?}");
    assert!(
        report.accepted >= 3 * SHAPES.len() * scopes.len(),
        "{}",
        report.accepted
    );
}
