//! An upper bound on the selection MILP's optimum, and the executions it
//! rules out: the cost of one feasible plan found greedily, and every
//! execution that alone costs more than it. Leaving those out before the
//! objective is scaled keeps a candidate costing millions per second from
//! shrinking the differences between plans costing thousandths under the
//! solver's tolerances.
//!
//! Example (`max by (job)(count_over_time(m[1m]))`): the cheapest plan
//! costs 0.0078/s and an unused ingestion-time chain 2.55e6/s. Scaled so
//! the largest coefficient was 1,000 (the scale then), a 0.001/s difference
//! between two plans became 4e-7, and the solver returned a plan 12.6%
//! dearer. The greedy plan bounds the optimum by about 0.0088/s, so the
//! chain is left out and the largest coefficient left is about 0.006/s.

use std::collections::{BTreeMap, HashMap};

use super::accuracy::AccuracyRules;
use super::model::{has_one_reader, selection_cost_per_second, SelectionInputs};
use super::COST_RELATIVE_TOLERANCE;
use crate::selectable::QueryId;
use asap_types::stages::MemoGroupId;

/// How many times the greedy plan is rebuilt without an execution that
/// made it infeasible before giving up on a bound.
const REPAIRS: usize = 32;

/// Per input of an execution, the edge its cheapest tree reads it over, as
/// (edge position, producer execution).
type TreeInputs = Vec<Option<(usize, usize)>>;

/// Per execution, by position in `SelectableExecutions::executions`,
/// whether it costs more than a feasible plan
/// ([`feasible_plan_cost_per_second`]): its own cost, plus the least
/// retention $\pi_v h_v$ (R) makes it pay, plus the most its input edges
/// can take off ($\min(0, c_{\kappa,u,v,i})$ per input) is over the
/// bound. Such an execution is in no optimal plan, as long as every
/// other selected execution costs at least 0 with its selected inputs.
/// None is ruled out when no feasible plan is found, the accuracy
/// constraint (K) (`rules`) included. Also returns the feasible plan, whose
/// cost per second is the bound; `None` without one.
///
/// With `sharing_aware` (`SearchSpacePruning::sharing_aware_bound`), the
/// feasible plan is the cheapest of the one-tree-per-query plan and two
/// greedy plans that charge executions already chosen for an earlier
/// query as free ([`feasible_plan`]), and an execution's least cost also
/// counts the least cost of each query output group other than its own
/// ([`least_cost_of_other_output_groups`]): every plan answers every
/// query from its output group, by executions other than this one.
pub(crate) fn dearer_than_a_feasible_plan(
    inputs: &SelectionInputs<'_>,
    rules: Option<&AccuracyRules>,
    sharing_aware: bool,
) -> (Option<FeasiblePlan>, Vec<bool>) {
    let selectable = inputs.selectable;
    let count = selectable.executions().len();
    let Some(plan) = feasible_plan(inputs, rules, sharing_aware) else {
        return (None, vec![false; count]);
    };
    let bound = plan.cost_per_second;
    let costs = inputs.costs;
    let executions = selectable.executions();
    // The least retention (R) makes each execution pay; a query-time
    // scan's retention is its source's $r_s$, left out.
    let least_retention_cost = |index: usize| {
        let v = executions[index].candidate;
        match costs.least_retention_ms_per_execution[index] {
            Some(need_ms) if !costs.raw_data_source.contains_key(&v) => {
                costs.retention_cost_per_second(v, need_ms)
            }
            _ => 0.0,
        }
    };
    // Under a bound on the kept query-time results, what an execution
    // keeps may be dropped (`super::kept_results`): its own cost less them
    // is what it surely pays.
    let bounded_kept_results_cost = |index: usize| {
        let bounded = inputs
            .kept_query_time_results
            .is_some_and(|results| results.max_bytes.is_some());
        match (bounded, costs.per_kept_byte) {
            (true, Some(per_byte)) => {
                per_byte
                    * costs
                        .kept_result_bytes_per_execution
                        .get(index)
                        .copied()
                        .unwrap_or(0.0)
            }
            _ => 0.0,
        }
    };
    // Looser than `COST_RELATIVE_TOLERANCE` on purpose: the bound and each
    // execution's least cost are sums over many executions, and leaving
    // out an execution the optimum needs cannot be undone, so an execution
    // is left out only when it is clearly dearer (by 1e-9 of the bound;
    // 1e-21/s when the bound is 0).
    let tolerance = 1e-9 * bound.abs().max(1e-12);
    // Each execution's least cost: its own, less what a bound on the kept
    // results may drop, plus its least retention and the most its input
    // edges can take off. `None` when it could not be costed.
    let least: Vec<Option<f64>> = (0..count)
        .map(|index| {
            let own = costs.per_execution[index].as_ref().ok()?;
            Some(
                own - bounded_kept_results_cost(index)
                    + least_retention_cost(index)
                    + (0..selectable.input_count(index))
                        .map(|i| {
                            selectable
                                .edges_into(index, i)
                                .iter()
                                .filter_map(|&position| costs.per_edge[position])
                                .fold(0.0, f64::min)
                        })
                        .sum::<f64>(),
            )
        })
        .collect();
    let others = sharing_aware.then(|| least_cost_of_other_output_groups(inputs, &least));
    let dearer = (0..count)
        .map(|index| {
            least[index].is_some_and(|least| {
                let others = others.as_ref().map_or(0.0, |others| others(index));
                least + others > bound + tolerance
            })
        })
        .collect();
    (Some(plan), dearer)
}

/// Per execution, by position, the least cost every plan also pays to
/// answer the queries whose outputs are in a group other than its own: per
/// output group whose query outputs it alone answers, the least `least`
/// cost of the executions answering them (at most 0 for a group none of
/// which could be costed). Distinct groups are distinct executions, and an
/// execution's least cost counts only its own edges, so the costs add.
///
/// Example: two queries, `quantile_over_time(0.99, m[5m])` and
/// `sum(sum_over_time(n[5m]))`, answered from two groups whose cheapest
/// answering executions cost at least 0.03/s and 0.02/s: an execution of
/// the first query's chain also pays 0.02/s for the second, so it is left
/// out when its own least cost is over the bound less 0.02/s.
fn least_cost_of_other_output_groups<'a>(
    inputs: &'a SelectionInputs<'_>,
    least: &[Option<f64>],
) -> impl Fn(usize) -> f64 + 'a {
    let selectable = inputs.selectable;
    let executions = selectable.executions();
    let group_of = |index: usize| {
        inputs.physical.physical_candidates[executions[index].candidate.0]
            .logical_candidate_id
            .group
    };
    // Per output group (each query whose answering executions are all in
    // one group), its least answering cost.
    let mut least_of_group: BTreeMap<MemoGroupId, f64> = BTreeMap::new();
    for (_, answering) in selectable.answering() {
        let Some(&first) = answering.first() else {
            continue;
        };
        let group = group_of(first);
        if answering.iter().any(|&index| group_of(index) != group) {
            continue;
        }
        let cheapest = answering
            .iter()
            .filter_map(|&index| least[index])
            .fold(f64::INFINITY, f64::min);
        let cheapest = if cheapest.is_finite() { cheapest } else { 0.0 };
        let entry = least_of_group.entry(group).or_insert(cheapest);
        *entry = entry.min(cheapest);
    }
    let total: f64 = least_of_group.values().sum();
    let group_of_execution: Vec<MemoGroupId> = (0..executions.len()).map(group_of).collect();
    move |index| {
        total
            - least_of_group
                .get(&group_of_execution[index])
                .copied()
                .unwrap_or(0.0)
    }
}

/// A feasible plan and its cost per second (less the request overhead
/// every plan pays, [`selection_cost_per_second`]): each query output reads
/// the cheapest tree of executions below it, each execution's input from
/// the producer whose tree is cheapest ($\rho_\kappa c_{\kappa,v} + \sum_i
/// \min_u (\rho_\kappa c_{\kappa,u,v,i} + \text{tree}_u)$), over the
/// executions its query may read (constraint (X)). The query outputs are
/// added in `query_order` (positions in `SelectableExecutions::answering`),
/// a later tree reusing the executions an earlier one added; with
/// `reuse_chosen_as_free`, a later query's trees also cost the executions
/// already added as nothing, so it reads them where that is cheapest.
/// When that gives an execution with one reader per context a second
/// consumer, closes a cycle, or reuses a tree with an execution
/// the query must not read, the plan is rebuilt with that execution left
/// out of the query's trees. `None` when no feasible plan is found that
/// way, or when the plan found misses an accuracy target under (K)
/// (`rules`, [`AccuracyRules::selection_meets_targets`]): the cheapest
/// readouts of a ratio may each meet their level and compose above the
/// target, and a bound from such a plan could leave out the readouts the
/// optimum needs.
fn greedy_plan(
    inputs: &SelectionInputs<'_>,
    rules: Option<&AccuracyRules>,
    query_order: &[usize],
    reuse_chosen_as_free: bool,
) -> Option<FeasiblePlan> {
    let selectable = inputs.selectable;
    let executions = selectable.executions();
    let count = executions.len();
    let one_reader: Vec<bool> = (0..count)
        .map(|index| has_one_reader(inputs, index))
        .collect();
    let forbidden = |query: QueryId, e: usize| {
        inputs.physical.logical_candidates.query_must_not_read(
            query.0,
            inputs.physical.physical_candidates[executions[e].candidate.0].logical_candidate_id,
        )
    };
    // Per query, the executions its trees leave out: those it must not
    // read, and those a repair left out for it.
    let mut excluded: BTreeMap<QueryId, Vec<bool>> = BTreeMap::new();
    for (query, _) in selectable.answering() {
        excluded
            .entry(*query)
            .or_insert_with(|| (0..count).map(|e| forbidden(*query, e)).collect());
    }
    // The cheapest trees, per set of executions left out (when no
    // execution is free, the trees depend on that set alone).
    let mut trees: HashMap<Vec<bool>, (Vec<f64>, Vec<TreeInputs>)> = HashMap::new();
    'repair: for _ in 0..REPAIRS {
        let mut plan = GreedyPlan::new(count);
        let mut answering: Vec<(QueryId, usize)> = Vec::new();
        for &position in query_order {
            let (query, outputs) = &selectable.answering()[position];
            let left_out = &excluded[query];
            let free: Option<Vec<bool>> =
                reuse_chosen_as_free.then(|| plan.producers.iter().map(Option::is_some).collect());
            let shared;
            let (tree_cost, chosen) = match &free {
                Some(free) => {
                    shared = cheapest_trees(inputs, left_out, Some(free));
                    (&shared.0, &shared.1)
                }
                None => {
                    let (cost, chosen) = trees
                        .entry(left_out.clone())
                        .or_insert_with(|| cheapest_trees(inputs, left_out, None));
                    (&*cost, &*chosen)
                }
            };
            let best = outputs
                .iter()
                .copied()
                .filter(|&index| tree_cost[index].is_finite())
                .min_by(|&a, &b| tree_cost[a].total_cmp(&tree_cost[b]))?;
            answering.push((*query, best));
            if let Err(repair) = plan.add_tree(best, chosen, &one_reader, |e| forbidden(*query, e))
            {
                excluded.get_mut(query).expect("a query")[repair] = true;
                continue 'repair;
            }
        }
        // In the selection's query order, as `selection_cost_per_second`
        // and (K) read it.
        answering.sort_by_key(|&(query, _)| query);
        if rules.is_some_and(|rules| {
            rules
                .selection_meets_targets(inputs, &plan.edges, &answering)
                .is_err()
        }) {
            return None;
        }
        let (cost, _) =
            selection_cost_per_second(inputs, &plan.executions, &plan.edges, &answering);
        return cost.is_finite().then_some(FeasiblePlan {
            cost_per_second: cost,
            executions: plan.executions,
            edges: plan.edges,
            answering,
        });
    }
    None
}

/// A feasible plan found greedily ([`greedy_plan`]): its cost per second,
/// less the request overhead every plan pays, which bounds the MILP's
/// optimum from above, and its executions, edges and query outputs (by
/// position in `SelectableExecutions`), a starting solution for the MILP.
#[derive(Debug, Clone)]
pub(crate) struct FeasiblePlan {
    pub(crate) cost_per_second: f64,
    pub(crate) executions: Vec<usize>,
    pub(crate) edges: Vec<usize>,
    pub(crate) answering: Vec<(QueryId, usize)>,
}

/// The cheapest feasible plan found greedily: one tree per query
/// ([`greedy_plan`] in query order); with `sharing_aware`, also the plans
/// that charge the executions earlier queries chose as free, the queries
/// taken in order and in reverse order, so that a state a later query
/// reads is not bought twice.
///
/// Example: `quantile_over_time(0.99, m[1h])` every 15 s and every 60 s:
/// one tree per query buys a 15 s chain and a 60 s chain; charging the
/// 15 s chain as free, the 60 s query reads it too, a cheaper plan.
fn feasible_plan(
    inputs: &SelectionInputs<'_>,
    rules: Option<&AccuracyRules>,
    sharing_aware: bool,
) -> Option<FeasiblePlan> {
    let forward: Vec<usize> = (0..inputs.selectable.answering().len()).collect();
    let mut plans = vec![greedy_plan(inputs, rules, &forward, false)];
    if sharing_aware {
        let reverse: Vec<usize> = forward.iter().rev().copied().collect();
        plans.push(greedy_plan(inputs, rules, &forward, true));
        plans.push(greedy_plan(inputs, rules, &reverse, true));
    }
    plans
        .into_iter()
        .flatten()
        .min_by(|a, b| a.cost_per_second.total_cmp(&b.cost_per_second))
}

/// Per execution not `excluded`, the cost of its cheapest tree and, per
/// input, the edge it reads it over: relaxed until no tree gets cheaper
/// (at most once per execution), so a tree over a cycle of executions is
/// never cheaper than one without it when costs are not negative.
fn cheapest_trees(
    inputs: &SelectionInputs<'_>,
    excluded: &[bool],
    free: Option<&[bool]>,
) -> (Vec<f64>, Vec<TreeInputs>) {
    let selectable = inputs.selectable;
    let count = excluded.len();
    // An execution already in the plan, with its tree, costs nothing more.
    let mut tree_cost: Vec<f64> = (0..count)
        .map(|v| match free {
            Some(free) if free[v] && !excluded[v] => 0.0,
            _ => f64::INFINITY,
        })
        .collect();
    let mut chosen: Vec<TreeInputs> = (0..count)
        .map(|v| vec![None; selectable.input_count(v)])
        .collect();
    for _ in 0..count.max(1) {
        let mut changed = false;
        for v in 0..count {
            let Ok(own) = &inputs.costs.per_execution[v] else {
                continue;
            };
            if excluded[v] || free.is_some_and(|free| free[v]) {
                continue;
            }
            let mut cost = *own;
            let mut reads = Vec::with_capacity(selectable.input_count(v));
            for i in 0..selectable.input_count(v) {
                let cheapest = selectable
                    .edges_into(v, i)
                    .iter()
                    .filter_map(|&position| {
                        let c = inputs.costs.per_edge[position]?;
                        let u = selectable.edges()[position].producer;
                        (!excluded[u] && tree_cost[u].is_finite())
                            .then(|| (position, u, c + tree_cost[u]))
                    })
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
            // Cheaper by more than rounding (the smallest positive
            // magnitude when the cost is 0).
            if cost < tree_cost[v] - COST_RELATIVE_TOLERANCE * cost.abs().max(1e-300) {
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
    /// Its executions, each once, producers first.
    executions: Vec<usize>,
    /// Its edges, by position in `SelectableExecutions::edges`.
    edges: Vec<usize>,
    /// Per execution, its producers in the plan; `None` when not in it yet.
    producers: Vec<Option<Vec<usize>>>,
}

impl GreedyPlan {
    /// The empty plan over `count` executions.
    fn new(count: usize) -> Self {
        Self {
            executions: Vec::new(),
            edges: Vec::new(),
            producers: vec![None; count],
        }
    }

    /// Adds the tree `chosen` below `output` (each execution's inputs, by
    /// execution), reusing the executions already in the plan with their
    /// trees; Err with an execution to leave out of the query's trees: one
    /// with `one_reader` per context read a second time, one closing a
    /// cycle, or one already in the plan whose tree reads an execution
    /// `forbidden` to the query.
    fn add_tree(
        &mut self,
        output: usize,
        chosen: &[TreeInputs],
        one_reader: &[bool],
        forbidden: impl Fn(usize) -> bool,
    ) -> Result<(), usize> {
        if self.producers[output].is_some() {
            return self.reuse(output, one_reader, &forbidden);
        }
        // Executions on the walk, not yet in the plan.
        let mut on_walk = vec![false; self.producers.len()];
        on_walk[output] = true;
        let mut stack = vec![(output, 0usize)];
        let mut walked: Vec<Vec<usize>> = vec![Vec::new(); self.producers.len()];
        while let Some((v, i)) = stack.pop() {
            if i == chosen[v].len() {
                on_walk[v] = false;
                self.producers[v] = Some(std::mem::take(&mut walked[v]));
                self.executions.push(v);
                continue;
            }
            stack.push((v, i + 1));
            let (position, u) = chosen[v][i].expect("a finite tree reads every input");
            self.edges.push(position);
            walked[v].push(u);
            if on_walk[u] {
                return Err(u);
            }
            match self.producers[u] {
                Some(_) => self.reuse(u, one_reader, &forbidden)?,
                None => {
                    on_walk[u] = true;
                    stack.push((u, 0));
                }
            }
        }
        Ok(())
    }

    /// Reads `execution`, already in the plan, a second time: Err with it
    /// when it has `one_reader` per context or its tree reads an execution
    /// `forbidden` to the reader's query.
    fn reuse(
        &self,
        execution: usize,
        one_reader: &[bool],
        forbidden: &impl Fn(usize) -> bool,
    ) -> Result<(), usize> {
        if one_reader[execution] {
            return Err(execution);
        }
        let mut pending = vec![execution];
        let mut seen = vec![false; self.producers.len()];
        while let Some(e) = pending.pop() {
            if std::mem::replace(&mut seen[e], true) {
                continue;
            }
            if forbidden(e) {
                return Err(execution);
            }
            pending.extend(self.producers[e].iter().flatten());
        }
        Ok(())
    }
}
