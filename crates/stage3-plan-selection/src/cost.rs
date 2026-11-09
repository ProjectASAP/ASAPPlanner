//! The objective's coefficients (concepts doc, sections 3 and 5, "Cost
//! and retention") under cost model 1, from `asap-cost`: per execution of
//! a candidate in a context ([`crate::milp::execution_contexts`]), per
//! edge in a context, and per amount a candidate retains:
//!
//! $$\sum_v W^{\mathrm{mat}}_v x^{\mathrm{mat}}_v + \sum_{\kappa,v}
//! \rho_\kappa c_{\kappa,v} x_{\kappa,v} + \sum_{\kappa,u,c,i}
//! \rho_\kappa c_{\kappa,u,c,i} y_{\kappa,u,c,i} + \sum_v \pi_v h_v.$$
//!
//! Each candidate is costed as one plan node ([`candidate_node`],
//! [`asap_cost::node_usage`]) over a baseline of each input: the most any
//! producer it may read (on its admissible edges) outputs (rows, bytes and,
//! for a state, its θ), at its execution's rate $\rho_\kappa$
//! (`executions_per_second`). That does not depend on the producers
//! chosen. An ingestion-time consumer reads one batch of raw data, also
//! from a `Scan` materialized at ingestion time ([`reads_one_batch_of`]). The part of a consumer's cost that changes with the producer
//! read is on the edge: $\rho_\kappa c_{\kappa,u,c,i}$ is the consumer's
//! cost in $\kappa$ with input $i$ read from $u$'s own output, minus its
//! own. It is exact when a consumer's cost changes with one input's
//! producer only (every consumer of a state reads one state input); a state
//! candidate without θ of its own (a merge, a `TimeRange` over a chain)
//! outputs the most of its input group's θ, so a consumer two hops above a
//! sketch build is costed at the largest θ below it.
//!
//! A candidate that retains time-indexed rows ([`retains_rows`]) is costed
//! retaining the most its consumers read back ([`retention_need_ms`]), the
//! largest $h_v$ (R) allows. Retention is linear in that amount: $\pi_v$ is
//! what retaining one more window (or second) costs there, and $W^{\mathrm{mat}}_v$
//! of a materialized candidate is its cost less $\pi_v h_v$; a query-time
//! scan's executions keep reading what it retains, and only its raw data on
//! disk is in $\pi_v h_v$. A candidate one of whose consumers reads it
//! unbounded (a `Scan` read whole, a `TimeShift` anchored with `@`) has no
//! $h_v$: it is costed retaining everything.

use std::collections::{BTreeSet, HashMap};

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

use crate::milp::execution_contexts::{executions_per_second, Execution, ExecutionContexts};
use crate::milp::model::QueryId;

/// The objective's coefficients (concepts doc, section 5, "Cost and
/// retention"), per second.
pub(crate) struct ExecutionCosts {
    /// $\rho_\kappa c_{\kappa,v}$ of each execution, $W^{\mathrm{mat}}_v$
    /// of an execution in `Materialized(v)`, by position in
    /// `ExecutionContexts::executions`; or why `asap-cost` could not cost
    /// it (the MILP leaves it out).
    pub(crate) per_execution: Vec<Result<f64, String>>,
    /// $\rho_\kappa c_{\kappa,u,c,i}$ of each edge, by position in
    /// `ExecutionContexts::edges`; `None` when it could not be costed (the
    /// MILP leaves it out).
    pub(crate) per_edge: Vec<Option<f64>>,
    /// $\pi_v$, per ms retained, of each candidate that retains rows
    /// ([`retains_rows`]) only within a bound: it has $h_v$ and
    /// constraint (R). One whose consumer reads it unbounded is costed
    /// retaining everything, in its executions' costs.
    pub(crate) retention_cost_per_second: HashMap<PhysicalCandidateId, f64>,
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
/// reaching the `TimeRange`; a tumbling-window chain materialized across
/// evaluations reads its newest window back, and one that is not what its
/// own consumers read. A consumer that reads it otherwise reads it at the
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
            // A materialized chain builds its newest window from it; one
            // that is not builds, in each run, every window its own
            // consumers read.
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => match physical.physical_candidates[consumer.0]
                .physical_properties
                .lifetime()
            {
                Lifetime::AcrossEvaluations(_) => {
                    offset_ms.checked_add(i64::try_from(*tumbling_window_width_ms).ok()?)?
                }
                Lifetime::None | Lifetime::OneEvaluation(_) => {
                    pending.extend(
                        consumers_of(consumer)
                            .into_iter()
                            .map(|edge| (physical.edge(edge).consumer_candidate_id, offset_ms)),
                    );
                    continue;
                }
            },
            _ if reads_raw_data => return None,
            _ => offset_ms.checked_add(span_ms(consumer))?,
        };
        need_ms = need_ms.max(read_ms);
    }
    u64::try_from(need_ms).ok()
}

/// The objective's coefficients of every execution and edge of `contexts`
/// ([`ExecutionCosts`]; module doc). `query_outputs` are the executions
/// that may answer each query output. Errs on nothing: what `asap-cost`
/// cannot cost is reported in [`ExecutionCosts::per_execution`] or left
/// out of [`ExecutionCosts::per_edge`].
pub(crate) fn execution_costs(
    physical: &PhysicalASAPMemoCandidates,
    contexts: &ExecutionContexts,
    query_outputs: &[(QueryId, Vec<Execution>)],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    cost_inputs: &CostInputs,
    formula_scale: f64,
) -> ExecutionCosts {
    let memo = &physical.logical_candidates.memo;
    let count = physical.physical_candidates.len();
    // The most any consumer of each candidate that retains rows reads back
    // ($h_v$ at its largest): `None` when one reads it unbounded.
    let mut most_read_ms: HashMap<PhysicalCandidateId, Option<u64>> = (0..count)
        .map(PhysicalCandidateId)
        .filter(|&v| retains_rows(physical, v))
        .map(|v| (v, Some(0)))
        .collect();
    let mut read = |v: PhysicalCandidateId, need_ms: Option<u64>| {
        if let Some(most) = most_read_ms.get_mut(&v) {
            *most = most.zip(need_ms).map(|(most, need)| most.max(need));
        }
    };
    for &(_, edge, producer) in contexts.edges() {
        read(
            producer.candidate,
            edge_retention_need_ms(physical, entries, edge),
        );
    }
    for (query, executions) in query_outputs {
        for execution in executions {
            read(
                execution.candidate,
                Some(output_retention_need_ms(&entries[query.0])),
            );
        }
    }
    // Each candidate's node, its inputs numbered 0.. (the ids do not
    // matter to `node_usage`), retaining the most its consumers read back.
    let node_retaining = |v: PhysicalCandidateId, retention_ms: Option<u64>| {
        let inputs: Vec<PhysicalASAPNodeId> = (0..input_count(physical, v)).collect();
        let mut node = candidate_node(physical, v, v.0, &inputs);
        node.retention_ms = retention_ms;
        node
    };
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
    let cost = |v: PhysicalCandidateId,
                node: &PhysicalASAPDAGNode,
                inputs: &[ProducerOutput<'_>],
                per_second: f64| {
        cost_node(
            physical,
            v,
            node,
            inputs,
            per_second,
            data,
            cost_inputs,
            formula_scale,
        )
        .and_then(|usage| cost_per_second(&usage, cost_inputs))
    };
    // Each candidate's usage of one computation (what it outputs does not
    // depend on how often it runs) over its baseline inputs.
    let mut usage: Vec<Option<Result<NodeUsage<'_>, String>>> = (0..count).map(|_| None).collect();
    let mut baseline: Vec<Option<Vec<ProducerOutput<'_>>>> = vec![None; count];
    for group in order {
        for &v in &candidates_of_group[group.0 as usize] {
            baseline[v.0] = baseline_inputs(memo, physical, &group_payloads, &usage, v);
            usage[v.0] = Some(match &baseline[v.0] {
                Some(inputs) => cost_node(
                    physical,
                    v,
                    &nodes[v.0],
                    inputs,
                    1.0,
                    data,
                    cost_inputs,
                    formula_scale,
                ),
                None => Err("an input has no costable producer".to_string()),
            });
        }
    }
    // Each execution's (own, raw data) cost per second at its rate, over
    // its baseline inputs.
    let at_rate: Vec<Result<(f64, f64, f64), String>> = contexts
        .executions()
        .iter()
        .map(|&execution| {
            let v = execution.candidate;
            if let Some(Err(error)) = &usage[v.0] {
                return Err(error.clone());
            }
            let per_second = executions_per_second(
                physical,
                entries,
                contexts,
                execution,
                cost_inputs.non_repeating_query_horizon_seconds,
            )?;
            let inputs = baseline[v.0]
                .as_ref()
                .ok_or("it reads a group on a cycle of candidates")?;
            let (own, raw_data) = cost(v, &nodes[v.0], inputs, per_second)?;
            Ok((own, raw_data, per_second))
        })
        .collect();
    // π_v of a candidate retaining rows every consumer reads within a bound:
    // what one more Δ ms of retention costs, over Δ (a window of a chain,
    // else a second), at its retention's largest; its whole own cost when
    // it is materialized, its raw data otherwise (each of its runs reads
    // what it reads at that retention, ρ_κ c_{κ,v}).
    // The one execution of each materialized candidate, by position.
    let own_execution: HashMap<PhysicalCandidateId, usize> = contexts
        .executions()
        .iter()
        .enumerate()
        .filter(|(_, execution)| {
            matches!(
                physical.physical_candidates[execution.candidate.0]
                    .physical_properties
                    .lifetime(),
                Lifetime::AcrossEvaluations(_)
            )
        })
        .map(|(index, execution)| (execution.candidate, index))
        .collect();
    let mut retention_cost_per_second = HashMap::new();
    for (&v, &most_read_ms) in &most_read_ms {
        let Some(retention_ms) = most_read_ms else {
            continue;
        };
        let candidate = &physical.physical_candidates[v.0];
        let materialized = matches!(
            candidate.physical_properties.lifetime(),
            Lifetime::AcrossEvaluations(_)
        );
        let delta_ms = candidate.chain_width_ms(physical).unwrap_or(1_000).max(1);
        let longer = node_retaining(v, Some(retention_ms + delta_ms));
        let per_ms = match materialized {
            true => own_execution.get(&v).and_then(|&index| {
                let (own, raw_data, per_second) = at_rate[index].as_ref().ok()?;
                let inputs = baseline[v.0].as_ref()?;
                let (longer_own, longer_raw_data) = cost(v, &longer, inputs, *per_second).ok()?;
                Some((longer_own + longer_raw_data - own - raw_data) / delta_ms as f64)
            }),
            false => usage[v.0]
                .as_ref()
                .and_then(|usage| usage.as_ref().ok())
                .and_then(|_| {
                    let inputs = baseline[v.0].as_ref()?;
                    let (_, raw_data) = cost(v, &nodes[v.0], inputs, 1.0).ok()?;
                    let (_, longer_raw_data) = cost(v, &longer, inputs, 1.0).ok()?;
                    Some((longer_raw_data - raw_data) / delta_ms as f64)
                }),
        };
        if let Some(per_ms) = per_ms {
            retention_cost_per_second.insert(v, per_ms.max(0.0));
        }
    }
    let per_execution: Vec<Result<f64, String>> = contexts
        .executions()
        .iter()
        .zip(&at_rate)
        .map(|(execution, at_rate)| {
            let &(own, raw_data, _) = at_rate.as_ref().map_err(Clone::clone)?;
            let v = execution.candidate;
            let materialized = matches!(
                physical.physical_candidates[v.0]
                    .physical_properties
                    .lifetime(),
                Lifetime::AcrossEvaluations(_)
            );
            Ok(match (retention_cost_per_second.get(&v), materialized) {
                (None, _) => own + raw_data,
                (Some(per_ms), true) => {
                    let retention_ms = most_read_ms[&v].unwrap_or(0) as f64;
                    own + raw_data - per_ms * retention_ms
                }
                (Some(_), false) => own,
            })
        })
        .collect();
    let per_edge = contexts
        .edges()
        .iter()
        .map(|&(consumer, edge, producer)| {
            let e = physical.edge(edge);
            let (u, c) = (e.producer_candidate_id, e.consumer_candidate_id);
            let consumer_index = contexts.index_of(consumer);
            per_execution[contexts.index_of(producer)].as_ref().ok()?;
            let &(own, raw_data, per_second) = at_rate[consumer_index].as_ref().ok()?;
            let produced = usage[u.0].as_ref()?.as_ref().ok()?;
            let mut inputs = baseline[c.0].clone()?;
            if !reads_one_batch_of(physical, c, u) {
                inputs[e.input_index_of_consumer] = producer_output(&nodes[u.0], produced);
            }
            let (with_u_own, with_u_raw_data) = cost(c, &nodes[c.0], &inputs, per_second).ok()?;
            Some(with_u_own + with_u_raw_data - own - raw_data)
        })
        .collect();
    ExecutionCosts {
        per_execution,
        per_edge,
        retention_cost_per_second,
    }
}

/// $\operatorname{need}_{\kappa,u,c,i}$ of `edge`: how far back its
/// producer's rows are read through it ([`retention_need_ms`] over the
/// admissible edges above its consumer); `None`, unbounded.
pub(crate) fn edge_retention_need_ms(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    edge: PhysicalEdgeId,
) -> Option<u64> {
    retention_need_ms(
        physical,
        entries,
        physical.edge(edge).producer_candidate_id,
        &[edge],
        &|consumer| physical.admissible_edges_out_of(consumer).to_vec(),
    )
}

/// How far back a query reads the output it reads directly: its
/// range-evaluation span (0 without one).
pub(crate) fn output_retention_need_ms(entry: &QueryWorkloadEntry) -> u64 {
    entry.range_evaluation.map_or(0, |range| range.span_ms)
}

/// Candidate `v`'s baseline inputs, by input index: over the producers it
/// may read (its admissible edges) whose usage is known, the most any
/// outputs ([`logical_output`]); `None` when an input has none. A producer
/// of which `v` reads one batch ([`reads_one_batch_of`]) is left out.
fn baseline_inputs<'a>(
    memo: &'a Memo,
    physical: &PhysicalASAPMemoCandidates,
    group_payloads: &'a [PhysicalASAPOperatorPayload],
    usage: &[Option<Result<NodeUsage<'a>, String>>],
    v: PhysicalCandidateId,
) -> Option<Vec<ProducerOutput<'a>>> {
    input_groups(memo, physical, v)
        .iter()
        .enumerate()
        .map(|(i, input)| {
            logical_output(
                &group_payloads[input.0 as usize],
                &memo.groups()[input.0 as usize].group_properties.schema,
                physical
                    .admissible_edges_into(v, i)
                    .iter()
                    .map(|&edge| physical.edge(edge).producer_candidate_id)
                    .filter(|&u| !reads_one_batch_of(physical, v, u))
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
    physical: &PhysicalASAPMemoCandidates,
    c: PhysicalCandidateId,
    u: PhysicalCandidateId,
) -> bool {
    let ingestion_time = |v: PhysicalCandidateId| {
        physical.physical_candidates[v.0]
            .physical_properties
            .timing()
            == ExecutionTiming::IngestionTime
    };
    ingestion_time(c) && ingestion_time(u) && retains_rows(physical, u) && {
        let scan = physical.physical_candidates[u.0].logical_candidate_id;
        matches!(
            physical
                .logical_candidates
                .memo
                .candidate(scan)
                .expression
                .operator,
            Operator::NonASAP(NonASAPOp::Scan { .. })
        )
    }
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

/// An input's output over its producers' `usages`: the most
/// rows and bytes any outputs, the state family of the one with the most
/// bytes, the sources any reads and the columns every one leaves unchanged;
/// `payload` and `schema` the input group's own. `None` without a costed
/// producer.
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

/// Candidate `v`'s usage as `node` over `inputs`, computed `per_second`
/// times per second.
#[allow(clippy::too_many_arguments)]
fn cost_node<'a>(
    physical: &PhysicalASAPMemoCandidates,
    v: PhysicalCandidateId,
    node: &'a PhysicalASAPDAGNode,
    inputs: &[ProducerOutput<'a>],
    per_second: f64,
    data: &DataWorkload,
    cost_inputs: &'a CostInputs,
    formula_scale: f64,
) -> Result<NodeUsage<'a>, String> {
    let memo = &physical.logical_candidates.memo;
    let candidate = &physical.physical_candidates[v.0];
    let ingestion = candidate.physical_properties.timing() == ExecutionTiming::IngestionTime;
    let ingestion_window_ms = match ingestion {
        false => None,
        true => candidate
            .chain_width_ms(physical)
            .or_else(|| window_ms_below(memo, candidate.logical_candidate_id.group)),
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

/// A candidate's cost per second from its usage under cost model 1: its
/// own, and the raw data a scan retains on disk.
fn cost_per_second(usage: &NodeUsage<'_>, cost_inputs: &CostInputs) -> Result<(f64, f64), String> {
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use asap_types::ir::operator::operator_properties::Reduction;
    use asap_types::ir::scalar::expr_ir::ColumnRef;
    use asap_types::ir::schema::{FieldDataType, SketchAlgorithm, SummaryUpdate};
    use asap_types::ir::{QueryRoot, TimeRangeKind};
    use asap_types::physical::{PhysicalProperties, StorageMedium};
    use asap_types::stages::{
        parameter_list, CandidateForMemoGroupId, LogicalASAPMemoCandidates, MemoQuery,
        PhysicalCandidate, PhysicalEdge,
    };
    use asap_types::test_support::{
        add_candidate, group_with_own_candidate, series_schema, time_series_scan,
    };
    use asap_types::workload::{
        Predictability, Query, QueryRecurrence, QueryRequirements, RepeatedDemand,
        RepetitionInterval, TimestampMs,
    };

    use super::*;

    const DAY_MS: u64 = 86_400_000;

    fn range(days: u64, child: MemoGroupId) -> Operator<MemoGroupId> {
        Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_millis(days * DAY_MS),
            kind: TimeRangeKind::Range,
            child,
        })
    }

    /// The group of `operator`, with its own candidate at θ `parameters`.
    fn kll_group(
        memo: &mut Memo,
        operator: Operator<MemoGroupId>,
        parameters: &asap_types::ir::schema::SketchParams,
    ) -> MemoGroupId {
        let group = memo.find_or_add_group(operator.clone()).unwrap();
        add_candidate(memo, group, operator, Some(parameters.clone())).unwrap();
        group
    }

    /// Example 3a's shape: a KLL state over the last five years of
    /// 1,000,000 series sampled every 15 s, once a day, either built over
    /// the scan's five years or merged from the windows of a 1-day chain
    /// built at ingestion time; a second daily query reads the chain's
    /// newest day of windows. The scan has a query-time candidate, one at
    /// ingestion time and one materialized there (C0, C1, C2); the chain
    /// (C3) may read C1 or C2, the five-year range (C4) C0 or C2.
    ///
    /// An ingestion-time consumer reads one batch of the scan, from C1 or
    /// C2 alike: the chain's baseline input is that batch, not the five
    /// years C2 retains for its query-time consumer. Selecting an
    /// execution, one of its edges and the least retention its consumers
    /// may need then never costs less than nothing (was −1,356,902/s: the
    /// chain costed over five years of rows, the edge from C1 −22,398,978
    /// and a retention cost per ms inflated by those rows).
    #[test]
    fn no_execution_reading_a_producer_costs_less_than_nothing() {
        let kll = &parameter_list(SketchAlgorithm::Kll)[6];
        let summary_type = FieldDataType::Sketch(SketchAlgorithm::Kll);
        let mut memo = Memo::default();
        let scan =
            group_with_own_candidate(&mut memo, time_series_scan("m", series_schema(&[]), vec![]));
        let chain = kll_group(
            &mut memo,
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                child: scan,
                summary_type: summary_type.clone(),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::PerEntity,
                filter: None,
                tumbling_window_width_ms: DAY_MS,
            }),
            kll,
        );
        let five_years = 5 * 365;
        let windows = group_with_own_candidate(&mut memo, range(five_years, chain));
        let newest_day = group_with_own_candidate(&mut memo, range(1, chain));
        let last_five_years = group_with_own_candidate(&mut memo, range(five_years, scan));
        let state = kll_group(
            &mut memo,
            Operator::ASAP(ASAPOp::SummaryAgg {
                child: last_five_years,
                summary_type,
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::PerEntity,
                filter: None,
            }),
            kll,
        );
        add_candidate(
            &mut memo,
            state,
            Operator::ASAP(ASAPOp::SummaryMerge {
                children: vec![windows],
                group_by: None,
            }),
            None,
        )
        .unwrap();
        for root in [state, newest_day] {
            memo.add_query(MemoQuery {
                root: QueryRoot::Operator(root),
                accuracy_target: None,
                values_per_evaluation: None,
            });
        }
        use ExecutionTiming::{IngestionTime, QueryTime};
        let materialized = Lifetime::AcrossEvaluations(StorageMedium::Memory);
        let candidate = |group, index_in_group, timing, lifetime| PhysicalCandidate {
            logical_candidate_id: CandidateForMemoGroupId {
                group,
                index_in_group,
            },
            physical_properties: PhysicalProperties::new(timing, lifetime).unwrap(),
        };
        let candidates = vec![
            candidate(scan, 0, QueryTime, Lifetime::None),
            candidate(scan, 0, IngestionTime, Lifetime::None),
            candidate(scan, 0, IngestionTime, materialized),
            candidate(chain, 0, IngestionTime, materialized),
            candidate(last_five_years, 0, QueryTime, Lifetime::None),
            candidate(windows, 0, QueryTime, Lifetime::None),
            candidate(state, 0, QueryTime, Lifetime::None),
            candidate(state, 1, QueryTime, Lifetime::None),
            candidate(newest_day, 0, QueryTime, Lifetime::None),
        ];
        let edge = |u: usize, c: usize, admissible: bool| PhysicalEdge {
            producer_candidate_id: PhysicalCandidateId(u),
            consumer_candidate_id: PhysicalCandidateId(c),
            input_index_of_consumer: 0,
            satisfies_logical_requirement: true,
            satisfies_physical_requirement: admissible,
        };
        let edges = vec![
            edge(0, 3, false),
            edge(1, 3, true),
            edge(2, 3, true),
            edge(0, 4, true),
            edge(1, 4, false),
            edge(2, 4, true),
            edge(3, 5, true),
            edge(3, 8, true),
            edge(4, 6, true),
            edge(5, 7, true),
        ];
        let groups = memo.groups().len();
        let physical = PhysicalASAPMemoCandidates::new(
            LogicalASAPMemoCandidates {
                memo,
                edges: vec![],
                pruned: vec![],
                evaluation_grid_of_group: vec![None; groups],
            },
            candidates,
            edges,
            vec![],
        );
        let daily = QueryWorkloadEntry {
            query: Query("daily".into()),
            requirements: QueryRequirements::default(),
            predictability: Predictability::default(),
            recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
                interval: RepetitionInterval(DAY_MS as u32),
                evaluation_phase: TimestampMs(0),
            }),
            range_evaluation: None,
        };
        let entries = vec![daily.clone(), daily];
        let data: DataWorkload = serde_json::from_value(serde_json::json!({
            "arrival": {"ingesting": {"rows_per_second": 1_000_000.0 / 15.0}},
            "sources": {"m": {"series_count": 1_000_000, "sample_interval": 15_000}}
        }))
        .unwrap();
        let (contexts, query_outputs) = ExecutionContexts::new(
            &physical,
            &entries,
            &[(QueryId(0), state), (QueryId(1), newest_day)],
        );
        let costs = execution_costs(
            &physical,
            &contexts,
            &query_outputs,
            &entries,
            &data,
            &CostInputs::default(),
            1.0,
        );
        // π_v times the least any consumer of `v` reads back, the least
        // retention (R) lets a plan selecting `v` keep.
        let least_retention = |v: PhysicalCandidateId| {
            let least_need_ms = contexts
                .edges()
                .iter()
                .filter(|(_, _, producer)| producer.candidate == v)
                .filter_map(|&(_, edge, _)| edge_retention_need_ms(&physical, &entries, edge))
                .min()
                .unwrap_or(0);
            costs
                .retention_cost_per_second
                .get(&v)
                .copied()
                .unwrap_or(0.0)
                * least_need_ms as f64
        };
        let mut checked = 0;
        for (index, &(consumer, edge, _)) in contexts.edges().iter().enumerate() {
            let (Ok(execution), Some(edge_cost)) = (
                &costs.per_execution[contexts.index_of(consumer)],
                costs.per_edge[index],
            ) else {
                continue;
            };
            let total = execution + edge_cost + least_retention(consumer.candidate);
            assert!(
                total >= 0.0,
                "{:?} reading through {edge:?}: {execution} + {edge_cost} + retention = {total}",
                consumer.candidate
            );
            checked += 1;
        }
        // Among them the chain reading either scan.
        assert!(checked >= 2, "{checked} edges costed");
    }
}
