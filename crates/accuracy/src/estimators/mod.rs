//! Dispatch committed estimator parameters to their accuracy models.
use super::*;
#[cfg(test)]
use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::FamilyCapability;

pub(crate) mod cardinality;
pub(crate) mod cms;
pub(crate) mod count_sketch;
pub(crate) mod ddsketch;
pub mod hll;
pub(crate) mod kll;
pub mod univmon;

pub(super) fn sketch_guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
) -> Option<AccuracyGuarantee> {
    match params {
        SketchParams::Kll { .. } => kll::guarantee(algorithm, params, query),
        SketchParams::DDSketch { .. } => ddsketch::guarantee(algorithm, params, query),
        SketchParams::Hll { .. } => hll::generic_guarantee(algorithm, params, query),
        SketchParams::Cms { .. } | SketchParams::CmsWithHeap { .. } => {
            cms::guarantee(algorithm, params, query)
        }
        SketchParams::CountSketch { .. } | SketchParams::CountSketchWithHeap { .. } => {
            count_sketch::guarantee(algorithm, params, query)
        }
        SketchParams::Kmv { .. } => cardinality::guarantee(algorithm, params, query),
        SketchParams::UnivMon { .. } => univmon::guarantee(algorithm, params, query),
        SketchParams::Hydra(hydra) => {
            sketch_guarantee(algorithm, &hydra.per_subpopulation(), query)
        }
    }
}

fn bounded_guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
    metric: AccuracyMetric,
    bound: f64,
    delta: ProbabilityExpr,
    contract: &str,
) -> AccuracyGuarantee {
    AccuracyGuarantee::new(
        metric,
        BoundExpr::Constant { value: bound },
        delta,
        vec![GuaranteeSource::SketchEvaluation {
            algorithm: format!("{algorithm:?}"),
            contract: contract.into(),
            params: serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
            query: format!("{query:?}"),
        }],
    )
}

/// The built-in guarantee of reading `statistic` out of a summary of family
/// `family` with parameters `parameters` ($\boldsymbol\theta$, a candidate's
/// `LogicalProperties::parameters`) built over an **exact** input, derived
/// by inverting the same sizing formulas [`size_params`] uses. Exact for a
/// plain value or an exact aggregate. `None` when a sketch does not answer
/// `statistic` (its family-capability table row), when a sketch has no
/// parameters, and when the family has no error model (`SummarySample`,
/// `Wavelet`, `StatModel`, and sketch evaluations without a certified
/// bound).
pub fn local_guarantee(
    family: &FieldDataType,
    parameters: Option<&SketchParams>,
    statistic: &SketchStatistic,
) -> Option<AccuracyGuarantee> {
    match family {
        FieldDataType::Plain(_) => Some(AccuracyGuarantee::exact("Plain value")),
        FieldDataType::ExactAggregate(kind, _) => Some(AccuracyGuarantee::exact(format!(
            "ExactAggregate({kind:?})"
        ))),
        FieldDataType::Sketch(algorithm, _) if FamilyCapability::of(family).answers(statistic) => {
            sketch_guarantee(algorithm, parameters?, statistic)
        }
        FieldDataType::Sketch(..) => None,
        // No error model is registered for these families.
        FieldDataType::SummarySample(..)
        | FieldDataType::Wavelet(..)
        | FieldDataType::StatModel(..) => None,
    }
}
/// The guarantee of a distinct count read from a summary with parameters
/// `parameters` built over the own rows of one source `source` (behind
/// filters and time windows at most), whose distinct items per evaluation
/// are at most `max_distinct`
/// (`SourceFacts::max_distinct_per_evaluation`): classic HLL's
/// ([`hll::classic_guarantee`]) or a distinct-count UnivMon's
/// ([`univmon::distinct_guarantee`]), with its failure probability bound
/// at `max_distinct` (1, no certificate, where the contract does not cover
/// it). `None` for other parameters and for a shape the contracts do not
/// cover; the generic guarantee of [`local_guarantee`] then applies, whose
/// failure probability is unknown.
pub fn distinct_count_guarantee_over_source(
    parameters: &SketchParams,
    source: &str,
    max_distinct: u64,
) -> Option<AccuracyGuarantee> {
    let guarantee = match parameters {
        SketchParams::Hll {
            precision,
            relative_error,
        } => hll::classic_guarantee(*precision, *relative_error, source)?,
        SketchParams::UnivMon { .. } => univmon::distinct_guarantee(parameters, source)?,
        _ => return None,
    };
    Some(guarantee.bind(&|statistic| {
        if let Some((precision, relative_error, _)) =
            hll::parse_classic_failure_statistic(statistic)
        {
            return Some(hll::classic_failure_probability(
                precision,
                relative_error,
                max_distinct,
            ));
        }
        let (heap_size, layers, _) = univmon::parse_incomplete_top_layer_statistic(statistic)?;
        Some(univmon::incomplete_top_layer_probability(
            heap_size,
            layers,
            max_distinct,
        ))
    }))
}

/// `params` with the parameters its per-value failure probability depends
/// on enlarged, never reduced, so that each value it answers fails with
/// probability at most `delta` and its error bound is unchanged or smaller
/// (#669b): Count-Min's depth, Count Sketch's depth (the binomial tail of
/// its median), KLL's failure probability (its δ scale), a distinct count
/// UnivMon's heap, and KMV's `k` at the relative error it was sized for
/// (#694). Other parameters are returned as they are: DDSketch never fails;
/// classic HLL's failure probability comes from its source's distinct bound
/// ([`hll::min_precision`]); a KMV not
/// sized for a relative error and an L2 or entropy UnivMon are not
/// enlarged.
pub fn with_failure_probability(params: &SketchParams, delta: f64) -> SketchParams {
    if !(delta > 0.0 && delta < 1.0) {
        return params.clone();
    }
    match params {
        SketchParams::Cms { width, depth } => SketchParams::Cms {
            width: *width,
            depth: (*depth).max(cms::cms_depth(delta)),
        },
        SketchParams::CmsWithHeap {
            width,
            depth,
            heap_size,
        } => SketchParams::CmsWithHeap {
            width: *width,
            depth: (*depth).max(cms::cms_depth(delta)),
            heap_size: *heap_size,
        },
        SketchParams::CountSketch { width, depth } => SketchParams::CountSketch {
            width: *width,
            depth: (*depth).max(count_sketch::count_sketch_depth(*width, delta)),
        },
        SketchParams::CountSketchWithHeap {
            width,
            depth,
            heap_size,
        } => SketchParams::CountSketchWithHeap {
            width: *width,
            depth: (*depth).max(count_sketch::count_sketch_depth(*width, delta)),
            heap_size: *heap_size,
        },
        SketchParams::Kll { .. } => kll::with_failure_probability(params, delta),
        SketchParams::Kmv {
            k,
            relative_error: Some(eps),
        } => SketchParams::Kmv {
            k: (*k).max(cardinality::kmv_k(*eps, delta)),
            relative_error: Some(*eps),
        },
        SketchParams::UnivMon { .. } => univmon::with_failure_probability(params, delta),
        _ => params.clone(),
    }
}

/// Today's sizing: the parameters of family `kind` for error `eps` at failure
/// probability `delta`. Test-only: the predefined lists (`parameter_list`)
/// are this sizing over an error grid, which a test keeps equal.
#[cfg(test)]
pub(crate) fn size_params(
    kind: SketchAlgorithm,
    intent: &AggIntent,
    eps: f64,
    delta: f64,
) -> SketchParams {
    match kind {
        SketchAlgorithm::UnivMon => univmon::size_params(intent, eps, delta),
        SketchAlgorithm::Kll => kll::size_params(eps, delta),
        SketchAlgorithm::Cms => SketchParams::Cms {
            width: cms::cms_width(eps),
            depth: cms::cms_depth(delta),
        },
        SketchAlgorithm::Hll => SketchParams::Hll {
            precision: hll::hll_precision(eps, delta),
            relative_error: Some(eps),
        },
        SketchAlgorithm::CmsWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CmsWithHeap is only a TopK candidate"),
            };
            SketchParams::CmsWithHeap {
                width: cms::cms_width(eps),
                depth: cms::cms_depth(delta),
                heap_size: topk_capacity(k, eps),
            }
        }
        // Non-preferred candidates (DDSketch / Kmv / CountSketch /
        // CountSketchWithHeap) are only reachable once a cost model picks
        // them; sized here so that wiring is local.
        SketchAlgorithm::DDSketch => ddsketch::size_params(eps),
        SketchAlgorithm::Kmv => SketchParams::Kmv {
            k: cardinality::kmv_k(eps, delta),
            relative_error: Some(eps),
        },
        SketchAlgorithm::CountSketch => {
            let width = count_sketch::count_sketch_width(eps);
            SketchParams::CountSketch {
                width,
                depth: count_sketch::count_sketch_depth(width, delta),
            }
        }
        SketchAlgorithm::CountSketchWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CountSketchWithHeap is only a TopK candidate"),
            };
            let width = count_sketch::count_sketch_width(eps);
            SketchParams::CountSketchWithHeap {
                width,
                depth: count_sketch::count_sketch_depth(width, delta),
                heap_size: topk_capacity(k, eps),
            }
        }
        // Listed, not `_`: a family `asap_sketchlib` adds fails to compile
        // here until the planner sizes it or lists it as not offered.
        SketchAlgorithm::CountSketchWithL2Norm
        | SketchAlgorithm::CmsWithSpaceSaving
        | SketchAlgorithm::CountSketchWithSpaceSaving
        | SketchAlgorithm::CmsFold
        | SketchAlgorithm::CountSketchFold
        | SketchAlgorithm::HllHip
        | SketchAlgorithm::KllDynamic
        | SketchAlgorithm::SpaceSaving
        | SketchAlgorithm::Elastic
        | SketchAlgorithm::Coco
        | SketchAlgorithm::Bloom
        | SketchAlgorithm::SetAggregator
        | SketchAlgorithm::UnivMonTwoTier
        | SketchAlgorithm::UnivMonQ
        | SketchAlgorithm::HydraCms
        | SketchAlgorithm::HydraCountSketch
        | SketchAlgorithm::HydraKll
        | SketchAlgorithm::HydraHll
        | SketchAlgorithm::HydraUnivMon => {
            unreachable!("Pass 1 offers only the families in FAMILY_PREFERENCE, not {kind:?}")
        }
    }
}

/// `P(Binomial(n, p) ≥ k)`, summed term by term in log space and rounded
/// up by a relative 1e-12 so rounding never certifies a smaller tail.
pub(crate) fn binomial_upper_tail(n: u32, p: f64, k: u32) -> f64 {
    if k == 0 || p >= 1.0 {
        return 1.0;
    }
    if k > n || p <= 0.0 {
        return 0.0;
    }
    let (ln_p, ln_q) = (p.ln(), (-p).ln_1p());
    let mut ln_choose = 0.0;
    let mut tail = 0.0;
    for i in 0..=n {
        if i > 0 {
            ln_choose += f64::from(n - i + 1).ln() - f64::from(i).ln();
        }
        if i >= k {
            tail += (ln_choose + f64::from(i) * ln_p + f64::from(n - i) * ln_q).exp();
        }
    }
    (tail * (1.0 + 1e-12)).min(1.0)
}

/// Accuracy-dependent candidate budget, not a completeness theorem. The
/// membership model must still certify the selected set independently.
#[cfg(test)]
pub(crate) fn topk_capacity(k: usize, eps: f64) -> u32 {
    u32::try_from(k)
        .unwrap_or(u32::MAX)
        .max(saturating_ceil(1.0 / eps, 1, 1 << 26))
}

/// `⌈x⌉` clamped to `[lo, hi]`; NaN / non-positive x saturate to `hi`
/// (a degenerate ε means "as accurate as this family goes").
pub(crate) fn saturating_ceil(x: f64, lo: u32, hi: u32) -> u32 {
    if !x.is_finite() || x <= 0.0 {
        return hi;
    }
    (x.ceil() as u32).clamp(lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::operator::agg_intent::{default_cardinality, default_quantile};

    /// Each family's predefined list is today's sizing at failure
    /// probability 0.01 over the error grid, weakest first, without repeats
    /// (a UnivMon's: its L2-norm, distinct-count and other shapes). KMV's and
    /// UnivMon's distinct counts stop at ε = 0.01. A heap variant's width × depth is the top-1
    /// sizing, each with every heap size of the 1-2-5 series 10 … 10 000.
    #[test]
    fn parameter_list_is_the_sizing_over_the_error_grid() {
        use asap_types::ir::properties::AccuracyTarget;
        use asap_types::stages::parameter_list;
        let full_grid = [0.1, 0.05, 0.02, 0.01, 0.005, 0.002, 0.001];
        let distinct_grid = [0.1, 0.05, 0.02, 0.01];
        let heap_sizes = [10, 20, 50, 100, 200, 500, 1_000, 2_000, 5_000, 10_000];
        let l2 = AggIntent::FrequencyL2 {
            col: None,
            accuracy: AccuracyTarget::max_error(0.01),
        };
        let top_1 = AggIntent::TopK {
            k: 1,
            accuracy: AccuracyTarget::max_error(0.01),
        };
        let quantile = default_quantile(0.5);
        let distinct = default_cardinality();
        for (family, intents) in [
            (SketchAlgorithm::Kll, vec![&quantile]),
            (SketchAlgorithm::DDSketch, vec![&quantile]),
            (SketchAlgorithm::Hll, vec![&distinct]),
            (SketchAlgorithm::Kmv, vec![&distinct]),
            (SketchAlgorithm::Cms, vec![&distinct]),
            (SketchAlgorithm::CountSketch, vec![&distinct]),
            (SketchAlgorithm::CmsWithHeap, vec![&top_1]),
            (SketchAlgorithm::CountSketchWithHeap, vec![&top_1]),
            (SketchAlgorithm::UnivMon, vec![&l2, &distinct, &quantile]),
        ] {
            let mut sized: Vec<SketchParams> = Vec::new();
            for intent in intents {
                let grid: &[f64] = match (family, intent) {
                    (
                        SketchAlgorithm::Kmv | SketchAlgorithm::UnivMon,
                        AggIntent::Cardinality { .. },
                    ) => &distinct_grid,
                    _ => &full_grid,
                };
                for &eps in grid {
                    let params = size_params(family, intent, eps, sizing::DEFAULT_DELTA);
                    let with_heaps: Vec<SketchParams> = match params {
                        SketchParams::CmsWithHeap { width, depth, .. } => heap_sizes
                            .iter()
                            .map(|&heap_size| SketchParams::CmsWithHeap {
                                width,
                                depth,
                                heap_size,
                            })
                            .collect(),
                        SketchParams::CountSketchWithHeap { width, depth, .. } => heap_sizes
                            .iter()
                            .map(|&heap_size| SketchParams::CountSketchWithHeap {
                                width,
                                depth,
                                heap_size,
                            })
                            .collect(),
                        other => vec![other],
                    };
                    for params in with_heaps {
                        if !sized.contains(&params) {
                            sized.push(params);
                        }
                    }
                }
            }
            assert_eq!(
                format!("{:?}", parameter_list(family)),
                format!("{sized:?}"),
                "{family:?}"
            );
        }
    }

    /// Within each family's list, a configuration that satisfies another's
    /// floor (`producer_satisfies_logical_requirements`) has no larger error, at the lists' failure
    /// probability 0.01, for every statistic the family answers.
    #[test]
    fn a_configuration_satisfying_a_listed_floor_is_no_less_accurate() {
        use asap_types::ir::operator::operator_properties::Reduction;
        use asap_types::ir::schema::{GroupingStrategy, SummaryUpdate};
        use asap_types::ir::ASAPOp;
        use asap_types::ir::Operator;
        use asap_types::stages::{
            parameter_list, producer_satisfies_logical_requirements, CandidateExpression,
            LogicalProperties, LogicalRequirements, MemoGroupId,
        };
        let point = SketchStatistic::PointCount {
            key: asap_types::ir::scalar::expr_ir::ColumnRef::SampleValue,
            value: None,
        };
        let statistics = [
            SketchStatistic::Quantile { q: 0.5 },
            SketchStatistic::Cardinality,
            SketchStatistic::FrequencyL2,
            SketchStatistic::FrequencyEntropy,
            SketchStatistic::TopK { k: 1 },
            point,
        ];
        for family in [
            SketchAlgorithm::Kll,
            SketchAlgorithm::DDSketch,
            SketchAlgorithm::Hll,
            SketchAlgorithm::Kmv,
            SketchAlgorithm::Cms,
            SketchAlgorithm::CountSketch,
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap,
            SketchAlgorithm::UnivMon,
        ] {
            let build = CandidateExpression {
                operator: Operator::ASAP(ASAPOp::SummaryAgg {
                    child: MemoGroupId(0),
                    family: FieldDataType::Sketch(family, GroupingStrategy::default()),
                    input: SummaryUpdate::column(
                        asap_types::ir::scalar::expr_ir::ColumnRef::SampleValue,
                    ),
                    reduction: Reduction::PerEntity,
                    grouping: GroupingStrategy::default(),
                    filter: None,
                }),
            };
            let capability =
                FamilyCapability::of(&FieldDataType::Sketch(family, GroupingStrategy::default()));
            let list = parameter_list(family);
            for producer in list {
                let properties =
                    LogicalProperties::new(&build, Some(producer.clone()), None).unwrap();
                for floor in list {
                    let requirement = LogicalRequirements::new(Some(floor.clone()));
                    if !producer_satisfies_logical_requirements(&properties, &requirement) {
                        continue;
                    }
                    for statistic in statistics.iter().filter(|s| capability.answers(s)) {
                        let Some(needed) = sketch_guarantee(&family, floor, statistic) else {
                            continue;
                        };
                        let delivered = sketch_guarantee(&family, producer, statistic)
                            .unwrap_or_else(|| panic!("{producer:?} {statistic:?}"));
                        // A bound that does not evaluate (an unknown term) is
                        // compared by its expression.
                        let error_no_larger = match (
                            delivered.spec.error.evaluate(),
                            needed.spec.error.evaluate(),
                        ) {
                            (Some(error), Some(needed)) => error <= needed * (1.0 + 1e-9),
                            (_, _) => delivered.spec.error == needed.spec.error,
                        };
                        let failure_no_likelier = match (
                            delivered.spec.failure_probability.evaluate(),
                            needed.spec.failure_probability.evaluate(),
                        ) {
                            (Some(delta), Some(needed)) => {
                                delta <= needed.max(sizing::DEFAULT_DELTA) * (1.0 + 1e-9)
                            }
                            (_, _) => {
                                delivered.spec.failure_probability
                                    == needed.spec.failure_probability
                            }
                        };
                        assert!(
                            error_no_larger && failure_no_likelier,
                            "{producer:?} ⪰ {floor:?} for {statistic:?}: {delivered:?} vs {needed:?}"
                        );
                    }
                }
            }
        }
    }

    fn delta_and_error(params: &SketchParams, query: &SketchStatistic) -> (f64, f64) {
        let algorithm = match params {
            SketchParams::Cms { .. } => SketchAlgorithm::Cms,
            SketchParams::CountSketch { .. } => SketchAlgorithm::CountSketch,
            SketchParams::Kll { .. } => SketchAlgorithm::Kll,
            other => panic!("{other:?}"),
        };
        let g = sketch_guarantee(&algorithm, params, query).unwrap();
        (
            g.spec.failure_probability.evaluate().unwrap(),
            g.spec.error.evaluate().unwrap(),
        )
    }

    /// Enlarging for a smaller failure probability deepens Count-Min and
    /// Count Sketch and grows KLL until each meets it, at no larger error;
    /// it never shrinks a sketch that already meets it. (Count Sketch's
    /// depth is capped at 255 rows, which a non-power-of-two width reaches
    /// before 1e-6.)
    #[test]
    fn enlarging_meets_the_failure_probability_at_no_larger_error() {
        let point = SketchStatistic::PointCount {
            key: asap_types::ir::scalar::expr_ir::ColumnRef::SampleValue,
            value: None,
        };
        let quantile = SketchStatistic::Quantile { q: 0.99 };
        for (params, query, delta) in [
            (
                size_params(SketchAlgorithm::Cms, &default_cardinality(), 0.01, 0.01),
                &point,
                1e-6,
            ),
            (
                size_params(
                    SketchAlgorithm::CountSketch,
                    &default_cardinality(),
                    0.01,
                    0.01,
                ),
                &point,
                1e-3,
            ),
            (
                size_params(SketchAlgorithm::Kll, &default_quantile(0.99), 0.01, 0.01),
                &quantile,
                1e-6,
            ),
        ] {
            let (_, error) = delta_and_error(&params, query);
            let enlarged = with_failure_probability(&params, delta);
            let (new_delta, new_error) = delta_and_error(&enlarged, query);
            assert!(new_delta <= delta, "{enlarged:?}");
            assert!(new_error <= error * (1.0 + 1e-9), "{enlarged:?}");
            assert_ne!(enlarged, params);
            assert_eq!(with_failure_probability(&enlarged, 0.01), enlarged);
        }
        let ddsketch = SketchParams::DDSketch { alpha: 0.01 };
        assert_eq!(with_failure_probability(&ddsketch, 1e-6), ddsketch);
    }

    /// A distinct-count UnivMon grows its heap for a smaller failure
    /// probability at no larger relative error.
    #[test]
    fn a_distinct_count_univmon_grows_its_heap() {
        let params = size_params(SketchAlgorithm::UnivMon, &default_cardinality(), 0.05, 0.01);
        let enlarged = with_failure_probability(&params, 1e-4);
        let error = |p: &SketchParams| {
            univmon::distinct_guarantee(p, "m")
                .unwrap()
                .spec
                .error
                .evaluate()
                .unwrap()
        };
        let (
            SketchParams::UnivMon { heap_size: k, .. },
            SketchParams::UnivMon {
                heap_size: grown,
                distinct_failure_probability,
                ..
            },
        ) = (&params, &enlarged)
        else {
            panic!("{enlarged:?}")
        };
        assert!(grown > k);
        assert_eq!(*distinct_failure_probability, Some(1e-4));
        assert!(error(&enlarged) <= error(&params) * (1.0 + 1e-9));
    }

    /// Classic HLL's precision grows until its failure probability over the
    /// source's distinct bound meets the target, up to 18.
    #[test]
    fn classic_hll_precision_grows_for_its_failure_probability() {
        let p = hll::min_precision(0.05, 0.01, 64, 4);
        assert!(p > 4);
        assert!(hll::classic_failure_probability(p, 0.05, 64) <= 0.01);
        assert!(hll::classic_failure_probability(p - 1, 0.05, 64) > 0.01);
        assert_eq!(hll::min_precision(0.05, 1e-30, 4096, 12), 12);
    }

    /// A sketch read for a statistic its table row does not answer (an HLL
    /// for a total) has no guarantee, nor one without θ; one it answers
    /// has the guarantee of its θ, and a plain value or exact aggregate
    /// stays exact.
    #[test]
    fn local_guarantee_needs_a_statistic_the_family_answers_and_its_parameters() {
        use asap_types::ir::scalar::expr_ir::ColumnRef;
        use asap_types::ir::schema::{DataType, ExactKind, ExactParams, GroupingStrategy};
        use asap_types::stages::parameter_list;
        let hll = FieldDataType::Sketch(SketchAlgorithm::Hll, GroupingStrategy::default());
        let total = SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        };
        let hll_parameters = &parameter_list(SketchAlgorithm::Hll)[0];
        assert!(local_guarantee(&hll, Some(hll_parameters), &total).is_none());
        let kll = FieldDataType::Sketch(SketchAlgorithm::Kll, GroupingStrategy::default());
        let median = SketchStatistic::Quantile { q: 0.5 };
        assert!(local_guarantee(&kll, None, &median).is_none());
        let errors: Vec<f64> = parameter_list(SketchAlgorithm::Kll)
            .iter()
            .map(|parameters| {
                let guarantee = local_guarantee(&kll, Some(parameters), &median).unwrap();
                assert_eq!(guarantee.spec.accuracy_metric, AccuracyMetric::Rank);
                guarantee.spec.error.evaluate().unwrap()
            })
            .collect();
        // The list runs weakest first: each listed k bounds the rank error
        // more tightly than the one before.
        assert!(
            errors.windows(2).all(|pair| pair[1] < pair[0]),
            "{errors:?}"
        );
        for exact in [
            FieldDataType::Plain(DataType::Float64),
            FieldDataType::ExactAggregate(ExactKind::Count, ExactParams::Count),
        ] {
            assert!(local_guarantee(&exact, None, &total).is_some(), "{exact:?}");
        }
    }

    /// A distinct count over a source's own rows gets classic HLL's or a
    /// distinct-count UnivMon's failure probability at the source's
    /// distinct bound: certified (below 1) for a small bound, 1 (no
    /// certificate) past the contract's range; other parameters get none.
    #[test]
    fn a_distinct_count_over_a_source_binds_its_failure_probability() {
        let hll = asap_types::stages::parameter_list(SketchAlgorithm::Hll)[0].clone();
        let univmon = asap_types::stages::parameter_list(SketchAlgorithm::UnivMon)
            .iter()
            .find(|p| {
                matches!(
                    p,
                    SketchParams::UnivMon {
                        distinct_failure_probability: Some(_),
                        ..
                    }
                )
            })
            .unwrap()
            .clone();
        for parameters in [&hll, &univmon] {
            let small = distinct_count_guarantee_over_source(parameters, "flows", 100).unwrap();
            assert_eq!(small.spec.accuracy_metric, AccuracyMetric::Relative);
            let delta = small.spec.failure_probability.evaluate().unwrap();
            assert!(delta < 1.0, "{parameters:?}: {delta}");
            let huge = distinct_count_guarantee_over_source(parameters, "flows", u64::MAX).unwrap();
            assert_eq!(huge.spec.failure_probability.evaluate(), Some(1.0));
        }
        let kll = asap_types::stages::parameter_list(SketchAlgorithm::Kll)[0].clone();
        assert!(distinct_count_guarantee_over_source(&kll, "flows", 1_000).is_none());
    }
}
