//! Executions the selection MILP need not choose freely because another
//! does as well: one with $\ell$ = None whose `OneEvaluation` twin does
//! all it does at no more cost is left out
//! ([`dominated_by_one_evaluation_twin`]); a copy of an equivalent
//! evaluation context shares the original's variables
//! ([`copies_in_equivalent_contexts`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use asap_types::ir::properties::ExecutionTiming;
use asap_types::stages::{ExecutionContext, ExecutionContextId, Lifetime, PhysicalEdgeId};

use super::model::SelectionInputs;
use crate::selectable::QueryId;

/// Per execution, by position in `SelectableExecutions::executions`,
/// whether it is a query-time execution with $\ell$ = None that its
/// `OneEvaluation` twin $b$ (the same logical candidate, so the same θ and
/// the same error bounds, in the same context) dominates: $b$ costs no
/// more, reads each of its producers on the same input at no more, is
/// read by each of its consumers on the same input at no more, and answers
/// every query output it answers. A plan selecting it selects $b$ in its
/// place at no more cost and stays feasible: $b$'s (F) is the looser
/// $x \le$ uses, its edges are the same ones (so (A), (B), (E) and (X)
/// hold as before), and (K), per execution, sees the same θ through the
/// same producers. A candidate with $h_v$ or raw data on disk is never
/// left out so, its retention depending on its own readers.
///
/// Example: a `SummaryAgg` KLL build read by one readout in a context,
/// offered with $\ell$ = None and `OneEvaluation` at equal cost: the None
/// execution is left out.
pub(crate) fn dominated_by_one_evaluation_twin(inputs: &SelectionInputs<'_>) -> Vec<bool> {
    let selectable = inputs.selectable;
    let physical = inputs.physical;
    let costs = inputs.costs;
    let executions = selectable.executions();
    let edges = selectable.edges();
    let count = executions.len();
    let no_more = |cost: f64, than: f64| cost <= than + 1e-12 * than.abs().max(1e-12);
    let mut answers: Vec<BTreeSet<QueryId>> = vec![BTreeSet::new(); count];
    for (query, answering) in selectable.answering() {
        for &index in answering {
            answers[index].insert(*query);
        }
    }
    // The query-time executions of each logical candidate in each context,
    // without $h_v$ or raw data and costed: (with ℓ = None, OneEvaluation).
    let mut twins: HashMap<_, (Vec<usize>, Vec<usize>)> = HashMap::new();
    for (index, execution) in executions.iter().enumerate() {
        let v = execution.candidate;
        let candidate = &physical.physical_candidates[v.0];
        let properties = candidate.physical_properties;
        if properties.timing() != ExecutionTiming::QueryTime
            || costs.retention_cost_per_retained_second.contains_key(&v)
            || costs.raw_data_source.contains_key(&v)
            || costs.per_execution[index].is_err()
        {
            continue;
        }
        let (none, one_evaluation) = twins
            .entry((execution.context, candidate.logical_candidate_id))
            .or_default();
        match properties.lifetime() {
            Lifetime::None => none.push(index),
            Lifetime::OneEvaluation(_) => one_evaluation.push(index),
            Lifetime::AcrossEvaluations(_) => {}
        }
    }
    let cost = |index: usize| *costs.per_execution[index].as_ref().expect("costed");
    let input_of = |position: usize| physical.edge(edges[position].edge).input_index_of_consumer;
    // Whether `b` reads each producer `a` reads on an input, and is read by
    // each consumer that reads `a` on its input, over a costed edge at no
    // more.
    let edges_no_dearer = |a: usize, b: usize| {
        let covered = |position: usize, twins: &[usize], same_end: &dyn Fn(usize) -> bool| {
            let Some(edge_cost) = costs.per_edge[position] else {
                return true;
            };
            twins.iter().any(|&twin| {
                same_end(twin)
                    && costs.per_edge[twin].is_some_and(|twin_cost| no_more(twin_cost, edge_cost))
            })
        };
        (0..selectable.input_count(a)).all(|input| {
            selectable.edges_into(a, input).iter().all(|&position| {
                covered(position, selectable.edges_into(b, input), &|twin| {
                    edges[twin].producer == edges[position].producer
                })
            })
        }) && selectable.edges_out_of(a).iter().all(|&position| {
            covered(position, selectable.edges_out_of(b), &|twin| {
                edges[twin].consumer == edges[position].consumer
                    && input_of(twin) == input_of(position)
            })
        })
    };
    let mut dominated = vec![false; count];
    for (none, one_evaluation) in twins.values() {
        for &a in none {
            dominated[a] = one_evaluation.iter().any(|&b| {
                no_more(cost(b), cost(a))
                    && edges_no_dearer(a, b)
                    && answers[a].is_subset(&answers[b])
            });
        }
    }
    dominated
}

/// Per execution, edge and query, the one whose variables it shares
/// (itself when none): a root evaluation context (no request anchor)
/// equivalent to an earlier one is a copy of it, and so are its
/// executions, edges and queries. Two such contexts are equivalent when
/// their executions are of the same candidates, kept alike, their edges
/// the same memo edges from producers that correspond (in the context, or
/// the same execution outside every evaluation context), their queries
/// pair up with the same output executions, exact-only groups and
/// range-evaluation span, and every cost in one is the same multiple
/// $\alpha$ of the other's (to a relative 1e-9, the rounding of rate times
/// cost per run).
///
/// Exact: from an optimal plan, give both contexts the selection of the
/// one whose own cost $A$ or $B$ is lower, say $A$. The copied context
/// then costs $\alpha A \le \alpha B$, the retention $h_v$ and raw data
/// $r_s$ (each the most of their readers' needs, equal in both copies) do
/// not grow, and an execution outside them that only the other selection
/// read is left out at no cost; with $B \le A$ symmetrically. So some
/// optimum selects alike in both, and sharing their variables keeps it.
/// HiGHS no longer branches over each context's copy.
///
/// Example: the same queries every 60 s and every 300 s (one user each):
/// the 300 s context's executions cost 1/5 of the 60 s ones' and share
/// their variables.
pub(crate) fn copies_in_equivalent_contexts(
    inputs: &SelectionInputs<'_>,
    kept: &[bool],
) -> (Vec<usize>, Vec<usize>, Vec<QueryId>) {
    let selectable = inputs.selectable;
    let costs = inputs.costs;
    let executions = selectable.executions();
    let edges = selectable.edges();
    let mut original_of_execution: Vec<usize> = (0..executions.len()).collect();
    let mut original_of_edge: Vec<usize> = (0..edges.len()).collect();
    let mut original_of_query: Vec<QueryId> = (0..inputs.entries.len()).map(QueryId).collect();
    let is_root_evaluation = |context: ExecutionContextId| {
        matches!(
            selectable.context(context),
            ExecutionContext::Evaluation { .. }
        ) && selectable.request_anchor(context).is_none()
    };
    let is_shared = |context: ExecutionContextId| {
        !matches!(
            selectable.context(context),
            ExecutionContext::Evaluation { .. }
        )
    };
    // Each root evaluation context's executions (ascending, so by
    // candidate) and queries.
    let mut executions_of: BTreeMap<ExecutionContextId, Vec<usize>> = BTreeMap::new();
    for (index, execution) in executions.iter().enumerate() {
        if is_root_evaluation(execution.context) {
            executions_of
                .entry(execution.context)
                .or_default()
                .push(index);
        }
    }
    let mut outputs_of: BTreeMap<QueryId, Vec<&[usize]>> = BTreeMap::new();
    for (query, answering) in selectable.answering() {
        outputs_of.entry(*query).or_default().push(answering);
    }
    let mut queries_of: BTreeMap<ExecutionContextId, Vec<QueryId>> = BTreeMap::new();
    for (&query, outputs) in &outputs_of {
        // A query's outputs are answered in its own context.
        if let Some(&index) = outputs.first().and_then(|answering| answering.first()) {
            queries_of
                .entry(executions[index].context)
                .or_default()
                .push(query);
        }
    }
    let edge_at: HashMap<(usize, PhysicalEdgeId, usize), usize> = edges
        .iter()
        .enumerate()
        .map(|(position, edge)| ((edge.consumer, edge.edge, edge.producer), position))
        .collect();
    // Whether `b` is `a`'s cost times `alpha` (fixed by the first pair
    // with a cost).
    let proportional = |alpha: &mut Option<f64>, a: f64, b: f64| match (a == 0.0, b == 0.0) {
        (true, true) => true,
        (false, false) => {
            let ratio = *alpha.get_or_insert(b / a);
            (b - ratio * a).abs() <= 1e-9 * b.abs()
        }
        _ => false,
    };
    let contexts: Vec<ExecutionContextId> = executions_of.keys().copied().collect();
    let mut originals: Vec<ExecutionContextId> = Vec::new();
    for &copy in &contexts {
        let copied = originals.iter().find_map(|&original| {
            let (mine, theirs) = (&executions_of[&copy], &executions_of[&original]);
            if mine.len() != theirs.len() {
                return None;
            }
            let mut alpha = None;
            let mut mapped: HashMap<usize, usize> = HashMap::new();
            for (&b, &a) in mine.iter().zip(theirs) {
                let same = executions[a].candidate == executions[b].candidate
                    && kept[a] == kept[b]
                    && costs.least_need_ms_per_execution[a] == costs.least_need_ms_per_execution[b]
                    && selectable.input_count(a) == selectable.input_count(b)
                    && selectable.edges_out_of(a).len() == selectable.edges_out_of(b).len()
                    && match (&costs.per_execution[a], &costs.per_execution[b]) {
                        (Ok(cost_a), Ok(cost_b)) => proportional(&mut alpha, *cost_a, *cost_b),
                        (Err(_), Err(_)) => true,
                        _ => false,
                    };
                if !same {
                    return None;
                }
                mapped.insert(b, a);
            }
            let mut edges_mapped: Vec<(usize, usize)> = Vec::new();
            for (&b, &a) in mine.iter().zip(theirs) {
                for input in 0..selectable.input_count(b) {
                    let (into_b, into_a) = (
                        selectable.edges_into(b, input),
                        selectable.edges_into(a, input),
                    );
                    if into_b.len() != into_a.len() {
                        return None;
                    }
                    for &position in into_b {
                        let edge = edges[position];
                        let producer = match mapped.get(&edge.producer) {
                            Some(&producer) => producer,
                            None if is_shared(executions[edge.producer].context) => edge.producer,
                            None => return None,
                        };
                        let twin = *edge_at.get(&(a, edge.edge, producer))?;
                        let same = match (costs.per_edge[twin], costs.per_edge[position]) {
                            (Some(cost_a), Some(cost_b)) => {
                                proportional(&mut alpha, cost_a, cost_b)
                            }
                            (None, None) => true,
                            _ => false,
                        };
                        if !same {
                            return None;
                        }
                        edges_mapped.push((position, twin));
                    }
                }
            }
            // Each query of `copy` pairs with one of `original` reading the
            // same output executions, exact-only groups and span.
            let signature = |query: QueryId, map: &dyn Fn(usize) -> usize| {
                let outputs: Vec<Vec<usize>> = outputs_of[&query]
                    .iter()
                    .map(|answering| answering.iter().map(|&index| map(index)).collect())
                    .collect();
                let entry = &inputs.entries[query.0];
                (
                    outputs,
                    inputs
                        .physical
                        .logical_candidates
                        .exact_only_groups_of_query
                        .get(query.0),
                    entry.range_evaluation.map(|range| range.span_ms),
                )
            };
            let (my_queries, their_queries) = (
                queries_of.get(&copy).map_or(&[][..], Vec::as_slice),
                queries_of.get(&original).map_or(&[][..], Vec::as_slice),
            );
            if my_queries.len() != their_queries.len() {
                return None;
            }
            let mut unpaired: Vec<QueryId> = their_queries.to_vec();
            let mut queries_mapped = Vec::new();
            for &query in my_queries {
                let mine = signature(query, &|index| mapped[&index]);
                let at = unpaired
                    .iter()
                    .position(|&theirs| signature(theirs, &|index| index) == mine)?;
                queries_mapped.push((query, unpaired.swap_remove(at)));
            }
            Some((original, mapped, edges_mapped, queries_mapped))
        });
        match copied {
            Some((_, mapped, edges_mapped, queries_mapped)) => {
                for (b, a) in mapped {
                    original_of_execution[b] = a;
                }
                for (b, a) in edges_mapped {
                    original_of_edge[b] = a;
                }
                for (b, a) in queries_mapped {
                    original_of_query[b.0] = a;
                }
            }
            None => originals.push(copy),
        }
    }
    (original_of_execution, original_of_edge, original_of_query)
}
