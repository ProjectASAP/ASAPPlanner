//! Decision-tree levels 3 and 4 (concepts doc, section 2): each logical
//! candidate's physical properties $p^{\mathrm{phys}} = (\tau, \ell)$, the
//! product of the values each dimension offers the candidate less the
//! invalid combination (`PhysicalProperties::new` rejects it), each with
//! the reason a prune rule removes it. Each prune rule reads only the value
//! it removes and the values decided above it.

use std::collections::{BTreeSet, HashMap, HashSet};

use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::ir::operator::arguments::Source;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::FieldDataType;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::plan::physical_asap_dag::{
    PhysicalASAPDAG, PhysicalASAPDAGNode, PhysicalASAPNodeId,
};
use asap_types::stages::{
    CandidateForMemoGroupId, Lifetime, Memo, MemoGroupId, PhysicalProperties, PruneReason,
    StorageMedium, ValueSign,
};
use asap_types::summary_type::SketchAlgorithm;
use asap_types::workload::bounds::source_name;
use asap_types::workload::{AssumedDataFact, DataFactAssumption, DataWorkload};

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
///   known bound ([`ingestion_row_order`] is not `Unordered`);
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
            let scanned_sources = &memo.groups()[child.0 as usize]
                .group_properties
                .scanned_sources;
            ingestion_row_order(summary_type, scanned_sources.iter(), data)
                != IngestionRowOrder::Unordered
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
fn summary_build<C: Copy>(operator: &Operator<C>) -> Option<(&FieldDataType, C)> {
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

/// How an ingestion-time build of a summary state gets its rows in the
/// timestamp order it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IngestionRowOrder {
    /// It needs no order (not a counter state), or the data declares how
    /// late a row may arrive (`max_out_of_order_ms`), so a reorder buffer
    /// restores the order.
    Ordered,
    /// A counter state over PromQL samples whose arrival declares no
    /// `max_out_of_order_ms`: the samples are taken to arrive in timestamp
    /// order, an assumed data fact ([`assumed_in_order_arrival`]).
    AssumedOrdered,
    /// A counter state over SQL rows whose arrival declares no
    /// `max_out_of_order_ms`: nothing orders its rows, so it is not offered
    /// at ingestion time.
    Unordered,
}

/// The row order an ingestion-time build of `summary_type` over rows of
/// `scanned_sources` gets: the one rule both the Stage 2 offer
/// ([`offered_physical_properties`]) and the report of what the selected
/// plan assumed ([`assumed_in_order_arrival`]) read. E.g. a `rate` state
/// over the PromQL metric `requests_total` with no `max_out_of_order_ms`
/// is `AssumedOrdered`; over an SQL table it is `Unordered`.
fn ingestion_row_order<'a>(
    summary_type: &FieldDataType,
    mut scanned_sources: impl Iterator<Item = &'a Source>,
    data: &DataWorkload,
) -> IngestionRowOrder {
    let counter_state = matches!(
        summary_type,
        FieldDataType::ExactAggregate(kind, _) if kind.tracks_counter_increase()
    );
    if !counter_state || data.arrival.max_out_of_order_ms().is_some() {
        IngestionRowOrder::Ordered
    } else if scanned_sources.all(Source::is_time_series) {
        IngestionRowOrder::AssumedOrdered
    } else {
        IngestionRowOrder::Unordered
    }
}

/// The data fact `plan` assumed by [`ingestion_row_order`]: each
/// ingestion-time counter state it builds without a declared
/// `max_out_of_order_ms` was offered only because its PromQL samples are
/// taken to arrive in timestamp order. One fact naming the metrics those
/// states read, in name order; none when there is no such state. E.g. an
/// ingestion-time `rate(requests_total[1m])` state gives `InOrderArrival`
/// over `requests_total`. One walk from those states down to their scans,
/// O(nodes + edges); every such state in a selected plan was offered by
/// the same rule, so the sources they scan together are PromQL samples.
pub fn assumed_in_order_arrival(
    plan: &PhysicalASAPDAG,
    data: &DataWorkload,
) -> Option<AssumedDataFact> {
    let nodes: HashMap<PhysicalASAPNodeId, &PhysicalASAPDAGNode> =
        plan.nodes.iter().map(|node| (node.id, node)).collect();
    // The ingestion-time builds whose row order depends on the sources
    // they scan (counter states with no declared `max_out_of_order_ms`,
    // not `Ordered` whatever they scan), and the nodes below them.
    let mut builds = Vec::new();
    let mut below = Vec::new();
    for node in &plan.nodes {
        let Some((summary_type, _)) = summary_build(&node.payload) else {
            continue;
        };
        if node.output_state.timing == ExecutionTiming::IngestionTime
            && ingestion_row_order(summary_type, std::iter::empty(), data)
                != IngestionRowOrder::Ordered
        {
            builds.push(summary_type);
            below.extend(node.producers());
        }
    }
    // The sources their samples come from: the scans below them.
    let mut scanned_sources = BTreeSet::new();
    let mut seen = HashSet::new();
    while let Some(id) = below.pop() {
        if !seen.insert(id) {
            continue;
        }
        let producer = nodes[&id];
        match &producer.payload {
            Operator::NonASAP(NonASAPOp::Scan { source, .. }) => {
                scanned_sources.insert(source);
            }
            _ => below.extend(producer.producers()),
        }
    }
    let assumed = builds.iter().any(|summary_type| {
        ingestion_row_order(summary_type, scanned_sources.iter().copied(), data)
            == IngestionRowOrder::AssumedOrdered
    });
    assumed.then(|| AssumedDataFact {
        sources: scanned_sources
            .into_iter()
            .map(|source| source_name(source).to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        assumption: DataFactAssumption::InOrderArrival,
    })
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
    let weight_sign = memo.groups()[group.0 as usize]
        .group_properties
        .state_column_sign();
    weight_sign != Some(ValueSign::NonNegative)
}
