//! The objective's coefficients
//! (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
//! sections 3 and 5, "Cost and retention") under cost model 1, from
//! `asap-cost`: per execution of a candidate in a context
//! ([`crate::selectable`]), per edge in a context, and per amount a
//! candidate retains:
//!
//! $$\sum_v W^{\mathrm{mat}}_v x^{\mathrm{mat}}_v + \sum_{\kappa,v}
//! \rho_\kappa c_{\kappa,v} x_{\kappa,v} + \sum_{\kappa,u,c,i}
//! \rho_\kappa c_{\kappa,u,c,i} y_{\kappa,u,c,i} + \sum_v \pi_v h_v + \sum_s
//! r_s,$$
//!
//! less the request overhead every plan pays (`crate::select_plan` adds
//! it). Each candidate is costed as
//! one plan node ([`candidate_node`],
//! [`asap_cost::node_resource_usage_and_cost`]) over a baseline of each
//! input: the most any producer it may read (on its admissible edges)
//! outputs (rows, bytes and, for a state, its θ), at its execution's rate
//! $\rho_\kappa$ ([`executions_per_second`]). That does not depend on the
//! producers chosen. An ingestion-time consumer reads one batch of raw
//! data, also from a `Scan` materialized at ingestion time
//! ([`reads_one_batch_of`]). The part of a consumer's cost that changes
//! with the producer read is on the edge: $\rho_\kappa c_{\kappa,u,c,i}$ is
//! the consumer's cost in $\kappa$ with input $i$ read from $u$'s own
//! output, minus its own. It is exact when a consumer's cost changes with
//! one input's producer only (every consumer of a state reads one state
//! input). Every state candidate has its own θ (a merge, a `TimeRange` over
//! a chain carry the θ of the states they read, and may read only
//! producers with it), so the baseline over its admissible producers is
//! that θ's.
//!
//! A query-time `Scan` that is not materialized has one consumer per
//! context, and reads in each run what that consumer reads back in one
//! run ([`edge_scan_read_ms`]): that read is on the edge to the consumer,
//! a scan's read cost on its edges. A materialized one (`OneEvaluation`,
//! shared within a context) reads the most any consumer it may have in
//! the context reads, a constant of the execution: exact when that
//! consumer is selected.
//!
//! A candidate that retains time-indexed rows
//! (`PhysicalASAPDAGNode::retains_rows`) is costed retaining the most its
//! consumers read back ([`edge_retention_need_ms`]), the largest $h_v$ (R)
//! allows. Retention is linear in that amount: $\pi_v$ is what retaining
//! one more window (or second) costs there, and $W^{\mathrm{mat}}_v$ of a
//! materialized candidate is its cost less $\pi_v h_v$. A query-time
//! scan's retention is raw data on disk, kept once per source for the
//! selected scan that keeps the most: $r_s \ge \pi_v h_v$
//! ([`asap_cost::raw_data_disk_cost_per_second`]). A candidate one of whose
//! consumers reads it unbounded (a `Scan` read whole, a `TimeShift`
//! anchored with `@`) has no $h_v$: it is costed retaining everything.

use std::collections::HashMap;

use asap_cost::plan::execution_demand::{
    context_executions_per_second, executions_per_second as rate_of_materialized,
};
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
    ExecutionContext, Lifetime, Memo, MemoGroupId, PhysicalASAPMemoCandidates, PhysicalCandidateId,
    PhysicalEdgeId,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::plan::candidate_node;
use crate::retention::{edge_retention_need_ms, edge_scan_read_ms, output_retention_need_ms};
use crate::selectable::{evaluation_grid, Execution, SelectableExecutions};

/// The objective's coefficients
/// (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
/// section 5, "Cost and retention"), per second.
pub(crate) struct ExecutionCosts {
    /// $\rho_\kappa c_{\kappa,v}$ of each execution, $W^{\mathrm{mat}}_v$
    /// of an execution in `Materialized(v)`, by position in
    /// `SelectableExecutions::executions`; or why `asap-cost` could not
    /// cost it (the MILP leaves it out).
    pub(crate) per_execution: Vec<Result<f64, String>>,
    /// $\rho_\kappa c_{\kappa,u,c,i}$ of each edge, by position in
    /// `SelectableExecutions::edges`; `None` when it could not be costed
    /// (the MILP leaves it out).
    pub(crate) per_edge: Vec<Option<f64>>,
    /// $\pi_v$, the cost per second of each second retained, of each
    /// candidate that retains rows (`PhysicalASAPDAGNode::retains_rows`)
    /// only within a bound: it has $h_v$ and constraint (R). One whose
    /// consumer reads it unbounded is costed retaining everything.
    pub(crate) retention_cost_per_retained_second: HashMap<PhysicalCandidateId, f64>,
    /// $\operatorname{need}_{\kappa,u,c,i}$ in ms of each edge, by position
    /// in `SelectableExecutions::edges` ([`edge_retention_need_ms`]);
    /// `None` when its producer retains no rows or the edge reads it
    /// unbounded.
    pub(crate) retention_need_ms_per_edge: Vec<Option<u64>>,
    /// The source of each query-time `Scan`: its retention is raw data on
    /// disk, which a plan keeps once per source for the selected scan that
    /// keeps the most, as `plan_cost` charges it ($r_s$).
    pub(crate) raw_data_source: HashMap<PhysicalCandidateId, Source>,
    /// The cost per second of the raw data a query-time `Scan` without
    /// $h_v$ keeps on disk, retaining everything.
    pub(crate) unbounded_raw_data_on_disk_cost_per_second: HashMap<PhysicalCandidateId, f64>,
}

/// The objective's coefficients of every execution and edge of
/// `selectable` ([`ExecutionCosts`]; module doc). Errs on nothing: what
/// `asap-cost` cannot cost is reported in [`ExecutionCosts::per_execution`]
/// or left out of [`ExecutionCosts::per_edge`].
pub(crate) fn execution_costs(
    physical: &PhysicalASAPMemoCandidates,
    selectable: &SelectableExecutions,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    cost_inputs: &CostInputs,
) -> ExecutionCosts {
    let memo = &physical.logical_candidates.memo;
    let count = physical.physical_candidates.len();
    let executions = selectable.executions();
    // Each candidate's node, its inputs numbered 0.. (the ids do not
    // matter to `node_resource_usage_and_cost`), retaining nothing yet.
    let node_retaining = |v: PhysicalCandidateId, retention_ms: Option<u64>| {
        let inputs: Vec<PhysicalASAPNodeId> = (0..physical.input_count(v)).collect();
        let mut node = candidate_node(physical, v, v.0, &inputs);
        node.retention_ms = retention_ms;
        node
    };
    let retains_rows: Vec<bool> = (0..count)
        .map(|v| node_retaining(PhysicalCandidateId(v), None).retains_rows())
        .collect();
    let query_time_scan: Vec<bool> = (0..count)
        .map(|v| is_query_time_scan(physical, PhysicalCandidateId(v)))
        .collect();
    let producer_of = |position: usize| {
        let (_, edge, producer) = selectable.edges()[position];
        (edge, executions[producer].candidate)
    };
    // Each edge's need, computed once per edge of the physical memo: an
    // edge is read in every context it is selectable in.
    let mut need_ms_of_edge: HashMap<PhysicalEdgeId, Option<u64>> = HashMap::new();
    let retention_need_ms_per_edge: Vec<Option<u64>> = (0..selectable.edges().len())
        .map(|position| {
            let (edge, u) = producer_of(position);
            if !retains_rows[u.0] {
                return None;
            }
            *need_ms_of_edge
                .entry(edge)
                .or_insert_with(|| edge_retention_need_ms(physical, entries, edge))
        })
        .collect();
    // The most any consumer of each candidate that retains rows reads back
    // ($h_v$ at its largest): `None` when one reads it unbounded.
    let mut most_read_ms: HashMap<PhysicalCandidateId, Option<u64>> = (0..count)
        .filter(|&v| retains_rows[v])
        .map(|v| (PhysicalCandidateId(v), Some(0)))
        .collect();
    let mut read = |v: PhysicalCandidateId, need_ms: Option<u64>| {
        if let Some(most) = most_read_ms.get_mut(&v) {
            *most = most.zip(need_ms).map(|(most, need)| most.max(need));
        }
    };
    for (position, &need_ms) in retention_need_ms_per_edge.iter().enumerate() {
        read(producer_of(position).1, need_ms);
    }
    for (query, answering) in &selectable.answering {
        for &index in answering {
            read(
                executions[index].candidate,
                Some(output_retention_need_ms(&entries[query.0])),
            );
        }
    }
    // How far back a query-time `Scan` reads for the consumer of each edge
    // out of it, in one run (`None`: read whole), once per memo edge.
    let mut scan_read_ms_of_edge: HashMap<PhysicalEdgeId, Option<u64>> = HashMap::new();
    let scan_read_ms_per_edge: Vec<Option<Option<u64>>> = (0..selectable.edges().len())
        .map(|position| {
            let (edge, u) = producer_of(position);
            query_time_scan[u.0].then(|| {
                *scan_read_ms_of_edge
                    .entry(edge)
                    .or_insert_with(|| edge_scan_read_ms(physical, edge))
            })
        })
        .collect();
    // Each query-time `Scan`'s read for its baseline output (what its
    // consumers' baselines read): the most any of its consumers reads.
    let mut baseline_scan_read_ms: HashMap<PhysicalCandidateId, Option<u64>> = (0..count)
        .filter(|&v| query_time_scan[v])
        .map(|v| (PhysicalCandidateId(v), Some(0)))
        .collect();
    // A query-time `Scan`'s read per execution: one that is not
    // materialized has one reader per context, and its read is on the edge
    // to it (0 here); one materialized, shared by its readers, reads the
    // most any of them reads in one run.
    let mut execution_read_ms: Vec<Option<u64>> = executions
        .iter()
        .map(|execution| query_time_scan[execution.candidate.0].then_some(0))
        .collect();
    for (position, read_ms) in scan_read_ms_per_edge.iter().enumerate() {
        let Some(read_ms) = *read_ms else {
            continue;
        };
        let (_, _, producer) = selectable.edges()[position];
        let u = executions[producer].candidate;
        let most = baseline_scan_read_ms
            .get_mut(&u)
            .expect("a query-time scan");
        *most = most.zip(read_ms).map(|(most, read)| most.max(read));
        if physical.physical_candidates[u.0]
            .physical_properties
            .is_materialized()
        {
            let of_execution = &mut execution_read_ms[producer];
            *of_execution = of_execution.zip(read_ms).map(|(most, read)| most.max(read));
        }
    }
    let baseline_scan_read_of =
        |v: PhysicalCandidateId| baseline_scan_read_ms.get(&v).copied().flatten();
    let nodes: Vec<PhysicalASAPDAGNode> = (0..count)
        .map(PhysicalCandidateId)
        .map(|v| node_retaining(v, most_read_ms.get(&v).copied().flatten()))
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
    // A candidate's (own, raw data on disk) cost per second as `node` over
    // `inputs`, a query-time `Scan` reading `scan_read_ms` per execution.
    let cost_reading = |v: PhysicalCandidateId,
                        node: &PhysicalASAPDAGNode,
                        inputs: &[ProducerOutput<'_>],
                        per_second: f64,
                        scan_read_ms: Option<u64>| {
        cost_node(
            physical,
            v,
            node,
            inputs,
            per_second,
            scan_read_ms,
            data,
            cost_inputs,
        )
        .and_then(|usage| cost_per_second(&usage, cost_inputs))
    };
    let cost = |v: PhysicalCandidateId,
                node: &PhysicalASAPDAGNode,
                inputs: &[ProducerOutput<'_>],
                per_second: f64| {
        cost_reading(v, node, inputs, per_second, baseline_scan_read_of(v))
    };
    // Each candidate's usage of one computation (what it outputs does not
    // depend on how often it runs) over its baseline inputs.
    let mut usage: Vec<Option<Result<NodeResourceUsageAndCost<'_>, String>>> =
        (0..count).map(|_| None).collect();
    let mut baseline: Vec<Option<Vec<ProducerOutput<'_>>>> = vec![None; count];
    for group in order {
        for &v in &candidates_of_group[group.0 as usize] {
            baseline[v.0] = baseline_inputs(memo, physical, &group_payloads, &nodes, &usage, v);
            usage[v.0] = Some(match &baseline[v.0] {
                Some(inputs) => cost_node(
                    physical,
                    v,
                    &nodes[v.0],
                    inputs,
                    1.0,
                    baseline_scan_read_of(v),
                    data,
                    cost_inputs,
                ),
                None => Err("an input has no costable producer".to_string()),
            });
        }
    }
    // Each execution's (own, raw data on disk) cost per second at its
    // rate, over its baseline inputs, and that rate.
    let at_rate: Vec<Result<(f64, f64, f64), String>> = executions
        .iter()
        .zip(&execution_read_ms)
        .map(|(&execution, &read_ms)| {
            let v = execution.candidate;
            if let Some(Err(error)) = &usage[v.0] {
                return Err(error.clone());
            }
            let per_second = executions_per_second(
                physical,
                entries,
                selectable,
                execution,
                cost_inputs.non_repeating_query_horizon_seconds,
            )?;
            let inputs = baseline[v.0]
                .as_ref()
                .ok_or("it reads a group on a cycle of candidates")?;
            let (own, raw_data) = cost_reading(v, &nodes[v.0], inputs, per_second, read_ms)?;
            Ok((own, raw_data, per_second))
        })
        .collect();
    // The one execution of each candidate materialized across evaluations,
    // by position.
    let own_execution: HashMap<PhysicalCandidateId, usize> = executions
        .iter()
        .enumerate()
        .filter(|(_, execution)| is_across_evaluations(physical, execution.candidate))
        .map(|(index, execution)| (execution.candidate, index))
        .collect();
    // π_v of a candidate retaining rows every consumer reads within a bound:
    // what one more Δ ms of retention costs, per second retained (Δ a
    // window of a chain, else a second), at its retention's largest; its
    // whole own cost when it is materialized, its raw data on disk for a
    // query-time scan (each of its runs reads what it reads at that
    // retention, ρ_κ c_{κ,v}).
    let mut retention_cost_per_retained_second = HashMap::new();
    for (&v, &most_read_ms) in &most_read_ms {
        let Some(retention_ms) = most_read_ms else {
            continue;
        };
        let delta_ms = physical.physical_candidates[v.0]
            .chain_tumbling_window_width_ms(physical)
            .unwrap_or(1_000)
            .max(1);
        let longer = node_retaining(v, Some(retention_ms + delta_ms));
        let delta_seconds = delta_ms as f64 / 1_000.0;
        let per_retained_second = match is_across_evaluations(physical, v) {
            true => own_execution.get(&v).and_then(|&index| {
                let (own, raw_data, per_second) = at_rate[index].as_ref().ok()?;
                let inputs = baseline[v.0].as_ref()?;
                let (longer_own, longer_raw_data) = cost(v, &longer, inputs, *per_second).ok()?;
                Some((longer_own + longer_raw_data - own - raw_data) / delta_seconds)
            }),
            false => usage[v.0]
                .as_ref()
                .and_then(|usage| usage.as_ref().ok())
                .and_then(|_| {
                    let inputs = baseline[v.0].as_ref()?;
                    let (_, raw_data) = cost(v, &nodes[v.0], inputs, 1.0).ok()?;
                    let (_, longer_raw_data) = cost(v, &longer, inputs, 1.0).ok()?;
                    Some((longer_raw_data - raw_data) / delta_seconds)
                }),
        };
        if let Some(per_retained_second) = per_retained_second {
            retention_cost_per_retained_second.insert(v, per_retained_second.max(0.0));
        }
    }
    // Each query-time scan's source and, without $h_v$, the raw data it
    // keeps on disk retaining everything.
    let mut raw_data_source = HashMap::new();
    let mut unbounded_raw_data_on_disk_cost_per_second = HashMap::new();
    for v in (0..count)
        .filter(|&v| query_time_scan[v])
        .map(PhysicalCandidateId)
    {
        let Some(Ok(scan)) = &usage[v.0] else {
            continue;
        };
        let Some((source, _)) = scan.raw_scan else {
            continue;
        };
        raw_data_source.insert(v, source.clone());
        if !retention_cost_per_retained_second.contains_key(&v) {
            if let Some(Ok((_, raw_data))) = baseline[v.0]
                .as_ref()
                .map(|inputs| cost(v, &nodes[v.0], inputs, 1.0))
            {
                unbounded_raw_data_on_disk_cost_per_second.insert(v, raw_data);
            }
        }
    }
    let per_execution: Vec<Result<f64, String>> = executions
        .iter()
        .zip(&at_rate)
        .map(|(execution, at_rate)| {
            let &(own, raw_data, _) = at_rate.as_ref().map_err(Clone::clone)?;
            let v = execution.candidate;
            Ok(match retention_cost_per_retained_second.get(&v) {
                // A query-time scan's raw data is charged per source.
                _ if query_time_scan[v.0] => own,
                Some(per_retained_second) if is_across_evaluations(physical, v) => {
                    let retention_seconds = most_read_ms[&v].unwrap_or(0) as f64 / 1_000.0;
                    own + raw_data - per_retained_second * retention_seconds
                }
                _ => own + raw_data,
            })
        })
        .collect();
    let per_edge = selectable
        .edges()
        .iter()
        .zip(&scan_read_ms_per_edge)
        .map(|(&(consumer, edge, producer), scan_read_ms)| {
            let e = physical.edge(edge);
            let (u, c) = (e.producer_candidate_id, e.consumer_candidate_id);
            per_execution[producer].as_ref().ok()?;
            let &(own, raw_data, per_second) = at_rate[consumer].as_ref().ok()?;
            let produced = usage[u.0].as_ref()?.as_ref().ok()?;
            let mut inputs = baseline[c.0].clone()?;
            if !reads_one_batch_of(&nodes, c, u) {
                inputs[e.input_index_of_consumer] =
                    produced.producer_output(&nodes[u.0].payload, &nodes[u.0].output_schema);
            }
            let (with_u_own, with_u_raw_data) = cost(c, &nodes[c.0], &inputs, per_second).ok()?;
            // A scan that is not materialized reads, in its one consumer's
            // run, what that consumer reads: on this edge.
            let scan_read = match scan_read_ms {
                Some(read_ms) if nodes[u.0].lifetime == Lifetime::None => {
                    let &(scan_own, _, scan_per_second) = at_rate[producer].as_ref().ok()?;
                    let (reading_own, _) = cost_reading(
                        u,
                        &nodes[u.0],
                        baseline[u.0].as_ref()?,
                        scan_per_second,
                        *read_ms,
                    )
                    .ok()?;
                    reading_own - scan_own
                }
                _ => 0.0,
            };
            Some(with_u_own + with_u_raw_data - own - raw_data + scan_read)
        })
        .collect();
    ExecutionCosts {
        per_execution,
        per_edge,
        retention_cost_per_retained_second,
        retention_need_ms_per_edge,
        raw_data_source,
        unbounded_raw_data_on_disk_cost_per_second,
    }
}

/// Whether candidate `v` is materialized across evaluations.
fn is_across_evaluations(physical: &PhysicalASAPMemoCandidates, v: PhysicalCandidateId) -> bool {
    physical.physical_candidates[v.0]
        .physical_properties
        .is_across_evaluations()
}

/// Whether candidate `v` is a `Scan` at query time, which reads raw data
/// in each execution.
fn is_query_time_scan(physical: &PhysicalASAPMemoCandidates, v: PhysicalCandidateId) -> bool {
    let candidate = &physical.physical_candidates[v.0];
    candidate.physical_properties.timing() == ExecutionTiming::QueryTime
        && matches!(
            physical
                .logical_candidates
                .memo
                .candidate(candidate.logical_candidate_id)
                .expression
                .operator,
            Operator::NonASAP(NonASAPOp::Scan { .. })
        )
}

/// $\rho$ of `execution`'s context, how often it runs, from the queries it
/// serves: in an evaluation context, as often as that context runs for
/// the queries on its grid that reach the execution's group
/// ([`context_executions_per_second`], which `plan_cost` uses too): at
/// their evaluations, or, for the inputs of a `TimeShift` anchored with
/// `@`, once per request of the queries on the shift's grid that reach the
/// shift;
/// in `Materialized(m)`, as often as $m$ runs
/// ([`asap_cost::plan::execution_demand::executions_per_second`]); in the
/// ingestion pipeline, once per second of arriving rows.
///
/// Example: queries every 10 s, 10 s and 30 s reaching a scan: 1/10 per
/// second in the 10 s context, 1/30 in the 30 s one.
pub(crate) fn executions_per_second(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    selectable: &SelectableExecutions,
    execution: Execution,
    non_repeating_query_horizon_seconds: f64,
) -> Result<f64, String> {
    let memo = &physical.logical_candidates.memo;
    let reaching = |v: PhysicalCandidateId| {
        let group = physical.physical_candidates[v.0].logical_candidate_id.group;
        memo.groups()[group.0 as usize]
            .reached_by_queries()
            .iter()
            .filter_map(|&query| entries.get(query))
    };
    let rate = match selectable.context(execution.context) {
        ExecutionContext::Evaluation { grid } => match selectable.request_anchor(execution.context)
        {
            Some((at, queries)) => context_executions_per_second(
                Some(at),
                queries.iter().filter_map(|query| entries.get(query.0)),
                non_repeating_query_horizon_seconds,
            ),
            None => context_executions_per_second(
                None,
                reaching(execution.candidate).filter(|entry| evaluation_grid(entry) == grid),
                non_repeating_query_horizon_seconds,
            ),
        },
        ExecutionContext::Materialized(m) => {
            let properties = physical.physical_candidates[m.0].physical_properties;
            rate_of_materialized(
                properties.timing(),
                properties.lifetime(),
                reaching(m),
                non_repeating_query_horizon_seconds,
            )
        }
        // Each arriving row once.
        ExecutionContext::IngestionPipeline(_) => Ok(1.0),
    };
    rate.map_err(|error| error.to_string())
}

/// Candidate `v`'s baseline inputs, by input index: over the producers it
/// may read (its admissible edges) whose usage is known, the most any
/// outputs ([`logical_output`]); `None` when an input has none. A producer
/// of which `v` reads one batch ([`reads_one_batch_of`]) is left out.
fn baseline_inputs<'a>(
    memo: &'a Memo,
    physical: &PhysicalASAPMemoCandidates,
    group_payloads: &'a [PhysicalASAPOperatorPayload],
    nodes: &[PhysicalASAPDAGNode],
    usage: &[Option<Result<NodeResourceUsageAndCost<'a>, String>>],
    v: PhysicalCandidateId,
) -> Option<Vec<ProducerOutput<'a>>> {
    memo.candidate(physical.physical_candidates[v.0].logical_candidate_id)
        .expression
        .operator
        .children()
        .into_iter()
        .enumerate()
        .map(|(i, input)| {
            logical_output(
                &group_payloads[input.0 as usize],
                &memo.groups()[input.0 as usize].group_properties.schema,
                physical
                    .admissible_edges_into(v, i)
                    .iter()
                    .map(|&edge| physical.edge(edge).producer_candidate_id)
                    .filter(|&u| !reads_one_batch_of(nodes, v, u))
                    .filter_map(|u| usage[u.0].as_ref()?.as_ref().ok()),
            )
        })
        .collect()
}

/// Whether consumer `c` reads only one batch of producer `u`'s output: `c`
/// runs at ingestion time and `u` is a raw `Scan` materialized there, whose
/// output is the rows it retains, what a query-time consumer reads. Such an
/// edge costs what reading a `Scan` that is not materialized does.
fn reads_one_batch_of(
    nodes: &[PhysicalASAPDAGNode],
    c: PhysicalCandidateId,
    u: PhysicalCandidateId,
) -> bool {
    let ingestion_time =
        |v: PhysicalCandidateId| nodes[v.0].output_state.timing == ExecutionTiming::IngestionTime;
    ingestion_time(c)
        && ingestion_time(u)
        && nodes[u.0].retains_rows()
        && matches!(
            nodes[u.0].payload,
            Operator::NonASAP(NonASAPOp::Scan { .. })
        )
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

/// Candidate `v`'s usage as `node` over `inputs`, computed `per_second`
/// times per second, over the window an ingestion-time grouping maintains
/// ([`asap_cost::maintained_window_ms`], over the memo's groups) or, for a
/// query-time `Scan`, `scan_read_ms` per execution.
#[allow(clippy::too_many_arguments)]
fn cost_node<'a>(
    physical: &PhysicalASAPMemoCandidates,
    v: PhysicalCandidateId,
    node: &'a PhysicalASAPDAGNode,
    inputs: &[ProducerOutput<'a>],
    per_second: f64,
    scan_read_ms: Option<u64>,
    data: &DataWorkload,
    cost_inputs: &'a CostInputs,
) -> Result<NodeResourceUsageAndCost<'a>, String> {
    let memo = &physical.logical_candidates.memo;
    let group = physical.physical_candidates[v.0].logical_candidate_id.group;
    let window_ms = match node.output_state.timing {
        ExecutionTiming::QueryTime => scan_read_ms,
        // A chain maintains its own window; any other grouping the first
        // range below its group.
        ExecutionTiming::IngestionTime => physical.physical_candidates[v.0]
            .chain_tumbling_window_width_ms(physical)
            .or_else(|| {
                maintained_window_ms(
                    group,
                    |group: MemoGroupId| &memo.groups()[group.0 as usize].expression,
                    |group| {
                        memo.groups()[group.0 as usize]
                            .expression
                            .children()
                            .first()
                            .map(|g| **g)
                    },
                )
            }),
    };
    node_resource_usage_and_cost(node, inputs, (1, per_second), window_ms, data, cost_inputs)
        .map_err(|error| error.to_string())
}

/// A candidate's cost per second from its usage under cost model 1: its
/// own, and the raw data a query-time scan keeps on disk
/// ([`asap_cost::raw_data_disk_cost_per_second`]).
fn cost_per_second(
    usage: &NodeResourceUsageAndCost<'_>,
    cost_inputs: &CostInputs,
) -> Result<(f64, f64), String> {
    let own = usage
        .cost
        .cost_per_second
        .ok_or("the selection MILP costs under cost model 1 only")?;
    let raw_data = match usage.raw_scan {
        Some((_, bytes)) => raw_data_disk_cost_per_second(bytes, cost_inputs)
            .ok_or("the selection MILP costs under cost model 1 only")?,
        None => 0.0,
    };
    match [own, raw_data]
        .iter()
        .all(|cost| cost.is_finite() && *cost >= 0.0)
    {
        true => Ok((own, raw_data)),
        false => Err(format!(
            "its cost per second {own} + {raw_data} is not a finite non-negative number"
        )),
    }
}

#[cfg(test)]
mod tests;
