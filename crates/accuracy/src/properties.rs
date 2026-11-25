//! Property tests of how ε and δ combine (T14 of the testing-infrastructure
//! design): the per-algorithm guarantees (`guarantee`), the comparison of a
//! bound with a limit and a build's check against a readout level
//! (`logical_edge`), and the operators' error contracts (`error_contract`).
//! Generators include the edge values −0.0, NaN, ±Inf and q ∉ [0, 1];
//! failures shrink and are kept in `proptest-regressions/properties.txt`.

use proptest::prelude::*;

use asap_types::accuracy::AccuracyMetric;
use asap_types::ir::operator::arguments::PromQLVectorSetOpKind;
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{DataType, FieldDataType};
use asap_types::stages::{Memo, MemoGroupId, RequiredAccuracy, ValueSign};
use asap_types::summary_type::{SketchAlgorithm, SketchParams, SketchStatistic, KLL_MAX_K};
use asap_types::test_support::memo_fixtures::{arithmetic, scan, set_operator, state};
use asap_types::workload::{DataWorkload, SourceFacts};
use std::collections::BTreeMap;

use crate::error_contract::{error_contract, ErrorContract};
use crate::guarantee::theoretical_guarantee;
use crate::logical_edge::build_meets_level;
use crate::{tolerated_limit, within_limit, NoMeasuredGuarantees};

/// A float that is often an edge value: ±0, NaN, ±Inf, the smallest
/// positive normal, 1, or one just past [0, 1].
fn edge_f64() -> impl Strategy<Value = f64> {
    prop_oneof![
        Just(-0.0),
        Just(0.0),
        Just(f64::NAN),
        Just(f64::INFINITY),
        Just(f64::NEG_INFINITY),
        Just(f64::MIN_POSITIVE),
        Just(1.0),
        Just(1.0 + f64::EPSILON),
        Just(-f64::MIN_POSITIVE),
        -2.0..2.0f64,
        any::<f64>(),
    ]
}

/// [`edge_f64`] without NaN.
fn ordered_edge_f64() -> impl Strategy<Value = f64> {
    edge_f64().prop_map(|x| if x.is_nan() { f64::NEG_INFINITY } else { x })
}

/// A failure probability that is none: NaN or negative.
fn invalid_probability() -> impl Strategy<Value = f64> {
    prop_oneof![
        Just(f64::NAN),
        Just(f64::NEG_INFINITY),
        Just(-f64::MIN_POSITIVE),
        -2.0..-f64::MIN_POSITIVE,
    ]
}

/// A failure probability in (0, 1), log-uniform down to 1e-15.
fn probability() -> impl Strategy<Value = f64> {
    (0.0..15.0f64).prop_map(|exponent| 10f64.powf(-exponent))
}

/// Parameters of every non-Hydra structure, sizes from 0 past their
/// maxima, DDSketch's α in (0, 1), and the algorithm whose state they
/// size. (An α outside (0, 1) is in [`an_invalid_alpha_has_no_guarantee`].)
fn parameters() -> impl Strategy<Value = (SketchAlgorithm, SketchParams)> {
    let size = || prop_oneof![Just(0u32), Just(1), Just(u32::MAX), 0..100_000u32];
    prop_oneof![
        (0..KLL_MAX_K + 10).prop_map(|k| (SketchAlgorithm::Kll, SketchParams::Kll { k })),
        size().prop_map(|k| (SketchAlgorithm::Kmv, SketchParams::Kmv { k })),
        (size(), 0..64u32)
            .prop_map(|(width, depth)| (SketchAlgorithm::Cms, SketchParams::Cms { width, depth })),
        (size(), 0..64u32, size()).prop_map(|(width, depth, heap_size)| (
            SketchAlgorithm::CmsWithHeap,
            SketchParams::CmsWithHeap {
                width,
                depth,
                heap_size
            }
        )),
        (size(), 0..64u32).prop_map(|(width, depth)| (
            SketchAlgorithm::CountSketch,
            SketchParams::CountSketch { width, depth }
        )),
        (size(), 0..64u32, size()).prop_map(|(width, depth, heap_size)| (
            SketchAlgorithm::CountSketchWithHeap,
            SketchParams::CountSketchWithHeap {
                width,
                depth,
                heap_size
            }
        )),
        (0..=32u8).prop_map(|precision| (SketchAlgorithm::Hll, SketchParams::Hll { precision })),
        prop_oneof![
            Just(f64::MIN_POSITIVE),
            Just(1.0 - f64::EPSILON),
            0.0..1.0f64
        ]
        .prop_filter("α in (0, 1)", |alpha| *alpha > 0.0)
        .prop_map(|alpha| (SketchAlgorithm::DDSketch, SketchParams::DDSketch { alpha })),
        (size(), 0..16u32, size(), 0..=32u8).prop_map(
            |(heap_size, sketch_rows, sketch_cols, layers)| (
                SketchAlgorithm::UnivMon,
                SketchParams::UnivMon {
                    heap_size,
                    sketch_rows,
                    sketch_cols,
                    layers
                }
            )
        ),
    ]
}

/// Every statistic, with an edge-valued quantile.
fn statistic() -> impl Strategy<Value = SketchStatistic> {
    prop_oneof![
        Just(SketchStatistic::FrequencyL2),
        Just(SketchStatistic::FrequencyEntropy),
        Just(SketchStatistic::Cardinality),
        edge_f64().prop_map(|q| SketchStatistic::Quantile { q }),
        prop_oneof![Just(None), Just(Some("1".to_string()))].prop_map(|value| {
            SketchStatistic::PointCount {
                key: ColumnRef::SampleValue,
                value,
            }
        }),
        (0..1000usize).prop_map(|k| SketchStatistic::TopK { k }),
    ]
}

fn sign() -> impl Strategy<Value = Option<ValueSign>> {
    prop_oneof![
        Just(None),
        Just(Some(ValueSign::NonNegative)),
        Just(Some(ValueSign::NonPositive)),
        Just(Some(ValueSign::Unknown)),
    ]
}

fn distinct_bound() -> impl Strategy<Value = Option<u64>> {
    prop_oneof![
        Just(None),
        Just(Some(0)),
        Just(Some(u64::MAX)),
        (1..10_000_000u64).prop_map(Some)
    ]
}

/// The error θ guarantees for `statistic` at `failure_probability`, over
/// values of one sign and a source of 10,000 distinct items, or `None`.
fn error_at(
    algorithm: SketchAlgorithm,
    parameters: &SketchParams,
    statistic: &SketchStatistic,
    failure_probability: f64,
) -> Option<f64> {
    theoretical_guarantee(
        &FieldDataType::Sketch(algorithm),
        parameters,
        statistic,
        failure_probability,
        Some(ValueSign::NonNegative),
        Some(10_000),
    )
    .map(|guarantee| guarantee.spec.error.evaluate().unwrap_or(f64::INFINITY))
}

/// The statistic each algorithm's guarantee is for.
fn statistic_of(algorithm: SketchAlgorithm) -> SketchStatistic {
    match algorithm {
        SketchAlgorithm::Kll | SketchAlgorithm::DDSketch => SketchStatistic::Quantile { q: 0.5 },
        SketchAlgorithm::Kmv | SketchAlgorithm::Hll => SketchStatistic::Cardinality,
        SketchAlgorithm::UnivMon => SketchStatistic::FrequencyL2,
        _ => SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: Some("1".into()),
        },
    }
}

/// θ grown by `factor` (≥ 1) in every size, and α shrunk by it: a
/// structure at least as accurate. Count Sketch depths stay odd, which its
/// median needs.
fn grown(parameters: &SketchParams, factor: u32) -> SketchParams {
    let grow = |x: u32| x.saturating_mul(factor);
    let odd = |depth: u32| {
        let depth = grow(depth);
        if depth % 2 == 0 {
            depth.saturating_add(1)
        } else {
            depth
        }
    };
    match parameters.clone() {
        SketchParams::Kll { k } => SketchParams::Kll {
            k: grow(k).min(KLL_MAX_K),
        },
        SketchParams::Kmv { k } => SketchParams::Kmv { k: grow(k) },
        SketchParams::Cms { width, depth } => SketchParams::Cms {
            width: grow(width),
            depth: grow(depth),
        },
        SketchParams::CmsWithHeap {
            width,
            depth,
            heap_size,
        } => SketchParams::CmsWithHeap {
            width: grow(width),
            depth: grow(depth),
            heap_size: grow(heap_size),
        },
        SketchParams::CountSketch { width, depth } => SketchParams::CountSketch {
            width: grow(width),
            depth: odd(depth),
        },
        SketchParams::CountSketchWithHeap {
            width,
            depth,
            heap_size,
        } => SketchParams::CountSketchWithHeap {
            width: grow(width),
            depth: odd(depth),
            heap_size: grow(heap_size),
        },
        SketchParams::Hll { precision } => SketchParams::Hll {
            precision: precision
                .saturating_add((factor - 1).min(255) as u8)
                .min(18),
        },
        SketchParams::DDSketch { alpha } => SketchParams::DDSketch {
            alpha: alpha / f64::from(factor),
        },
        other => other,
    }
}

/// Valid parameters of the structures whose error shrinks as they grow,
/// at sizes the planner lists (Count Sketch depth odd).
fn valid_parameters() -> impl Strategy<Value = (SketchAlgorithm, SketchParams)> {
    prop_oneof![
        (2..KLL_MAX_K).prop_map(|k| (SketchAlgorithm::Kll, SketchParams::Kll { k })),
        (2..100_000u32).prop_map(|k| (SketchAlgorithm::Kmv, SketchParams::Kmv { k })),
        (1..100_000u32, 1..32u32)
            .prop_map(|(width, depth)| (SketchAlgorithm::Cms, SketchParams::Cms { width, depth })),
        (1..100_000u32, 0..16u32).prop_map(|(width, half)| (
            SketchAlgorithm::CountSketch,
            SketchParams::CountSketch {
                width,
                depth: 2 * half + 1
            }
        )),
        (4..=18u8).prop_map(|precision| (SketchAlgorithm::Hll, SketchParams::Hll { precision })),
        (1e-6..0.5f64)
            .prop_map(|alpha| (SketchAlgorithm::DDSketch, SketchParams::DDSketch { alpha })),
    ]
}

proptest! {
    /// G1: a guarantee θ gives at δ fails at most δ rarely, and its error
    /// is a bound: not NaN and not negative (+Inf, no bound, is allowed).
    #[test]
    fn a_guarantee_is_well_formed(
        (algorithm, parameters) in parameters(),
        statistic in statistic(),
        failure_probability in edge_f64(),
        sign in sign(),
        bound in distinct_bound(),
    ) {
        let Some(guarantee) = theoretical_guarantee(
            &FieldDataType::Sketch(algorithm),
            &parameters,
            &statistic,
            failure_probability,
            sign,
            bound,
        ) else {
            return Ok(());
        };
        let own = guarantee.spec.failure_probability.evaluate();
        prop_assert!(
            own.is_some_and(|own| within_limit(own, failure_probability)),
            "fails with {own:?} at δ = {failure_probability}"
        );
        let asap_types::accuracy::BoundExpr::Constant { value: error } = guarantee.spec.error;
        prop_assert!(error >= 0.0, "error {error}");
    }

    /// G2: allowing θ to fail more often never loses its guarantee nor
    /// raises its error.
    #[test]
    fn a_guarantee_is_monotone_in_the_failure_probability(
        (algorithm, parameters) in parameters(),
        a in probability(),
        b in probability(),
    ) {
        let (tight, loose) = (a.min(b), a.max(b));
        let statistic = statistic_of(algorithm);
        if let Some(at_tight) = error_at(algorithm, &parameters, &statistic, tight) {
            let at_loose = error_at(algorithm, &parameters, &statistic, loose);
            prop_assert!(
                at_loose.is_some_and(|at_loose| at_loose <= at_tight),
                "{at_loose:?} at δ = {loose} vs {at_tight} at δ = {tight}"
            );
        }
    }

    /// G3: a larger structure (or a smaller α) never loses a guarantee nor
    /// raises its error.
    #[test]
    fn a_guarantee_is_monotone_in_the_structure_size(
        (algorithm, parameters) in valid_parameters(),
        factor in 1..8u32,
        failure_probability in probability(),
    ) {
        let statistic = statistic_of(algorithm);
        let larger = grown(&parameters, factor);
        if let Some(small) = error_at(algorithm, &parameters, &statistic, failure_probability) {
            let large = error_at(algorithm, &larger, &statistic, failure_probability);
            prop_assert!(
                large.is_some_and(|large| large <= small),
                "{larger:?}: {large:?} vs {parameters:?}: {small}"
            );
        }
    }

    /// G4: no guarantee holds at a NaN or negative failure probability.
    /// (A quantile outside [0, 1] gets no sketch readout: Pass 1 excludes
    /// it, `computation_targets`.)
    #[test]
    fn no_guarantee_holds_at_an_invalid_failure_probability(
        (algorithm, parameters) in parameters(),
        statistic in statistic(),
        failure_probability in invalid_probability(),
        sign in sign(),
        bound in distinct_bound(),
    ) {
        let guarantee = theoretical_guarantee(
            &FieldDataType::Sketch(algorithm),
            &parameters,
            &statistic,
            failure_probability,
            sign,
            bound,
        );
        prop_assert!(guarantee.is_none(), "{guarantee:?}");
    }

    /// G5: a DDSketch α outside (0, 1) is no relative error a sketch can
    /// be built with, so it has no guarantee. α = 0 would otherwise claim
    /// an exact quantile, and a negative α a negative error.
    #[test]
    fn an_invalid_alpha_has_no_guarantee(
        alpha in prop_oneof![
            Just(-0.0),
            Just(0.0),
            Just(1.0),
            Just(f64::NAN),
            Just(f64::INFINITY),
            Just(f64::NEG_INFINITY),
            Just(-f64::MIN_POSITIVE),
            -2.0..-f64::MIN_POSITIVE,
            1.0..2.0f64,
        ],
        failure_probability in edge_f64(),
        sign in sign(),
    ) {
        let guarantee = theoretical_guarantee(
            &FieldDataType::Sketch(SketchAlgorithm::DDSketch),
            &SketchParams::DDSketch { alpha },
            &SketchStatistic::Quantile { q: 0.5 },
            failure_probability,
            sign,
            None,
        );
        prop_assert!(guarantee.is_none(), "{guarantee:?}");
    }

    /// L1: a limit admits itself: the tolerated limit is never tighter
    /// than the limit. NaN meets nothing. Limits are errors and failure
    /// probabilities, so not negative (#1495); −0.0 is included.
    #[test]
    fn a_limit_is_never_tightened(limit in edge_f64()) {
        prop_assert!(!within_limit(f64::NAN, limit));
        if limit >= 0.0 {
            prop_assert!(tolerated_limit(limit) >= limit, "{}", tolerated_limit(limit));
            prop_assert!(within_limit(limit, limit));
        }
    }

    /// L2: what meets a limit meets every larger limit, and every smaller
    /// bound meets it.
    #[test]
    fn within_limit_is_monotone(
        a in ordered_edge_f64(),
        b in ordered_edge_f64(),
        limit in edge_f64(),
    ) {
        let (small, large) = if a <= b { (a, b) } else { (b, a) };
        if within_limit(large, limit) {
            prop_assert!(within_limit(small, limit));
        }
        if within_limit(limit, small) {
            prop_assert!(within_limit(limit, large));
        }
    }

    /// L3: a build that meets a readout level meets every looser one (a
    /// larger error, a larger failure probability) and a larger build meets
    /// it too.
    #[test]
    fn meeting_a_readout_level_is_monotone(
        (algorithm, parameters) in valid_parameters()
            .prop_filter("HLL needs a declared distinct bound", |(algorithm, _)| {
                *algorithm != SketchAlgorithm::Hll
            }),
        max_error in 1e-6..1.0f64,
        failure_probability in probability(),
        looser in 1.0..10.0f64,
        factor in 1..8u32,
    ) {
        let (memo, summary_input) = state_memo(algorithm);
        let statistic = statistic_of(algorithm);
        let metric = theoretical_guarantee(
            &FieldDataType::Sketch(algorithm),
            &parameters,
            &statistic,
            1.0,
            Some(ValueSign::NonNegative),
            None,
        )
        .map(|guarantee| guarantee.spec.accuracy_metric);
        let Some(metric) = metric else {
            return Ok(());
        };
        let meets = |parameters: &SketchParams, max_error: f64, failure_probability: f64| {
            build_meets_level(
                &memo,
                summary_input,
                &FieldDataType::Sketch(algorithm),
                parameters,
                &statistic,
                RequiredAccuracy {
                    metric,
                    max_error,
                    failure_probability,
                },
                &NoMeasuredGuarantees,
            )
        };
        if meets(&parameters, max_error, failure_probability) {
            prop_assert!(meets(&parameters, max_error * looser, failure_probability));
            prop_assert!(meets(&parameters, max_error, (failure_probability * looser).min(1.0)));
            prop_assert!(meets(&grown(&parameters, factor), max_error, failure_probability));
        }
    }

    /// E3: each arithmetic contract bounds the true error. Relative errors
    /// x and y on X and Y (y within the right operand's cap b) give X·Y off
    /// by |(1+x)(1+y) − 1| and X/Y by |(1+x)/(1+y) − 1|; absolute errors
    /// give X ± Y off by at most |x| + |y|. Each is at most Σ β·|error|.
    #[test]
    fn arithmetic_contracts_bound_the_true_error(
        x in -1.0..1.0f64,
        y_share in -1.0..=1.0f64,
        kind in prop_oneof![
            Just(ArithmeticOpKind::Add),
            Just(ArithmeticOpKind::Sub),
            Just(ArithmeticOpKind::Mul),
            Just(ArithmeticOpKind::Div),
        ],
    ) {
        let (memo, operand) = state_memo(SketchAlgorithm::Kll);
        let consumer = arithmetic(kind.clone(), operand, operand);
        for metric in AccuracyMetric::ALL {
            let Some(contract) = error_contract(&memo, &consumer, metric) else {
                continue;
            };
            let cap = contract.linear_bound_holds_up_to_per_input[1].unwrap_or(1.0);
            let y = y_share * cap;
            let bound = contract.error_factor_per_input[0] * x.abs()
                + contract.error_factor_per_input[1] * y.abs();
            let actual = match (&kind, metric) {
                (ArithmeticOpKind::Mul, AccuracyMetric::Relative) => ((1.0 + x) * (1.0 + y) - 1.0).abs(),
                (ArithmeticOpKind::Div, AccuracyMetric::Relative) => ((1.0 + x) / (1.0 + y) - 1.0).abs(),
                (ArithmeticOpKind::Add, AccuracyMetric::Absolute { .. }) => (x + y).abs(),
                (ArithmeticOpKind::Sub, AccuracyMetric::Absolute { .. }) => (x - y).abs(),
                other => return Err(TestCaseError::fail(format!("unexpected contract {other:?}"))),
            };
            prop_assert!(
                actual <= bound * (1.0 + 1e-12),
                "{kind:?} {metric:?}: {actual} > {bound} at x = {x}, y = {y}"
            );
        }
    }
}

/// A memo of a scan of non-negative `latency` and an `algorithm` state
/// of it: the state's group.
fn state_memo(algorithm: SketchAlgorithm) -> (Memo, MemoGroupId) {
    let (memo, _, summary_input) = scan_and_state_memo(algorithm);
    (memo, summary_input)
}

/// [`state_memo`] with the scan's group too.
fn scan_and_state_memo(algorithm: SketchAlgorithm) -> (Memo, MemoGroupId, MemoGroupId) {
    let data = DataWorkload {
        sources: BTreeMap::from([(
            "latency".to_string(),
            SourceFacts {
                column_signs: BTreeMap::from([("value".to_string(), ValueSign::NonNegative)]),
                ..SourceFacts::default()
            },
        )]),
        ..DataWorkload::default()
    };
    let mut memo = Memo::new(&data);
    let source = memo
        .find_or_add_group(scan("latency", &[("job", DataType::Utf8)], vec![]))
        .unwrap();
    let summary_input = memo
        .find_or_add_group(state(FieldDataType::Sketch(algorithm), source))
        .unwrap();
    (memo, source, summary_input)
}

/// The binary operators with an error contract in some metric.
fn binary_operators(
    lhs: MemoGroupId,
    rhs: MemoGroupId,
) -> Vec<asap_types::ir::Operator<MemoGroupId>> {
    let mut operators: Vec<_> = [
        ArithmeticOpKind::Add,
        ArithmeticOpKind::Sub,
        ArithmeticOpKind::Mul,
        ArithmeticOpKind::Div,
    ]
    .into_iter()
    .map(|kind| arithmetic(kind, lhs, rhs))
    .collect();
    operators.extend(
        [
            PromQLVectorSetOpKind::And,
            PromQLVectorSetOpKind::Unless,
            PromQLVectorSetOpKind::Or,
        ]
        .into_iter()
        .map(|kind| set_operator(kind, lhs, rhs)),
    );
    operators
}

/// E1: every contract has one β and one b per input; an input's error
/// either does not reach the output (β = 0) or reaches it undiminished
/// (β ≥ 1, finite); each b is in (0, 1). Checked over every binary
/// operator with a contract and every metric (an exhaustive list, not a
/// sample).
#[test]
fn contracts_never_shrink_an_input_error() {
    let (memo, operand) = state_memo(SketchAlgorithm::Kll);
    for consumer in binary_operators(operand, operand) {
        for metric in AccuracyMetric::ALL {
            let Some(ErrorContract {
                error_factor_per_input,
                linear_bound_holds_up_to_per_input,
            }) = error_contract(&memo, &consumer, metric)
            else {
                continue;
            };
            assert_eq!(error_factor_per_input.len(), 2, "{consumer:?}");
            assert_eq!(linear_bound_holds_up_to_per_input.len(), 2, "{consumer:?}");
            for beta in error_factor_per_input {
                assert!(
                    beta == 0.0 || (beta >= 1.0 && beta.is_finite()),
                    "{consumer:?} {metric:?}: β = {beta}"
                );
            }
            for b in linear_bound_holds_up_to_per_input.into_iter().flatten() {
                assert!(b > 0.0 && b < 1.0, "{consumer:?} {metric:?}: b = {b}");
            }
        }
    }
}

/// E2: `+`, `-` and `or` treat their operands alike: the contract is the
/// same with them swapped. `and` and `unless` read only the left values:
/// (1, 0) in every metric.
#[test]
fn symmetric_operators_have_order_independent_contracts() {
    let (mut memo, source, left) = scan_and_state_memo(SketchAlgorithm::Kll);
    let right = memo
        .find_or_add_group(state(
            FieldDataType::Sketch(SketchAlgorithm::DDSketch),
            source,
        ))
        .unwrap();
    for metric in AccuracyMetric::ALL {
        for (forward, swapped) in [
            (
                arithmetic(ArithmeticOpKind::Add, left, right),
                arithmetic(ArithmeticOpKind::Add, right, left),
            ),
            (
                arithmetic(ArithmeticOpKind::Sub, left, right),
                arithmetic(ArithmeticOpKind::Sub, right, left),
            ),
            (
                set_operator(PromQLVectorSetOpKind::Or, left, right),
                set_operator(PromQLVectorSetOpKind::Or, right, left),
            ),
        ] {
            let forward_contract = error_contract(&memo, &forward, metric);
            let swapped_contract = error_contract(&memo, &swapped, metric);
            assert_eq!(forward_contract, swapped_contract, "{forward:?} {metric:?}");
            if let Some(contract) = forward_contract {
                assert_eq!(
                    contract.error_factor_per_input[0], contract.error_factor_per_input[1],
                    "{forward:?} {metric:?}"
                );
            }
        }
        for kind in [PromQLVectorSetOpKind::And, PromQLVectorSetOpKind::Unless] {
            assert_eq!(
                error_contract(&memo, &set_operator(kind.clone(), left, right), metric)
                    .map(|contract| contract.error_factor_per_input),
                Some(vec![1.0, 0.0]),
                "{kind:?} {metric:?}"
            );
        }
    }
}
