//! Dispatch committed estimator parameters to their accuracy models.
use super::*;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::{GroupingStrategy, HydraKind, HydraParams};

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
    query: &SketchStatistic,
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
        SketchParams::UnivMon { .. } => univmon::guarantee(params, query),
    }
}

fn bounded_guarantee(
    algorithm: &SketchAlgorithm,
    params: &SketchParams,
    query: &SketchStatistic,
    metric: ErrorMetric,
    bound: f64,
    delta: ProbabilityExpr,
    contract: &str,
) -> ResultGuarantee {
    ResultGuarantee {
        metric,
        bound: BoundExpr::Constant { value: bound },
        failure_probability: delta,
        provenance: vec![GuaranteeSource::SketchEvaluation {
            algorithm: format!("{algorithm:?}"),
            contract: contract.into(),
            params: serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
            query: format!("{query:?}"),
        }],
    }
}

pub(super) fn local_guarantee(
    family: &FieldDataType,
    query: &SketchStatistic,
) -> Option<ResultGuarantee> {
    match family {
        FieldDataType::Plain(_) => Some(ResultGuarantee::exact("Plain value")),
        FieldDataType::ExactAggregate(kind, _) => {
            Some(ResultGuarantee::exact(format!("ExactAggregate({kind:?})")))
        }
        FieldDataType::Sketch(kind, GroupingStrategy::PerSubpopulationInstance) => {
            sketch_guarantee(kind.algorithm(), kind.params(), query)
        }
        FieldDataType::Sketch(
            kind,
            GroupingStrategy::SharedMultiSubpopulation {
                kind: HydraKind::HydraCms,
                params:
                    HydraParams::HydraCms {
                        shared_rows,
                        shared_columns,
                        ..
                    },
            },
        ) => {
            // Groups that share a grid cell add their weight: at most
            // e·N/shared_columns per row, and the minimum over rows exceeds
            // it with probability e^-shared_rows. N is the whole input's
            // weight, not one group's, so this bounds error relative to it.
            let inner = sketch_guarantee(kind.algorithm(), kind.params(), query)?;
            let stats = crate::accuracy::PropagationStats {
                hydra_shared_grid_collision_bound: Some(
                    std::f64::consts::E / f64::from(*shared_columns),
                ),
                hydra_shared_grid_failure_probability: Some((-f64::from(*shared_rows)).exp()),
                ..Default::default()
            };
            Some(hydra_guarantee(&inner, &stats))
        }
        // No accuracy model for the other shared groupings.
        FieldDataType::Sketch(..) => None,
        // No error model is registered for these families.
        FieldDataType::Sample(..) | FieldDataType::Wavelet(..) | FieldDataType::StatModel(..) => {
            None
        }
    }
}
pub(crate) fn size_params(
    kind: SketchAlgorithm,
    intent: &AggIntent,
    eps: f64,
    delta: f64,
) -> SketchParams {
    match kind {
        // Sized for the L2 readout whatever the intent (see `univmon`).
        SketchAlgorithm::UnivMon => univmon::size_params(eps, delta),
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
                heap_size: topk_capacity(k, eps),
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
                heap_size: topk_capacity(k, eps),
            }
        }
    }
}
/// Accuracy-dependent candidate budget, not a completeness theorem. The
/// membership model must still certify the selected set independently.
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
/// Compose the inner per-subpopulation guarantee with Hydra's outer shared
/// grid. The paper's collision term depends on deployment/data statistics;
/// keeping those leaves symbolic makes the formula explicit while ensuring
/// target satisfaction fails closed until a caller supplies them.
pub(crate) fn hydra_guarantee(
    inner: &ResultGuarantee,
    stats: &PropagationStats,
) -> ResultGuarantee {
    let mut provenance = inner.provenance.clone();
    provenance.extend(stats.evidence_provenance.clone());
    provenance.push(GuaranteeSource::ChildGuarantee {
        input_index: 0,
        guarantee: Box::new(inner.clone()),
    });
    if stats.hydra_shared_grid_collision_bound.is_none() {
        provenance.push(GuaranteeSource::UnavailableStatistic {
            statistic: "hydra_shared_grid_collision_bound".into(),
        });
    }
    if stats.hydra_shared_grid_failure_probability.is_none() {
        provenance.push(GuaranteeSource::UnavailableStatistic {
            statistic: "hydra_shared_grid_failure_probability".into(),
        });
    }
    provenance.push(GuaranteeSource::CompositionStep {
        operator: CompositionOperator::ApproximateAggregate,
        rule: "hydra_shared_grid_union_bound".into(),
    });
    ResultGuarantee {
        metric: inner.metric,
        bound: BoundExpr::Sum {
            terms: vec![
                inner.bound.clone(),
                stats.hydra_shared_grid_collision_bound.map_or_else(
                    || BoundExpr::Unknown {
                        statistic: "hydra_shared_grid_collision_bound".into(),
                    },
                    |value| BoundExpr::Constant { value },
                ),
            ],
        },
        failure_probability: ProbabilityExpr::UnionBound {
            terms: vec![
                inner.failure_probability.clone(),
                stats.hydra_shared_grid_failure_probability.map_or_else(
                    || ProbabilityExpr::Unknown {
                        statistic: "hydra_shared_grid_failure_probability".into(),
                    },
                    |value| ProbabilityExpr::Constant { value },
                ),
            ],
        },
        provenance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hydra_composes_inner_and_shared_grid_error_symbolically() {
        let inner = ResultGuarantee {
            metric: ErrorMetric::Frequency,
            bound: BoundExpr::Constant { value: 0.01 },
            failure_probability: ProbabilityExpr::Constant { value: 0.02 },
            provenance: vec![],
        };
        let composed = hydra_guarantee(&inner, &PropagationStats::default());

        assert_eq!(composed.metric, ErrorMetric::Frequency);
        assert!(matches!(
            composed.bound,
            BoundExpr::Sum { ref terms }
                if matches!(terms.as_slice(), [
                    BoundExpr::Constant { value },
                    BoundExpr::Unknown { statistic },
                ] if *value == 0.01 && statistic == "hydra_shared_grid_collision_bound")
        ));
        assert!(matches!(
            composed.failure_probability,
            ProbabilityExpr::UnionBound { ref terms }
                if matches!(terms.as_slice(), [
                    ProbabilityExpr::Constant { value },
                    ProbabilityExpr::Unknown { statistic },
                ] if *value == 0.02 && statistic == "hydra_shared_grid_failure_probability")
        ));
    }
}
