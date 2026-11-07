//! Decision-tree levels 4 and 5 (concepts doc, section 2): each logical
//! candidate's physical properties $p^{\mathrm{phys}} = (\tau, \ell)$, the
//! product of the timing and lifetime values less the invalid combination
//! (`PhysicalProperties::new` rejects it) and the values a prune rule
//! removes. Each prune rule reads only the value it removes and the values
//! decided above it.

use asap_types::ir::operator::{agg_has_exact_per_entity_kernel, Reduction};
use asap_types::ir::{NonASAPOp, Operator};
use asap_types::physical::{ExecutionTiming, Lifetime, PhysicalProperties, StorageMedium};
use asap_types::stages::{CandidateForMemoGroup, PruneReason};

/// What the deployment and the data allow, read by the prune rules.
pub(super) struct Deployment {
    /// The data keeps arriving (`DataArrival::Ingesting` or `Mixed`), so an
    /// ingestion pipeline can run candidates at ingestion time.
    pub(super) has_ingestion_pipeline: bool,
    pub(super) query_time_results_retained: bool,
    pub(super) raw_data_retained: bool,
}

/// Level 4's values, $\tau$.
const TIMINGS: [ExecutionTiming; 2] = [ExecutionTiming::QueryTime, ExecutionTiming::IngestionTime];

/// Level 5's values, $\ell$. A materialized result is stored in memory
/// only: the executor does not store `Disk` results yet (#1217).
const LIFETIMES: [Lifetime; 3] = [
    Lifetime::None,
    Lifetime::OneEvaluation(StorageMedium::Memory),
    Lifetime::AcrossEvaluations(StorageMedium::Memory),
];

/// Every valid $(\tau, \ell)$ of `candidate`, in [`TIMINGS`] ×
/// [`LIFETIMES`] order: `Ok` for an offered one, `Err` with the reason for
/// a pruned one. The invalid (IngestionTime, OneEvaluation) is not a
/// physical property, so it is neither.
pub(super) fn enumerate_candidate_configurations(
    candidate: &CandidateForMemoGroup,
    deployment: &Deployment,
) -> Vec<Result<PhysicalProperties, (PhysicalProperties, PruneReason)>> {
    let operator = &candidate.expression.operator;
    let mut alternatives = Vec::new();
    for timing in TIMINGS {
        for lifetime in LIFETIMES {
            let Ok(properties) = PhysicalProperties::new(timing, lifetime) else {
                continue;
            };
            let prune = |reason| Err((properties, reason));
            alternatives.push(if !executor_can_run(operator) {
                // Level 1, checked in Stage 2: Pass 1 and Pass 2 record no
                // pruned candidates.
                prune(PruneReason::PerEntityAggregateNotExecutable)
            } else if timing == ExecutionTiming::IngestionTime && !deployment.has_ingestion_pipeline
            {
                prune(PruneReason::NoIngestionPipeline)
            } else if timing == ExecutionTiming::QueryTime
                && !deployment.raw_data_retained
                && matches!(operator, Operator::NonASAP(NonASAPOp::Scan { .. }))
            {
                prune(PruneReason::RawDataNotRetained)
            } else if !matches!(lifetime, Lifetime::AcrossEvaluations(_)) {
                Ok(properties)
            } else if candidate.logical_properties.window_layout().is_none() {
                prune(PruneReason::AcrossEvaluationsWithoutWindowLayout)
            } else if timing == ExecutionTiming::QueryTime
                && !deployment.query_time_results_retained
            {
                prune(PruneReason::QueryTimeResultsNotRetained)
            } else {
                Ok(properties)
            });
        }
    }
    alternatives
}

/// Whether the executor can run `operator`: false only for an exact
/// per-entity `Aggregate` (a per-series range reduction over raw data
/// samples) that has more than one measure, a measure without an exact
/// per-entity kernel ([`agg_has_exact_per_entity_kernel`]), a measure
/// filter or a `HAVING`.
fn executor_can_run<C>(operator: &Operator<C>) -> bool {
    match operator {
        Operator::NonASAP(NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            measures,
            filters,
            having,
            ..
        }) => {
            matches!(measures.as_slice(), [measure] if agg_has_exact_per_entity_kernel(measure))
                && filters.iter().all(Option::is_none)
                && having.is_none()
        }
        _ => true,
    }
}
