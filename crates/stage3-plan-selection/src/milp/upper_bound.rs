//! An upper bound on the selection MILP's optimum, and the executions it
//! rules out: the cost of one feasible plan found greedily, and every
//! execution that alone costs more than it. Leaving those out before the
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

use asap_types::ir::properties::ExecutionTiming;
use asap_types::stages::Lifetime;

use super::model::{selection_cost_per_second, QueryId, SelectionInputs};
use crate::selectable::Execution;

/// How many times the greedy plan is rebuilt without an execution that
/// made it infeasible before giving up on a bound.
const REPAIRS: usize = 32;

/// Per (consumer execution, input), the costed edges into it, each as
/// (edge position, producer execution, $\rho_\kappa c_{\kappa,u,c,i}$).
type EdgesInto = HashMap<(usize, usize), Vec<(usize, usize, f64)>>;

/// Per input of an execution, the edge its cheapest tree reads it over, as
/// (edge position, producer execution).
type TreeInputs = Vec<Option<(usize, usize)>>;

/// Per execution, by position in `ExecutionContexts::executions`, whether
/// it costs more than a feasible plan ([`feasible_plan_cost_per_second`]):
/// its own cost plus the most its input edges can take off ($\min(0,
/// c_{\kappa,u,v,i})$ per input) is over the bound. Such an execution is in
/// no optimal plan, as long as every other selected execution costs at
/// least 0 with its selected inputs. None is ruled out when no feasible
/// plan is found.
pub(crate) fn dearer_than_a_feasible_plan(inputs: &SelectionInputs<'_>) -> Vec<bool> {
    let contexts = inputs.contexts;
    let count = contexts.executions().len();
    let Some(bound) = feasible_plan_cost_per_second(inputs) else {
        return vec![false; count];
    };
    let mut cheapest_input_edge: HashMap<(usize, usize), f64> = HashMap::new();
    for (&(consumer, edge, _), cost) in contexts.edges().iter().zip(&inputs.costs.per_edge) {
        let Some(cost) = cost else {
            continue;
        };
        let key = (
            contexts.index_of(consumer),
            inputs.physical.edge(edge).input_index_of_consumer,
        );
        let cheapest = cheapest_input_edge.entry(key).or_insert(0.0);
        *cheapest = cheapest.min(*cost);
    }
    let tolerance = 1e-9 * bound.abs().max(1e-12);
    (0..count)
        .map(|index| {
            let Ok(own) = &inputs.costs.per_execution[index] else {
                return false;
            };
            let v = contexts.executions()[index].candidate;
            let least = own
                + (0..inputs.physical.input_count(v))
                    .filter_map(|i| cheapest_input_edge.get(&(index, i)))
                    .sum::<f64>();
            least > bound + tolerance
        })
        .collect()
}

/// The cost per second (without the request overhead every plan pays) of
/// a feasible plan ([`selection_cost_per_second`]): each query output
/// reads the cheapest tree of executions below it, each execution's input
/// from the producer whose tree is cheapest ($\rho_\kappa c_{\kappa,v} +
/// \sum_i \min_u (\rho_\kappa c_{\kappa,u,v,i} + \text{tree}_u)$). Trees
/// share the executions they have in common; when that gives a query-time
/// result that is not materialized two consumers, or a cycle, the plan is
/// rebuilt without that execution. `None` when no feasible plan is found
/// that way.
fn feasible_plan_cost_per_second(inputs: &SelectionInputs<'_>) -> Option<f64> {
    let physical = inputs.physical;
    let contexts = inputs.contexts;
    let count = contexts.executions().len();
    let mut into: EdgesInto = HashMap::new();
    for (position, (&(consumer, edge, producer), cost)) in contexts
        .edges()
        .iter()
        .zip(&inputs.costs.per_edge)
        .enumerate()
    {
        let Some(cost) = cost else {
            continue;
        };
        into.entry((
            contexts.index_of(consumer),
            physical.edge(edge).input_index_of_consumer,
        ))
        .or_default()
        .push((position, contexts.index_of(producer), *cost));
    }
    let inputs_of: Vec<usize> = contexts
        .executions()
        .iter()
        .map(|execution| physical.input_count(execution.candidate))
        .collect();
    let unshared = |index: usize| {
        let v = contexts.executions()[index].candidate;
        let properties = physical.physical_candidates[v.0].physical_properties;
        properties.lifetime() == Lifetime::None && properties.timing() == ExecutionTiming::QueryTime
    };
    let mut excluded = vec![false; count];
    for _ in 0..REPAIRS {
        let (tree_cost, chosen) = cheapest_trees(inputs, &into, &inputs_of, &excluded);
        // Each query output's cheapest execution, then the trees below.
        let mut uses = vec![0usize; count];
        let mut outputs = Vec::new();
        let mut answering: Vec<(QueryId, Execution)> = Vec::new();
        for (query, executions) in inputs.query_outputs {
            let best = executions
                .iter()
                .map(|&execution| contexts.index_of(execution))
                .filter(|&index| tree_cost[index].is_finite())
                .min_by(|&a, &b| tree_cost[a].total_cmp(&tree_cost[b]))?;
            uses[best] += 1;
            outputs.push(best);
            answering.push((*query, contexts.executions()[best]));
        }
        let plan = match plan_below(&outputs, &chosen, &inputs_of) {
            Ok(plan) => plan,
            Err(on_cycle) => {
                excluded[on_cycle] = true;
                continue;
            }
        };
        for &(_, producer) in &plan.edges {
            uses[producer] += 1;
        }
        if let Some(shared) = plan
            .executions
            .iter()
            .copied()
            .find(|&index| unshared(index) && uses[index] > 1)
        {
            excluded[shared] = true;
            continue;
        }
        let edges: Vec<usize> = plan.edges.iter().map(|&(position, _)| position).collect();
        let (cost, _) = selection_cost_per_second(inputs, &plan.executions, &edges, &answering);
        return cost.is_finite().then_some(cost);
    }
    None
}

/// Per execution not `excluded`, the cost of its cheapest tree and, per
/// input, the edge it reads it over: relaxed until no tree gets cheaper
/// (at most once per execution), so a tree over a cycle of executions is
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
            let Ok(own) = &inputs.costs.per_execution[v] else {
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
                    .filter(|&&(_, u, _)| !excluded[u] && tree_cost[u].is_finite())
                    .map(|&(position, u, c)| (position, u, c + tree_cost[u]))
                    .min_by(|a, b| a.2.total_cmp(&b.2));
                match cheapest {
                    Some((position, u, through)) => {
                        cost += through;
                        reads.push(Some((position, u)));
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
    /// Its executions, each once.
    executions: Vec<usize>,
    /// Its edges, as (edge position, producer execution).
    edges: Vec<(usize, usize)>,
}

/// The plan of the trees `chosen` below `outputs`; Err with an execution
/// on a cycle of them.
fn plan_below(
    outputs: &[usize],
    chosen: &[TreeInputs],
    inputs_of: &[usize],
) -> Result<GreedyPlan, usize> {
    // 0 unvisited, 1 on the walk, 2 done.
    let mut state = vec![0u8; inputs_of.len()];
    let mut plan = GreedyPlan {
        executions: Vec::new(),
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
                plan.executions.push(v);
                continue;
            }
            stack.push((v, i + 1));
            let (position, u) = chosen[v][i].expect("a finite tree reads every input");
            plan.edges.push((position, u));
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
