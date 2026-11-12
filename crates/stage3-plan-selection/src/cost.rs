//! The baseline objective's coefficients (concepts doc, section 3, "Stage
//! 3: baseline selection and plan output"): $w_v$ of every physical
//! candidate and $c_{u,c,i}$ of every edge a plan may select, from
//! `asap-cost` under cost model 1.
//!
//! Each candidate is costed as one plan node ([`candidate_node`],
//! [`asap_cost::node_resource_usage_and_cost`]) over the logical output of each input: the
//! most any costed candidate it may read there (`Memo::producers`)
//! outputs (rows, bytes and, for a state, its family). That is $w_v$, which
//! does not depend on the producers chosen. The part of a consumer's cost
//! that changes with the producer read is on the edge: $c_{u,c,i}$ is the
//! consumer's cost with input $i$ read from $u$'s own output, minus $w_c$.
//! It is exact when a consumer's cost changes with one input's producer
//! only (every consumer of a state reads one state input). Every state
//! candidate has its own θ (a merge, a `TimeRange` over a chain carry the
//! θ of the states they read, and read only producers with it), so a
//! consumer above a sketch build is costed at that build's θ.
//!
//! How often a candidate runs is the baseline's fixed rate: at ingestion
//! time, one second of arriving rows per second; at query time, once per
//! evaluation of the queries reaching its group (once per request when it
//! is materialized across evaluations). A materialized candidate and a
//! query-time `Scan` retain the most their selectable consumers read back
//! ([`read_back_ms`]). Execution contexts make both per context.

use std::collections::HashMap;

use asap_cost::plan::execution_demand::evaluation_rate;
use asap_cost::{
    node_resource_usage_and_cost, raw_data_disk_cost_per_second, CostInputs,
    NodeResourceUsageAndCost, ProducerOutput,
};
use asap_types::ir::operator::Source;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::properties::{DataPrimitive, ExecutionDataState};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::plan::physical_asap_dag::{
    PhysicalASAPDAGNode, PhysicalASAPNodeId, PhysicalASAPOperatorPayload,
};
use asap_types::plan::view_coverage;
use asap_types::stages::{
    Lifetime, Memo, MemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

/// $w_v$ and $c_{u,c,i}$.
pub(crate) struct SelectionCosts {
    /// $w_v$ per second, by [`PhysicalCandidateId`], or why `asap-cost`
    /// could not cost it (the MILP leaves such a candidate out).
    pub(crate) candidate: Vec<Result<f64, String>>,
    /// $c_{u,c,i}$ per second of each selectable edge whose producer and
    /// consumer are costable; an edge missing here cannot be selected.
    pub(crate) edge: HashMap<PhysicalEdgeId, f64>,
    /// By [`PhysicalCandidateId`], for a query-time `Scan`: its source and
    /// the cost per second of keeping the raw data it reads on disk
    /// ([`asap_cost::raw_data_disk_cost_per_second`]). A plan keeps each
    /// source once, for the selected scan that keeps the most, as
    /// `plan_cost` charges it.
    pub(crate) raw_data_on_disk: Vec<Option<(Source, f64)>>,
    /// The runtime's fixed overhead per query request, per second
    /// ([`asap_cost::request_overhead_cost_per_second`]): every plan pays
    /// it.
    pub(crate) request_overhead_cost_per_second: f64,
}

/// Candidate `v` as a plan node with id `id`, its input `i` read from node
/// `input_ids[i]`: its operator, timing, lifetime and θ, its group's schema
/// and, materialized across evaluations, its group's coverage.
/// `retention_ms` is left `None` for the caller to set.
pub(crate) fn candidate_node(
    physical: &PhysicalASAPMemoCandidates,
    v: PhysicalCandidateId,
    id: PhysicalASAPNodeId,
    input_ids: &[PhysicalASAPNodeId],
) -> PhysicalASAPDAGNode {
    let memo = &physical.logical_candidates.memo;
    let candidate = &physical.physical_candidates[v.0];
    let logical = memo.candidate(candidate.logical_candidate_id);
    let group = &memo.groups()[candidate.logical_candidate_id.group.0 as usize].group_properties;
    let mut inputs = input_ids.iter();
    let payload = logical
        .expression
        .operator
        .map_children(|_| *inputs.next().expect("one node per input"));
    let lifetime = candidate.physical_properties.lifetime();
    PhysicalASAPDAGNode {
        id,
        output_state: ExecutionDataState {
            timing: candidate.physical_properties.timing(),
            primitive: DataPrimitive::of(&payload, group.result_kind),
        },
        payload,
        lifetime,
        parameters: logical.logical_properties.parameters().cloned(),
        output_schema: group.schema.clone(),
        retention_ms: None,
        selected_physical_candidate: Some(v),
        view_coverage: matches!(lifetime, Lifetime::AcrossEvaluations(_))
            .then(|| view_coverage(memo, candidate.logical_candidate_id.group))
            .flatten(),
    }
}

/// The number of inputs of candidate `v`.
pub(crate) fn input_count(physical: &PhysicalASAPMemoCandidates, v: PhysicalCandidateId) -> usize {
    physical
        .logical_candidates
        .memo
        .candidate(physical.physical_candidates[v.0].logical_candidate_id)
        .expression
        .operator
        .children()
        .len()
}

/// Whether candidate `v` keeps time-indexed rows for its consumers to read
/// back: materialized across evaluations, or a `Scan` at query time (raw
/// data). Its node carries `retention_ms`.
pub(crate) fn retains_rows(physical: &PhysicalASAPMemoCandidates, v: PhysicalCandidateId) -> bool {
    let candidate = &physical.physical_candidates[v.0];
    let scan = matches!(
        physical
            .logical_candidates
            .memo
            .candidate(candidate.logical_candidate_id)
            .expression
            .operator,
        Operator::NonASAP(NonASAPOp::Scan { .. })
    );
    matches!(
        candidate.physical_properties.lifetime(),
        Lifetime::AcrossEvaluations(_)
    ) || (scan && candidate.physical_properties.timing() == ExecutionTiming::QueryTime)
}

/// How far back from the evaluation time `producer`'s rows are read
/// through `edges` (its edges to consumers; `consumers_of` gives each
/// candidate's edges to its own consumers): [`asap_cost::read_back_ms`]
/// with the longest range-evaluation span of the queries reaching each
/// `TimeRange` when `with_span` (retention), without it (one execution's
/// read of a query-time `Scan`).
pub(crate) fn read_back_ms(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    producer: PhysicalCandidateId,
    edges: &[PhysicalEdgeId],
    consumers_of: &dyn Fn(PhysicalCandidateId) -> Vec<PhysicalEdgeId>,
    with_span: bool,
) -> Option<u64> {
    let memo = &physical.logical_candidates.memo;
    let operator_of = |v: PhysicalCandidateId| {
        &memo
            .candidate(physical.physical_candidates[v.0].logical_candidate_id)
            .expression
            .operator
    };
    let span_ms = |v: PhysicalCandidateId| {
        if !with_span {
            return 0;
        }
        let group = physical.physical_candidates[v.0].logical_candidate_id.group;
        memo.groups()[group.0 as usize]
            .reached_by_queries()
            .iter()
            .filter_map(|&query| entries.get(query)?.range_evaluation)
            .map(|range| i64::try_from(range.span_ms).unwrap_or(i64::MAX))
            .max()
            .unwrap_or(0)
    };
    let readers = |edges: Vec<PhysicalEdgeId>| -> Vec<PhysicalCandidateId> {
        edges
            .into_iter()
            .map(|edge| physical.edge(edge).consumer_candidate_id)
            .collect()
    };
    asap_cost::read_back_ms(
        readers(edges.to_vec()),
        operator_of,
        |v| readers(consumers_of(v)),
        span_ms,
        |v| {
            physical.physical_candidates[v.0]
                .physical_properties
                .timing()
                == ExecutionTiming::QueryTime
        },
        matches!(
            operator_of(producer),
            Operator::NonASAP(NonASAPOp::Scan { .. })
        ),
    )
}

/// [`SelectionCosts`] over `selectable` (each admissible edge a plan may
/// select, by consumer and input). Errs on nothing: a candidate
/// `asap-cost` cannot cost is reported in [`SelectionCosts::candidate`].
pub(crate) fn selection_costs(
    physical: &PhysicalASAPMemoCandidates,
    selectable: &[PhysicalEdgeId],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    cost_inputs: &CostInputs,
    formula_scale: f64,
    request_overhead_cost_per_second: f64,
) -> SelectionCosts {
    let memo = &physical.logical_candidates.memo;
    let count = physical.physical_candidates.len();
    let mut out_edges: Vec<Vec<PhysicalEdgeId>> = vec![Vec::new(); count];
    for &edge in selectable {
        out_edges[physical.edge(edge).producer_candidate_id.0].push(edge);
    }
    // Each candidate's node, its inputs numbered 0.. (the ids do not
    // matter to `node_resource_usage_and_cost`), with the retention it would need.
    let nodes: Vec<PhysicalASAPDAGNode> = (0..count)
        .map(PhysicalCandidateId)
        .map(|v| {
            let inputs: Vec<PhysicalASAPNodeId> = (0..input_count(physical, v)).collect();
            let mut node = candidate_node(physical, v, v.0, &inputs);
            if retains_rows(physical, v) {
                node.retention_ms = read_back_ms(
                    physical,
                    entries,
                    v,
                    &out_edges[v.0],
                    &|consumer| out_edges[consumer.0].clone(),
                    true,
                );
            }
            node
        })
        .collect();
    // Each query-time `Scan`'s read per execution: the most any selectable
    // consumer reads (no range-evaluation span), charged once.
    let scan_read: Vec<Option<u64>> = (0..count)
        .map(PhysicalCandidateId)
        .map(|v| {
            let candidate = &physical.physical_candidates[v.0];
            let scan = matches!(
                memo.candidate(candidate.logical_candidate_id)
                    .expression
                    .operator,
                Operator::NonASAP(NonASAPOp::Scan { .. })
            );
            (scan && candidate.physical_properties.timing() == ExecutionTiming::QueryTime)
                .then(|| {
                    read_back_ms(
                        physical,
                        entries,
                        v,
                        &out_edges[v.0],
                        &|consumer| out_edges[consumer.0].clone(),
                        false,
                    )
                })
                .flatten()
        })
        .collect();
    // Each group's expression as a payload, for its logical output.
    let group_payloads: Vec<PhysicalASAPOperatorPayload> = memo
        .groups()
        .iter()
        .map(|group| group.expression.map_children(|input| input.0 as usize))
        .collect();
    let mut candidates_of_group: Vec<Vec<PhysicalCandidateId>> =
        vec![Vec::new(); memo.groups().len()];
    for (v, candidate) in physical.physical_candidates.iter().enumerate() {
        candidates_of_group[candidate.logical_candidate_id.group.0 as usize]
            .push(PhysicalCandidateId(v));
    }
    let order = memo
        .topological_order()
        .unwrap_or_else(|| (0..memo.groups().len() as u32).map(MemoGroupId).collect());
    let mut usage: Vec<Option<Result<NodeResourceUsageAndCost<'_>, String>>> =
        (0..count).map(|_| None).collect();
    for group in order {
        for &v in &candidates_of_group[group.0 as usize] {
            let inputs = logical_inputs(physical, &group_payloads, &candidates_of_group, &usage, v);
            usage[v.0] = Some(match inputs {
                Some(inputs) => cost_node(
                    physical,
                    v,
                    &nodes[v.0],
                    &inputs,
                    entries,
                    data,
                    cost_inputs,
                    formula_scale,
                    scan_read[v.0],
                ),
                None => Err("an input group has no costable candidate".to_string()),
            });
        }
    }
    let candidate: Vec<Result<f64, String>> = usage
        .iter()
        .map(|usage| match usage {
            Some(Ok(usage)) => cost_per_second(usage),
            Some(Err(error)) => Err(error.clone()),
            None => Err("it reads a group on a cycle of candidates".to_string()),
        })
        .collect();
    let raw_data_on_disk = usage
        .iter()
        .map(|usage| {
            let (source, bytes) = usage.as_ref()?.as_ref().ok()?.raw_scan?;
            let cost = raw_data_disk_cost_per_second(bytes, cost_inputs)?;
            Some((source.clone(), cost))
        })
        .collect();
    let mut edge = HashMap::new();
    for &id in selectable {
        let e = physical.edge(id);
        let (u, c) = (e.producer_candidate_id, e.consumer_candidate_id);
        let (Some(Ok(produced)), Ok(own)) = (&usage[u.0], &candidate[c.0]) else {
            continue;
        };
        let Some(mut inputs) =
            logical_inputs(physical, &group_payloads, &candidates_of_group, &usage, c)
        else {
            continue;
        };
        inputs[e.input_index_of_consumer] = producer_output(&nodes[u.0], produced);
        let with_u = cost_node(
            physical,
            c,
            &nodes[c.0],
            &inputs,
            entries,
            data,
            cost_inputs,
            formula_scale,
            scan_read[c.0],
        )
        .and_then(|usage| cost_per_second(&usage));
        if let Ok(with_u) = with_u {
            edge.insert(id, with_u - own);
        }
    }
    SelectionCosts {
        candidate,
        edge,
        raw_data_on_disk,
        request_overhead_cost_per_second,
    }
}

/// The logical output of each input of `v` over the costed candidates
/// (`usage`) it may read there (`Memo::producers`); `None` when one has
/// none.
fn logical_inputs<'a>(
    physical: &'a PhysicalASAPMemoCandidates,
    group_payloads: &'a [PhysicalASAPOperatorPayload],
    candidates_of_group: &[Vec<PhysicalCandidateId>],
    usage: &[Option<Result<NodeResourceUsageAndCost<'a>, String>>],
    v: PhysicalCandidateId,
) -> Option<Vec<ProducerOutput<'a>>> {
    let memo = &physical.logical_candidates.memo;
    let consumer = physical.physical_candidates[v.0].logical_candidate_id;
    input_groups(memo, physical, v)
        .into_iter()
        .enumerate()
        .map(|(input, group)| {
            let readable = memo.producers(consumer, input);
            logical_output(
                &group_payloads[group.0 as usize],
                &memo.groups()[group.0 as usize].group_properties.schema,
                candidates_of_group[group.0 as usize]
                    .iter()
                    .filter(|u| {
                        readable.contains(&physical.physical_candidates[u.0].logical_candidate_id)
                    })
                    .filter_map(|u| usage[u.0].as_ref()?.as_ref().ok()),
            )
        })
        .collect()
}

/// The input groups of candidate `v`, by input index.
fn input_groups(
    memo: &Memo,
    physical: &PhysicalASAPMemoCandidates,
    v: PhysicalCandidateId,
) -> Vec<MemoGroupId> {
    memo.candidate(physical.physical_candidates[v.0].logical_candidate_id)
        .expression
        .operator
        .children()
        .into_iter()
        .copied()
        .collect()
}

/// What a consumer reads of candidate `node` with `usage`.
fn producer_output<'a>(
    node: &'a PhysicalASAPDAGNode,
    usage: &NodeResourceUsageAndCost<'a>,
) -> ProducerOutput<'a> {
    ProducerOutput {
        payload: &node.payload,
        output_schema: &node.output_schema,
        output: usage.output,
        sources: usage.sources.clone(),
        family: usage.family,
        unchanged_columns: usage.unchanged_columns.clone(),
        tumbling_windows: usage.tumbling_windows,
    }
}

/// An input's logical output over its readable costed candidates' `usages`: the most
/// rows and bytes any outputs, the state family of the one with the most
/// bytes, the sources any reads and the columns every one leaves unchanged;
/// `payload` and `schema` the group's own. `None` without a costed
/// candidate.
fn logical_output<'a: 'b, 'b>(
    payload: &'a PhysicalASAPOperatorPayload,
    schema: &'a asap_types::ir::schema::Schema,
    usages: impl Iterator<Item = &'b NodeResourceUsageAndCost<'a>>,
) -> Option<ProducerOutput<'a>> {
    let mut output: Option<ProducerOutput<'a>> = None;
    for usage in usages {
        let Some(known) = output.as_mut() else {
            output = Some(ProducerOutput {
                payload,
                output_schema: schema,
                output: usage.output,
                sources: usage.sources.clone(),
                family: usage.family,
                unchanged_columns: usage.unchanged_columns.clone(),
                tumbling_windows: usage.tumbling_windows,
            });
            continue;
        };
        if usage.output.bytes > known.output.bytes {
            known.family = usage.family;
        }
        known.output.rows = known.output.rows.max(usage.output.rows);
        known.output.bytes = known.output.bytes.max(usage.output.bytes);
        known.sources.extend(usage.sources.iter().copied());
        known
            .unchanged_columns
            .retain(|column| usage.unchanged_columns.contains(column));
    }
    output
}

/// Candidate `v`'s usage as `node` over `inputs`, at its rate (module doc);
/// a query-time `Scan` reads `scan_read_ms` per execution.
#[allow(clippy::too_many_arguments)]
fn cost_node<'a>(
    physical: &PhysicalASAPMemoCandidates,
    v: PhysicalCandidateId,
    node: &'a PhysicalASAPDAGNode,
    inputs: &[ProducerOutput<'a>],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    cost_inputs: &'a CostInputs,
    formula_scale: f64,
    scan_read_ms: Option<u64>,
) -> Result<NodeResourceUsageAndCost<'a>, String> {
    let memo = &physical.logical_candidates.memo;
    let candidate = &physical.physical_candidates[v.0];
    let group = candidate.logical_candidate_id.group;
    let ingestion = candidate.physical_properties.timing() == ExecutionTiming::IngestionTime;
    let per_second = match ingestion {
        true => 1.0,
        false => {
            let per_request = matches!(
                candidate.physical_properties.lifetime(),
                Lifetime::AcrossEvaluations(_)
            );
            evaluation_rate(
                memo.groups()[group.0 as usize]
                    .reached_by_queries()
                    .iter()
                    .filter_map(|&query| entries.get(query))
                    .map(|entry| {
                        (
                            &entry.recurrence,
                            (!per_request).then_some(entry.range_evaluation).flatten(),
                        )
                    }),
                cost_inputs.non_repeating_query_horizon_seconds,
            )
            .map_err(|error| error.to_string())?
        }
    };
    let window_ms = match (&node.payload, ingestion) {
        (_, false) => scan_read_ms,
        (
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }),
            true,
        ) => Some(*tumbling_window_width_ms),
        (_, true) => window_ms_below(memo, group),
    };
    node_resource_usage_and_cost(
        node,
        inputs,
        (1, per_second),
        window_ms,
        data,
        cost_inputs,
        formula_scale,
    )
    .map_err(|error| error.to_string())
}

/// $w_v$ from a candidate's usage: its cost per second under cost model 1
/// (the raw data a query-time scan keeps on disk is charged per source,
/// [`SelectionCosts::raw_data_on_disk`]).
fn cost_per_second(usage: &NodeResourceUsageAndCost<'_>) -> Result<f64, String> {
    let cost = usage
        .cost
        .cost_per_second
        .ok_or("the selection MILP costs under cost model 1 only")?;
    match cost.is_finite() && cost >= 0.0 {
        true => Ok(cost),
        false => Err(format!(
            "its cost per second {cost} is not a finite non-negative number"
        )),
    }
}

/// The window, in ms, of the first range below `group`'s operator,
/// through filters, projections and time shifts: what an ingestion-time
/// grouping maintains (what asap-cost reads below a plan node, over the memo's
/// groups, since a candidate is costed before any plan exists).
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
