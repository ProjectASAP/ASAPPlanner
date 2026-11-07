//! $w_v$: the fixed cost per second of each physical candidate under cost
//! model 1, from `asap-cost` (concepts doc, section 3, "Stage 3: baseline
//! selection and realization"). Each candidate is costed as one node of a
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
//! the raw data a query-time scan retains on disk; the memory a query-time
//! result holds while its evaluation runs is left out, since it depends on
//! the latency of the queries reading it.

use std::collections::BTreeSet;

use asap_cost::execution_demand::evaluation_rate;
use asap_cost::{node_usage, CostInputs, CostModel, NodeUsage, ProducerOutput, StateFamily};
use asap_types::ir::physical_export::{PhysicalASAPDAGNode, ScanWindow};
use asap_types::ir::properties::{DataPrimitive, ExecutionDataState};
use asap_types::ir::schema::{ExactKind, SketchParams};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorResultKind, TimeRangeKind};
use asap_types::physical::{ExecutionTiming, Lifetime};
use asap_types::stages::{
    Memo, MemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdge, WindowLayout,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

/// $w_v$ of every physical candidate, by [`PhysicalCandidateId`]: its cost
/// per second, or why `asap-cost` could not cost it (the selection MILP
/// leaves such a candidate out). Candidates are costed producers first
/// (Kahn's order over the admissible edges): a candidate is costed once
/// each of its inputs has a costed producer, and its representative
/// producer per input is the first costed one, so the representatives
/// never form a cycle. A candidate one of whose inputs never gets a
/// costed producer is not costable either.
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
    let mut result: Vec<Result<f64, String>> = (0..count)
        .map(|_| Err("one of its inputs has no producer that can be costed".to_string()))
        .collect();
    let mut costed = vec![false; count];
    // Per candidate, its inputs that have no costed producer yet.
    let mut input_without_producer: Vec<Vec<bool>> =
        (0..count).map(|id| vec![true; input_count(id)]).collect();
    let mut missing: Vec<usize> = (0..count).map(input_count).collect();
    let mut ready: Vec<usize> = (0..count).filter(|&id| missing[id] == 0).collect();
    // Costed nodes and usages, in costing order, by candidate.
    let mut representative: Vec<Vec<usize>> = vec![Vec::new(); count];
    let mut node_of: Vec<Option<usize>> = vec![None; count];
    let mut nodes: Vec<PhysicalASAPDAGNode> = Vec::new();
    let mut usage_outputs: Vec<UsageOutput> = Vec::new();
    while let Some(id) = ready.pop() {
        let producers: Vec<usize> = (0..input_count(id))
            .map(|input| {
                physical
                    .admissible_edges_into(PhysicalCandidateId(id), input)
                    .iter()
                    .map(|&edge| edges[edge.0].producer_candidate_id.0)
                    .find(|&producer| costed[producer])
                    .expect("ready: every input has a costed producer")
            })
            .collect();
        let costing = cost_candidate(
            physical,
            id,
            &producers,
            &representative,
            &node_of,
            &nodes,
            &usage_outputs,
            entries,
            data,
            cost_inputs,
            formula_scale,
        );
        representative[id] = producers;
        let (node, usage) = match costing {
            Ok(costed_node) => costed_node,
            Err(error) => {
                result[id] = Err(error);
                continue;
            }
        };
        result[id] = Ok(usage.cost_per_second);
        node_of[id] = Some(nodes.len());
        nodes.push(node);
        usage_outputs.push(usage);
        costed[id] = true;
        for &edge in physical.admissible_edges_out_of(PhysicalCandidateId(id)) {
            let edge = &edges[edge.0];
            let (consumer, input) = (edge.consumer_candidate_id.0, edge.input_index_of_consumer);
            if std::mem::take(&mut input_without_producer[consumer][input]) {
                missing[consumer] -= 1;
                if missing[consumer] == 0 {
                    ready.push(consumer);
                }
            }
        }
    }
    result
}

/// What a costed candidate's consumers read of it ([`ProducerOutput`]
/// without its borrowed node), and its $w_v$.
struct UsageOutput {
    output: asap_cost::EdgeStatistics,
    sources: BTreeSet<String>,
    family: OwnedStateFamily,
    unchanged_columns: BTreeSet<String>,
    cost_per_second: f64,
}

/// Cost candidate `id` over `producers` (its input `i` read from candidate
/// `producers[i]`, already costed: `nodes[node_of[u]]` and
/// `usage_outputs[node_of[u]]`): its node and what its consumers read of it.
#[allow(clippy::too_many_arguments)]
fn cost_candidate(
    physical: &PhysicalASAPMemoCandidates,
    id: usize,
    producers: &[usize],
    representative: &[Vec<usize>],
    node_of: &[Option<usize>],
    nodes: &[PhysicalASAPDAGNode],
    usage_outputs: &[UsageOutput],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    cost_inputs: &CostInputs,
    formula_scale: f64,
) -> Result<(PhysicalASAPDAGNode, UsageOutput), String> {
    let memo = &physical.logical_candidates.memo;
    let node = candidate_node(physical, id, producers)?;
    // A tumbling-window chain reads the input below its range: the range's
    // own representative producer, as the realizer builds it.
    let producers: Vec<usize> = match &node.payload {
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }) => producers
            .iter()
            .map(|&range| representative[range].first().copied().unwrap_or(range))
            .collect(),
        _ => producers.to_vec(),
    };
    let producer_outputs: Vec<ProducerOutput<'_>> = producers
        .iter()
        .map(|&producer| {
            let index = node_of[producer].expect("costed");
            let output = &usage_outputs[index];
            ProducerOutput {
                payload: &nodes[index].payload,
                output_schema: &nodes[index].output_schema,
                output: output.output,
                sources: output.sources.iter().map(String::as_str).collect(),
                family: match &output.family {
                    OwnedStateFamily::None => StateFamily::None,
                    OwnedStateFamily::Sketch(parameters) => StateFamily::Sketch(parameters),
                    OwnedStateFamily::Exact(kind) => StateFamily::Exact(kind),
                },
                unchanged_columns: output
                    .unchanged_columns
                    .iter()
                    .map(String::as_str)
                    .collect(),
            }
        })
        .collect();
    let candidate = &physical.physical_candidates[id];
    let group = candidate.logical_candidate_id.group;
    let per_second = computations_per_second(memo, group, candidate, entries, cost_inputs)?;
    let usage = node_usage(
        &node,
        &producer_outputs,
        (1, per_second),
        scan_window_of(memo, group),
        asap_cost::tumbling_windows_of(&node),
        (candidate.physical_properties.timing() == ExecutionTiming::IngestionTime)
            .then(|| window_ms_below(memo, group))
            .flatten(),
        data,
        cost_inputs,
        formula_scale,
    )
    .map_err(|error| error.to_string())?;
    let cost_per_second = cost_per_second(&usage, cost_inputs)?;
    let output = UsageOutput {
        output: usage.output,
        sources: usage.sources.iter().map(|s| s.to_string()).collect(),
        family: match usage.family {
            StateFamily::None => OwnedStateFamily::None,
            StateFamily::Sketch(parameters) => OwnedStateFamily::Sketch(parameters.clone()),
            StateFamily::Exact(kind) => OwnedStateFamily::Exact(*kind),
        },
        unchanged_columns: usage
            .unchanged_columns
            .iter()
            .map(|s| s.to_string())
            .collect(),
        cost_per_second,
    };
    Ok((node, output))
}

/// [`StateFamily`] owned, so a costed candidate outlives its node's borrow.
enum OwnedStateFamily {
    None,
    Sketch(SketchParams),
    Exact(ExactKind),
}

/// Candidate `id` as one exported plan node over `producers` (its input
/// `i` read from the node of candidate `producers[i]`): its operator, or
/// for a tumbling-window layout the chain the realizer builds; its timing,
/// lifetime and θ; its group's schema. Err for a tumbling-window layout the
/// realizer would refuse: not on a build over a range whose window its width
/// divides.
fn candidate_node(
    physical: &PhysicalASAPMemoCandidates,
    id: usize,
    producers: &[usize],
) -> Result<PhysicalASAPDAGNode, String> {
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
            let range_input = match &logical.expression.operator {
                Operator::ASAP(ASAPOp::SummaryAgg { child, .. }) => *child,
                _ => unreachable!("matched a build"),
            };
            let window_ms = match &memo.groups()[range_input.0 as usize].expression {
                Operator::NonASAP(NonASAPOp::TimeRange {
                    range,
                    kind: TimeRangeKind::Range,
                    ..
                }) => u64::try_from(range.as_millis()).unwrap_or(u64::MAX),
                _ => {
                    return Err(
                        "a tumbling-window layout on a build that does not read a range".into(),
                    )
                }
            };
            if width_ms == 0 || window_ms % width_ms != 0 {
                return Err(format!(
                    "tumbling windows {width_ms} ms wide do not divide the range of {window_ms} ms"
                ));
            }
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                child,
                family,
                input,
                reduction,
                grouping,
                filter,
                tumbling_window_width_ms: width_ms,
                tumbling_window_count: u32::try_from(window_ms / width_ms)
                    .map_err(|_| "too many tumbling windows".to_string())?,
            })
        }
        (Some(_), _) => {
            return Err("a tumbling-window layout on an operator other than a summary build".into())
        }
        (None, operator) => operator,
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
    Ok(PhysicalASAPDAGNode {
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
    })
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
/// plus the raw data a query-time scan retains on disk.
fn cost_per_second(usage: &NodeUsage<'_>, cost_inputs: &CostInputs) -> Result<f64, String> {
    let CostModel::UsageOverTime(unit_cost) = &cost_inputs.cost_model else {
        return Err("the selection MILP costs under cost model 1 only".into());
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
