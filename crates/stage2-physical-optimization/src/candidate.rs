//! Decision-tree levels 3 and 4 (concepts doc, section 2): each logical
//! candidate's physical properties $p^{\mathrm{phys}} = (\tau, \ell)$, the
//! product of the values each dimension offers the candidate less the
//! invalid combination (`PhysicalProperties::new` rejects it), each with
//! the reason a prune rule removes it. Each prune rule reads only the value
//! it removes and the values decided above it.

use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::operator::arguments::Source;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::stages::{
    CandidateForMemoGroupId, Lifetime, Memo, MemoGroupId, PhysicalProperties, PruneReason,
    StorageMedium, ValueSign,
};
use asap_types::summary_type::{ExactKind, SketchAlgorithm};
use asap_types::workload::{DataArrival, DataWorkload};

/// Level 3's values, $\tau$.
const TIMINGS: [ExecutionTiming; 2] = [ExecutionTiming::QueryTime, ExecutionTiming::IngestionTime];

/// Level 4's values, $\ell$. A materialized result is stored in memory
/// only: the deployment capabilities offer no disk storage
/// (`DeploymentCapabilities` has no disk field).
const LIFETIMES: [Lifetime; 3] = [
    Lifetime::None,
    Lifetime::OneEvaluation(StorageMedium::Memory),
    Lifetime::AcrossEvaluations(StorageMedium::Memory),
];

/// One $(\tau, \ell)$ offered to a logical candidate, with the reason a
/// prune rule removes it.
pub(super) struct OfferedPhysicalProperties {
    pub(super) physical_properties: PhysicalProperties,
    /// `None` when no prune rule removes it.
    pub(super) prune_reason: Option<PruneReason>,
}

/// Every valid $(\tau, \ell)$ offered to `candidate`, in [`TIMINGS`] ×
/// [`LIFETIMES`] order, each with the reason a prune rule removes it.
/// Offered are:
///
/// - `IngestionTime` only when `ingestion_time_offered_for_group` (the
///   candidate's group is time-independent or an input of one) and, for a
///   counter state, when its rows arrive in timestamp order or within a
///   known bound ([`counter_state_rows_ordered`]);
/// - `AcrossEvaluations` only when `across_evaluations_offered_for_group`
///   (a chain, a group on a known grid or a time-independent group, not
///   read under an absolute `@ T`);
/// - (IngestionTime, None) not for a summary build (`SummaryAgg`, a chain,
///   `MaintainPopulation`): fed at ingestion time, it keeps its state
///   between batches, so it is materialized.
///
/// A value not offered is not returned, and neither is the invalid
/// (IngestionTime, OneEvaluation).
pub(super) fn offered_physical_properties(
    memo: &Memo,
    candidate: CandidateForMemoGroupId,
    ingestion_time_offered_for_group: bool,
    across_evaluations_offered_for_group: bool,
    data: &DataWorkload,
    capabilities: &DeploymentCapabilities,
) -> Vec<OfferedPhysicalProperties> {
    let operator = &memo.candidate(candidate).expression.operator;
    let build = summary_build(operator);
    let ingestion_time_offered = ingestion_time_offered_for_group
        && build.is_none_or(|(summary_type, child)| {
            counter_state_rows_ordered(memo, summary_type, child, data)
        });
    let count_min_weights_not_proven_non_negative = build.is_some_and(|(summary_type, _)| {
        count_min_weights_not_proven_non_negative(memo, candidate.group, summary_type)
    });
    let has_ingestion_pipeline = data.arrival.rows_per_second().is_some();
    let scans_raw_data = matches!(operator, Operator::NonASAP(NonASAPOp::Scan { .. }));
    let builds_summary =
        build.is_some() || matches!(operator, Operator::ASAP(ASAPOp::MaintainPopulation { .. }));
    let mut offered = Vec::new();
    for timing in TIMINGS {
        if timing == ExecutionTiming::IngestionTime && !ingestion_time_offered {
            continue;
        }
        for lifetime in LIFETIMES {
            let across_evaluations = matches!(lifetime, Lifetime::AcrossEvaluations(_));
            if across_evaluations && !across_evaluations_offered_for_group {
                continue;
            }
            if builds_summary
                && timing == ExecutionTiming::IngestionTime
                && lifetime == Lifetime::None
            {
                continue;
            }
            let Ok(physical_properties) = PhysicalProperties::new(timing, lifetime) else {
                continue;
            };
            let query_time = timing == ExecutionTiming::QueryTime;
            let prune_reason = if count_min_weights_not_proven_non_negative {
                Some(PruneReason::CountMinWeightsNotProvenNonNegative)
            } else if !query_time && !has_ingestion_pipeline {
                Some(PruneReason::NoIngestionPipeline)
            } else if (query_time || across_evaluations)
                && scans_raw_data
                && !capabilities.raw_data_retained
            {
                Some(PruneReason::RawDataNotRetained)
            } else if query_time && across_evaluations && !capabilities.query_time_results_retained
            {
                Some(PruneReason::QueryTimeResultsNotRetained)
            } else {
                None
            };
            offered.push(OfferedPhysicalProperties {
                physical_properties,
                prune_reason,
            });
        }
    }
    offered
}

/// The summary type and input group of `operator` when it builds a
/// summary state from rows (`SummaryAgg`, `SummaryTumblingWindowChain`).
fn summary_build(operator: &Operator<MemoGroupId>) -> Option<(&FieldDataType, MemoGroupId)> {
    match operator {
        Operator::ASAP(
            ASAPOp::SummaryAgg {
                child,
                summary_type,
                ..
            }
            | ASAPOp::SummaryTumblingWindowChain {
                child,
                summary_type,
                ..
            },
        ) => Some((summary_type, *child)),
        _ => None,
    }
}

/// Whether a build of `summary_type` over group `child`, when it is a
/// counter state (`rate`, `increase`, `irate`), gets its rows in timestamp
/// order at ingestion time: the data declares how late a row may arrive
/// (`max_out_of_order_ms`), so a reorder buffer restores the order, or its
/// rows are PromQL samples, taken to arrive in order. True for any other
/// summary type.
fn counter_state_rows_ordered(
    memo: &Memo,
    summary_type: &FieldDataType,
    child: MemoGroupId,
    data: &DataWorkload,
) -> bool {
    let counter_state = matches!(
        summary_type,
        FieldDataType::ExactAggregate(ExactKind::Rate | ExactKind::Increase | ExactKind::IRate, _)
    );
    let out_of_order_bound_known = matches!(
        data.arrival,
        DataArrival::Ingesting {
            max_out_of_order_ms: Some(_),
            ..
        } | DataArrival::Mixed {
            max_out_of_order_ms: Some(_),
            ..
        }
    );
    !counter_state
        || out_of_order_bound_known
        || memo.groups()[child.0 as usize]
            .group_properties
            .scanned_sources
            .iter()
            .all(Source::is_time_series)
}

/// Whether a build of `summary_type` in group `group` is a Count-Min state
/// (`Cms`, `CmsWithHeap`, `HydraCms`) whose weights are not proven
/// non-negative: the state column of its group, which records the sign of
/// the weights it is updated with (`column_signs`), is not `NonNegative`.
fn count_min_weights_not_proven_non_negative(
    memo: &Memo,
    group: MemoGroupId,
    summary_type: &FieldDataType,
) -> bool {
    if !matches!(
        summary_type,
        FieldDataType::Sketch(
            SketchAlgorithm::Cms | SketchAlgorithm::CmsWithHeap | SketchAlgorithm::HydraCms
        )
    ) {
        return false;
    }
    let properties = &memo.groups()[group.0 as usize].group_properties;
    let weight_sign = properties
        .schema
        .fields
        .iter()
        .position(|field| !field.is_plain())
        .map(|state| properties.column_signs[state]);
    weight_sign != Some(ValueSign::NonNegative)
}
