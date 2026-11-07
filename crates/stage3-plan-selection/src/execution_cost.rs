//! The costs of the selection MILP's objective under cost model 1, from
//! `asap-cost` (concepts doc, section 5, "Cost and retention"):
//!
//! $$\sum_v W^{\mathrm{mat}}_v x^{\mathrm{mat}}_v + \sum_{\kappa,v}
//! \rho_\kappa c_{\kappa,v} x_{\kappa,v} + \sum_v \pi_v h_v.$$
//!
//! Each execution ([`Execution`]) is costed as one node of a plan
//! ([`asap_cost::node_usage`]) over one representative producer per input,
//! at its context's executions per second $\rho_\kappa$
//! ([`executions_per_second`]):
//!
//! - $\rho_\kappa c_{\kappa,v}$: a candidate with $\ell$ = None or
//!   `OneEvaluation` run in $\kappa$, including a `OneEvaluation` result's
//!   storage within its evaluation and the raw data a query-time scan
//!   retains on disk. A query-time scan reads the windows of the consumers
//!   that may read it in $\kappa$. The range a tumbling-window chain is
//!   built over costs nothing: the realizer builds the chain over the
//!   range's input.
//! - $W^{\mathrm{mat}}_v$: an `AcrossEvaluations` candidate's own
//!   computation, one build per tumbling window, retaining no more windows
//!   than its range holds; $\pi_v$: the cost of retaining one more window.
//!   So $W^{\mathrm{mat}}_v + \pi_v h_v$ is its cost retaining $h_v$
//!   windows.
//!
//! Query latency is ignored (decided 2026-11-06): a result counts as read
//! when its evaluation starts, so the memory a query-time result holds
//! while its evaluation runs, which `asap_cost::plan_cost` charges for the
//! latency of the queries reading it, is left out.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use asap_cost::execution_demand::evaluation_rate;
use asap_cost::{node_usage, CostInputs, CostModel, NodeUsage, ProducerOutput, StateFamily};
use asap_types::ir::physical_export::{PhysicalASAPDAGNode, ScanWindow};
use asap_types::ir::properties::{DataPrimitive, ExecutionDataState};
use asap_types::ir::schema::{ExactKind, SketchParams};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorResultKind, TimeRangeKind};
use asap_types::physical::{ExecutionTiming, Lifetime};
use asap_types::stages::{
    ExecutionContext, ExecutionContextId, Memo, MemoGroupId, PhysicalASAPMemoCandidates,
    PhysicalCandidateId, WindowLayout,
};
use asap_types::workload::DataWorkload;

use crate::milp::execution_contexts::{Execution, ExecutionContexts, TumblingWindowChainGeometry};

/// The objective's costs per second: of each execution that could be
/// costed, and why the others could not ([`execution_costs`]).
#[derive(Debug, Clone, Default)]
pub(crate) struct ExecutionCosts {
    /// $\rho_\kappa c_{\kappa,v}$ of an [`Execution::InContext`];
    /// $W^{\mathrm{mat}}_v$ of an [`Execution::Materialized`].
    pub(crate) cost_per_second: HashMap<Execution, f64>,
    /// $\pi_v$, per window retained, of each costed `AcrossEvaluations`
    /// candidate.
    pub(crate) retention_cost_per_window_per_second: HashMap<PhysicalCandidateId, f64>,
    /// Why `asap-cost` could not cost an execution (the MILP leaves it out).
    pub(crate) uncostable: BTreeMap<Execution, String>,
    /// The producer execution each costed execution was costed over, per
    /// input: its cost assumes that producer's output.
    pub(crate) representative_producers: HashMap<Execution, Vec<Execution>>,
}

/// $\rho_\kappa$: the executions per second of `context`. An evaluation
/// runs at its queries' evaluation rate
/// (`asap_cost::execution_demand::evaluation_rate` of its recurrence and
/// range evaluation; a one-off query's invocations over the horizon); an
/// ingestion pipeline is costed over one second of arriving rows; an
/// `AcrossEvaluations` candidate's computation builds one tumbling window
/// per window width at query time, and is costed over one second of
/// arriving rows at ingestion time.
pub(crate) fn executions_per_second(
    physical: &PhysicalASAPMemoCandidates,
    context: &ExecutionContext,
    cost_inputs: &CostInputs,
) -> Result<f64, String> {
    match context {
        ExecutionContext::Evaluation {
            recurrence,
            range_evaluation,
        } => evaluation_rate(
            [(recurrence, *range_evaluation)],
            cost_inputs.non_repeating_query_horizon_seconds,
        )
        .map_err(|error| error.to_string()),
        ExecutionContext::IngestionPipeline(_) => Ok(1.0),
        ExecutionContext::Materialized(v) => {
            let candidate = &physical.physical_candidates[v.0];
            if candidate.physical_properties.timing() == ExecutionTiming::IngestionTime {
                return Ok(1.0);
            }
            match physical
                .logical_candidates
                .memo
                .candidate(candidate.logical_candidate_id)
                .logical_properties
                .window_layout()
            {
                Some(&WindowLayout::Tumbling { width_ms }) if width_ms > 0 => {
                    Ok(1_000.0 / width_ms as f64)
                }
                _ => Err("materialized across evaluations without tumbling windows".into()),
            }
        }
    }
}

/// The objective's costs of every execution of `contexts` ([`ExecutionCosts`]).
/// Executions are costed producers first (Kahn's order over the edges
/// $y_{\kappa,u,c,i}$ may select): an execution is costed once each of its
/// inputs has a costed producer, and its representative producer per input
/// is the first costed one, so the representatives never form a cycle. An
/// execution one of whose inputs never gets a costed producer is not
/// costable either.
pub(crate) fn execution_costs(
    physical: &PhysicalASAPMemoCandidates,
    contexts: &ExecutionContexts,
    data: &DataWorkload,
    cost_inputs: &CostInputs,
    formula_scale: f64,
) -> ExecutionCosts {
    let memo = &physical.logical_candidates.memo;
    let executions = contexts.executions();
    let index: HashMap<Execution, usize> = executions
        .iter()
        .enumerate()
        .map(|(index, &execution)| (execution, index))
        .collect();
    let input_count = |execution: Execution| {
        memo.candidate(physical.physical_candidates[execution.candidate().0].logical_candidate_id)
            .expression
            .operator
            .children()
            .len()
    };
    let edges: Vec<_> = physical.edges().map(|(_, edge)| edge).collect();
    // Per execution and input, the producer executions it may read; per
    // execution, the (consumer, input) it may supply.
    let mut producers_of: HashMap<(usize, usize), Vec<usize>> = HashMap::new();
    let mut supplies: Vec<Vec<(usize, usize)>> = vec![Vec::new(); executions.len()];
    for &(context, edge) in contexts.edges() {
        let edge = &edges[edge.0];
        let consumer =
            index[&ExecutionContexts::execution_in(physical, context, edge.consumer_candidate_id)];
        let producer =
            index[&ExecutionContexts::execution_in(physical, context, edge.producer_candidate_id)];
        producers_of
            .entry((consumer, edge.input_index_of_consumer))
            .or_default()
            .push(producer);
        supplies[producer].push((consumer, edge.input_index_of_consumer));
    }
    let rate_of_context: Vec<Result<f64, String>> = contexts
        .contexts()
        .iter()
        .map(|context| executions_per_second(physical, context, cost_inputs))
        .collect();
    // The groups with an execution in each context, and the range each
    // tumbling-window chain reads in its computation context: the
    // realizer builds the chain over the range's input, so the range is
    // not run.
    let groups_in_context: HashSet<(ExecutionContextId, MemoGroupId)> = executions
        .iter()
        .filter_map(|&execution| match execution {
            Execution::InContext(context, v) => Some((
                context,
                physical.physical_candidates[v.0].logical_candidate_id.group,
            )),
            Execution::Materialized(_) => None,
        })
        .collect();
    let mut chain_range_of_context: HashMap<ExecutionContextId, Vec<(MemoGroupId, u64)>> =
        HashMap::new();
    for &execution in executions {
        if let Execution::Materialized(v) = execution {
            if let (Some(context), Some(chain)) = (
                contexts.computation_context(v),
                TumblingWindowChainGeometry::of(memo, v, physical),
            ) {
                chain_range_of_context
                    .entry(context)
                    .or_default()
                    .push((chain.range_input, chain.width_ms));
            }
        }
    }
    let mut costs = ExecutionCosts::default();
    let mut costed = vec![false; executions.len()];
    let mut input_without_producer: Vec<Vec<bool>> = executions
        .iter()
        .map(|&execution| vec![true; input_count(execution)])
        .collect();
    let mut missing: Vec<usize> = executions.iter().map(|&e| input_count(e)).collect();
    let mut ready: Vec<usize> = (0..executions.len())
        .filter(|&execution| missing[execution] == 0)
        .collect();
    let mut representative: Vec<Vec<usize>> = vec![Vec::new(); executions.len()];
    let mut nodes: Vec<Option<(PhysicalASAPDAGNode, UsageOutput)>> =
        (0..executions.len()).map(|_| None).collect();
    let mut uncostable: Vec<Option<String>> =
        vec![
            Some("one of its inputs has no producer that can be costed".to_string());
            executions.len()
        ];
    while let Some(at) = ready.pop() {
        let execution = executions[at];
        let producers: Vec<usize> = (0..input_count(execution))
            .map(|input| {
                producers_of[&(at, input)]
                    .iter()
                    .copied()
                    .find(|&producer| costed[producer])
                    .expect("ready: every input has a costed producer")
            })
            .collect();
        let context = contexts
            .input_context(execution)
            .expect("a reached execution has a context");
        let costing = (|| {
            let rate = rate_of_context[context.0].clone()?;
            let candidate = execution.candidate();
            let group = physical.physical_candidates[candidate.0]
                .logical_candidate_id
                .group;
            let chain_ranges = chain_range_of_context
                .get(&context)
                .map_or(&[][..], Vec::as_slice);
            let scan_window = scan_window_in_context(memo, group, |parent| {
                if let Some(&(_, width_ms)) =
                    chain_ranges.iter().find(|(range, _)| *range == parent)
                {
                    return Some(Some(width_ms));
                }
                groups_in_context
                    .contains(&(context, parent))
                    .then_some(None)
            });
            let bypassed = matches!(execution, Execution::InContext(..))
                && chain_ranges.iter().any(|(range, _)| *range == group);
            cost_execution(
                physical,
                at,
                candidate,
                &producers,
                &representative,
                &nodes,
                (rate, scan_window),
                data,
                cost_inputs,
                formula_scale,
            )
            .map(|(node, mut output, retention)| {
                if bypassed {
                    output.cost_per_second = 0.0;
                }
                (node, output, retention)
            })
        })();
        representative[at] = producers;
        let (node, output, retention) = match costing {
            Ok(costed_node) => costed_node,
            Err(error) => {
                uncostable[at] = Some(error);
                continue;
            }
        };
        uncostable[at] = None;
        costs
            .cost_per_second
            .insert(execution, output.cost_per_second);
        costs.representative_producers.insert(
            execution,
            representative[at].iter().map(|&p| executions[p]).collect(),
        );
        if let Some(retention) = retention {
            costs
                .retention_cost_per_window_per_second
                .insert(execution.candidate(), retention);
        }
        nodes[at] = Some((node, output));
        costed[at] = true;
        for &(consumer, input) in &supplies[at] {
            if std::mem::take(&mut input_without_producer[consumer][input]) {
                missing[consumer] -= 1;
                if missing[consumer] == 0 {
                    ready.push(consumer);
                }
            }
        }
    }
    costs.uncostable = executions
        .iter()
        .zip(uncostable)
        .filter_map(|(&execution, reason)| reason.map(|reason| (execution, reason)))
        .collect();
    costs
}

/// What a costed execution's consumers read of it ([`ProducerOutput`]
/// without its borrowed node), and its cost per second at its context's
/// rate: $\rho_\kappa c_{\kappa,v}$, or $W^{\mathrm{mat}}_v$.
struct UsageOutput {
    output: asap_cost::EdgeStatistics,
    sources: BTreeSet<String>,
    family: OwnedStateFamily,
    unchanged_columns: BTreeSet<String>,
    cost_per_second: f64,
}

/// Cost execution `at` of `candidate` over `producers` (its input `i` read
/// from execution `producers[i]`, already costed: `nodes[producers[i]]`),
/// at `rate` executions per second, a query-time scan reading
/// `scan_window`: its node, what its consumers read of it, and for an
/// `AcrossEvaluations` candidate $\pi_v$ (its cost retaining one window
/// more than its range holds, less its cost retaining those). Its cost
/// retaining as many windows as its range holds is
/// $W^{\mathrm{mat}}_v + \pi_v \cdot$ that count: the objective prices
/// $h_v$ windows at $\pi_v$ each over a fixed cost
/// $W^{\mathrm{mat}}_v$ = that cost less $\pi_v$ times the count.
#[allow(clippy::too_many_arguments)]
fn cost_execution(
    physical: &PhysicalASAPMemoCandidates,
    at: usize,
    candidate: PhysicalCandidateId,
    producers: &[usize],
    representative: &[Vec<usize>],
    nodes: &[Option<(PhysicalASAPDAGNode, UsageOutput)>],
    (rate, scan_window): (f64, ScanWindow),
    data: &DataWorkload,
    cost_inputs: &CostInputs,
    formula_scale: f64,
) -> Result<(PhysicalASAPDAGNode, UsageOutput, Option<f64>), String> {
    let memo = &physical.logical_candidates.memo;
    let node = candidate_node(physical, candidate.0, at, producers)?;
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
            let (node, output) = nodes[producer].as_ref().expect("costed");
            ProducerOutput {
                payload: &node.payload,
                output_schema: &node.output_schema,
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
    let physical_candidate = &physical.physical_candidates[candidate.0];
    let group = physical_candidate.logical_candidate_id.group;
    let ingestion_window_ms = (physical_candidate.physical_properties.timing()
        == ExecutionTiming::IngestionTime)
        .then(|| window_ms_below(memo, group))
        .flatten();
    let windows = asap_cost::tumbling_windows_of(&node);
    let usage_retaining = |retained_windows: u64| {
        node_usage(
            &node,
            &producer_outputs,
            (1, rate),
            scan_window,
            retained_windows,
            ingestion_window_ms,
            data,
            cost_inputs,
            formula_scale,
        )
        .map_err(|error| error.to_string())
    };
    let usage = usage_retaining(windows)?;
    let cost = cost_per_second(&usage, cost_inputs)?;
    let retention = match physical_candidate.physical_properties.lifetime() {
        Lifetime::AcrossEvaluations(_) => {
            let retaining_one_more = cost_per_second(&usage_retaining(windows + 1)?, cost_inputs)?;
            Some(retaining_one_more - cost)
                .filter(|cost| cost.is_finite() && *cost >= 0.0)
                .map(Some)
                .ok_or("its cost per retained window is not a finite non-negative number")?
        }
        Lifetime::None | Lifetime::OneEvaluation(_) => None,
    };
    // W^mat_v: its cost retaining its range's windows, less those windows'
    // retention, which pi_v h_v prices.
    let cost = cost - retention.unwrap_or(0.0) * windows as f64;
    let owned = |usage: &NodeUsage<'_>, cost_per_second| UsageOutput {
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
    if !matches!(
        node.payload,
        Operator::ASAP(ASAPOp::SummaryTumblingWindowChain { .. })
    ) {
        let output = owned(&usage, cost);
        return Ok((node, output, retention));
    }
    // The realizer reads a chain through a merge of all its windows, at
    // query time: its consumers read the merge. A chain run in its
    // consumers' context (None or `OneEvaluation`) runs the merge there
    // too; the merge over a chain materialized across evaluations runs
    // once per context reading it, which no execution's cost can hold (an
    // inexact term of the objective).
    let merge = PhysicalASAPDAGNode {
        id: at,
        payload: Operator::ASAP(ASAPOp::SummaryMerge {
            children: vec![at],
            newest_tumbling_windows: None,
            group_by: None,
        }),
        output_state: ExecutionDataState {
            timing: ExecutionTiming::QueryTime,
            primitive: DataPrimitive::SummaryState,
        },
        lifetime: Lifetime::None,
        parameters: None,
        output_schema: node.output_schema.clone(),
        guarantee: None,
        retention_ms: None,
        summary_coverage: None,
    };
    let chain_output = owned(&usage, cost);
    let chain = ProducerOutput {
        payload: &node.payload,
        output_schema: &node.output_schema,
        output: usage.output,
        sources: usage.sources.clone(),
        family: usage.family,
        unchanged_columns: usage.unchanged_columns.clone(),
    };
    let merge_usage = node_usage(
        &merge,
        &[chain],
        (1, rate),
        ScanWindow::Unbounded,
        1,
        None,
        data,
        cost_inputs,
        formula_scale,
    )
    .map_err(|error| error.to_string())?;
    let merge_cost = match retention {
        Some(_) => 0.0,
        None => cost_per_second(&merge_usage, cost_inputs)?,
    };
    let output = owned(&merge_usage, chain_output.cost_per_second + merge_cost);
    Ok((merge, output, retention))
}

/// The rows a query-time `Scan` of `group` reads in a context, back from
/// the evaluation time: the union of the windows of the ranges over it
/// that run in the context, through time shifts without an `@` anchor
/// (`PhysicalASAPDAG::scan_window` on the memo's groups instead of a
/// plan's nodes). `in_context(parent)` is `None` for a parent group that
/// does not run in the context, `Some(Some(w))` for the range a
/// tumbling-window chain of width `w` is built over (it reads its newest
/// window), `Some(None)` otherwise. `Unbounded` when another operator in
/// the context reads it or none does.
fn scan_window_in_context(
    memo: &Memo,
    group: MemoGroupId,
    in_context: impl Fn(MemoGroupId) -> Option<Option<u64>>,
) -> ScanWindow {
    fn windows(
        memo: &Memo,
        group: MemoGroupId,
        end_offset_ms: i64,
        in_context: &dyn Fn(MemoGroupId) -> Option<Option<u64>>,
        seen: &mut BTreeSet<MemoGroupId>,
    ) -> ScanWindow {
        if !seen.insert(group) {
            return ScanWindow::Unbounded;
        }
        let mut union: Option<ScanWindow> = None;
        for &parent in &memo.groups()[group.0 as usize].parents {
            let Some(chain_width_ms) = in_context(parent) else {
                continue;
            };
            let window = match &memo.groups()[parent.0 as usize].expression {
                Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) => ScanWindow::Within {
                    end_offset_ms,
                    width_ms: i64::try_from(
                        chain_width_ms
                            .unwrap_or(u64::try_from(range.as_millis()).unwrap_or(u64::MAX)),
                    )
                    .unwrap_or(i64::MAX),
                },
                Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) if shift.at.is_none() => {
                    windows(
                        memo,
                        parent,
                        end_offset_ms.saturating_add(shift.offset_ms),
                        in_context,
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
    windows(memo, group, 0, &in_context, &mut BTreeSet::new())
}

/// [`StateFamily`] owned, so a costed candidate outlives its node's borrow.
enum OwnedStateFamily {
    None,
    Sketch(SketchParams),
    Exact(ExactKind),
}

/// Candidate `candidate_id` as plan node `node_id` over `producers` (its
/// input `i` read from node `producers[i]`): its operator, or
/// for a tumbling-window layout the chain the realizer builds; its timing,
/// lifetime and θ; its group's schema. Err for a tumbling-window layout the
/// realizer would refuse: not on a build over a range whose window its width
/// divides.
fn candidate_node(
    physical: &PhysicalASAPMemoCandidates,
    candidate_id: usize,
    node_id: usize,
    producers: &[usize],
) -> Result<PhysicalASAPDAGNode, String> {
    let memo = &physical.logical_candidates.memo;
    let candidate = &physical.physical_candidates[candidate_id];
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
        id: node_id,
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

/// An execution's cost per second from its usage under cost model 1, plus
/// the raw data a query-time scan retains on disk.
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
