//! $w_v$: the fixed cost per second of each physical candidate under cost
//! model 1, from `asap-cost` (concepts doc, section 3, "Stage 3: baseline
//! selection and realization"). Each candidate is priced as one node of a
//! plan ([`asap_cost::node_usage`]) over one representative producer per
//! input, at a fixed number of computations per second:
//!
//! - at ingestion time, over one second of arriving rows;
//! - at query time, materialized across evaluations, once per request of
//!   the queries reaching its group (a materialized tumbling-window chain
//!   builds only its newest window per request);
//! - at query time otherwise, once per evaluation of each schedule of the
//!   queries reaching its group (`asap_cost::execution_demand::evaluation_rate`).
//!
//! It is the baseline's fixed-cost assumption: a candidate's cost does not
//! depend on which consumers select it. It is exact only when that holds;
//! execution contexts (#1207 PR 7) make the cost per context, and the
//! retention amount $h_v$ the cost of an `AcrossEvaluations` candidate per
//! selected consumer. The cost includes the candidate's own computation and
//! the raw data a query-time scan keeps on disk; the memory a query-time
//! result holds while its evaluation runs is left out, since it depends on
//! the latency of the queries reading it.

use std::collections::BTreeSet;

use asap_cost::execution_demand::evaluation_rate;
use asap_cost::{node_usage, CostInputs, CostModel, NodeUsage, ProducerOutput};
use asap_types::ir::physical_export::{PhysicalASAPDAGNode, ScanWindow};
use asap_types::ir::properties::{DataPrimitive, ExecutionDataState};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorResultKind};
use asap_types::physical::{ExecutionTiming, Lifetime};
use asap_types::stages::{
    Memo, MemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdge, WindowLayout,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

/// $w_v$ of every physical candidate, by [`PhysicalCandidateId`]: its cost
/// per second, or why `asap-cost` could not cost it (the selection MILP
/// leaves such a candidate out). A candidate one of whose inputs has no
/// costable producer is not costable either.
pub(crate) fn candidate_costs(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    cost_inputs: &CostInputs,
    formula_scale: f64,
) -> Vec<Result<f64, String>> {
    let memo = &physical.logical_candidates.memo;
    let count = physical.physical_candidates.len();
    let edges: Vec<PhysicalEdge> = physical.edges().map(|(_, edge)| edge).collect();
    let input_count = |id: usize| {
        memo.candidate(physical.physical_candidates[id].logical_candidate_id)
            .expression
            .operator
            .children()
            .len()
    };
    // A representative producer per input, chosen among costed candidates
    // only, so the choices never form a cycle: each round costs the
    // candidates whose every input has a costed admissible producer.
    let mut representative: Vec<Option<Vec<usize>>> = vec![None; count];
    let mut costs: Vec<Option<Result<f64, String>>> = vec![None; count];
    let mut nodes: Vec<Option<PhysicalASAPDAGNode>> = vec![None; count];
    let mut order: Vec<usize> = Vec::new();
    loop {
        let mut progressed = false;
        for id in 0..count {
            if costs[id].is_some() {
                continue;
            }
            let producers: Option<Vec<usize>> = (0..input_count(id))
                .map(|input| {
                    physical
                        .admissible_edges_into(PhysicalCandidateId(id), input)
                        .iter()
                        .map(|&edge| edges[edge.0].producer_candidate_id.0)
                        .find(|&producer| matches!(costs[producer], Some(Ok(_))))
                })
                .collect();
            let Some(producers) = producers else {
                continue;
            };
            let node = candidate_node(physical, id, &producers);
            nodes[id] = Some(node);
            representative[id] = Some(producers);
            // Placeholder until the usage pass below; marks it costable for
            // the choice of its consumers' representatives.
            costs[id] = Some(Ok(0.0));
            order.push(id);
            progressed = true;
        }
        if !progressed {
            break;
        }
    }
    let nodes: Vec<Option<PhysicalASAPDAGNode>> = nodes;
    let mut usages: Vec<Option<NodeUsage<'_>>> = (0..count).map(|_| None).collect();
    let mut result: Vec<Result<f64, String>> = (0..count)
        .map(|_| Err("one of its inputs has no producer that can be costed".to_string()))
        .collect();
    for &id in &order {
        let node = nodes[id]
            .as_ref()
            .expect("built with its representative producers");
        let producers: Vec<usize> = representative[id].clone().unwrap_or_default();
        // A tumbling-window chain reads the input below its range: one
        // level further down its range's representative producer.
        let producers: Vec<usize> = match &node.payload {
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }) => producers
                .iter()
                .map(|&range| {
                    representative[range]
                        .as_ref()
                        .map_or(range, |below| below[0])
                })
                .collect(),
            _ => producers,
        };
        let Some(producer_outputs) = producers
            .iter()
            .map(|&producer| {
                let usage = usages[producer].as_ref()?;
                let producer_node = nodes[producer].as_ref()?;
                Some(ProducerOutput {
                    payload: &producer_node.payload,
                    output_schema: &producer_node.output_schema,
                    output: usage.output,
                    sources: usage.sources.clone(),
                    family: usage.family,
                    unchanged_columns: usage.unchanged_columns.clone(),
                })
            })
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let candidate = &physical.physical_candidates[id];
        let group = candidate.logical_candidate_id.group;
        let per_second = match computations_per_second(memo, group, candidate, entries, cost_inputs)
        {
            Ok(per_second) => per_second,
            Err(error) => {
                result[id] = Err(error);
                continue;
            }
        };
        let window_ms = window_ms_below(memo, group);
        let usage = node_usage(
            node,
            &producer_outputs,
            (1, per_second),
            scan_window_of(memo, group),
            asap_cost::tumbling_windows_of(node),
            (candidate.physical_properties.timing() == ExecutionTiming::IngestionTime)
                .then_some(window_ms)
                .flatten(),
            data,
            cost_inputs,
            formula_scale,
        );
        match usage {
            Ok(usage) => {
                result[id] = cost_per_second(&usage, cost_inputs);
                usages[id] = Some(usage);
            }
            Err(error) => result[id] = Err(error.to_string()),
        }
    }
    result
}

/// Candidate `id` as one exported plan node over `producers` (its input
/// `i` read from the node of candidate `producers[i]`): its operator, or
/// for a tumbling-window layout the chain the realizer builds; its timing,
/// lifetime and θ; its group's schema.
fn candidate_node(
    physical: &PhysicalASAPMemoCandidates,
    id: usize,
    producers: &[usize],
) -> PhysicalASAPDAGNode {
    let memo = &physical.logical_candidates.memo;
    let candidate = &physical.physical_candidates[id];
    let logical = memo.candidate(candidate.logical_candidate_id);
    let group = &memo.groups()[candidate.logical_candidate_id.group.0 as usize];
    let mut next = producers.iter();
    let operator = logical
        .expression
        .operator
        .map_children(|_| *next.next().expect("one producer per input"));
    let payload = match (logical.logical_properties.window_layout(), operator) {
        (
            Some(&WindowLayout::Tumbling { width_ms }),
            Operator::ASAP(ASAPOp::SummaryAgg {
                child,
                family,
                input,
                reduction,
                grouping,
                filter,
            }),
        ) => {
            let window_ms =
                window_ms_below(memo, candidate.logical_candidate_id.group).unwrap_or(width_ms);
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                child,
                family,
                input,
                reduction,
                grouping,
                filter,
                tumbling_window_width_ms: width_ms,
                tumbling_window_count: u32::try_from(window_ms / width_ms.max(1))
                    .unwrap_or(u32::MAX)
                    .max(1),
            })
        }
        (_, operator) => operator,
    };
    // As the realizer's export sets it: a summary state or a maintained
    // population is state, everything else raw values.
    let primitive = match &payload {
        Operator::ASAP(
            ASAPOp::SummaryAgg { .. }
            | ASAPOp::SummaryTumblingWindowChain { .. }
            | ASAPOp::SummaryJoin { .. }
            | ASAPOp::MaintainPopulation { .. },
        ) => DataPrimitive::SummaryState,
        _ if group.group_properties.result_kind == OperatorResultKind::State => {
            DataPrimitive::SummaryState
        }
        _ => DataPrimitive::Raw,
    };
    PhysicalASAPDAGNode {
        id,
        payload,
        output_state: ExecutionDataState {
            timing: candidate.physical_properties.timing(),
            primitive,
        },
        lifetime: candidate.physical_properties.lifetime(),
        parameters: logical.logical_properties.parameters().cloned(),
        output_schema: group.group_properties.schema.clone(),
        guarantee: None,
        retention_ms: None,
        summary_coverage: None,
    }
}

/// How often `candidate` of `group` is computed per second (module doc).
fn computations_per_second(
    memo: &Memo,
    group: MemoGroupId,
    candidate: &asap_types::stages::PhysicalCandidate,
    entries: &[QueryWorkloadEntry],
    cost_inputs: &CostInputs,
) -> Result<f64, String> {
    let properties = candidate.physical_properties;
    if properties.timing() == ExecutionTiming::IngestionTime {
        return Ok(1.0);
    }
    let per_request = matches!(properties.lifetime(), Lifetime::AcrossEvaluations(_));
    evaluation_rate(
        memo.groups()[group.0 as usize]
            .queries
            .iter()
            .filter_map(|&query| entries.get(query))
            .map(|entry| {
                (
                    &entry.recurrence,
                    if per_request {
                        None
                    } else {
                        entry.range_evaluation
                    },
                )
            }),
        cost_inputs.non_repeating_query_horizon_seconds,
    )
    .map_err(|error| error.to_string())
}

/// $w_v$ from a candidate's usage: its cost per second under cost model 1
/// plus the raw data a query-time scan keeps on disk.
fn cost_per_second(usage: &NodeUsage<'_>, cost_inputs: &CostInputs) -> Result<f64, String> {
    let CostModel::UsageOverTime(unit_cost) = &cost_inputs.cost_model else {
        return Err("the selection MILP prices under cost model 1 only".into());
    };
    let own = usage
        .cost
        .cost_per_second
        .ok_or("cost model 1 gives every node a cost per second")?;
    let raw_data = usage.raw_scan.map_or(0.0, |(_, bytes)| {
        unit_cost.cost_per_disk_byte_second * bytes as f64
    });
    let cost = own + raw_data;
    if cost.is_finite() && cost >= 0.0 {
        Ok(cost)
    } else {
        Err(format!(
            "its cost per second {cost} is not a finite non-negative number"
        ))
    }
}

/// The rows a query-time `Scan` of `group` reads, back from the evaluation
/// time: the union of the windows of the ranges over it, through time
/// shifts without an `@` anchor (`PhysicalASAPDAG::scan_window` on the
/// memo's groups instead of a plan's nodes). `Unbounded` when another
/// operator reads it or nothing does.
fn scan_window_of(memo: &Memo, group: MemoGroupId) -> ScanWindow {
    fn windows(
        memo: &Memo,
        group: MemoGroupId,
        end_offset_ms: i64,
        seen: &mut BTreeSet<MemoGroupId>,
    ) -> ScanWindow {
        if !seen.insert(group) {
            return ScanWindow::Unbounded;
        }
        let parents = &memo.groups()[group.0 as usize].parents;
        let mut union: Option<ScanWindow> = None;
        for &parent in parents {
            let window = match &memo.groups()[parent.0 as usize].expression {
                Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) => ScanWindow::Within {
                    end_offset_ms,
                    width_ms: i64::try_from(range.as_millis()).unwrap_or(i64::MAX),
                },
                Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) if shift.at.is_none() => {
                    windows(
                        memo,
                        parent,
                        end_offset_ms.saturating_add(shift.offset_ms),
                        seen,
                    )
                }
                _ => ScanWindow::Unbounded,
            };
            union = Some(match union {
                None => window,
                Some(known) => known.union(window),
            });
        }
        union.unwrap_or(ScanWindow::Unbounded)
    }
    windows(memo, group, 0, &mut BTreeSet::new())
}

/// The window, in ms, of the first range below `group`'s operator,
/// through filters, projections and time shifts
/// (`asap_cost::maintained_window_ms` on the memo's groups): what an
/// ingestion-time grouping maintains, and a tumbling-window chain covers.
fn window_ms_below(memo: &Memo, group: MemoGroupId) -> Option<u64> {
    let range_ms = |range: &std::time::Duration| u64::try_from(range.as_millis()).ok();
    let expression = &memo.groups()[group.0 as usize].expression;
    if let Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) = expression {
        return range_ms(range);
    }
    let mut below = **expression.children().first()?;
    for _ in 0..memo.groups().len() {
        let expression = &memo.groups()[below.0 as usize].expression;
        match expression {
            Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) => return range_ms(range),
            Operator::NonASAP(
                NonASAPOp::Filter { .. } | NonASAPOp::Project { .. } | NonASAPOp::TimeShift { .. },
            ) => below = **expression.children().first()?,
            _ => return None,
        }
    }
    None
}
