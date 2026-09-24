//! Dispatch committed estimator parameters to their accuracy models.
use super::*;
use asap_types::post_asap::GroupingStrategy;
use asap_types::pre_asap::AggIntent;

pub mod cardinality;
pub mod cms;
pub mod count_sketch;
pub mod ddsketch;
pub mod hll;
pub mod kll;
pub mod univmon;

pub(super) fn sketch_guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchQuery,
) -> Option<ResultGuarantee> {
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
        SketchParams::Kmv { .. } | SketchParams::Theta { .. } => {
            cardinality::guarantee(algorithm, params, query)
        }
        SketchParams::UnivMon { .. } => univmon::guarantee(query),
    }
}

fn bounded_guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchQuery,
    metric: ErrorMetric,
    bound: f64,
    delta: ProbabilityExpr,
    contract: &str,
) -> ResultGuarantee {
    ResultGuarantee {
        metric,
        bound: BoundExpr::Constant { value: bound },
        failure_probability: delta,
        provenance: vec![GuaranteeSource::SketchReadout {
            algorithm: format!("{algorithm:?}"),
            contract: contract.into(),
            params: serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
            query: format!("{query:?}"),
        }],
    }
}

pub(super) fn local_guarantee(
    family: &SummaryFamilyType,
    query: &SketchQuery,
) -> Option<ResultGuarantee> {
    match family {
        SummaryFamilyType::Plain(_) => Some(ResultGuarantee::exact("Plain value")),
        SummaryFamilyType::ExactAggregate(kind, _) => {
            Some(ResultGuarantee::exact(format!("ExactAggregate({kind:?})")))
        }
        SummaryFamilyType::Sketch(kind, _) => {
            sketch_guarantee(kind.algorithm(), kind.params(), query)
        }
        // No error model is registered for these families.
        SummaryFamilyType::Sample(..)
        | SummaryFamilyType::Wavelet(..)
        | SummaryFamilyType::StatModel(..) => None,
    }
}
pub(crate) fn size_params(
    kind: SketchAlgorithm,
    intent: &AggIntent,
    eps: f64,
    delta: f64,
) -> SketchParams {
    match kind {
        // Baseline dimensions are candidates, not an inverted error bound.
        // Empirical models may size these; no theoretical guarantee is claimed.
        SketchAlgorithm::UnivMon => univmon::size_params(),
        SketchAlgorithm::Kll => SketchParams::Kll { k: kll::kll_k(eps) },
        SketchAlgorithm::Cms => SketchParams::Cms {
            width: cms::cms_width(eps),
            depth: cms::cms_depth(delta),
        },
        SketchAlgorithm::Hll => SketchParams::Hll {
            precision: hll::hll_precision(eps),
        },
        SketchAlgorithm::CmsWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CmsWithHeap is only a TopK candidate"),
            };
            SketchParams::CmsWithHeap {
                width: cms::cms_width(eps),
                depth: cms::cms_depth(delta),
                heap_size: k as u32,
            }
        }
        // Non-preferred candidates (DDSketch / Theta / Kmv / CountSketch /
        // CountSketchWithHeap) are only reachable once a cost model picks
        // them; sized here so that wiring is local.
        SketchAlgorithm::DDSketch => ddsketch::size_params(eps),
        SketchAlgorithm::Theta => SketchParams::Theta {
            k: cardinality::kmv_k_99(eps),
        },
        SketchAlgorithm::Kmv => SketchParams::Kmv {
            k: cardinality::kmv_k_99(eps),
        },
        SketchAlgorithm::CountSketch => SketchParams::CountSketch {
            width: count_sketch::count_sketch_width(eps),
            depth: count_sketch::count_sketch_depth(delta),
        },
        SketchAlgorithm::CountSketchWithHeap => {
            let k = match intent {
                AggIntent::TopK { k, .. } => *k,
                _ => unreachable!("CountSketchWithHeap is only a TopK candidate"),
            };
            SketchParams::CountSketchWithHeap {
                width: count_sketch::count_sketch_width(eps),
                depth: count_sketch::count_sketch_depth(delta),
                heap_size: k as u32,
            }
        }
    }
}
/// `⌈x⌉` clamped to `[lo, hi]`; NaN / non-positive x saturate to `hi`
/// (a degenerate ε means "as accurate as this family goes").
pub(crate) fn saturating_ceil(x: f64, lo: u32, hi: u32) -> u32 {
    if !x.is_finite() || x <= 0.0 {
        return hi;
    }
    (x.ceil() as u32).clamp(lo, hi)
}
pub(crate) struct EstimatorAccuracy<'a> {
    base: &'a dyn AccuracyModel,
    contract: Option<EstimatorContract>,
    epsilon: f64,
    delta: f64,
}

impl<'a> EstimatorAccuracy<'a> {
    pub(crate) fn new(
        base: &'a dyn AccuracyModel,
        contract: Option<EstimatorContract>,
        target: Option<&AccuracyTarget>,
    ) -> Self {
        let (epsilon, delta) = target
            .map(crate::replacement::accuracy_budget)
            .unwrap_or((0.0, 0.0));
        Self {
            base,
            contract,
            epsilon,
            delta,
        }
    }

    fn hll(&self) -> Option<hll::ClassicHllConfidence> {
        let EstimatorContract::ClassicHll {
            max_distinct_per_readout,
        } = self.contract?;
        hll::ClassicHllConfidence::new(max_distinct_per_readout, self.epsilon)
    }

    pub(crate) fn size_params(&self, algorithm: &SketchAlgorithm) -> Option<SketchParams> {
        if *algorithm != SketchAlgorithm::Hll || self.contract.is_none() {
            return None;
        }
        // Retain the strongest supported parameter for diagnostics if sizing
        // is infeasible. The normal guarantee check rejects it below.
        Some(SketchParams::Hll {
            precision: self
                .hll()
                .and_then(|model| model.precision(self.delta))
                .unwrap_or(18),
        })
    }
}

impl AccuracyModel for EstimatorAccuracy<'_> {
    fn exact_operation_rule(&self, operation: &ExactOperation) -> Option<CompositionOperator> {
        self.base.exact_operation_rule(operation)
    }
    fn local_guarantee(
        &self,
        family: &SummaryFamilyType,
        query: &SketchQuery,
    ) -> Option<ResultGuarantee> {
        if let (Some(_), SummaryFamilyType::Sketch(kind, grouping), SketchQuery::Cardinality) =
            (self.contract, family, query)
        {
            if let (SketchAlgorithm::Hll, SketchParams::Hll { precision }) =
                (kind.algorithm(), kind.params())
            {
                if *grouping != GroupingStrategy::PerSubpopulationInstance {
                    return None;
                }
                return self.hll()?.guarantee(*precision);
            }
        }
        self.base.local_guarantee(family, query)
    }
    fn propagate(
        &self,
        op: &CompositionOperator,
        inputs: &[ResultGuarantee],
        local: Option<&ResultGuarantee>,
        stats: &PropagationStats,
    ) -> Result<ResultGuarantee, AccuracyError> {
        self.base.propagate(op, inputs, local, stats)
    }
    fn satisfies(&self, guarantee: &ResultGuarantee, target: &AccuracyTarget) -> bool {
        self.base.satisfies(guarantee, target)
    }
}
