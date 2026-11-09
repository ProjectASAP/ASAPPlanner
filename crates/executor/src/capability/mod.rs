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

use crate::summary_kernels::hll_sketch::HLL_PRECISIONS;
use crate::summary_kernels::weighted_frequency::{self, FrequencyAlgorithm};
use crate::Error;
use planner_types::ir::schema::FieldDataType as SummaryFamilyType;
use planner_types::summary_type::{
    ExactKind, ExactParams, SketchAlgorithm, SketchParams, SketchStatistic, StatisticKind,
    SummaryTypeCapability, SummaryUpdate,
};

/// Check the same contract used by `create_planner_accumulator` before a plan
/// is accepted. Execution timing is deliberately not a kernel property.
///
/// `parameters` is θ, which the exported build node carries
/// (`PhysicalASAPDAGNode::parameters`): required for a sketch family,
/// `None` for an exact one.
pub fn validate_summary_kernel(
    family: &SummaryFamilyType,
    parameters: Option<&SketchParams>,
    input: &SummaryUpdate,
) -> Result<(), String> {
    if SummaryTypeCapability::shares_one_state_across_groups(family) {
        return validate_hydra_cms(family, parameters, input);
    }
    let keyed = match family {
        SummaryFamilyType::ExactAggregate(..) => {
            if !exact_kernel(family) {
                return Err(format!("unsupported exact kernel {family:?}"));
            }
            if parameters.is_some() {
                return Err("an exact summary has no parameters θ".into());
            }
            input.item.is_some()
        }
        SummaryFamilyType::Sketch(algorithm) => {
            let Some(parameters) = parameters else {
                return Err(format!("a {algorithm:?} summary needs its parameters θ"));
            };
            validate_sketch_parameters(algorithm, parameters)?
        }
        _ => return Err(format!("unsupported summary kernel {family:?}")),
    };
    if keyed != input.item.is_some() && unit_frequency_column(input).is_none() {
        return Err("Planner item expression does not match kernel layout".into());
    }
    Ok(())
}

/// Whether an exact family has a kernel.
fn exact_kernel(family: &SummaryFamilyType) -> bool {
    use ExactKind as K;
    use ExactParams as P;
    matches!(
        family,
        SummaryFamilyType::ExactAggregate(K::Sum, P::Sum)
            | SummaryFamilyType::ExactAggregate(K::Count, P::Count)
            | SummaryFamilyType::ExactAggregate(K::Min, P::Min)
            | SummaryFamilyType::ExactAggregate(K::Max, P::Max)
            | SummaryFamilyType::ExactAggregate(K::Rate, P::Rate)
            | SummaryFamilyType::ExactAggregate(K::Increase, P::Increase)
    )
}

/// Check θ of a per-instance sketch of `algorithm` against what its kernel
/// builds; whether the kernel is keyed (its updates name an item).
fn validate_sketch_parameters(
    algorithm: &SketchAlgorithm,
    parameters: &SketchParams,
) -> Result<bool, String> {
    use SketchAlgorithm as A;
    use SketchParams as P;
    let keyed = match (algorithm, parameters) {
        // `asap_sketchlib` silently caps a larger `k`.
        (A::Kll, P::Kll { k }) if (8..=planner_types::summary_type::KLL_MAX_K).contains(k) => false,
        (A::DDSketch, P::DDSketch { alpha })
            if alpha.is_finite() && *alpha > 0.0 && *alpha < 1.0 =>
        {
            false
        }
        (A::Hll, P::Hll { precision }) if HLL_PRECISIONS.contains(precision) => false,
        (A::Kmv, P::Kmv { k }) if *k >= crate::summary_kernels::kmv::KMV_MIN_K => false,
        (A::Cms, P::Cms { width, depth }) if valid_matrix(*width, *depth) => true,
        // The kernel's shape rules (an odd depth, for the median).
        (A::CountSketch, P::CountSketch { width, depth })
            if valid_matrix(*width, *depth)
                && weighted_frequency::check_shape(
                    FrequencyAlgorithm::CountSketch,
                    *width as usize,
                    *depth as usize,
                )
                .is_ok() =>
        {
            true
        }
        (A::CmsWithHeap | A::CountSketchWithHeap, _)
            if weighted_frequency::WeightedFrequency::configuration(algorithm, parameters)
                .is_ok_and(|(_, width, depth, _)| valid_matrix(width as u32, depth as u32)) =>
        {
            true
        }
        (
            A::UnivMon,
            P::UnivMon {
                heap_size,
                sketch_rows,
                sketch_cols,
                layers,
            },
        ) if crate::summary_kernels::univmon::check_shape(
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
                "unsupported kernel or invalid parameters: {algorithm:?} {parameters:?}"
            ))
        }
    };
    Ok(keyed)
}

/// The only Hydra algorithm with a kernel: Hydra over Count-Min. Each update
/// adds one item column's value to its group, with weight 1 (a row count), a
/// positive integer constant or a weight column. The planner offers a
/// Count-Min build only over weights proven non-negative (a Count-Min point
/// count only overestimates when no weight is negative), and the build
/// rejects a negative weight value.
fn validate_hydra_cms(
    family: &SummaryFamilyType,
    parameters: Option<&SketchParams>,
    input: &SummaryUpdate,
) -> Result<(), String> {
    use planner_types::summary_type::SummaryInputExpr;
    hydra_cms_shape(family, parameters)?;
    if !matches!(input.item, Some(SummaryInputExpr::Column(_))) {
        return Err("HydraCms requires one item column".into());
    }
    match &input.weight {
        SummaryInputExpr::Constant(w) => {
            let weight = constant_row_weight(*w).map_err(|error| format!("HydraCms {error}"))?;
            i32::try_from(weight.get()).map(|_| ()).map_err(|_| {
                format!(
                    "HydraCms constant weight must be at most i32::MAX (asap_sketchlib's \
                     Hydra adds i32 weights), got {weight}"
                )
            })
        }
        SummaryInputExpr::Column(_) => Ok(()),
        _ => Err(
            "HydraCms weight must be the unit count, a positive constant or a weight column".into(),
        ),
    }
}

/// A `SummaryUpdate` constant weight as the positive integer a
/// [`RowWeight::Constant`](crate::operators::RowWeight::Constant) holds
/// (at most `u32::MAX`); an error otherwise. The all-groups (HydraCms) build
/// further bounds it by `i32::MAX`.
pub(crate) fn constant_row_weight(weight: f64) -> Result<std::num::NonZeroU32, String> {
    if weight.is_finite() && weight > 0.0 && weight.fract() == 0.0 && weight <= f64::from(u32::MAX)
    {
        Ok(std::num::NonZeroU32::new(weight as u32).expect("weight > 0"))
    } else {
        Err(format!(
            "constant weight must be a positive integer within u32, got {weight}"
        ))
    }
}

/// A HydraCms family: the only Hydra algorithm with a kernel.
fn validate_hydra_cms_family(family: &SummaryFamilyType) -> Result<(), String> {
    match family {
        SummaryFamilyType::Sketch(SketchAlgorithm::HydraCms) => Ok(()),
        _ => Err("a Hydra algorithm other than HydraCms has no registered kernel".into()),
    }
}

/// `(shared_rows, shared_columns, width, depth)` of a HydraCms state whose
/// θ is `parameters` (`SketchParams::Hydra(HydraParams::HydraCms { .. })`):
/// the grid's own rows and columns, and the width and depth of the
/// per-group Count-Min it emulates.
pub(crate) fn hydra_cms_shape(
    family: &SummaryFamilyType,
    parameters: Option<&SketchParams>,
) -> Result<(usize, usize, usize, usize), String> {
    use planner_types::summary_type::HydraParams;
    validate_hydra_cms_family(family)?;
    let Some(SketchParams::Hydra(HydraParams::HydraCms {
        width,
        depth,
        shared_rows,
        shared_columns,
    })) = parameters
    else {
        return Err(format!(
            "a HydraCms summary needs HydraCms parameters θ, got {parameters:?}"
        ));
    };
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
/// build reads raw data samples (`$population`, `$timestamp`, `value`), which carry
/// none of the query's columns for the filter predicate to read, so only a
/// query-time build can apply it. The ingestion-time compiler and the
/// binding of any ingestion-timed `SummaryAgg` apply this check. The planner
/// offers a filtered build at ingestion time when its input is produced at
/// ingestion time; this check refuses it until the executor feeds such a
/// build its input rows.
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
    update: &planner_types::summary_type::SummaryUpdate,
) -> Option<&planner_types::ir::scalar::ColumnRef> {
    use planner_types::summary_type::SummaryInputExpr;
    match (&update.item, &update.weight) {
        (Some(SummaryInputExpr::Column(column)), SummaryInputExpr::Constant(weight))
            if *weight == 1.0 =>
        {
            Some(column)
        }
        _ => None,
    }
}

/// Whether the executor holds, merges and reads states of `family` natively.
/// θ is not part of a family: each kernel checks it when building a state,
/// and merges check that their states have one shape.
pub fn validate_native_family(family: &SummaryFamilyType) -> Result<(), Error> {
    use planner_types::summary_type::SketchAlgorithm as A;
    let native = match family {
        SummaryFamilyType::ExactAggregate(..) => exact_kernel(family),
        // HydraCms is built by the all-groups summary build. Plain Count-Min
        // is native as stored state only: it merges and reads its bare
        // count, but the DAG does not build it from rows.
        SummaryFamilyType::Sketch(
            A::HydraCms
            | A::Cms
            | A::CmsWithHeap
            | A::CountSketchWithHeap
            | A::Kll
            | A::DDSketch
            | A::Hll
            | A::Kmv
            | A::UnivMon,
        ) => true,
        _ => false,
    };
    if native {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "summary family {family:?} has no native DAG state implementation"
        )))
    }
}

/// What the executor evaluates from a per-instance state of each family it
/// builds and reads: its supported-summary declaration. Every readout here is
/// in the family's row of the family-capability table; a readout the row
/// lists and this omits is not evaluated yet (Count-Min's and the heap
/// families' item frequency, which the executor answers only from a Hydra
/// grid; KLL's rank; UnivMon's G-sum). Plain Count Sketch is built but read
/// from nothing.
pub(crate) const EVALUATED_READOUTS: &[(SketchAlgorithm, &[StatisticKind])] = &[
    (SketchAlgorithm::Cms, &[StatisticKind::Total]),
    (SketchAlgorithm::CmsWithHeap, &[StatisticKind::TopK]),
    (SketchAlgorithm::CountSketchWithHeap, &[StatisticKind::TopK]),
    (SketchAlgorithm::Hll, &[StatisticKind::Distinct]),
    (SketchAlgorithm::Kmv, &[StatisticKind::Distinct]),
    (SketchAlgorithm::Kll, &[StatisticKind::Quantile]),
    (SketchAlgorithm::DDSketch, &[StatisticKind::Quantile]),
    (
        SketchAlgorithm::UnivMon,
        &[
            StatisticKind::Total,
            StatisticKind::FrequencyL2,
            StatisticKind::Distinct,
            StatisticKind::FrequencyEntropy,
        ],
    ),
];

/// Whether the executor reads `readout` from a state of `family` whose
/// family-capability table row answers it.
fn evaluates(family: &SummaryFamilyType, readout: StatisticKind) -> bool {
    match family {
        SummaryFamilyType::Sketch(SketchAlgorithm::HydraCms | SketchAlgorithm::HydraKll) => true,
        SummaryFamilyType::Sketch(kind) => EVALUATED_READOUTS
            .iter()
            .any(|(family, readouts)| family == kind && readouts.contains(&readout)),
        _ => false,
    }
}

/// A sketch evaluation is native when the family-capability table says the
/// family answers the statistic and the executor evaluates that readout.
/// Top-k is a keyed evaluation (`Operator::keyed_evaluation`), not a sketch
/// evaluation.
pub fn validate_sketch_evaluation(
    family: &SummaryFamilyType,
    query: &SketchStatistic,
) -> Result<(), Error> {
    validate_native_family(family)?;
    let readout = query.kind();
    // HydraCms answers a group's item frequency. A group's total needs a
    // group-mass query asap_sketchlib's Hydra lacks.
    if readout == StatisticKind::Total
        && matches!(family, SummaryFamilyType::Sketch(SketchAlgorithm::HydraCms))
    {
        return Err(Error::Invalid(
            "HydraCms does not answer a group total: asap_sketchlib's Hydra has no group-mass \
             query; only an item's point count within a group is answered"
                .into(),
        ));
    }
    if readout == StatisticKind::TopK
        || !SummaryTypeCapability::of(family).answers(query)
        || !evaluates(family, readout)
    {
        return Err(Error::Invalid(
            "evaluation is not implemented for this summary family".into(),
        ));
    }
    if let SketchStatistic::Quantile { q } = query {
        if !(0.0..=1.0).contains(q) {
            return Err(Error::Invalid(
                "quantile evaluation requires quantile in [0,1]".into(),
            ));
        }
    }
    Ok(())
}

/// An exact evaluation must match the exact family it reads.
pub fn validate_exact_evaluation(
    family: &SummaryFamilyType,
    evaluation: &crate::summary_kernels::exact::ExactEvaluation,
) -> Result<(), Error> {
    validate_native_family(family)?;
    use planner_types::summary_type::ExactKind as E;
    let supported = matches!(
        family,
        SummaryFamilyType::ExactAggregate(
            kind @ (E::Sum | E::Count | E::Min | E::Max | E::Rate | E::Increase),
            _,
        ) if *kind == evaluation.statistic
    );
    if !supported {
        return Err(Error::Invalid(
            "evaluation is not implemented for this summary family".into(),
        ));
    }
    if evaluation.lookback_ms.is_some_and(|lookback| {
        lookback <= 0 || !matches!(evaluation.statistic, E::Rate | E::Increase)
    }) {
        return Err(Error::Invalid("invalid exact counter lookback".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
