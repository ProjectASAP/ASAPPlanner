//! Symmetry reduction of the selection MILP: a copy of an equivalent
//! evaluation context shares the original's variables
//! ([`copies_in_equivalent_contexts`]), so HiGHS does not branch over each
//! copy.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use asap_types::accuracy::AccuracyTarget;
use asap_types::stages::{
    ExecutionContext, ExecutionContextId, MemoGroupId, PhysicalCandidateId, PhysicalEdgeId,
};

use super::model::SelectionInputs;
use super::COST_RELATIVE_TOLERANCE;
use crate::selectable::QueryId;

/// Per execution, edge (by position) and query, the one whose MILP
/// variables it takes: itself unless it is in a copy of an equivalent
/// context ([`copies_in_equivalent_contexts`]).
///
/// Example: the same query every 60 s (Q0) and every 300 s (Q1): each
/// execution and edge of the 300 s context maps to its counterpart in the
/// 60 s one, and `original_of_query` is `[Q0, Q0]`.
pub(crate) struct SharedVariables {
    /// By position in `SelectableExecutions::executions`.
    pub(crate) original_of_execution: Vec<usize>,
    /// By position in `SelectableExecutions::edges`.
    pub(crate) original_of_edge: Vec<usize>,
    /// By [`QueryId`].
    pub(crate) original_of_query: Vec<QueryId>,
}

/// An execution as a root evaluation context sees it: its own, by position
/// among the context's executions, or one outside every evaluation context
/// (`Materialized(v)` or an ingestion pipeline), by position in
/// `SelectableExecutions::executions`, which every context reading it
/// shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum ExecutionInContext {
    Own(usize),
    Shared(usize),
}

/// What two equivalent contexts have alike, up to their costs' common
/// multiple $\alpha$: per execution (ascending, so by candidate) its
/// candidate, whether it is kept and costed, its least retention need, and
/// per input the memo edges into it from each producer, costed or not; and
/// the sorted signatures of its queries ([`QuerySignature`]).
#[derive(Debug, PartialEq, Eq, Hash)]
struct ContextKey<'a> {
    executions: Vec<ExecutionKey>,
    queries: Vec<QuerySignature<'a>>,
}

/// One execution's part of a [`ContextKey`].
#[derive(Debug, PartialEq, Eq, Hash)]
struct ExecutionKey {
    candidate: PhysicalCandidateId,
    kept: bool,
    costed: bool,
    least_need_ms: Option<u64>,
    /// Per input, each edge into it: (memo edge, producer, costed).
    inputs: Vec<Vec<(PhysicalEdgeId, ExecutionInContext, bool)>>,
}

/// A query's part of a [`ContextKey`]: the executions that may answer each
/// of its outputs, its exact-only groups, its range-evaluation span and
/// its accuracy target (by index among the workload's distinct targets,
/// [`AccuracyTarget`] holding floats: metric, ε, δ and scope all equal).
type QuerySignature<'a> = (
    Vec<Vec<ExecutionInContext>>,
    Option<&'a BTreeSet<MemoGroupId>>,
    u64,
    usize,
);

/// Which executions, edges and queries share variables
/// ([`SharedVariables`]): a root evaluation context (no request anchor)
/// equivalent to an earlier one is a copy of it, and so are its
/// executions, edges and queries. Two such contexts are equivalent when
/// they have the same [`ContextKey`] (the same candidates, kept alike, the
/// same memo edges from producers that correspond, in the context or the
/// same execution outside every evaluation context, and queries that pair
/// up with the same output executions, exact-only groups, span and
/// accuracy target, so the accuracy constraint holds alike in both) and
/// every cost in one is the same multiple $\alpha$ of the other's, to
/// [`COST_RELATIVE_TOLERANCE`]. Each query belongs to its own evaluation
/// context (`SelectableExecutions::evaluation_context`), wherever its
/// answering executions run. Contexts are bucketed by key, so only the
/// cost check compares two contexts.
///
/// Exact: from an optimal plan, give both contexts the selection of the
/// one whose own cost $A$ or $B$ is lower, say $A$. The copied context
/// then costs $\alpha A \le \alpha B$, the retention $h_v$ and raw data
/// $r_s$ (each the most of their readers' needs, equal in both copies) do
/// not grow, and an execution outside them that only the other selection
/// read is left out at no cost; with $B \le A$ symmetrically. So some
/// optimum selects alike in both, and sharing their variables keeps it.
///
/// Example: the same queries every 60 s and every 300 s (one user each):
/// the 300 s context's executions cost 1/5 of the 60 s ones' and share
/// their variables.
pub(crate) fn copies_in_equivalent_contexts(
    inputs: &SelectionInputs<'_>,
    kept: &[bool],
) -> SharedVariables {
    let selectable = inputs.selectable;
    let costs = inputs.costs;
    let executions = selectable.executions();
    let edges = selectable.edges();
    let mut shared = SharedVariables {
        original_of_execution: (0..executions.len()).collect(),
        original_of_edge: (0..edges.len()).collect(),
        original_of_query: (0..inputs.entries.len()).map(QueryId).collect(),
    };
    // Each root evaluation context's executions, ascending (so by
    // candidate), and each execution's position among them.
    let mut executions_of: BTreeMap<ExecutionContextId, Vec<usize>> = BTreeMap::new();
    let mut position_in_context: Vec<Option<usize>> = vec![None; executions.len()];
    for (index, execution) in executions.iter().enumerate() {
        if selectable.is_root_evaluation_context(execution.context) {
            let own = executions_of.entry(execution.context).or_default();
            position_in_context[index] = Some(own.len());
            own.push(index);
        }
    }
    let mut outputs_of: BTreeMap<QueryId, Vec<&[usize]>> = BTreeMap::new();
    for (query, answering) in selectable.answering() {
        outputs_of.entry(*query).or_default().push(answering);
    }
    let mut queries_of: BTreeMap<ExecutionContextId, Vec<QueryId>> = BTreeMap::new();
    for &query in outputs_of.keys() {
        if let Some(context) = selectable.evaluation_context(query) {
            queries_of.entry(context).or_default().push(query);
        }
    }
    // `index` as `context` sees it; `None` for one in another evaluation
    // context (a request context), which no copy shares.
    let in_context = |context: ExecutionContextId, index: usize| {
        let execution_context = executions[index].context;
        match selectable.context(execution_context) {
            _ if execution_context == context => {
                position_in_context[index].map(ExecutionInContext::Own)
            }
            ExecutionContext::Evaluation { .. } => None,
            ExecutionContext::IngestionPipeline(_) | ExecutionContext::Materialized(_) => {
                Some(ExecutionInContext::Shared(index))
            }
        }
    };
    // Each query's accuracy target, as an index among the distinct ones.
    let mut targets: Vec<&AccuracyTarget> = Vec::new();
    let target_of_query: Vec<usize> = inputs
        .entries
        .iter()
        .map(|entry| {
            let target = &entry.requirements.accuracy;
            targets
                .iter()
                .position(|&known| known == target)
                .unwrap_or_else(|| {
                    targets.push(target);
                    targets.len() - 1
                })
        })
        .collect();
    let signature = |context: ExecutionContextId, query: QueryId| -> Option<QuerySignature<'_>> {
        let outputs = outputs_of[&query]
            .iter()
            .map(|answering| {
                answering
                    .iter()
                    .map(|&index| in_context(context, index))
                    .collect::<Option<Vec<_>>>()
            })
            .collect::<Option<Vec<_>>>()?;
        Some((
            outputs,
            inputs
                .physical
                .logical_candidates
                .exact_only_groups_of_query
                .get(query.0),
            inputs.entries[query.0].range_span_ms(),
            target_of_query[query.0],
        ))
    };
    let key_of = |context: ExecutionContextId| -> Option<ContextKey<'_>> {
        let executions = executions_of[&context]
            .iter()
            .map(|&index| {
                let inputs = (0..selectable.input_count(index))
                    .map(|input| {
                        selectable
                            .edges_into(index, input)
                            .iter()
                            .map(|&position| {
                                let edge = edges[position];
                                Some((
                                    edge.edge,
                                    in_context(context, edge.producer)?,
                                    costs.per_edge[position].is_some(),
                                ))
                            })
                            .collect::<Option<Vec<_>>>()
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(ExecutionKey {
                    candidate: executions[index].candidate,
                    kept: kept[index],
                    costed: costs.per_execution[index].is_ok(),
                    least_need_ms: costs.least_need_ms_per_execution[index],
                    inputs,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let mut queries = queries_of
            .get(&context)
            .into_iter()
            .flatten()
            .map(|&query| signature(context, query))
            .collect::<Option<Vec<_>>>()?;
        queries.sort();
        Some(ContextKey {
            executions,
            queries,
        })
    };
    // Whether every cost of `copy` is the same multiple of `original`'s,
    // their executions and edges paired in order (equal keys).
    let proportional_costs = |original: ExecutionContextId, copy: ExecutionContextId| {
        let mut alpha: Option<f64> = None;
        let mut proportional = |a: f64, b: f64| match (a == 0.0, b == 0.0) {
            (true, true) => true,
            (false, false) => {
                let ratio = *alpha.get_or_insert(b / a);
                (b - ratio * a).abs() <= COST_RELATIVE_TOLERANCE * b.abs()
            }
            _ => false,
        };
        executions_of[&original]
            .iter()
            .zip(&executions_of[&copy])
            .all(|(&a, &b)| {
                let own = match (&costs.per_execution[a], &costs.per_execution[b]) {
                    (Ok(cost_a), Ok(cost_b)) => proportional(*cost_a, *cost_b),
                    _ => true,
                };
                own && (0..selectable.input_count(a)).all(|input| {
                    selectable
                        .edges_into(a, input)
                        .iter()
                        .zip(selectable.edges_into(b, input))
                        .all(|(&edge_a, &edge_b)| {
                            match (costs.per_edge[edge_a], costs.per_edge[edge_b]) {
                                (Some(cost_a), Some(cost_b)) => proportional(cost_a, cost_b),
                                _ => true,
                            }
                        })
                })
            })
    };
    let mut originals_of_key: HashMap<ContextKey<'_>, Vec<ExecutionContextId>> = HashMap::new();
    for &copy in executions_of.keys() {
        let Some(key) = key_of(copy) else {
            continue;
        };
        let originals = originals_of_key.entry(key).or_default();
        let Some(&original) = originals
            .iter()
            .find(|&&original| proportional_costs(original, copy))
        else {
            originals.push(copy);
            continue;
        };
        for (&a, &b) in executions_of[&original].iter().zip(&executions_of[&copy]) {
            shared.original_of_execution[b] = a;
            for input in 0..selectable.input_count(b) {
                for (&edge_a, &edge_b) in selectable
                    .edges_into(a, input)
                    .iter()
                    .zip(selectable.edges_into(b, input))
                {
                    shared.original_of_edge[edge_b] = edge_a;
                }
            }
            // Implied by the key: an execution of a root context is read
            // only by consumers in that context, whose in-edges the key
            // pairs.
            debug_assert_eq!(
                selectable.edges_out_of(a).len(),
                selectable.edges_out_of(b).len()
            );
        }
        // Equal keys: the queries pair up in signature order.
        let by_signature = |context: ExecutionContextId| {
            let mut queries: Vec<(QuerySignature<'_>, QueryId)> = queries_of
                .get(&context)
                .into_iter()
                .flatten()
                .map(|&query| (signature(context, query).expect("keyed"), query))
                .collect();
            queries.sort();
            queries
        };
        for ((_, a), (_, b)) in by_signature(original).into_iter().zip(by_signature(copy)) {
            shared.original_of_query[b.0] = a;
        }
    }
    shared
}
