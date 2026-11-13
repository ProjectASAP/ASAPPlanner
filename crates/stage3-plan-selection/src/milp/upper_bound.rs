//! An upper bound on the baseline MILP's optimum, and the candidates it
//! rules out: the cost of one feasible plan found greedily, and every
//! candidate that alone costs more than it. Leaving those out before the
//! objective is scaled keeps a candidate costing millions per second from
//! shrinking the differences between plans costing thousandths under the
//! solver's tolerances.
//!
//! Example (`max by (job)(count_over_time(m[1m]))`): the cheapest plan
//! costs 0.0078/s and an unused ingestion-time chain 2.55e6/s. Scaled so
//! the largest coefficient is 1,000, a 0.001/s difference between two
//! plans became 4e-7, and the solver returned a plan 12.6% dearer. The
//! greedy plan bounds the optimum by about 0.0088/s, so the chain is left
//! out and the largest coefficient left is about 0.006/s.

use std::collections::HashMap;

use asap_types::stages::PhysicalCandidateId;

use super::model::{is_unmaterialized_query_time, plan_cost_per_second, SelectionInputs};

/// How many times the greedy plan is rebuilt without a candidate that
/// made it infeasible before giving up on a bound.
const REPAIRS: usize = 32;

/// Per (consumer, input), the costed edges into it, each as (producer,
/// $c_{u,c,i}$).
type EdgesInto = HashMap<(usize, usize), Vec<(usize, f64)>>;

/// Per input of a candidate, the edge its cheapest tree reads it over, as
/// (producer, $c_{u,c,i}$).
type TreeInputs = Vec<Option<(usize, f64)>>;

/// Per candidate, whether it costs more than a feasible plan
/// ([`feasible_plan_cost_per_second`]): its own $w_v$ plus the most its
/// input edges can take off ($\min(0, c_{u,v,i})$ per input) is over the
/// bound. Such a candidate is in no optimal plan, as long as every other
/// selected candidate costs at least 0 with its selected inputs. None is
/// ruled out when no feasible plan is found.
pub(crate) fn dearer_than_a_feasible_plan(inputs: &SelectionInputs<'_>) -> Vec<bool> {
    let count = inputs.physical.physical_candidates.len();
    let Some(bound) = feasible_plan_cost_per_second(inputs) else {
        return vec![false; count];
    };
    let mut cheapest_input_edge: HashMap<(PhysicalCandidateId, usize), f64> = HashMap::new();
    for (&edge, &cost) in &inputs.costs.edge_cost_change_per_second {
        let edge = inputs.physical.edge(edge);
        let key = (edge.consumer_candidate_id, edge.input_index_of_consumer);
        let cheapest = cheapest_input_edge.entry(key).or_insert(0.0);
        *cheapest = cheapest.min(cost);
    }
    let tolerance = 1e-9 * bound.abs().max(1e-12);
    (0..count)
        .map(PhysicalCandidateId)
        .map(|v| {
            let Ok(own) = &inputs.costs.candidate_cost_per_second[v.0] else {
                return false;
            };
            let least = own
                + (0..inputs.physical.input_count(v))
                    .filter_map(|i| cheapest_input_edge.get(&(v, i)))
                    .sum::<f64>();
            least > bound + tolerance
        })
        .collect()
}

/// The cost per second (without the request overhead every plan pays) of
/// a feasible plan: each query output reads the cheapest tree of
/// candidates below it, each candidate's input from the producer whose
/// tree is cheapest ($w_v + \sum_i \min_u (c_{u,v,i} + \text{tree}_u)$).
/// Trees share the candidates they have in common; when that gives a
/// query-time result that is not materialized two consumers, or a cycle,
/// the plan is rebuilt without that candidate. `None` when no feasible
/// plan is found that way.
fn feasible_plan_cost_per_second(inputs: &SelectionInputs<'_>) -> Option<f64> {
    let physical = inputs.physical;
    let count = physical.physical_candidates.len();
    // Each (consumer, input)'s costed edges: (edge, producer, c_{u,c,i}).
    let mut into: EdgesInto = HashMap::new();
    for (&id, &cost) in &inputs.costs.edge_cost_change_per_second {
        let edge = physical.edge(id);
        into.entry((edge.consumer_candidate_id.0, edge.input_index_of_consumer))
            .or_default()
            .push((edge.producer_candidate_id.0, cost));
    }
    let inputs_of: Vec<usize> = (0..count)
        .map(|v| physical.input_count(PhysicalCandidateId(v)))
        .collect();
    let unshared = |v: usize| is_unmaterialized_query_time(inputs, v);
    let mut excluded = vec![false; count];
    for _ in 0..REPAIRS {
        let (tree_cost, chosen) = cheapest_trees(inputs, &into, &inputs_of, &excluded);
        // Each query output's cheapest candidate, then the trees below.
        let mut uses = vec![0usize; count];
        let mut outputs = Vec::new();
        for (_, candidates) in inputs.query_outputs {
            let best = candidates
                .iter()
                .map(|v| v.0)
                .filter(|&v| tree_cost[v].is_finite())
                .min_by(|&a, &b| tree_cost[a].total_cmp(&tree_cost[b]))?;
            uses[best] += 1;
            outputs.push(best);
        }
        let plan = match plan_below(&outputs, &chosen, &inputs_of) {
            Ok(plan) => plan,
            Err(on_cycle) => {
                excluded[on_cycle] = true;
                continue;
            }
        };
        for &(v, _) in &plan.edges {
            uses[v] += 1;
        }
        if let Some(shared) = plan
            .candidates
            .iter()
            .copied()
            .find(|&v| unshared(v) && uses[v] > 1)
        {
            excluded[shared] = true;
            continue;
        }
        let cost = plan_cost_per_second(
            inputs,
            plan.candidates.iter().copied(),
            plan.edges.iter().map(|&(_, cost)| cost),
        );
        return cost.is_finite().then_some(cost);
    }
    None
}

/// Per candidate not `excluded`, the cost of its cheapest tree and, per
/// input, the edge it reads it over: relaxed until no tree gets cheaper
/// (at most once per candidate), so a tree over a cycle of candidates is
/// never cheaper than one without it when costs are not negative.
fn cheapest_trees(
    inputs: &SelectionInputs<'_>,
    into: &EdgesInto,
    inputs_of: &[usize],
    excluded: &[bool],
) -> (Vec<f64>, Vec<TreeInputs>) {
    let count = inputs_of.len();
    let mut tree_cost = vec![f64::INFINITY; count];
    let mut chosen: Vec<TreeInputs> = inputs_of.iter().map(|&n| vec![None; n]).collect();
    for _ in 0..count.max(1) {
        let mut changed = false;
        for v in 0..count {
            let Ok(own) = &inputs.costs.candidate_cost_per_second[v] else {
                continue;
            };
            if excluded[v] {
                continue;
            }
            let mut cost = *own;
            let mut reads = Vec::with_capacity(inputs_of[v]);
            for i in 0..inputs_of[v] {
                let cheapest = into
                    .get(&(v, i))
                    .into_iter()
                    .flatten()
                    .filter(|&&(u, _)| !excluded[u] && tree_cost[u].is_finite())
                    .map(|&(u, c)| (u, c, c + tree_cost[u]))
                    .min_by(|a, b| a.2.total_cmp(&b.2));
                match cheapest {
                    Some((u, c, through)) => {
                        cost += through;
                        reads.push(Some((u, c)));
                    }
                    None => {
                        cost = f64::INFINITY;
                        break;
                    }
                }
            }
            if cost < tree_cost[v] - 1e-12 * cost.abs().max(1e-300) {
                tree_cost[v] = cost;
                chosen[v] = reads;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    (tree_cost, chosen)
}

/// A plan of the trees below some query outputs.
struct GreedyPlan {
    /// Its candidates, each once.
    candidates: Vec<usize>,
    /// Its edges, as (producer, $c_{u,c,i}$).
    edges: Vec<(usize, f64)>,
}

/// The plan of the trees `chosen` below `outputs`; Err with a candidate
/// on a cycle of them.
fn plan_below(
    outputs: &[usize],
    chosen: &[TreeInputs],
    inputs_of: &[usize],
) -> Result<GreedyPlan, usize> {
    // 0 unvisited, 1 on the walk, 2 done.
    let mut state = vec![0u8; inputs_of.len()];
    let mut plan = GreedyPlan {
        candidates: Vec::new(),
        edges: Vec::new(),
    };
    for &output in outputs {
        if state[output] == 2 {
            continue;
        }
        let mut stack = vec![(output, 0usize)];
        state[output] = 1;
        while let Some((v, i)) = stack.pop() {
            if i == inputs_of[v] {
                state[v] = 2;
                plan.candidates.push(v);
                continue;
            }
            stack.push((v, i + 1));
            let (u, c) = chosen[v][i].expect("a finite tree reads every input");
            plan.edges.push((u, c));
            match state[u] {
                0 => {
                    state[u] = 1;
                    stack.push((u, 0));
                }
                1 => return Err(u),
                _ => {}
            }
        }
    }
    Ok(plan)
}
