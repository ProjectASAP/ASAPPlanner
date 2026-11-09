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
    ExactKind, ExactParams, FieldDataType as SummaryFamilyType, SketchAlgorithm, SketchStatistic,
    StatisticKind, SummaryTypeCapability, SummaryUpdate,
};

/// Check the same contract used by `create_planner_accumulator` before a plan
/// is accepted. Execution timing is deliberately not a kernel property.
pub fn validate_summary_kernel(
    family: &SummaryFamilyType,
    input: &SummaryUpdate,
) -> Result<(), String> {
    if SummaryTypeCapability::shares_one_state_across_groups(family) {
        return validate_hydra_cms(family, input);
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
        SummaryFamilyType::Sketch(_) => {
            todo!("#1207 PR 6: check θ, which the exported node carries")
        }
        _ => return Err(format!("unsupported summary kernel {family:?}")),
    };
    if keyed != input.item.is_some() && unit_frequency_column(input).is_none() {
        return Err("Planner item expression does not match kernel layout".into());
    }
    Ok(())
}

/// The only Hydra algorithm with a kernel: Hydra over Count-Min. Each update
/// adds one item column's value to its group, with weight 1 (a row count), a
/// positive integer constant or a weight column. The planner offers a
/// Count-Min build only over weights proven non-negative (a Count-Min point
/// count only overestimates when no weight is negative), and the build
/// rejects a negative weight value.
fn validate_hydra_cms(family: &SummaryFamilyType, input: &SummaryUpdate) -> Result<(), String> {
    use planner_types::ir::schema::SummaryInputExpr;
    hydra_cms_shape(family)?;
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

/// `(shared_rows, shared_columns, width, depth)` of a HydraCms state.
pub(crate) fn hydra_cms_shape(
    family: &SummaryFamilyType,
) -> Result<(usize, usize, usize, usize), String> {
    let SummaryFamilyType::Sketch(SketchAlgorithm::HydraCms) = family else {
        return Err("a Hydra algorithm other than HydraCms has no registered kernel".into());
    };
    todo!("#1207 PR 6: the Hydra grid's sizes are θ, which the exported node carries")
}

/// A filtered `SummaryAgg` never runs at ingestion time. An ingestion-time
/// build reads raw data samples (`$population`, `$timestamp`, `value`), which carry
/// none of the query's columns for the filter predicate to read, so only a
/// query-time build can apply it. The ingestion-time compiler and the
/// binding of any ingestion-timed `SummaryAgg` apply this check. The planner
/// offers a filtered build at ingestion time when its input is produced at
/// ingestion time; feeding such a build its input rows is #1207 PR 6.
pub fn validate_ingestion_time_summary<C>(
    filter: Option<&planner_types::ir::Predicate<C>>,
) -> Result<(), String> {
    match filter {
        Some(_) => Err("a filtered summary update runs only at query time".into()),
        None => Ok(()),
    }
}

#[allow(dead_code)] // Used again in #1207 PR 6, with θ from the exported node.
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
    use planner_types::ir::schema::SummaryInputExpr;
    match (&update.item, &update.weight) {
        (Some(SummaryInputExpr::Column(column)), SummaryInputExpr::Constant(weight))
            if *weight == 1.0 =>
        {
            Some(column)
        }
        _ => None,
    }
}

pub fn validate_native_family(family: &SummaryFamilyType) -> Result<(), Error> {
    use planner_types::ir::schema::SketchAlgorithm as A;
    if let SummaryFamilyType::Sketch(kind) = family {
        // Plain Count-Min is native as stored state only: it merges and reads
        // its bare count, but the DAG does not build it from rows. HydraCms
        // is built by the all-groups summary build.
        if let A::HydraCms = kind {
            return hydra_cms_shape(family).map(|_| ()).map_err(Error::Invalid);
        }
        if let A::Cms = kind {
            todo!("#1207 PR 6: check the Count-Min θ, which the exported node carries")
        }
        if matches!(kind, A::CmsWithHeap | A::CountSketchWithHeap) {
            todo!("#1207 PR 6: check the weighted-frequency θ, which the exported node carries")
        }
    }
    match family {
        SummaryFamilyType::ExactAggregate(..) => {}
        SummaryFamilyType::Sketch(A::Kll | A::DDSketch | A::Hll | A::Kmv | A::UnivMon) => {}
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
    )
    .map_err(Error::Invalid)
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
    use planner_types::ir::schema::ExactKind as E;
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
