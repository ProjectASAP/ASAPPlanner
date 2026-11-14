//! Executions left out of the selection MILP because another execution
//! dominates them: one with $\ell$ = None whose `OneEvaluation` twin does
//! all it does at no more cost.

use std::collections::{BTreeSet, HashMap};

use asap_types::ir::properties::ExecutionTiming;
use asap_types::stages::Lifetime;

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
