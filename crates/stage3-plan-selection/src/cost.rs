//! The baseline objective's coefficients
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! section 3, "Stage
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
//! ([`retention_need_ms`]). Execution contexts make both per context.

use std::collections::{HashMap, HashSet};

use asap_cost::plan::execution_demand::executions_per_second;
use asap_cost::{
    maintained_window_ms, node_resource_usage_and_cost, raw_data_disk_cost_per_second, CostInputs,
    NodeResourceUsageAndCost, ProducerOutput,
};
use asap_types::ir::operator::Source;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{NonASAPOp, Operator};
use asap_types::plan::physical_asap_dag::{
    PhysicalASAPDAGNode, PhysicalASAPNodeId, PhysicalASAPOperatorPayload,
};
use asap_types::stages::{
    Memo, MemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::plan::candidate_node;
use crate::retention::{retention_need_ms, scan_read_ms};

/// $w_v$ and $c_{u,c,i}$.
pub(crate) struct SelectionCosts {
    /// $w_v$, cost per second, by [`PhysicalCandidateId`], or why
    /// `asap-cost` could not cost it (the MILP leaves such a candidate out).
    pub(crate) candidate_cost_per_second: Vec<Result<f64, String>>,
    /// $c_{u,c,i}$, the change of its consumer's cost per second, of each
    /// selectable edge whose producer and consumer are costable; an edge
    /// missing here cannot be selected.
    pub(crate) edge_cost_change_per_second: HashMap<PhysicalEdgeId, f64>,
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

/// [`SelectionCosts`] over `selectable` (each admissible edge a plan may
/// select, by consumer and input). Errs on nothing: a candidate
/// `asap-cost` cannot cost is reported in
/// [`SelectionCosts::candidate_cost_per_second`].
pub(crate) fn selection_costs(
    physical: &PhysicalASAPMemoCandidates,
    selectable: &[PhysicalEdgeId],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    cost_inputs: &CostInputs,
    request_overhead_cost_per_second: f64,
) -> SelectionCosts {
    let memo = &physical.logical_candidates.memo;
    let count = physical.physical_candidates.len();
    let mut out_edges: Vec<Vec<PhysicalEdgeId>> = vec![Vec::new(); count];
    for &edge in selectable {
        out_edges[physical.edge(edge).producer_candidate_id.0].push(edge);
    }
    // The candidates that may read each candidate.
    let readers_of = |v: PhysicalCandidateId| {
        out_edges[v.0]
            .iter()
            .map(|&edge| physical.edge(edge).consumer_candidate_id)
    };
    // Each candidate's node, its inputs numbered 0.. (the ids do not
    // matter to `node_resource_usage_and_cost`), with the retention it would need.
    let nodes: Vec<PhysicalASAPDAGNode> = (0..count)
        .map(PhysicalCandidateId)
        .map(|v| {
            let inputs: Vec<PhysicalASAPNodeId> = (0..physical.input_count(v)).collect();
            let mut node = candidate_node(physical, v, v.0, &inputs);
            if node.retains_rows() {
                node.retention_ms = retention_need_ms(physical, entries, v, readers_of);
            }
            node
        })
        .collect();
    // Each query-time `Scan`'s read per execution: the most any selectable
    // consumer reads (no range-evaluation span), charged once.
    let scan_read_ms_of: Vec<Option<u64>> = nodes
        .iter()
        .enumerate()
        .map(|(v, node)| {
            let query_time_scan = node.output_state.timing == ExecutionTiming::QueryTime
                && matches!(node.payload, Operator::NonASAP(NonASAPOp::Scan { .. }));
            query_time_scan
                .then(|| scan_read_ms(physical, PhysicalCandidateId(v), readers_of))
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
    // Each candidate's logical inputs, as costed for $w_v$; each edge into
    // it replaces one.
    let mut inputs_of: Vec<Option<Vec<ProducerOutput<'_>>>> = (0..count).map(|_| None).collect();
    for group in order {
        for &v in &candidates_of_group[group.0 as usize] {
            let inputs = logical_inputs(physical, &group_payloads, &candidates_of_group, &usage, v);
            usage[v.0] = Some(match &inputs {
                Some(inputs) => cost_node(
                    physical,
                    &nodes[v.0],
                    inputs,
                    entries,
                    data,
                    cost_inputs,
                    scan_read_ms_of[v.0],
                ),
                None => Err("an input group has no costable candidate".to_string()),
            });
            inputs_of[v.0] = inputs;
        }
    }
    let candidate_cost_per_second: Vec<Result<f64, String>> = usage
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
    let mut edge_cost_change_per_second = HashMap::new();
    for &id in selectable {
        let e = physical.edge(id);
        let (u, c) = (e.producer_candidate_id, e.consumer_candidate_id);
        let (Some(Ok(produced)), Ok(own)) = (&usage[u.0], &candidate_cost_per_second[c.0]) else {
            continue;
        };
        let Some(mut inputs) = inputs_of[c.0].clone() else {
            continue;
        };
        inputs[e.input_index_of_consumer] =
            produced.producer_output(&nodes[u.0].payload, &nodes[u.0].output_schema);
        let with_u = cost_node(
            physical,
            &nodes[c.0],
            &inputs,
            entries,
            data,
            cost_inputs,
            scan_read_ms_of[c.0],
        )
        .and_then(|usage| cost_per_second(&usage));
        if let Ok(with_u) = with_u {
            edge_cost_change_per_second.insert(id, with_u - own);
        }
    }
    SelectionCosts {
        candidate_cost_per_second,
        edge_cost_change_per_second,
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
            let readable: HashSet<_> = memo.producers(consumer, input).into_iter().collect();
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
            output = Some(usage.producer_output(payload, schema));
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

/// The usage of the candidate `node` over `inputs` (module doc): run
/// [`executions_per_second`] for the queries reaching its group, over the
/// window an ingestion-time grouping maintains
/// ([`asap_cost::maintained_window_ms`], over the memo's groups) or, for a
/// query-time `Scan`, `scan_read_ms` per execution.
fn cost_node<'a>(
    physical: &PhysicalASAPMemoCandidates,
    node: &'a PhysicalASAPDAGNode,
    inputs: &[ProducerOutput<'a>],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    cost_inputs: &'a CostInputs,
    scan_read_ms: Option<u64>,
) -> Result<NodeResourceUsageAndCost<'a>, String> {
    let memo = &physical.logical_candidates.memo;
    let candidate = node
        .selected_physical_candidate
        .expect("a candidate's node names it");
    let group = physical.physical_candidates[candidate.0]
        .logical_candidate_id
        .group;
    let per_second = executions_per_second(
        node.output_state.timing,
        node.lifetime,
        memo.groups()[group.0 as usize]
            .reached_by_queries()
            .iter()
            .filter_map(|&query| entries.get(query)),
        cost_inputs.non_repeating_query_horizon_seconds,
    )
    .map_err(|error| error.to_string())?;
    let window_ms = match node.output_state.timing {
        ExecutionTiming::QueryTime => scan_read_ms,
        // A chain's group computes the chain (Pass 2 adds chain candidates
        // only to it), so the group's window is the chain's.
        ExecutionTiming::IngestionTime => maintained_window_ms(
            group,
            |group: MemoGroupId| &memo.groups()[group.0 as usize].expression,
            |group| {
                memo.groups()[group.0 as usize]
                    .expression
                    .children()
                    .first()
                    .map(|g| **g)
            },
        ),
    };
    node_resource_usage_and_cost(node, inputs, (1, per_second), window_ms, data, cost_inputs)
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
