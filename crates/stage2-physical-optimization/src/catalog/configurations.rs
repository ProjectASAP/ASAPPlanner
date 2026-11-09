//! Decision-tree levels 3 and 4 (concepts doc, section 2): each logical
//! candidate's physical properties $p^{\mathrm{phys}} = (\tau, \ell)$, the
//! product of the values each dimension offers the candidate less the
//! invalid combination (`PhysicalProperties::new` rejects it), each with
//! the reason a prune rule removes it. Each prune rule reads only the value
//! it removes and the values decided above it.

use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::schema::{ExactKind, FieldDataType, SketchAlgorithm};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::physical::{ExecutionTiming, Lifetime, PhysicalProperties, StorageMedium};
use asap_types::stages::{
    reads_only_time_series, CandidateForMemoGroupId, LogicalASAPMemoCandidates, Memo, MemoGroupId,
    PruneReason, ValueSign,
};
use asap_types::workload::{DataArrival, DataWorkload};

/// Level 3's values, $\tau$.
const TIMINGS: [ExecutionTiming; 2] = [ExecutionTiming::QueryTime, ExecutionTiming::IngestionTime];

/// Level 4's values, $\ell$. A materialized result is stored in memory
/// only: the deployment capabilities offer no disk storage (#1217).
const LIFETIMES: [Lifetime; 3] = [
    Lifetime::None,
    Lifetime::OneEvaluation(StorageMedium::Memory),
    Lifetime::AcrossEvaluations(StorageMedium::Memory),
];

/// Every valid $(\tau, \ell)$ offered to `candidate`, in [`TIMINGS`] ×
/// [`LIFETIMES`] order, each with the reason a prune rule removes it
/// (`None` for a kept one). Offered are:
///
/// - `IngestionTime` only when `ingestion_time_offered` (the candidate's
///   group is time-independent or an input of one) and, for a counter
///   state, when its rows arrive in timestamp order or within a known
///   bound ([`counter_state_rows_ordered`]);
/// - `AcrossEvaluations` only for a chain and for a candidate of a group
///   evaluated on a known grid (`evaluation_grid_of_group`).
///
/// A value not offered is not recorded, and neither is the invalid
/// (IngestionTime, OneEvaluation).
pub(super) fn enumerate_candidate_configurations(
    logical: &LogicalASAPMemoCandidates,
    candidate: CandidateForMemoGroupId,
    ingestion_time_offered: bool,
    data: &DataWorkload,
    capabilities: &DeploymentCapabilities,
) -> Vec<(PhysicalProperties, Option<PruneReason>)> {
    let memo = &logical.memo;
    let operator = &memo.candidate(candidate).expression.operator;
    let ingestion_time_offered =
        ingestion_time_offered && counter_state_rows_ordered(memo, operator, data);
    let across_evaluations_offered =
        matches!(
            operator,
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
        ) || logical.evaluation_grid_of_group[candidate.group.0 as usize].is_some();
    let count_min_weights_not_proven_non_negative =
        count_min_weights_not_proven_non_negative(memo, candidate.group, operator);
    let has_ingestion_pipeline = data.arrival.rows_per_second().is_some();
    let scans_raw_data = matches!(operator, Operator::NonASAP(NonASAPOp::Scan { .. }));
    let mut configurations = Vec::new();
    for timing in TIMINGS {
        if timing == ExecutionTiming::IngestionTime && !ingestion_time_offered {
            continue;
        }
        for lifetime in LIFETIMES {
            let across_evaluations = matches!(lifetime, Lifetime::AcrossEvaluations(_));
            if across_evaluations && !across_evaluations_offered {
                continue;
            }
            let Ok(properties) = PhysicalProperties::new(timing, lifetime) else {
                continue;
            };
            let query_time = timing == ExecutionTiming::QueryTime;
            let reason = if count_min_weights_not_proven_non_negative {
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
            configurations.push((properties, reason));
        }
    }
    configurations
}

/// Whether `operator`, when it builds a counter state (`rate`, `increase`,
/// `irate`), gets its rows in timestamp order at ingestion time: the data
/// declares how late a row may arrive (`max_out_of_order_ms`), so a reorder
/// buffer restores the order, or its rows are PromQL samples, taken to
/// arrive in order. True for any other operator.
fn counter_state_rows_ordered(
    memo: &Memo,
    operator: &Operator<MemoGroupId>,
    data: &DataWorkload,
) -> bool {
    let (Operator::ASAP(ASAPOp::SummaryAgg {
        child,
        summary_type,
        ..
    })
    | Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child,
        summary_type,
        ..
    })) = operator
    else {
        return true;
    };
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
    !counter_state || out_of_order_bound_known || reads_only_time_series(memo, *child)
}

/// Whether `operator` builds a Count-Min state (`Cms`, `CmsWithHeap`,
/// `HydraCms`) whose weights are not proven non-negative: the state column
/// of its group, which records the sign of the weights it is updated with
/// (`column_sign`), is not `NonNegative`.
fn count_min_weights_not_proven_non_negative(
    memo: &Memo,
    group: MemoGroupId,
    operator: &Operator<MemoGroupId>,
) -> bool {
    let (Operator::ASAP(ASAPOp::SummaryAgg { summary_type, .. })
    | Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { summary_type, .. })) = operator
    else {
        return false;
    };
    if !matches!(
        summary_type,
        FieldDataType::Sketch(
            SketchAlgorithm::Cms | SketchAlgorithm::CmsWithHeap | SketchAlgorithm::HydraCms
        )
    ) {
        return false;
    }
    memo.groups()[group.0 as usize]
        .group_properties
        .state_column_sign()
        != Some(ValueSign::NonNegative)
}
