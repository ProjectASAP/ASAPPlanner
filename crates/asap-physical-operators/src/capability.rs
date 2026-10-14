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
                    if (8..=planner_types::post_asap::sketch::KLL_MAX_K).contains(k) =>
                {
                    false
                }
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
                        ..
                    },
                ) if crate::summary_kernels::univmon::check_dimensions(
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
    if keyed != input.item.is_some() && !is_unit_sample_frequency(input) {
        return Err("Planner item expression does not match kernel layout".into());
    }
    Ok(())
}

/// The only shared grouping with a kernel: Hydra over Count-Min, counting
/// rows. Each update adds one occurrence of one item column's value to its
/// group.
fn validate_hydra_cms(
    family: &SummaryFamilyType,
    input: &SummaryUpdate,
    grouping: &GroupingStrategy,
) -> Result<(), String> {
    use planner_types::post_asap::{NonNegativeWeightProof, SummaryInputExpr, WeightDomain};
    if !matches!(family, SummaryFamilyType::Sketch(_, layout) if layout == grouping) {
        return Err("Planner family and operator grouping disagree".into());
    }
    hydra_cms_shape(family)?;
    if !matches!(input.item, Some(SummaryInputExpr::Column(_))) {
        return Err("HydraCms requires one item column".into());
    }
    if !matches!(input.weight, SummaryInputExpr::Constant(1.0))
        || !matches!(
            input.weight_domain,
            WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::UnitCount
            }
        )
    {
        return Err("HydraCms counts rows; its update weight must be the unit count".into());
    }
    Ok(())
}

/// `(shared_rows, shared_columns, width, depth)` of a HydraCms family. The
/// family is the per-group Count-Min it emulates, with the Hydra's own width
/// and depth.
pub(crate) fn hydra_cms_shape(
    family: &SummaryFamilyType,
) -> Result<(usize, usize, usize, usize), String> {
    use planner_types::post_asap::{HydraKind, HydraParams};
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
/// query-time build can apply it. Ingestion-time eligibility relies on this
/// check; the ingestion-time compiler and the binding of any
/// ingestion-timed `SummaryAgg` apply it.
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
            if matches!(kind.algorithm(), A::Kll | A::DDSketch | A::Hll) => {}
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
            // HydraCms answers a group's item frequency. A group's total
            // needs a group-mass query asap_sketchlib's Hydra lacks.
            (A::Cms, SketchStatistic::PointCount { value, .. })
                if matches!(family, SummaryFamilyType::Sketch(_, grouping)
                    if grouping != &GroupingStrategy::PerSubpopulationInstance) =>
            {
                value.is_some()
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    // Only a query-time build applies a summary filter.
    #[test]
    fn filtered_summary_runs_only_at_query_time() {
        let filter = planner_types::ir::Predicate::<usize>(planner_types::ir::ScalarExpr::Literal(
            planner_types::pre_asap::ScalarValue::Boolean(true),
        ));
        assert!(validate_ingestion_time_summary(Some(&filter)).is_err());
        assert!(validate_ingestion_time_summary::<usize>(None).is_ok());
    }
}
