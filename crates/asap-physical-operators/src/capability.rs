//! Capability boundaries, checked without constructing accumulator state.
//!
//! `validate_summary_kernel` checks update kernels, including families without a
//! native batch representation. `validate_native_family` and
//! `validate_sketch_evaluation` / `validate_exact_evaluation` check native state and evaluation support.
//! Keyed weighted-frequency evaluations are checked by `Operator::keyed_evaluation`.
//! A successful kernel check alone does not mean a physical DAG will bind.
//!
//! Stored-state encodings belong to deployments. Full plan acceptance is
//! owned by `binding`, which also validates schemas, expressions and inputs.
use crate::Error;
use planner_types::post_asap::{
    ExactKind, ExactParams, FieldDataType as SummaryFamilyType, GroupingStrategy, SketchAlgorithm,
    SketchParams, SketchStatistic, SummaryUpdate,
};

/// Check the same contract used by `create_planner_accumulator` before a plan
/// is accepted. Execution timing is deliberately not a kernel property.
pub fn validate_summary_kernel(
    family: &SummaryFamilyType,
    input: &SummaryUpdate,
    grouping: &GroupingStrategy,
) -> Result<(), String> {
    if grouping != &GroupingStrategy::PerSubpopulationInstance {
        return Err("shared summary grouping has no registered kernel".into());
    }
    let keyed = match family {
        SummaryFamilyType::ExactAggregate(kind, params) => {
            use ExactKind as K;
            use ExactParams as P;
            if !matches!(
                (kind, params),
                (K::Sum, P::Sum)
                    | (K::Count, P::Count)
                    | (K::Min, P::Min)
                    | (K::Max, P::Max)
                    | (K::Rate, P::Rate)
                    | (K::Increase, P::Increase)
            ) {
                return Err(format!("unsupported exact kernel {family:?}"));
            }
            input.item.is_some()
        }
        SummaryFamilyType::Sketch(kind, layout) => {
            if layout != grouping {
                return Err("Planner family and operator grouping disagree".into());
            }
            use SketchAlgorithm as A;
            use SketchParams as P;
            match (kind.algorithm(), kind.params()) {
                (A::Kll, P::Kll { k }) if (8..=u16::MAX as u32).contains(k) => false,
                (A::DDSketch, P::DDSketch { alpha })
                    if alpha.is_finite() && *alpha > 0.0 && *alpha < 1.0 =>
                {
                    false
                }
                (A::Hll, P::Hll { precision }) if (4..=18).contains(precision) => false,
                (A::Cms, P::Cms { width, depth })
                | (A::CountSketch, P::CountSketch { width, depth })
                    if valid_matrix(*width, *depth) =>
                {
                    true
                }
                (
                    A::CmsWithHeap,
                    P::CmsWithHeap {
                        width,
                        depth,
                        heap_size,
                    },
                )
                | (
                    A::CountSketchWithHeap,
                    P::CountSketchWithHeap {
                        width,
                        depth,
                        heap_size,
                    },
                ) if valid_matrix(*width, *depth) && *heap_size > 0 => true,
                (
                    A::UnivMon,
                    P::UnivMon {
                        heap_size,
                        sketch_rows,
                        sketch_cols,
                        layers,
                    },
                ) if *heap_size > 0
                    && *sketch_cols > 0
                    && (1..=20).contains(sketch_rows)
                    && (1..=64).contains(layers)
                    && (*sketch_rows as usize)
                        .checked_mul(*sketch_cols as usize)
                        .and_then(|n| n.checked_mul(*layers as usize))
                        .is_some() =>
                {
                    false
                }
                _ => {
                    return Err(format!(
                        "unsupported kernel or invalid parameters: {kind:?}"
                    ))
                }
            }
        }
        _ => return Err(format!("unsupported summary kernel {family:?}")),
    };
    if keyed != input.item.is_some() && !is_unit_sample_frequency(input) {
        return Err("Planner item expression does not match kernel layout".into());
    }
    Ok(())
}

fn valid_matrix(width: u32, depth: u32) -> bool {
    // Construction uses the kernel's native row hashing, so no encoded-size
    // limit applies here.
    width > 0
        && depth > 0
        && (width as usize)
            .checked_mul(depth as usize)
            .and_then(|n| n.checked_mul(std::mem::size_of::<f64>()))
            .is_some()
}

pub(crate) fn is_unit_sample_frequency(update: &planner_types::post_asap::SummaryUpdate) -> bool {
    use planner_types::post_asap::{NonNegativeWeightProof, SummaryInputExpr, WeightDomain};
    matches!(
        update.item,
        Some(SummaryInputExpr::Column(
            planner_types::pre_asap::ColumnRef::SampleValue
        ))
    ) && matches!(update.weight, SummaryInputExpr::Constant(1.0))
        && matches!(
            update.weight_domain,
            WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::UnitCount
            }
        )
}

pub fn validate_native_family(family: &SummaryFamilyType) -> Result<(), Error> {
    use planner_types::post_asap::SketchAlgorithm as A;
    if let SummaryFamilyType::Sketch(kind, grouping) = family {
        // Plain Count-Min is native as stored state only: it merges and reads
        // its bare count, but the DAG does not build it from rows.
        if let (A::Cms, SketchParams::Cms { width, depth }) = (kind.algorithm(), kind.params()) {
            return if valid_matrix(*width, *depth) && grouping == &Default::default() {
                Ok(())
            } else {
                Err(Error::Invalid(
                    "invalid Count-Min dimensions or grouping strategy".into(),
                ))
            };
        }
        if matches!(kind.algorithm(), A::CmsWithHeap | A::CountSketchWithHeap) {
            let (_, width, depth, _) =
                crate::summary_kernels::weighted_frequency::WeightedFrequency::configuration(kind)?;
            return if valid_matrix(width as u32, depth as u32) && grouping == &Default::default() {
                Ok(())
            } else {
                Err(Error::Invalid(
                    "invalid weighted frequency dimensions or grouping strategy".into(),
                ))
            };
        }
    }
    match family {
        SummaryFamilyType::ExactAggregate(..) => {}
        SummaryFamilyType::Sketch(kind, _)
            if matches!(kind.algorithm(), A::Kll | A::DDSketch | A::Hll | A::UnivMon) => {}
        _ => {
            return Err(Error::Invalid(
                "summary family has no native DAG state implementation".into(),
            ))
        }
    }
    crate::capability::validate_summary_kernel(
        family,
        &planner_types::post_asap::SummaryUpdate::column(
            planner_types::pre_asap::ColumnRef::SampleValue,
        ),
        &Default::default(),
    )
    .map_err(Error::Invalid)
}

/// A sketch evaluation is native only for the families Planner can read directly.
pub fn validate_sketch_evaluation(
    family: &SummaryFamilyType,
    query: &SketchStatistic,
) -> Result<(), Error> {
    validate_native_family(family)?;
    use planner_types::post_asap::SketchAlgorithm as A;
    // A point count without an item value reads the total count.
    let bare_count = matches!(query, SketchStatistic::PointCount { value: None, .. });
    let supported = match family {
        SummaryFamilyType::Sketch(kind, _) => match (kind.algorithm(), query) {
            (A::Kll, SketchStatistic::Quantile { q })
            | (A::DDSketch, SketchStatistic::Quantile { q }) => {
                if !(0.0..=1.0).contains(q) {
                    return Err(Error::Invalid(
                        "quantile evaluation requires quantile in [0,1]".into(),
                    ));
                }
                true
            }
            (A::DDSketch, _) => bare_count,
            (A::Hll, SketchStatistic::Cardinality) => true,
            (A::Hll, _) => bare_count,
            (A::UnivMon, SketchStatistic::PointCount { value: None, .. })
            | (A::UnivMon, SketchStatistic::Cardinality)
            | (A::UnivMon, SketchStatistic::FrequencyL2)
            | (A::UnivMon, SketchStatistic::FrequencyEntropy) => true,
            // Only count intents read a Count-Min bare count, and their
            // updates have unit weight; the evaluation is typed Int64 on that basis.
            (A::Cms, _) => bare_count,
            _ => false,
        },
        _ => false,
    };
    if !supported {
        return Err(Error::Invalid(
            "evaluation is not implemented for this summary family".into(),
        ));
    }
    Ok(())
}

/// An exact evaluation must match the exact family it reads.
pub fn validate_exact_evaluation(
    family: &SummaryFamilyType,
    evaluation: &crate::summary_kernels::exact::ExactEvaluation,
) -> Result<(), Error> {
    validate_native_family(family)?;
    use crate::Statistic as S;
    use planner_types::post_asap::ExactKind as E;
    let supported = matches!(
        (family, evaluation.statistic),
        (SummaryFamilyType::ExactAggregate(E::Sum, _), S::Sum)
            | (SummaryFamilyType::ExactAggregate(E::Count, _), S::Count)
            | (SummaryFamilyType::ExactAggregate(E::Min, _), S::Min)
            | (SummaryFamilyType::ExactAggregate(E::Max, _), S::Max)
            | (SummaryFamilyType::ExactAggregate(E::Rate, _), S::Rate)
            | (
                SummaryFamilyType::ExactAggregate(E::Increase, _),
                S::Increase
            )
    );
    if !supported {
        return Err(Error::Invalid(
            "evaluation is not implemented for this summary family".into(),
        ));
    }
    if evaluation.lookback_ms.is_some_and(|lookback| {
        lookback <= 0 || !matches!(evaluation.statistic, S::Rate | S::Increase)
    }) {
        return Err(Error::Invalid("invalid exact counter lookback".into()));
    }
    Ok(())
}
