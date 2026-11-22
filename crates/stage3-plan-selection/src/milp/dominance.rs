//! Executions the selection MILP need not choose because another does as
//! well: one with $\ell$ = None whose `OneEvaluation` twin does all it
//! does at no more cost is left out, and so is a `OneEvaluation` one that
//! at most one consumer or output can read when its None twin does all it
//! does at less ([`dominated_by_lifetime_twin`]);
//! under cost model 3, the twin dominance holds in every quantity at once
//! ([`dominated_in_every_quantity`]), and whole θ-families another
//! θ-family does as well as are left out ([`dominated_theta_families`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use asap_types::ir::properties::ExecutionTiming;
use asap_types::stages::Lifetime;

use super::accuracy::AccuracyRules;
use super::model::SelectionInputs;
use super::COST_RELATIVE_TOLERANCE;
use crate::cost::ExecutionCosts;
use crate::selectable::QueryId;

/// Per execution, by position in `SelectableExecutions::executions`,
/// whether it is a query-time execution its twin $b$ (the same logical
/// candidate, so the same θ and the same error bounds, in the same context)
/// dominates: one with $\ell$ = None whose `OneEvaluation` twin $b$, or
/// one with `OneEvaluation` that at most one consumer edge or query output
/// can read whose None twin $b$ (not itself left out for it): $b$ costs no
/// more, reads each of its producers on the same input at no more, is
/// read by each of its consumers on the same input at no more, and answers
/// every query output it answers. A plan selecting it selects $b$ in its
/// place at no more cost and stays feasible: its edges are the same ones
/// (so (A), (B), (E) and (X) hold as before), (K), per execution, sees the
/// same θ through the same producers, and (F) holds: a `OneEvaluation`
/// $b$'s is the looser $x \le$ uses, and a None $b$'s, uses $= x$, holds
/// because the execution it replaces, selected, is used at least once (its
/// own (F)) and at most once. A candidate with $h_v$ or raw data on disk is
/// never left out so, its retention depending on its own readers.
///
/// Examples: a `SummaryAgg` KLL build read by one readout in a context,
/// offered with $\ell$ = None and `OneEvaluation` at equal cost: the None
/// execution is left out. A `TimeRangeCover` read only by its merge,
/// offered with $\ell$ = None (137 bytes held) and `OneEvaluation` (19 MB
/// held) at the same time: the `OneEvaluation` execution is left out.
pub(crate) fn dominated_by_lifetime_twin(inputs: &SelectionInputs<'_>) -> Vec<bool> {
    dominated_in_every_quantity(inputs, &[inputs.costs], Pairing::LifetimeTwin)
}

/// Which executions [`dominated_in_every_quantity`] compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pairing {
    /// An execution with $\ell$ = None and its `OneEvaluation` twin (the
    /// same logical candidate, context and timing), each of which may
    /// dominate the other.
    LifetimeTwin,
}

/// Per execution, whether another execution `b` it is paired with
/// ([`Pairing`]) dominates it in every one of `quantities`: $b$ is no more
/// in each, reads each of its producers on the same input at no more, is
/// read by each of its consumers on the same input at no more, answers
/// every query output it answers and may be read by every query that may
/// read it (exact-only reads, (X)). A plan selecting it selects $b$ in its
/// place at no more of any quantity and stays feasible (module doc of
/// [`dominated_by_lifetime_twin`]). A `OneEvaluation` execution is
/// left out only when its None twin is not itself left out for it (when
/// each is no worse than the other, the None one is left out), so the
/// twins of a class are never all left out, and "no worse" being
/// transitive, every execution left out has a kept one no worse than it.
/// A candidate with $h_v$ or raw data on disk is never
/// left out, its retention depending on its own readers.
pub(crate) fn dominated_in_every_quantity(
    inputs: &SelectionInputs<'_>,
    quantities: &[&ExecutionCosts],
    pairing: Pairing,
) -> Vec<bool> {
    let selectable = inputs.selectable;
    let physical = inputs.physical;
    let costs = inputs.costs;
    let executions = selectable.executions();
    let edges = selectable.edges();
    let count = executions.len();
    // No more, to rounding (to 1e-24/s when `than` is 0).
    let no_more =
        |cost: f64, than: f64| cost <= than + COST_RELATIVE_TOLERANCE * than.abs().max(1e-12);
    let mut answers: Vec<BTreeSet<QueryId>> = vec![BTreeSet::new(); count];
    for (query, answering) in selectable.answering() {
        for &index in answering {
            answers[index].insert(*query);
        }
    }
    let query_count = inputs.entries.len();
    let logical = |index: usize| {
        physical.physical_candidates[executions[index].candidate.0].logical_candidate_id
    };
    // The executions compared with each other.
    let mut classes: HashMap<_, Vec<usize>> = HashMap::new();
    for (index, execution) in executions.iter().enumerate() {
        let v = execution.candidate;
        let candidate = &physical.physical_candidates[v.0];
        let properties = candidate.physical_properties;
        if costs.retention_cost_per_retained_second.contains_key(&v)
            || costs.raw_data_source.contains_key(&v)
            || quantities
                .iter()
                .any(|quantity| quantity.per_execution[index].is_err())
            || costs.per_execution[index].is_err()
        {
            continue;
        }
        match pairing {
            Pairing::LifetimeTwin => {
                if properties.timing() != ExecutionTiming::QueryTime
                    || matches!(properties.lifetime(), Lifetime::AcrossEvaluations(_))
                {
                    continue;
                }
                classes
                    .entry((execution.context, candidate.logical_candidate_id))
                    .or_default()
                    .push(index);
            }
        }
    }
    let value = |quantity: &ExecutionCosts, index: usize| {
        *quantity.per_execution[index].as_ref().expect("costed")
    };
    let input_of = |position: usize| physical.edge(edges[position].edge).input_index_of_consumer;
    // Whether `b` reads each producer `a` reads on an input, and is read by
    // each consumer that reads `a` on its input, over an edge at no more of
    // `quantity`.
    let edges_no_dearer = |quantity: &ExecutionCosts, a: usize, b: usize| {
        let covered = |position: usize, others: &[usize], same_end: &dyn Fn(usize) -> bool| {
            let Some(edge_cost) = quantity.per_edge[position] else {
                return true;
            };
            others.iter().any(|&other| {
                same_end(other)
                    && quantity.per_edge[other]
                        .is_some_and(|other_cost| no_more(other_cost, edge_cost))
            })
        };
        (0..selectable.input_count(a)).all(|input| {
            selectable.edges_into(a, input).iter().all(|&position| {
                covered(position, selectable.edges_into(b, input), &|other| {
                    edges[other].producer == edges[position].producer
                })
            })
        }) && selectable.edges_out_of(a).iter().all(|&position| {
            covered(position, selectable.edges_out_of(b), &|other| {
                edges[other].consumer == edges[position].consumer
                    && input_of(other) == input_of(position)
            })
        })
    };
    let no_worse = |a: usize, b: usize| {
        quantities.iter().all(|quantity| {
            no_more(value(quantity, b), value(quantity, a)) && edges_no_dearer(quantity, a, b)
        }) && answers[a].is_subset(&answers[b])
            && (0..query_count).all(|query| {
                !physical
                    .logical_candidates
                    .query_must_not_read(query, logical(b))
                    || physical
                        .logical_candidates
                        .query_must_not_read(query, logical(a))
            })
    };
    // Whether at most one consumer edge or query output can read `index`
    // in any plan: a None execution is read exactly that once.
    let read_at_most_once =
        |index: usize| selectable.edges_out_of(index).len() + answers[index].len() <= 1;
    let lifetime = |index: usize| {
        physical.physical_candidates[executions[index].candidate.0]
            .physical_properties
            .lifetime()
    };
    let mut dominated = vec![false; count];
    for members in classes.values() {
        for &a in members {
            dominated[a] = members.iter().any(|&b| {
                b != a
                    && match pairing {
                        Pairing::LifetimeTwin => match (lifetime(a), lifetime(b)) {
                            // `OneEvaluation` does all `None` does.
                            (Lifetime::None, Lifetime::OneEvaluation(_)) => no_worse(a, b),
                            // `None` does all a `OneEvaluation` read at most
                            // once does; not when `OneEvaluation` is no
                            // worse too, which leaves `None` out above.
                            (Lifetime::OneEvaluation(_), Lifetime::None) => {
                                read_at_most_once(a) && no_worse(a, b) && !no_worse(b, a)
                            }
                            _ => false,
                        },
                    }
            });
        }
    }
    dominated
}

/// Per execution, whether its θ-family is dominated (cost model 3): a
/// θ-family is a connected set of `kept` executions with one sketch
/// parameterisation θ (linked by the selectable edges between executions of
/// that θ), each in its own slot (context, memo group, operator and
/// physical properties). A family is dominated by another over the same
/// slots that is no worse, slot by slot, in every one of `quantities`,
/// whose own error is no more in every metric and failure probability
/// (`rules`; none without accuracy targets), that answers every query
/// output it answers, and whose every edge (into, out of and within the
/// family) has a counterpart at no more of every quantity. A plan using the
/// dominated family uses the dominating one in its place at no more of any
/// quantity, meeting every accuracy target the other met: every optimum is
/// kept. Among families equal both ways the first dominates, so each
/// dominated family has an undominated dominator. Pruning single
/// executions by θ instead is not safe: a readout's more accurate chain
/// is a family, not one execution. Families with $h_v$ or raw data on disk
/// are never left out.
pub(crate) fn dominated_theta_families(
    inputs: &SelectionInputs<'_>,
    quantities: &[&ExecutionCosts],
    kept: &[bool],
    rules: Option<&AccuracyRules>,
) -> Vec<bool> {
    let selectable = inputs.selectable;
    let physical = inputs.physical;
    let memo = &physical.logical_candidates.memo;
    let executions = selectable.executions();
    let edges = selectable.edges();
    let count = executions.len();
    let theta = |index: usize| -> Option<String> {
        let candidate = &physical.physical_candidates[executions[index].candidate.0];
        memo.candidate(candidate.logical_candidate_id)
            .logical_properties
            .parameters()
            .map(|theta| format!("{theta:?}"))
    };
    let slot = |index: usize| -> String {
        let execution = executions[index];
        let candidate = &physical.physical_candidates[execution.candidate.0];
        let logical = memo.candidate(candidate.logical_candidate_id);
        format!(
            "{:?}|{:?}|{:?}|{:?}",
            execution.context,
            candidate.logical_candidate_id.group,
            logical.expression.operator,
            candidate.physical_properties
        )
    };
    let thetas: Vec<Option<String>> = (0..count)
        .map(|index| if kept[index] { theta(index) } else { None })
        .collect();
    // The families: union-find over edges between executions of one θ.
    let mut parent: Vec<usize> = (0..count).collect();
    fn find(parent: &mut [usize], mut index: usize) -> usize {
        while parent[index] != index {
            parent[index] = parent[parent[index]];
            index = parent[index];
        }
        index
    }
    for edge in edges {
        let (consumer, producer) = (edge.consumer, edge.producer);
        if thetas[consumer].is_some() && thetas[consumer] == thetas[producer] {
            let (a, b) = (find(&mut parent, consumer), find(&mut parent, producer));
            parent[a] = b;
        }
    }
    let mut families: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (index, theta) in thetas.iter().enumerate() {
        if theta.is_some() {
            families
                .entry(find(&mut parent, index))
                .or_default()
                .push(index);
        }
    }
    let no_more =
        |cost: f64, than: f64| cost <= than + COST_RELATIVE_TOLERANCE * than.abs().max(1e-12);
    let value =
        |costs: &ExecutionCosts, index: usize| costs.per_execution[index].as_ref().ok().copied();
    let mut answers: Vec<BTreeSet<QueryId>> = vec![BTreeSet::new(); count];
    for (query, answering) in selectable.answering() {
        for &index in answering {
            answers[index].insert(*query);
        }
    }
    let retained = |index: usize| {
        let v = executions[index].candidate;
        quantities.iter().chain([&inputs.costs]).any(|costs| {
            costs.retention_cost_per_retained_second.contains_key(&v)
                || costs.raw_data_source.contains_key(&v)
        })
    };
    // Families by their slots.
    type Family = (BTreeMap<String, usize>, usize);
    let mut by_slots: HashMap<Vec<String>, Vec<Family>> = HashMap::new();
    for (&root, members) in &families {
        let by_slot: BTreeMap<String, usize> = members.iter().map(|&i| (slot(i), i)).collect();
        if by_slot.len() != members.len() || members.iter().any(|&i| retained(i)) {
            continue;
        }
        by_slots
            .entry(by_slot.keys().cloned().collect())
            .or_default()
            .push((by_slot, root));
    }
    let input_of = |position: usize| physical.edge(edges[position].edge).input_index_of_consumer;
    let all_quantities: Vec<&ExecutionCosts> =
        quantities.iter().copied().chain([inputs.costs]).collect();
    let dominates = |b: &BTreeMap<String, usize>, a: &BTreeMap<String, usize>| -> bool {
        let counterpart: HashMap<usize, usize> = a.iter().map(|(k, &i)| (i, b[k])).collect();
        for (k, &ai) in a {
            let bi = b[k];
            for costs in &all_quantities {
                match (value(costs, bi), value(costs, ai)) {
                    (Some(cb), Some(ca)) if no_more(cb, ca) => {}
                    (_, None) => {}
                    _ => return false,
                }
            }
            if !answers[ai].is_subset(&answers[bi]) {
                return false;
            }
            if rules.is_some_and(|rules| !rules.own_error_no_more(bi, ai)) {
                return false;
            }
        }
        // Every edge touching a member of `a` has a counterpart touching
        // `b` at no more of every quantity.
        for &ai in a.values() {
            let touching = selectable.edges_out_of(ai).iter().chain(
                (0..selectable.input_count(ai))
                    .flat_map(|input| selectable.edges_into(ai, input).iter()),
            );
            for &position in touching {
                let (consumer, producer) = (edges[position].consumer, edges[position].producer);
                if !kept[consumer] || !kept[producer] {
                    continue;
                }
                let map = |index: usize| counterpart.get(&index).copied().unwrap_or(index);
                let (b_consumer, b_producer) = (map(consumer), map(producer));
                let found = selectable.edges_out_of(b_producer).iter().any(|&other| {
                    edges[other].consumer == b_consumer
                        && input_of(other) == input_of(position)
                        && all_quantities.iter().all(|costs| {
                            match (costs.per_edge[other], costs.per_edge[position]) {
                                (Some(cb), Some(ca)) => no_more(cb, ca),
                                (None, None) => true,
                                _ => false,
                            }
                        })
                });
                if !found {
                    return false;
                }
            }
        }
        true
    };
    let mut dominated = vec![false; count];
    for families in by_slots.values() {
        for (position, (a, a_root)) in families.iter().enumerate() {
            let is_dominated = families.iter().enumerate().any(|(other, (b, b_root))| {
                other != position && dominates(b, a) && (!dominates(a, b) || b_root < a_root)
            });
            if is_dominated {
                for &index in a.values() {
                    dominated[index] = true;
                }
            }
        }
    }
    dominated
}
