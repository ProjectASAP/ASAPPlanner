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
//!
//! Also left out: an execution with $\ell$ = None that its `OneEvaluation`
//! twin dominates ([`dominated_by_one_evaluation_twin`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use asap_types::ir::properties::ExecutionTiming;
use asap_types::stages::{CandidateForMemoGroupId, ExecutionContextId, Lifetime};

use super::exact_only::must_not_read;
use super::model::{selection_cost_per_second, SelectionInputs};
use crate::retention::output_retention_need_ms;
use crate::selectable::{has_one_reader_per_context, QueryId};

/// How many times the greedy plan is rebuilt without an execution that
/// made it infeasible before giving up on a bound.
const REPAIRS: usize = 32;

/// Per (context, logical candidate), its query-time executions as
/// (execution, lifetime, cost per second).
type Twins = HashMap<(ExecutionContextId, CandidateForMemoGroupId), Vec<(usize, Lifetime, f64)>>;

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
/// None is ruled out when no feasible plan is found.
pub(crate) fn dearer_than_a_feasible_plan(inputs: &SelectionInputs<'_>) -> Vec<bool> {
    let selectable = inputs.selectable;
    let count = selectable.executions().len();
    let Some(bound) = feasible_plan_cost_per_second(inputs) else {
        return vec![false; count];
    };
    let costs = inputs.costs;
    let executions = selectable.executions();
    // The least retention, in ms, each execution of a candidate with $h_v$
    // is read back over its uses (a costed edge out, or a query output):
    // (F) gives a selected execution a use, and (R) then retains at least
    // that. A query-time scan's retention is its source's $r_s$, left out.
    let mut least_need_ms: Vec<Option<u64>> = vec![None; count];
    let mut read = |index: usize, need_ms: u64| {
        let least = &mut least_need_ms[index];
        *least = Some(least.map_or(need_ms, |least| least.min(need_ms)));
    };
    for (position, &(_, _, producer)) in selectable.edges().iter().enumerate() {
        if let (Some(_), Some(need_ms)) = (
            costs.per_edge[position],
            costs.retention_need_ms_per_edge[position],
        ) {
            read(producer, need_ms);
        }
    }
    for (query, answering) in &selectable.answering {
        for &index in answering {
            read(index, output_retention_need_ms(&inputs.entries[query.0]));
        }
    }
    let least_retention_cost = |index: usize| {
        let v = executions[index].candidate;
        match (
            costs.retention_cost_per_retained_second.get(&v),
            least_need_ms[index],
        ) {
            (Some(per_retained_second), Some(need_ms))
                if !costs.raw_data_source.contains_key(&v) =>
            {
                per_retained_second * need_ms as f64 / 1_000.0
            }
            _ => 0.0,
        }
    };
    let tolerance = 1e-9 * bound.abs().max(1e-12);
    (0..count)
        .map(|index| {
            let Ok(own) = &costs.per_execution[index] else {
                return false;
            };
            let least = own
                + least_retention_cost(index)
                + (0..selectable.input_count(index))
                    .map(|i| {
                        selectable
                            .edges_into(index, i)
                            .iter()
                            .filter_map(|&position| costs.per_edge[position])
                            .fold(0.0, f64::min)
                    })
                    .sum::<f64>();
            least > bound + tolerance
        })
        .collect()
}

/// The cost per second (less the request overhead every plan pays) of a
/// feasible plan ([`selection_cost_per_second`]): each query output reads
/// the cheapest tree of executions below it, each execution's input from
/// the producer whose tree is cheapest ($\rho_\kappa c_{\kappa,v} + \sum_i
/// \min_u (\rho_\kappa c_{\kappa,u,v,i} + \text{tree}_u)$), over the
/// executions its query may read (constraint (X)). The query outputs are
/// added in order, a later tree reusing the executions an earlier one
/// added. When that gives an execution with one reader per context a
/// second consumer, closes a cycle, or reuses a tree with an execution
/// the query must not read, the plan is rebuilt with that execution left
/// out of the query's trees. `None` when no feasible plan is found that
/// way.
fn feasible_plan_cost_per_second(inputs: &SelectionInputs<'_>) -> Option<f64> {
    let selectable = inputs.selectable;
    let executions = selectable.executions();
    let count = executions.len();
    let one_reader: Vec<bool> = executions
        .iter()
        .map(|execution| {
            has_one_reader_per_context(
                inputs.physical.physical_candidates[execution.candidate.0].physical_properties,
            )
        })
        .collect();
    let forbidden =
        |query: QueryId, e: usize| must_not_read(inputs.physical, query, executions[e].candidate);
    // Per query, the executions its trees leave out: those it must not
    // read, and those a repair left out for it.
    let mut excluded: BTreeMap<QueryId, Vec<bool>> = BTreeMap::new();
    for (query, _) in &selectable.answering {
        excluded
            .entry(*query)
            .or_insert_with(|| (0..count).map(|e| forbidden(*query, e)).collect());
    }
    // The cheapest trees, per set of executions left out.
    let mut trees: HashMap<Vec<bool>, (Vec<f64>, Vec<TreeInputs>)> = HashMap::new();
    'repair: for _ in 0..REPAIRS {
        let mut plan = GreedyPlan::new(count);
        let mut answering: Vec<(QueryId, usize)> = Vec::new();
        for (query, outputs) in &selectable.answering {
            let left_out = &excluded[query];
            let (tree_cost, chosen) = trees
                .entry(left_out.clone())
                .or_insert_with(|| cheapest_trees(inputs, left_out));
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
        let (cost, _) =
            selection_cost_per_second(inputs, &plan.executions, &plan.edges, &answering);
        return cost.is_finite().then_some(cost);
    }
    None
}

/// Per execution not `excluded`, the cost of its cheapest tree and, per
/// input, the edge it reads it over: relaxed until no tree gets cheaper
/// (at most once per execution), so a tree over a cycle of executions is
/// never cheaper than one without it when costs are not negative.
fn cheapest_trees(inputs: &SelectionInputs<'_>, excluded: &[bool]) -> (Vec<f64>, Vec<TreeInputs>) {
    let selectable = inputs.selectable;
    let count = excluded.len();
    let mut tree_cost = vec![f64::INFINITY; count];
    let mut chosen: Vec<TreeInputs> = (0..count)
        .map(|v| vec![None; selectable.input_count(v)])
        .collect();
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
            let mut reads = Vec::with_capacity(selectable.input_count(v));
            for i in 0..selectable.input_count(v) {
                let cheapest = selectable
                    .edges_into(v, i)
                    .iter()
                    .filter_map(|&position| {
                        let c = inputs.costs.per_edge[position]?;
                        let (_, _, u) = selectable.edges()[position];
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
    let count = executions.len();
    let no_more = |cost: f64, than: f64| cost <= than + 1e-12 * than.abs().max(1e-12);
    // Per execution, its costed edges in, by (producer, input), and out, by
    // (consumer, input).
    let mut edges_in: Vec<HashMap<(usize, usize), f64>> = vec![HashMap::new(); count];
    let mut edges_out: Vec<HashMap<(usize, usize), f64>> = vec![HashMap::new(); count];
    for (position, &(consumer, edge, producer)) in selectable.edges().iter().enumerate() {
        let Some(cost) = costs.per_edge[position] else {
            continue;
        };
        let input = physical.edge(edge).input_index_of_consumer;
        edges_in[consumer].insert((producer, input), cost);
        edges_out[producer].insert((consumer, input), cost);
    }
    let mut answers: Vec<BTreeSet<QueryId>> = vec![BTreeSet::new(); count];
    for (query, answering) in &selectable.answering {
        for &index in answering {
            answers[index].insert(*query);
        }
    }
    // The query-time executions of each logical candidate in each context,
    // without $h_v$ or raw data, with their cost.
    let mut twins: Twins = HashMap::new();
    for (index, execution) in executions.iter().enumerate() {
        let v = execution.candidate;
        let candidate = &physical.physical_candidates[v.0];
        let properties = candidate.physical_properties;
        if properties.timing() != ExecutionTiming::QueryTime
            || costs.retention_cost_per_retained_second.contains_key(&v)
            || costs.raw_data_source.contains_key(&v)
        {
            continue;
        }
        let Ok(cost) = costs.per_execution[index] else {
            continue;
        };
        twins
            .entry((execution.context, candidate.logical_candidate_id))
            .or_default()
            .push((index, properties.lifetime(), cost));
    }
    let mut dominated = vec![false; count];
    for members in twins.values() {
        for &(a, lifetime, cost_a) in members {
            if lifetime != Lifetime::None {
                continue;
            }
            dominated[a] = members.iter().any(|&(b, lifetime, cost_b)| {
                matches!(lifetime, Lifetime::OneEvaluation(_))
                    && no_more(cost_b, cost_a)
                    && edges_in[a].iter().all(|(key, &cost)| {
                        edges_in[b]
                            .get(key)
                            .is_some_and(|&twin| no_more(twin, cost))
                    })
                    && edges_out[a].iter().all(|(key, &cost)| {
                        edges_out[b]
                            .get(key)
                            .is_some_and(|&twin| no_more(twin, cost))
                    })
                    && answers[a].is_subset(&answers[b])
            });
        }
    }
    dominated
}
