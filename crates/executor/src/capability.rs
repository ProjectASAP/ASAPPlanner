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
use planner_types::ir::schema::{
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
        return validate_hydra_cms(family, input, grouping);
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
                // `asap_sketchlib` silently caps a larger `k`.
                (A::Kll, P::Kll { k, .. })
                    if (8..=planner_types::ir::schema::sketch::KLL_MAX_K).contains(k) =>
                {
                    false
                }
                (A::DDSketch, P::DDSketch { alpha })
                    if alpha.is_finite() && *alpha > 0.0 && *alpha < 1.0 =>
                {
                    false
                }
                (A::Hll, P::Hll { precision, .. }) if (4..=18).contains(precision) => false,
                (A::Kmv, P::Kmv { k, .. }) if *k >= crate::summary_kernels::kmv::KMV_MIN_K => false,
                (A::Cms, P::Cms { width, depth }) if valid_matrix(*width, *depth) => true,
                // The kernel's shape rules (an odd depth, for the median).
                (A::CountSketch, P::CountSketch { width, depth })
                    if valid_matrix(*width, *depth)
                        && asap_sketchlib::WeightedFrequency::check_shape(
                            asap_sketchlib::FrequencyAlgorithm::CountSketch,
                            *width as usize,
                            *depth as usize,
                        )
                        .is_ok() =>
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
                        ..
                    },
                ) if asap_sketchlib::UnivMon::check_shape(
                    *heap_size as usize,
                    *sketch_rows as usize,
                    *sketch_cols as usize,
                    usize::from(*layers),
                )
                .is_ok() =>
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
    if keyed != input.item.is_some() && unit_frequency_column(input).is_none() {
        return Err("Planner item expression does not match kernel layout".into());
    }
    Ok(())
}

/// The only shared grouping with a kernel: Hydra over Count-Min. Each update
/// adds one item column's value to its group, with weight 1 (a row count,
/// `UnitCount`), a positive integer constant (`PositiveConstant`) or a weight
/// column proven non-negative (a Count-Min point count only overestimates
/// when no weight is negative).
fn validate_hydra_cms(
    family: &SummaryFamilyType,
    input: &SummaryUpdate,
    grouping: &GroupingStrategy,
) -> Result<(), String> {
    use planner_types::ir::schema::{NonNegativeWeightProof, SummaryInputExpr, WeightDomain};
    if !matches!(family, SummaryFamilyType::Sketch(_, layout) if layout == grouping) {
        return Err("Planner family and operator grouping disagree".into());
    }
    hydra_cms_shape(family)?;
    if !matches!(input.item, Some(SummaryInputExpr::Column(_))) {
        return Err("HydraCms requires one item column".into());
    }
    let WeightDomain::NonNegative { proof } = &input.weight_domain else {
        return Err("HydraCms weights must be non-negative; the weight's sign is unknown".into());
    };
    match (&input.weight, proof) {
        (SummaryInputExpr::Constant(w), NonNegativeWeightProof::UnitCount) if *w == 1.0 => Ok(()),
        (SummaryInputExpr::Constant(w), NonNegativeWeightProof::PositiveConstant) => {
            hydra_constant_weight(*w).map(|_| ())
        }
        (SummaryInputExpr::Column(_), _) => Ok(()),
        _ => Err(
            "HydraCms weight must be the unit count, a positive constant or a non-negative \
             weight column"
                .into(),
        ),
    }
}

/// A HydraCms constant weight as a positive integer at most `i32::MAX`
/// (asap_sketchlib's Hydra adds `i32` weights); an error otherwise.
pub(crate) fn hydra_constant_weight(weight: f64) -> Result<std::num::NonZeroU32, String> {
    if weight.is_finite() && weight > 0.0 && weight.fract() == 0.0 && weight <= f64::from(i32::MAX)
    {
        Ok(std::num::NonZeroU32::new(weight as u32).expect("weight > 0"))
    } else {
        Err(format!(
            "HydraCms constant weight must be a positive integer within i32 (asap_sketchlib's \
             Hydra adds i32 weights), got {weight}"
        ))
    }
}

/// `(shared_rows, shared_columns, width, depth)` of a HydraCms family. The
/// family is the per-group Count-Min it emulates, with the Hydra's own width
/// and depth.
pub(crate) fn hydra_cms_shape(
    family: &SummaryFamilyType,
) -> Result<(usize, usize, usize, usize), String> {
    use planner_types::ir::schema::{HydraKind, HydraParams};
    let SummaryFamilyType::Sketch(
        kind,
        GroupingStrategy::SharedMultiSubpopulation {
            kind: HydraKind::HydraCms,
            params:
                HydraParams::HydraCms {
                    width,
                    depth,
                    shared_rows,
                    shared_columns,
                },
        },
    ) = family
    else {
        return Err("shared summary grouping has no registered kernel".into());
    };
    if kind.algorithm() != &SketchAlgorithm::Cms
        || kind.params()
            != &(SketchParams::Cms {
                width: *width,
                depth: *depth,
            })
    {
        return Err("HydraCms family must be Count-Min with the Hydra width and depth".into());
    }
    if !valid_matrix(*width, *depth) || !valid_matrix(*shared_columns, *shared_rows) {
        return Err("invalid HydraCms dimensions".into());
    }
    Ok((
        *shared_rows as usize,
        *shared_columns as usize,
        *width as usize,
        *depth as usize,
    ))
}

/// A filtered `SummaryAgg` never runs at ingestion time. An ingestion-time
/// build reads raw samples (`$population`, `$timestamp`, `value`), which carry
/// none of the query's columns for the filter predicate to read, so only a
/// query-time build can apply it. The ingestion-time compiler and the
/// binding of any ingestion-timed `SummaryAgg` apply this check. The planner
/// applies the same rule: Stage 2 never offers `MatAtIngestion` for a
/// filtered build (`IngestionPruneReason::Filtered`), and materialization
/// timing rejects an explicit one (`FilteredSummaryAtIngestion`). This check
/// guards plans that did not come from the planner.
pub fn validate_ingestion_time_summary<C>(
    filter: Option<&planner_types::ir::Predicate<C>>,
) -> Result<(), String> {
    match filter {
        Some(_) => Err("a filtered summary update runs only at query time".into()),
        None => Ok(()),
    }
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

/// The one item column of a unit-count frequency update (each value is one
/// occurrence), the update an unkeyed frequency family (HLL, UnivMon) builds
/// from that column's values.
pub(crate) fn unit_frequency_column(
    update: &planner_types::ir::schema::SummaryUpdate,
) -> Option<&planner_types::ir::scalar::ColumnRef> {
    use planner_types::ir::schema::{NonNegativeWeightProof, SummaryInputExpr, WeightDomain};
    match (&update.item, &update.weight, &update.weight_domain) {
        (
            Some(SummaryInputExpr::Column(column)),
            SummaryInputExpr::Constant(weight),
            WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::UnitCount,
            },
        ) if *weight == 1.0 => Some(column),
        _ => None,
    }
}

pub(crate) fn is_unit_sample_frequency(update: &planner_types::ir::schema::SummaryUpdate) -> bool {
    use planner_types::ir::schema::{NonNegativeWeightProof, SummaryInputExpr, WeightDomain};
    matches!(
        update.item,
        Some(SummaryInputExpr::Column(
            planner_types::ir::scalar::ColumnRef::SampleValue
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
    use planner_types::ir::schema::SketchAlgorithm as A;
    if let SummaryFamilyType::Sketch(kind, grouping) = family {
        // Plain Count-Min is native as stored state only: it merges and reads
        // its bare count, but the DAG does not build it from rows. HydraCms
        // is built by the shared summary operator.
        if let (A::Cms, SketchParams::Cms { width, depth }) = (kind.algorithm(), kind.params()) {
            if grouping != &GroupingStrategy::PerSubpopulationInstance {
                return hydra_cms_shape(family).map(|_| ()).map_err(Error::Invalid);
            }
            return if valid_matrix(*width, *depth) {
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
            if matches!(
                kind.algorithm(),
                A::Kll | A::DDSketch | A::Hll | A::Kmv | A::UnivMon
            ) => {}
        _ => {
            return Err(Error::Invalid(
                "summary family has no native DAG state implementation".into(),
            ))
        }
    }
    crate::capability::validate_summary_kernel(
        family,
        &planner_types::ir::schema::SummaryUpdate::column(
            planner_types::ir::scalar::ColumnRef::SampleValue,
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
    use planner_types::ir::schema::SketchAlgorithm as A;
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
            // HydraCms answers a group's item frequency. A group's total
            // needs a group-mass query asap_sketchlib's Hydra lacks.
            (A::Cms, SketchStatistic::PointCount { value, .. })
                if matches!(family, SummaryFamilyType::Sketch(_, grouping)
                    if grouping != &GroupingStrategy::PerSubpopulationInstance) =>
            {
                if value.is_none() {
                    return Err(Error::Invalid(
                        "HydraCms does not answer a group total: asap_sketchlib's Hydra has no \
                         group-mass query; only an item's point count within a group is answered"
                            .into(),
                    ));
                }
                true
            }
            (A::Kmv, SketchStatistic::Cardinality) => true,
            (A::Kmv, _) => bare_count,
            // One frequency state answers its total, distinct count, L2 norm
            // and entropy.
            (
                A::UnivMon,
                SketchStatistic::Cardinality
                | SketchStatistic::FrequencyL2
                | SketchStatistic::FrequencyEntropy,
            ) => true,
            (A::UnivMon, _) => bare_count,
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
    use planner_types::ir::schema::ExactKind as E;
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

#[cfg(test)]
mod tests {
    use super::*;

    // Only a query-time build applies a summary filter.
    #[test]
    fn filtered_summary_runs_only_at_query_time() {
        let filter = planner_types::ir::Predicate::<usize>(planner_types::ir::ScalarExpr::Literal(
            planner_types::ir::scalar::ScalarValue::Boolean(true),
        ));
        assert!(validate_ingestion_time_summary(Some(&filter)).is_err());
        assert!(validate_ingestion_time_summary::<usize>(None).is_ok());
    }
}
