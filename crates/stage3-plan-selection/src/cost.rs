//! The baseline objective's coefficients (concepts doc, section 3, "Stage
//! 3: baseline selection and plan output"): $w_v$ of every physical
//! candidate and $c_{u,c,i}$ of every edge a plan may select, from
//! `asap-cost` under cost model 1.
//!
//! Each candidate is costed as one plan node ([`candidate_node`],
//! [`asap_cost::node_usage`]) over the logical output of each input group:
//! the most any of the group's costed candidates outputs (rows, bytes and,
//! for a state, its θ). That is $w_v$, which does not depend on the
//! producers chosen. The part of a consumer's cost that changes with the
//! producer read is on the edge: $c_{u,c,i}$ is the consumer's cost with
//! input $i$ read from $u$'s own output, minus $w_c$. It is exact when a
//! consumer's cost changes with one input's producer only (every consumer
//! of a state reads one state input); a state candidate without θ of its
//! own (a merge, a `TimeRange` over a chain) outputs the most of its input
//! group's θ, so a consumer two hops above a sketch build is costed at the
//! largest θ below it.
//!
//! How often a candidate runs is the baseline's fixed rate: at ingestion
//! time, one second of arriving rows per second; at query time, once per
//! evaluation of the queries reaching its group (once per request when it
//! is materialized across evaluations). A materialized candidate and a
//! query-time `Scan` retain the most their selectable consumers read back
//! ([`retention_need_ms`]). Execution contexts (#1207 PR 7) make both per
//! context.

use std::collections::{BTreeSet, HashMap};

use asap_cost::execution_demand::evaluation_rate;
use asap_cost::{node_usage, CostInputs, CostModel, NodeUsage, ProducerOutput};
use asap_types::ir::materialized_view::ViewCoverage;
use asap_types::ir::physical_export::{
    PhysicalASAPDAGNode, PhysicalASAPNodeId, PhysicalASAPOperatorPayload,
};
use asap_types::ir::properties::{DataPrimitive, ExecutionDataState};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::physical::{ExecutionTiming, Lifetime};
use asap_types::stages::{
    Memo, MemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId,
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
        guarantee: None,
        retention_ms: None,
        summary_coverage: matches!(lifetime, Lifetime::AcrossEvaluations(_))
            .then(|| group.aggregate_coverage.as_ref().map(ViewCoverage::from))
            .flatten(),
        selected_physical_candidate: Some(v),
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
/// through `edges` (its edges to consumers): the most, over them, of the
/// offset plus range of the `TimeShift`s and the `TimeRange` above it on
/// the consumers' side (`consumers_of` gives each candidate's edges to its
/// own consumers), plus the longest range-evaluation span of the queries
/// reaching the `TimeRange`; a tumbling-window chain reads its newest
/// window back. A consumer that reads it otherwise reads it at the
/// evaluation time (only the span), except a `Scan`, which it reads whole:
/// `None`, unbounded, as for a `TimeShift` anchored with `@`.
pub(crate) fn retention_need_ms(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    producer: PhysicalCandidateId,
    edges: &[PhysicalEdgeId],
    consumers_of: &dyn Fn(PhysicalCandidateId) -> Vec<PhysicalEdgeId>,
) -> Option<u64> {
    let memo = &physical.logical_candidates.memo;
    let operator_of = |v: PhysicalCandidateId| {
        &memo
            .candidate(physical.physical_candidates[v.0].logical_candidate_id)
            .expression
            .operator
    };
    let span_ms = |v: PhysicalCandidateId| {
        let group = physical.physical_candidates[v.0].logical_candidate_id.group;
        memo.groups()[group.0 as usize]
            .reached_by_queries()
            .iter()
            .filter_map(|&query| entries.get(query)?.range_evaluation)
            .map(|range| i64::try_from(range.span_ms).unwrap_or(i64::MAX))
            .max()
            .unwrap_or(0)
    };
    let reads_raw_data = matches!(
        operator_of(producer),
        Operator::NonASAP(NonASAPOp::Scan { .. })
    );
    let mut need_ms: i64 = 0;
    // (consumer, offset of the shifts below it), walked up through shifts.
    let mut pending: Vec<(PhysicalCandidateId, i64)> = edges
        .iter()
        .map(|&edge| (physical.edge(edge).consumer_candidate_id, 0))
        .collect();
    let mut seen = BTreeSet::new();
    while let Some((consumer, offset_ms)) = pending.pop() {
        if !seen.insert((consumer, offset_ms)) {
            continue;
        }
        let read_ms = match operator_of(consumer) {
            Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) => {
                let range_ms = i64::try_from(range.as_millis()).ok()?;
                offset_ms
                    .checked_add(range_ms)?
                    .checked_add(span_ms(consumer))?
            }
            Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) if shift.at.is_none() => {
                let offset_ms = offset_ms.checked_add(shift.offset_ms)?;
                pending.extend(
                    consumers_of(consumer)
                        .into_iter()
                        .map(|edge| (physical.edge(edge).consumer_candidate_id, offset_ms)),
                );
                continue;
            }
            Operator::NonASAP(NonASAPOp::TimeShift { .. }) => return None,
            // A chain builds its newest window from it.
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => offset_ms.checked_add(i64::try_from(*tumbling_window_width_ms).ok()?)?,
            _ if reads_raw_data => return None,
            _ => offset_ms.checked_add(span_ms(consumer))?,
        };
        need_ms = need_ms.max(read_ms);
    }
    u64::try_from(need_ms).ok()
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
) -> SelectionCosts {
    let memo = &physical.logical_candidates.memo;
    let count = physical.physical_candidates.len();
    let mut out_edges: Vec<Vec<PhysicalEdgeId>> = vec![Vec::new(); count];
    for &edge in selectable {
        out_edges[physical.edge(edge).producer_candidate_id.0].push(edge);
    }
    // Each candidate's node, its inputs numbered 0.. (the ids do not
    // matter to `node_usage`), with the retention it would need.
    let nodes: Vec<PhysicalASAPDAGNode> = (0..count)
        .map(PhysicalCandidateId)
        .map(|v| {
            let inputs: Vec<PhysicalASAPNodeId> = (0..input_count(physical, v)).collect();
            let mut node = candidate_node(physical, v, v.0, &inputs);
            if retains_rows(physical, v) {
                node.retention_ms =
                    retention_need_ms(physical, entries, v, &out_edges[v.0], &|consumer| {
                        out_edges[consumer.0].clone()
                    });
            }
            node
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
    let mut usage: Vec<Option<Result<NodeUsage<'_>, String>>> = (0..count).map(|_| None).collect();
    let mut logical: Vec<Option<ProducerOutput<'_>>> = vec![None; memo.groups().len()];
    for group in order {
        for &v in &candidates_of_group[group.0 as usize] {
            let inputs: Option<Vec<ProducerOutput<'_>>> = input_groups(memo, physical, v)
                .iter()
                .map(|input| logical[input.0 as usize].clone())
                .collect();
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
                ),
                None => Err("an input group has no costable candidate".to_string()),
            });
        }
        logical[group.0 as usize] = logical_output(
            &group_payloads[group.0 as usize],
            &memo.groups()[group.0 as usize].group_properties.schema,
            candidates_of_group[group.0 as usize]
                .iter()
                .filter_map(|v| usage[v.0].as_ref()?.as_ref().ok()),
        );
    }
    let candidate: Vec<Result<f64, String>> = usage
        .iter()
        .map(|usage| match usage {
            Some(Ok(usage)) => cost_per_second(usage, cost_inputs),
            Some(Err(error)) => Err(error.clone()),
            None => Err("it reads a group on a cycle of candidates".to_string()),
        })
        .collect();
    let mut edge = HashMap::new();
    for &id in selectable {
        let e = physical.edge(id);
        let (u, c) = (e.producer_candidate_id, e.consumer_candidate_id);
        let (Some(Ok(produced)), Ok(own)) = (&usage[u.0], &candidate[c.0]) else {
            continue;
        };
        let Some(mut inputs) = input_groups(memo, physical, c)
            .iter()
            .map(|input| logical[input.0 as usize].clone())
            .collect::<Option<Vec<_>>>()
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
        )
        .and_then(|usage| cost_per_second(&usage, cost_inputs));
        if let Ok(with_u) = with_u {
            edge.insert(id, with_u - own);
        }
    }
    SelectionCosts { candidate, edge }
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
fn producer_output<'a>(node: &'a PhysicalASAPDAGNode, usage: &NodeUsage<'a>) -> ProducerOutput<'a> {
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

/// A group's logical output over its costed candidates' `usages`: the most
/// rows and bytes any outputs, the state family of the one with the most
/// bytes, the sources any reads and the columns every one leaves unchanged;
/// `payload` and `schema` the group's own. `None` without a costed
/// candidate.
fn logical_output<'a: 'b, 'b>(
    payload: &'a PhysicalASAPOperatorPayload,
    schema: &'a asap_types::ir::schema::Schema,
    usages: impl Iterator<Item = &'b NodeUsage<'a>>,
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

/// Candidate `v`'s usage as `node` over `inputs`, at its rate (module doc).
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
) -> Result<NodeUsage<'a>, String> {
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
    let ingestion_window_ms = match (&node.payload, ingestion) {
        (_, false) => None,
        (
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }),
            true,
        ) => Some(*tumbling_window_width_ms),
        (_, true) => window_ms_below(memo, group),
    };
    node_usage(
        node,
        inputs,
        (1, per_second),
        ingestion_window_ms,
        data,
        cost_inputs,
        formula_scale,
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
