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
//! one more output (a window, or a second) costs there, and $W^{\mathrm{mat}}_v$ of a
//! materialized candidate is its cost less $\pi_v h_v$. A query-time
//! scan's retention is raw data on disk, kept once per source for the
//! selected scan that keeps the most: $r_s \ge \pi_v h_v$
//! ([`asap_cost::raw_data_disk_cost_per_second`]). A candidate one of whose
//! consumers reads it unbounded (a `Scan` read whole, a `TimeShift`
//! anchored with `@`) has no $h_v$: it is costed retaining everything.

use std::collections::HashMap;

use asap_cost::node::analytical::AnalyticalCostError;
use asap_cost::plan::execution_demand::{
    context_executions_per_second, executions_per_second as rate_of_materialized,
};
use asap_cost::{
    node_resource_usage_and_cost, output_interval_ms, raw_data_disk_cost_per_second, CostInputs,
    NodeResourceUsageAndCost, ProducerOutput, RawRowsShortOfRange,
};
use asap_types::ir::operator::Source;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::plan::materialized_view::RefreshWindow;
use asap_types::plan::physical_asap_dag::{
    PhysicalASAPDAGNode, PhysicalASAPNodeId, PhysicalASAPOperatorPayload,
};
use asap_types::stages::{
    queries_of_context, EvaluationGrid, ExecutionContext, Lifetime, Memo, MemoGroupId,
    PhysicalASAPMemoCandidates, PhysicalCandidateId, PhysicalEdgeId,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::plan::candidate_node;
use crate::retention::{
    edge_retention_need_ms, edge_scan_read_ms, least_retention_ms_per_execution, ms_to_seconds,
};
use crate::selectable::{Execution, SelectableExecutions};

/// The objective's coefficients
/// (`docs/design_docs/concepts/memo-groups-properties-requirements.md`,
/// section 5, "Cost and retention"), per second.
#[cfg_attr(test, derive(Default))]
pub(crate) struct ExecutionCosts {
    /// $\rho_\kappa c_{\kappa,v}$ of each execution, $W^{\mathrm{mat}}_v$
    /// of an execution in `Materialized(v)`, by position in
    /// `SelectableExecutions::executions`; or why `asap-cost` could not
    /// cost it (the MILP leaves it out).
    pub(crate) per_execution: Vec<Result<f64, String>>,
    /// Each execution's share at the largest retention its readers may
    /// need (`per_execution` before $\pi_v h_v$ is taken off): what the
    /// latency rows read, whose coefficients cannot depend on $h_v$.
    pub(crate) per_execution_at_largest_retention: Vec<Result<f64, String>>,
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
    /// Per candidate that retains rows, its output interval Δ in ms
    /// (`asap_cost::output_interval_ms`; a chain's window, the window an
    /// ingestion-time view maintains, else a second): a store always keeps a
    /// stored result's newest output whatever its retention, so a selected
    /// one retains at least Δ, as `plan_cost` charges at least one retained
    /// output ([`Self::retention_ms_for_need`]). Example: an ingestion-time
    /// chain of 60 s windows read only at its newest window retains 60 s,
    /// not 0.
    pub(crate) newest_output_retention_ms: HashMap<PhysicalCandidateId, u64>,
    /// The least retention, in ms, over each execution's costed reads
    /// ([`Self::retention_ms_for_need`] of each), by position in
    /// `SelectableExecutions::executions` ([`least_retention_ms_per_execution`]).
    pub(crate) least_retention_ms_per_execution: Vec<Option<u64>>,
    /// The source of each query-time `Scan`: its retention is raw data on
    /// disk, which a plan keeps once per source for the selected scan that
    /// keeps the most, as `plan_cost` charges it ($r_s$).
    pub(crate) raw_data_source: HashMap<PhysicalCandidateId, Source>,
    /// The cost per second of the raw data a query-time `Scan` without
    /// $h_v$ keeps on disk, retaining everything.
    pub(crate) unbounded_raw_data_on_disk_cost_per_second: HashMap<PhysicalCandidateId, f64>,
    /// Each query-time computation not offered because it would read
    /// fewer raw rows than arrive over its range
    /// ([`asap_cost::query_time_computation_short_of_source_rows`]), with that
    /// shortfall: reported as `PruneReason::ReadsFewerSourceRowsThanItsRange`.
    pub(crate) short_of_source_rows: HashMap<PhysicalCandidateId, RawRowsShortOfRange>,
}

impl ExecutionCosts {
    /// What candidate `v` retains, in ms, for a read needing `need_ms`: at
    /// least its newest output ([`Self::newest_output_retention_ms`]),
    /// which a store always keeps.
    ///
    /// Example: a chain of 60 s windows read at its newest window (need 0)
    /// retains 60,000 ms; read over the last 5 minutes, 300,000 ms.
    pub(crate) fn retention_ms_for_need(&self, v: PhysicalCandidateId, need_ms: u64) -> u64 {
        self.newest_output_retention_ms
            .get(&v)
            .map_or(need_ms, |&newest_ms| need_ms.max(newest_ms))
    }

    /// $\pi_v h_v$ of candidate `v` retaining `retention_ms`, per second; 0
    /// for a candidate with no retention cost.
    pub(crate) fn retention_cost_per_second(
        &self,
        v: PhysicalCandidateId,
        retention_ms: u64,
    ) -> f64 {
        self.retention_cost_per_retained_second
            .get(&v)
            .map_or(0.0, |per_retained_second| {
                per_retained_second * ms_to_seconds(retention_ms)
            })
    }
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
    execution_quantities(
        physical,
        selectable,
        entries,
        data,
        cost_inputs,
        Quantity::CostPerSecond,
    )
}

/// What [`execution_quantities`] attributes to executions, edges and
/// retention, by the rules cost model 1's cost follows: a candidate's
/// `quantity` over its baseline inputs on its execution, the difference
/// a producer makes on the edge, and its retention's share per retained
/// second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Quantity {
    /// Cost model 1's cost per second.
    CostPerSecond,
    /// Query-time work of one run, in ms (0 at ingestion time).
    QueryMsPerRun,
    /// Refresh wall time per window close, in ms (0 at query time).
    RefreshWallMsPerWindow,
    /// CPU core-seconds per second.
    CpuCoreSecondsPerSecond,
    /// Peak memory, in bytes.
    PeakMemoryBytes,
    /// Disk, in bytes, a query-time scan's raw data apart (per source).
    DiskBytes,
}

/// [`execution_costs`] of `quantity` instead of cost model 1's cost
/// ([`Quantity`]). `cost_inputs` must be cost model 1's, which every
/// quantity is costed under (only [`Quantity::CostPerSecond`] reads its
/// prices).
pub(crate) fn execution_quantities(
    physical: &PhysicalASAPMemoCandidates,
    selectable: &SelectableExecutions,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    cost_inputs: &CostInputs,
    quantity: Quantity,
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
    let unretained_nodes: Vec<PhysicalASAPDAGNode> = (0..count)
        .map(|v| node_retaining(PhysicalCandidateId(v), None))
        .collect();
    let retains_rows: Vec<bool> = unretained_nodes
        .iter()
        .map(PhysicalASAPDAGNode::retains_rows)
        .collect();
    let query_time_scan: Vec<bool> = (0..count)
        .map(|v| physical.is_query_time_scan(PhysicalCandidateId(v)))
        .collect();
    let producer_of = |position: usize| {
        let edge = selectable.edges()[position];
        (edge.edge, executions[edge.producer].candidate)
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
            *need_ms_of_edge.entry(edge).or_insert_with(|| {
                edge_retention_need_ms(physical, selectable.retained_span_ms_of_query(), edge)
            })
        })
        .collect();
    // Δ of each candidate that retains rows, the newest output a store
    // always keeps (`ExecutionCosts::newest_output_retention_ms`).
    let newest_output_retention_ms: HashMap<PhysicalCandidateId, u64> = (0..count)
        .filter(|&v| retains_rows[v])
        .map(PhysicalCandidateId)
        .map(|v| {
            let node = &unretained_nodes[v.0];
            let window_ms = match node.output_state.timing {
                ExecutionTiming::IngestionTime => ingestion_window_ms(physical, v),
                ExecutionTiming::QueryTime => None,
            };
            (v, output_interval_ms(&node.payload, window_ms))
        })
        .collect();
    // The most any consumer of each candidate that retains rows reads back
    // ($h_v$ at its largest), at least its newest output: `None` when one
    // reads it unbounded.
    let mut most_read_ms: HashMap<PhysicalCandidateId, Option<u64>> = newest_output_retention_ms
        .iter()
        .map(|(&v, &newest_ms)| (v, Some(newest_ms)))
        .collect();
    for (index, _, need_ms) in selectable.reads_back(&retention_need_ms_per_edge) {
        if let Some(most) = most_read_ms.get_mut(&executions[index].candidate) {
            *most = most.zip(need_ms).map(|(most, need)| most.max(need));
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
        let producer = selectable.edges()[position].producer;
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
    let nodes: Vec<PhysicalASAPDAGNode> = unretained_nodes
        .into_iter()
        .enumerate()
        .map(|(v, mut node)| {
            node.retention_ms = most_read_ms.get(&PhysicalCandidateId(v)).copied().flatten();
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
    // A candidate's (own, raw data on disk) cost per second as `node` over
    // `inputs`, a query-time `Scan` reading `scan_read_ms` per execution.
    // `evaluation_grid`: the grid of the context it runs in, which a
    // `TimeRangeCover`'s windows are averaged over.
    let cost_reading = |v: PhysicalCandidateId,
                        node: &PhysicalASAPDAGNode,
                        inputs: &[ProducerOutput<'_>],
                        per_second: f64,
                        scan_read_ms: Option<u64>,
                        evaluation_grid: Option<EvaluationGrid>| {
        cost_node(
            physical,
            v,
            node,
            inputs,
            per_second,
            scan_read_ms,
            evaluation_grid,
            data,
            cost_inputs,
        )
        .map_err(|error| error.to_string())
        .and_then(|usage| quantity_of(quantity, &usage, node, cost_inputs))
    };
    // The evaluation grid of a context, which a cover's windows are
    // averaged over; `None` for a context not on one evaluation grid.
    let context_grid = |context| match selectable.context(context) {
        ExecutionContext::Evaluation { grid } => Some(grid),
        _ => None,
    };
    let cost = |v: PhysicalCandidateId,
                node: &PhysicalASAPDAGNode,
                inputs: &[ProducerOutput<'_>],
                per_second: f64| {
        cost_reading(v, node, inputs, per_second, baseline_scan_read_of(v), None)
    };
    // Each candidate's usage of one computation (what it outputs does not
    // depend on how often it runs) over its baseline inputs.
    let mut usage: Vec<Option<Result<NodeResourceUsageAndCost<'_>, String>>> =
        (0..count).map(|_| None).collect();
    let mut baseline: Vec<Option<Vec<ProducerOutput<'_>>>> = vec![None; count];
    let mut short_of_source_rows = HashMap::new();
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
                    None,
                    data,
                    cost_inputs,
                )
                .map_err(|error| {
                    // A query-time computation over too few raw rows is
                    // reported by name (`PruneReason`), not as uncostable.
                    if let AnalyticalCostError::FewerSourceRowsThanItsRange {
                        rows,
                        rows_in_range,
                        ..
                    } = error
                    {
                        short_of_source_rows.insert(
                            v,
                            RawRowsShortOfRange {
                                rows,
                                rows_in_range,
                            },
                        );
                    }
                    error.to_string()
                }),
                None => Err("an input has no costable producer".to_string()),
            });
        }
    }
    // Producer `u`'s output at `evaluation_grid` when it is a
    // `TimeRangeCover` (its windows, and so its rows, depend on the grid it
    // runs on); `None` for any other producer, whose baseline output holds.
    let cover_output_at = |u: PhysicalCandidateId, evaluation_grid: Option<EvaluationGrid>| {
        if !matches!(
            physical.operator(u),
            asap_types::ir::Operator::NonASAP(asap_types::ir::NonASAPOp::TimeRangeCover { .. })
        ) {
            return None;
        }
        evaluation_grid?;
        let usage = cost_node(
            physical,
            u,
            &nodes[u.0],
            baseline[u.0].as_ref()?,
            1.0,
            None,
            evaluation_grid,
            data,
            cost_inputs,
        )
        .ok()?;
        Some(usage.producer_output(&nodes[u.0].payload, &nodes[u.0].output_schema))
    };
    // `v`'s baseline inputs, each cover among them as it runs at
    // `evaluation_grid` (the largest of its candidates', as the baseline).
    let inputs_at = |v: PhysicalCandidateId, evaluation_grid: Option<EvaluationGrid>| {
        let mut inputs = baseline[v.0].clone()?;
        for (i, input) in inputs.iter_mut().enumerate() {
            let at_grid: Vec<ProducerOutput<'_>> = physical
                .admissible_edges_into(v, i)
                .iter()
                .filter_map(|&edge| {
                    cover_output_at(physical.edge(edge).producer_candidate_id, evaluation_grid)
                })
                .collect();
            if let Some(largest) = at_grid.into_iter().max_by_key(|output| output.output.bytes) {
                *input = largest;
            }
        }
        Some(inputs)
    };
    // Each execution's (own, raw data on disk) cost per second at its
    // rate, over its baseline inputs (covers at its context's grid), and
    // that rate.
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
            // A context on one grid charges a cover its windows there.
            let evaluation_grid = context_grid(execution.context);
            let inputs =
                inputs_at(v, evaluation_grid).ok_or("it reads a group on a cycle of candidates")?;
            let (own, raw_data) = cost_reading(
                v,
                &nodes[v.0],
                &inputs,
                per_second,
                read_ms,
                evaluation_grid,
            )?;
            Ok((own, raw_data, per_second))
        })
        .collect();
    // The one execution of each candidate materialized across evaluations,
    // by position.
    let own_execution: HashMap<PhysicalCandidateId, usize> = executions
        .iter()
        .enumerate()
        .filter(|(_, execution)| {
            physical.physical_candidates[execution.candidate.0]
                .physical_properties
                .is_across_evaluations()
        })
        .map(|(index, execution)| (execution.candidate, index))
        .collect();
    // π_v of a candidate retaining rows every consumer reads within a bound:
    // what one more Δ ms of retention (one more output; a retention is a
    // whole number of outputs) costs, per second retained, at its
    // retention's largest; its whole own cost when it is materialized, its
    // raw data on disk for a query-time scan (each of its runs reads what
    // it reads at that retention, ρ_κ c_{κ,v}).
    let mut retention_cost_per_retained_second = HashMap::new();
    for (&v, &most_read_ms) in &most_read_ms {
        let Some(most_read_ms) = most_read_ms else {
            continue;
        };
        let delta_ms = newest_output_retention_ms[&v];
        let longer = node_retaining(v, Some(most_read_ms + delta_ms));
        let delta_seconds = ms_to_seconds(delta_ms);
        let per_retained_second = match physical.physical_candidates[v.0]
            .physical_properties
            .is_across_evaluations()
        {
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
    // Each execution's own share at its largest retention (no $\pi_v h_v$
    // taken off): a time row's coefficient, an upper bound whatever $h_v$
    // the plan selects, exact when retention does not change the time.
    let per_execution_at_largest_retention: Vec<Result<f64, String>> = executions
        .iter()
        .zip(&at_rate)
        .map(|(execution, at_rate)| {
            let &(own, raw_data, _) = at_rate.as_ref().map_err(Clone::clone)?;
            Ok(match query_time_scan[execution.candidate.0] {
                true => own,
                false => own + raw_data,
            })
        })
        .collect();
    let per_execution: Vec<Result<f64, String>> = executions
        .iter()
        .zip(&at_rate)
        .map(|(execution, at_rate)| {
            let &(own, raw_data, _) = at_rate.as_ref().map_err(Clone::clone)?;
            let v = execution.candidate;
            let per_retained_second = retention_cost_per_retained_second.get(&v);
            Ok(if query_time_scan[v.0] {
                // A query-time scan's raw data is charged per source.
                own
            } else if let (Some(per_retained_second), true) = (
                per_retained_second,
                physical.physical_candidates[v.0]
                    .physical_properties
                    .is_across_evaluations(),
            ) {
                let retention_ms =
                    most_read_ms[&v].expect("a candidate with π_v is read within a bound");
                own + raw_data - per_retained_second * ms_to_seconds(retention_ms)
            } else {
                own + raw_data
            })
        })
        .collect();
    let per_edge: Vec<Option<f64>> = selectable
        .edges()
        .iter()
        .zip(&scan_read_ms_per_edge)
        .map(|(edge, scan_read_ms)| {
            let (consumer, producer) = (edge.consumer, edge.producer);
            let e = physical.edge(edge.edge);
            let (u, c) = (e.producer_candidate_id, e.consumer_candidate_id);
            per_execution[producer].as_ref().ok()?;
            let &(own, raw_data, per_second) = at_rate[consumer].as_ref().ok()?;
            let produced = usage[u.0].as_ref()?.as_ref().ok()?;
            // At the consumer's context grid, as its own cost (`at_rate`).
            let evaluation_grid = context_grid(selectable.executions()[consumer].context);
            let mut inputs = inputs_at(c, evaluation_grid)?;
            if !reads_one_batch_of(physical, &nodes, c, u) {
                inputs[e.input_index_of_consumer] = cover_output_at(u, evaluation_grid)
                    .unwrap_or_else(|| {
                        produced.producer_output(&nodes[u.0].payload, &nodes[u.0].output_schema)
                    });
            }
            let (with_u_own, with_u_raw_data) = cost_reading(
                c,
                &nodes[c.0],
                &inputs,
                per_second,
                baseline_scan_read_of(c),
                evaluation_grid,
            )
            .ok()?;
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
                        // A scan's cost does not depend on its readers' grid.
                        None,
                    )
                    .ok()?;
                    reading_own - scan_own
                }
                _ => 0.0,
            };
            Some(with_u_own + with_u_raw_data - own - raw_data + scan_read)
        })
        .collect();
    let mut costs = ExecutionCosts {
        per_execution,
        per_execution_at_largest_retention,
        per_edge,
        retention_cost_per_retained_second,
        retention_need_ms_per_edge,
        newest_output_retention_ms,
        least_retention_ms_per_execution: Vec::new(),
        raw_data_source,
        unbounded_raw_data_on_disk_cost_per_second,
        short_of_source_rows,
    };
    costs.least_retention_ms_per_execution = least_retention_ms_per_execution(selectable, &costs);
    costs
}

/// $\rho$ of `execution`'s context, how often it runs, from the queries it
/// serves ([`context_executions_per_second`], which `plan_cost` uses too):
/// in an evaluation context, for the queries on its grid whose walk over
/// the memo reaches the execution's group in their evaluation context
/// (`SelectableExecutions::readers_of`, [`asap_types::stages::query_reads`],
/// the walk `plan_cost` takes over the plan), at their evaluations; for
/// the inputs of a `TimeShift` anchored with `@`, once per request of the
/// queries on the shift's grid that reach the shift. The memo walk goes
/// through every candidate of a group, so a query reading a group through
/// one candidate counts for its other candidates too;
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
    let group_of =
        |v: PhysicalCandidateId| physical.physical_candidates[v.0].logical_candidate_id.group;
    let reaching = |v: PhysicalCandidateId| {
        memo.groups()[group_of(v).0 as usize]
            .reached_by_queries()
            .iter()
            .copied()
    };
    let entries_of = |queries: Vec<usize>| -> Vec<&QueryWorkloadEntry> {
        queries
            .into_iter()
            .filter_map(|query| entries.get(query))
            .collect()
    };
    let rate = match selectable.context(execution.context) {
        ExecutionContext::Evaluation { grid } => match selectable.request_anchor(execution.context)
        {
            Some((at, queries)) => context_executions_per_second(
                Some(at),
                entries_of(queries.iter().map(|query| query.0).collect()),
                non_repeating_query_horizon_seconds,
            ),
            None => context_executions_per_second(
                None,
                entries_of(queries_of_context(
                    grid.period_ms,
                    selectable.readers_of(group_of(execution.candidate), |anchor| anchor.is_none()),
                    entries,
                )),
                non_repeating_query_horizon_seconds,
            ),
        },
        ExecutionContext::Materialized(m) => {
            let properties = physical.physical_candidates[m.0].physical_properties;
            rate_of_materialized(
                properties.timing(),
                properties.lifetime(),
                entries_of(reaching(m).collect()),
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
    physical
        .operator(v)
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
                    .filter(|&u| !reads_one_batch_of(physical, nodes, v, u))
                    .filter_map(|u| Some((&nodes[u.0], usage[u.0].as_ref()?.as_ref().ok()?))),
            )
        })
        .collect()
}

/// Whether consumer `c` reads only one batch of producer `u`'s output
/// ([`asap_cost::reads_one_batch_of`], over `u`'s input group's
/// operator): such an edge costs what reading a `Scan` that is not
/// materialized does.
fn reads_one_batch_of(
    physical: &PhysicalASAPMemoCandidates,
    nodes: &[PhysicalASAPDAGNode],
    c: PhysicalCandidateId,
    u: PhysicalCandidateId,
) -> bool {
    let memo = &physical.logical_candidates.memo;
    let input = physical
        .operator(u)
        .children()
        .first()
        .map(|group| &memo.groups()[group.0 as usize].expression);
    asap_cost::reads_one_batch_of(&nodes[c.0], &nodes[u.0], input)
}

/// An input's logical output over its readable costed candidates' `usages`: the most
/// rows and bytes any outputs and the longest time their raw rows span, the
/// state family and the rows each state folds of the one with the most
/// bytes, the sources any reads and the columns every one leaves unchanged;
/// `payload` and `schema` the group's own. `None` without a costed
/// candidate. Example: of two KLL candidates, one at k = 547 whose states
/// fold 10 rows and one at k = 1,404 whose states fold 3,600, the larger
/// gives both its family and its rows, so a merge over the input never
/// sizes one candidate's `k` for the other's rows.
fn logical_output<'a: 'b, 'b, 'c>(
    payload: &'a PhysicalASAPOperatorPayload,
    schema: &'a asap_types::ir::schema::Schema,
    usages: impl Iterator<Item = (&'c PhysicalASAPDAGNode, &'b NodeResourceUsageAndCost<'a>)>,
) -> Option<ProducerOutput<'a>> {
    let mut output: Option<ProducerOutput<'a>> = None;
    // The rows each state family (with θ) folds, of the first candidate
    // with it: every candidate of the input group with that family and θ
    // computes the same states, whichever operator and timing build them,
    // so it folds the same rows, and the baseline is exact for each.
    let mut folded_by_family: Vec<(&'c PhysicalASAPDAGNode, &'b NodeResourceUsageAndCost<'a>)> =
        Vec::new();
    for (candidate, usage) in usages {
        let folds_the_same = |first: &NodeResourceUsageAndCost<'_>| match (
            first.state_input_rows,
            usage.state_input_rows,
        ) {
            (Some(a), Some(b)) => a.folds_the_same_as(&b),
            (a, b) => a.is_none() && b.is_none(),
        };
        match folded_by_family
            .iter()
            .find(|(_, first)| first.family == usage.family)
        {
            Some((first_candidate, first)) => debug_assert!(
                usage.family == asap_cost::StateFamily::None || folds_the_same(first),
                "candidates with the same family and θ fold different rows: {:?}\n  {:?} {:?} {:?}\n  {:?} {:?} {:?}",
                usage.family,
                first_candidate.output_state.timing,
                first_candidate.payload,
                first.state_input_rows,
                candidate.output_state.timing,
                candidate.payload,
                usage.state_input_rows,
            ),
            None => folded_by_family.push((candidate, usage)),
        }
        let Some(known) = output.as_mut() else {
            output = Some(usage.producer_output(payload, schema));
            continue;
        };
        if known.raw_rows_per_ms != usage.raw_rows_per_ms {
            known.raw_rows_per_ms = None;
        }
        // The source rows behind an aggregate's rows are the same for every
        // candidate of the group (each folds the same samples); kept as a
        // total so that the rows the baseline takes (the most any outputs)
        // times the rows each folds still count each source row once.
        let source_rows_behind = known
            .folded_per_row
            .and_then(|folded| folded.folded_rows_per_state)
            .map(|per_row| per_row * known.output.rows as f64);
        // Short of its range only when every candidate is: a consumer may
        // read one that is not.
        if usage.raw_rows_short_of_range.is_none() {
            known.raw_rows_short_of_range = None;
        }
        // States built at query time from raw rows only when every
        // candidate's are: a consumer may read an ingestion-time chain,
        // which saw every row. An edge to a query-time chain is checked
        // against that chain's own output (`per_edge`).
        if usage.state_data_span_ms.is_none() {
            known.state_data_span_ms = None;
        }
        if usage.output.bytes > known.output.bytes {
            known.family = usage.family;
            known.state_input_rows = usage.state_input_rows;
        }
        known.raw_rows_span_ms = known.raw_rows_span_ms.max(usage.raw_rows_span_ms);
        known.output.rows = known.output.rows.max(usage.output.rows);
        if let (Some(folded), Some(total)) = (known.folded_per_row.as_mut(), source_rows_behind) {
            folded.folded_rows_per_state = Some(total / known.output.rows.max(1) as f64);
        }
        known.output.bytes = known.output.bytes.max(usage.output.bytes);
        known.sources.extend(usage.sources.iter().copied());
        known
            .unchanged_columns
            .retain(|column| usage.unchanged_columns.contains(column));
    }
    output
}

/// Candidate `v`'s usage as `node` over `inputs`, computed `per_second`
/// times per second, over its refresh window at ingestion time
/// ([`ingestion_window_ms`]) or, for a
/// query-time `Scan`, `scan_read_ms` per execution.
#[allow(clippy::too_many_arguments)]
fn cost_node<'a>(
    physical: &PhysicalASAPMemoCandidates,
    v: PhysicalCandidateId,
    node: &'a PhysicalASAPDAGNode,
    inputs: &[ProducerOutput<'a>],
    per_second: f64,
    scan_read_ms: Option<u64>,
    evaluation_grid: Option<EvaluationGrid>,
    data: &DataWorkload,
    cost_inputs: &'a CostInputs,
) -> Result<NodeResourceUsageAndCost<'a>, AnalyticalCostError> {
    let window_ms = match node.output_state.timing {
        ExecutionTiming::QueryTime => scan_read_ms,
        ExecutionTiming::IngestionTime => ingestion_window_ms(physical, v),
    };
    node_resource_usage_and_cost(
        node,
        inputs,
        (1, per_second),
        window_ms,
        evaluation_grid,
        data,
        cost_inputs,
    )
}

/// The time between two outputs of ingestion-time candidate `v`: the
/// width of its refresh window, by the rule the installed plan refreshes
/// it on (`RefreshWindow::computed_from` over the candidates, so
/// `plan_cost` charges the same); `None` without one (one output per
/// second).
fn ingestion_window_ms(
    physical: &PhysicalASAPMemoCandidates,
    v: PhysicalCandidateId,
) -> Option<u64> {
    RefreshWindow::computed_from(physical, v).map(|window| window.width_ms)
}

/// A candidate's `quantity` from its usage: its own, and a query-time
/// scan's raw data on disk apart ([`cost_per_second`]).
fn quantity_of(
    quantity: Quantity,
    usage: &NodeResourceUsageAndCost<'_>,
    node: &PhysicalASAPDAGNode,
    cost_inputs: &CostInputs,
) -> Result<(f64, f64), String> {
    let query_time = usage.cost.timing == ExecutionTiming::QueryTime;
    // The ingest threads building partial copies of its newest window, as
    // `plan_cost` counts them: the deployment's `ingest_threads`, else one
    // per core of its own ingestion work (`plan_cost` also counts its
    // producers' work there, which the candidate alone does not know).
    let copies = || -> f64 {
        let own_cores = match query_time {
            true => 0.0,
            false => usage.work_ms_per_execution / 1_000.0,
        };
        (asap_cost::ingest_threads_building_copies(cost_inputs, own_cores) - 1) as f64
    };
    Ok(match quantity {
        Quantity::CostPerSecond => return cost_per_second(usage, cost_inputs),
        Quantity::QueryMsPerRun => (
            if query_time {
                usage.work_ms_per_execution
            } else {
                0.0
            },
            0.0,
        ),
        // With the merging of the partial copies of its newest window.
        Quantity::RefreshWallMsPerWindow => {
            let refresh = &usage.cost.phase_times;
            let compaction = match refresh.refresh_window_width_ms {
                Some(_) => {
                    copies()
                        * usage.output.rows as f64
                        * asap_cost::partial_copy_merge_ms(node, usage, cost_inputs)
                }
                None => 0.0,
            };
            (refresh.refresh_wall_ms_per_window + compaction, 0.0)
        }
        Quantity::CpuCoreSecondsPerSecond => (usage.cost.cpu_core_seconds_per_second, 0.0),
        // With the partial copies of an in-memory state's newest window.
        Quantity::PeakMemoryBytes => (
            usage.peak_memory_bytes as f64 + copies() * usage.maintained.unwrap_or(0) as f64,
            0.0,
        ),
        Quantity::DiskBytes => (
            usage.disk_bytes as f64,
            usage.raw_scan.map_or(0.0, |(_, bytes)| bytes as f64),
        ),
    })
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
