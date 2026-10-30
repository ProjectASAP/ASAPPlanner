//! A feasible starting plan for the selection MILP, so HiGHS always has an
//! incumbent and a deadline yields a plan, never "no plan".
//!
//! Each query is planned alone, with no sharing: from each output reference
//! down, every input takes the producer of the cheapest subtree (a shortest
//! derivation over the candidate graph, costs at the query's rate, plus a
//! tiny constant per node so the chosen derivation is acyclic). A shareable
//! candidate becomes the query's own instance of it, the next unused index
//! of that candidate, so instances stay a prefix (symmetry breaking) and at
//! most one per query (`m_s` is the number of queries reaching it).
//!
//! Rows the derivation could break are checked per query and repaired:
//! - candidates the query may not use: approximate ones at an unusable
//!   position or in a denominator with `parts < 2`, any approximate one for
//!   an exact target, candidates needing two or more readers
//!   (`MinimumQueryReaders`), candidates whose `AllowedOutputs` exclude the
//!   query;
//! - accuracy: if the plan reads `N` approximate instances and one has
//!   `parts < N`, candidates with `parts < N` are excluded and the query is
//!   planned again, down to an exact plan;
//! - latency: if the plan is over the query's bound, the query is planned
//!   again by latency alone; still over it, no plan exists for the query.
//!
//! With no sharing, every consumer is reached by one query, so the highest
//! shared materialization rows hold. Only binaries are set; HiGHS fixes them
//! and solves an LP for the continuous variables
//! (`completeSolutionFromDiscreteAssignment`).
use std::collections::{BTreeSet, HashMap};

use asap_cost::execution_demand::evaluation_rate;
use asap_types::stages::{PhysicalCandidateId, ReaderConstraint};
use good_lp::Variable;

use crate::owners::AccuracyWeight;

use super::model::{
    latency_bound_ms, reader_constraints_of_candidates, SelectionMilp, SelectionMilpError,
};

/// The constant each node adds to a derivation's cost, so equal-cost
/// cycles are never chosen.
const NODE_TIE_BREAK: f64 = 1e-12;

/// What a derivation minimizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Objective {
    Cost,
    Latency,
}

/// The binaries of a feasible plan with no sharing: every other binary is
/// 0. Err names the first query with no plan meeting its rows.
pub(crate) fn warm_start(
    milp: &SelectionMilp<'_>,
) -> Result<Vec<(Variable, f64)>, SelectionMilpError> {
    let inputs = milp.inputs;
    let count = milp.physical.candidates.len();
    let horizon = milp.cost_inputs.non_repeating_query_horizon_seconds;
    let request_ms = milp.cost_inputs.measured_operation_costs.ns_per_evaluation * 1e-6;
    let constraints = reader_constraints_of_candidates(milp);
    let mut start = Start::new(milp);
    for query in 0..milp.query_count() {
        let entry = &milp.entries[query];
        let rate =
            evaluation_rate([(&entry.recurrence, entry.range_evaluation)], horizon).unwrap_or(0.0);
        let exact_target = entry.requirements.accuracy.is_exact();
        // Candidates the query may never use.
        let mut excluded = vec![false; count];
        for (candidate, data) in inputs.candidate_data.iter().enumerate() {
            let id = PhysicalCandidateId(candidate);
            if data.usage.is_err()
                || (milp.shareable(id) && !milp.instances_of_candidate.contains_key(&id))
            {
                excluded[candidate] = true;
                continue;
            }
            if data.approximate {
                excluded[candidate] |= exact_target
                    || match inputs.accuracy_weights[query].get(&id) {
                        None | Some(AccuracyWeight::One) => false,
                        Some(AccuracyWeight::Two) => data.accuracy_budget_parts < 2,
                        Some(AccuracyWeight::Unusable(_)) => true,
                    };
            }
            for constraint in constraints.get(&id).into_iter().flatten() {
                excluded[candidate] |= match constraint {
                    ReaderConstraint::MinimumQueryReaders { minimum, .. } => *minimum > 1,
                    ReaderConstraint::AllowedOutputs { outputs, .. } => {
                        (0..milp.query_of_reference.len()).any(|reference| {
                            milp.query_of_reference[reference] == query
                                && !outputs.contains(&reference)
                        })
                    }
                    _ => false,
                };
            }
        }
        let bound = latency_bound_ms(entry);
        let mut objective = Objective::Cost;
        let plan = loop {
            let plan = derive(milp, query, rate, objective, &excluded)
                .ok_or_else(|| no_plan(query, "no derivation over the candidates it may use"))?;
            // Accuracy: one share per approximate instance.
            let approximate: Vec<PhysicalCandidateId> = plan
                .shareable
                .iter()
                .copied()
                .filter(|candidate| inputs.candidate_data[candidate.0].approximate)
                .collect();
            let shares = approximate.len();
            if approximate
                .iter()
                .any(|candidate| inputs.candidate_data[candidate.0].accuracy_budget_parts < shares)
            {
                for (candidate, data) in inputs.candidate_data.iter().enumerate() {
                    if data.approximate && data.accuracy_budget_parts < shares {
                        excluded[candidate] = true;
                    }
                }
                continue;
            }
            let latency = request_ms + plan.latency_ms;
            match bound {
                Some(bound) if latency > bound => {
                    if objective == Objective::Latency {
                        return Err(no_plan(
                            query,
                            &format!(
                                "its fastest plan takes {latency:.3} ms, over its {bound} ms bound"
                            ),
                        ));
                    }
                    objective = Objective::Latency;
                }
                _ => break plan,
            }
        };
        start.add_query(query, &plan);
    }
    Ok(start.finish())
}

fn no_plan(query: usize, reason: &str) -> SelectionMilpError {
    SelectionMilpError::NoPlanForQuery {
        query,
        reason: reason.to_string(),
    }
}

/// One query's derivation: per candidate used, its chosen producer per
/// input, and the shareable candidates and latency of the whole tree.
struct Derivation {
    /// The output reference's chosen candidate, per output of the query.
    outputs: Vec<(usize, PhysicalCandidateId)>,
    producer_of: Vec<Vec<PhysicalCandidateId>>,
    shareable: BTreeSet<PhysicalCandidateId>,
    latency_ms: f64,
}

/// The cheapest derivation of query `query`'s outputs over the candidates
/// not `excluded` (by cost at `rate`, or by latency), or `None`.
fn derive(
    milp: &SelectionMilp<'_>,
    query: usize,
    rate: f64,
    objective: Objective,
    excluded: &[bool],
) -> Option<Derivation> {
    let inputs = milp.inputs;
    let count = milp.physical.candidates.len();
    let local: Vec<f64> = (0..count)
        .map(|candidate| {
            let Ok(usage) = &inputs.candidate_data[candidate].usage else {
                return f64::INFINITY;
            };
            let value = match objective {
                Objective::Cost => {
                    usage.cost_per_execution.unwrap_or(0.0) * rate
                        + usage.standing_cost_per_second.unwrap_or(0.0)
                }
                Objective::Latency => usage.own_latency_ms,
            };
            value.max(0.0) + NODE_TIE_BREAK
        })
        .collect();
    // Shortest derivations: a fixpoint, each candidate its own cost plus
    // its inputs' cheapest producers.
    let mut best = vec![f64::INFINITY; count];
    let mut changed = true;
    while changed {
        changed = false;
        for candidate in 0..count {
            if excluded[candidate] {
                continue;
            }
            let mut total = local[candidate];
            for producers in &inputs.producer_candidates_per_input[candidate] {
                let cheapest = producers
                    .iter()
                    .filter(|producer| !excluded[producer.0])
                    .map(|producer| best[producer.0])
                    .fold(f64::INFINITY, f64::min);
                total += cheapest;
            }
            if total < best[candidate] * (1.0 - 1e-15) {
                best[candidate] = total;
                changed = true;
            }
        }
    }
    let mut derivation = Derivation {
        outputs: Vec::new(),
        producer_of: vec![Vec::new(); count],
        shareable: BTreeSet::new(),
        latency_ms: 0.0,
    };
    let mut stack = Vec::new();
    for (reference, &owner) in milp.query_of_reference.iter().enumerate() {
        if owner != query {
            continue;
        }
        let chosen = inputs.output_candidates[reference]
            .iter()
            .copied()
            .filter(|candidate| best[candidate.0].is_finite())
            .min_by(|a, b| best[a.0].total_cmp(&best[b.0]))?;
        derivation.outputs.push((reference, chosen));
        stack.push(chosen);
    }
    let mut seen = vec![false; count];
    while let Some(candidate) = stack.pop() {
        if std::mem::replace(&mut seen[candidate.0], true) {
            continue;
        }
        if milp.shareable(candidate) {
            derivation.shareable.insert(candidate);
        }
        if let Ok(usage) = &inputs.candidate_data[candidate.0].usage {
            derivation.latency_ms += usage.own_latency_ms;
        }
        for producers in &inputs.producer_candidates_per_input[candidate.0] {
            let chosen = producers
                .iter()
                .copied()
                .filter(|producer| !excluded[producer.0] && best[producer.0].is_finite())
                .min_by(|a, b| best[a.0].total_cmp(&best[b.0]))?;
            derivation.producer_of[candidate.0].push(chosen);
            stack.push(chosen);
        }
    }
    Some(derivation)
}

/// The binaries set so far.
struct Start<'m, 'a> {
    milp: &'m SelectionMilp<'a>,
    values: HashMap<Variable, f64>,
    /// Per shareable candidate, the instances given out so far.
    next_instance: HashMap<PhysicalCandidateId, usize>,
}

impl<'m, 'a> Start<'m, 'a> {
    fn new(milp: &'m SelectionMilp<'a>) -> Self {
        Self {
            milp,
            values: HashMap::new(),
            next_instance: HashMap::new(),
        }
    }

    fn set(&mut self, variable: Variable) {
        self.values.insert(variable, 1.0);
    }

    /// Query `query`'s derivation `plan`, with its own instances.
    fn add_query(&mut self, query: usize, plan: &Derivation) {
        let milp = self.milp;
        // The query's instance of each shareable candidate it uses.
        let mut instance_of: HashMap<PhysicalCandidateId, usize> = HashMap::new();
        for &candidate in &plan.shareable {
            let next = self.next_instance.entry(candidate).or_default();
            let instance = milp.instances_of_candidate[&candidate][*next];
            *next += 1;
            instance_of.insert(candidate, instance);
            self.set(milp.active[instance]);
        }
        for &(reference, chosen) in &plan.outputs {
            if milp.shareable(chosen) {
                let instance = instance_of[&chosen];
                let read = milp.output_reads[reference]
                    .iter()
                    .find(|(known, _)| *known == instance)
                    .map(|(_, variable)| *variable)
                    .expect("an output read of each instance");
                self.set(read);
            } else {
                let output = milp.outputs[reference]
                    .iter()
                    .find(|(known, _)| *known == chosen)
                    .map(|(_, variable)| *variable)
                    .expect("an output variable of each stateless output");
                self.set(output);
                self.add_node(query, chosen, plan, &instance_of);
            }
        }
        for (&candidate, &instance) in &instance_of {
            self.add_node(milp.instance_owner(instance), candidate, plan, &instance_of);
        }
    }

    /// Owner `owner`'s use of `node` and, below it in the same owner, the
    /// stateless nodes and the reads of instances.
    fn add_node(
        &mut self,
        owner: usize,
        node: PhysicalCandidateId,
        plan: &Derivation,
        instance_of: &HashMap<PhysicalCandidateId, usize>,
    ) {
        let milp = self.milp;
        let variables = &milp.owners[owner];
        let mut stack = vec![node];
        let mut seen = BTreeSet::new();
        while let Some(reader) = stack.pop() {
            if !seen.insert(reader) {
                continue;
            }
            if let Some(&used) = variables.used.get(&reader) {
                self.set(used);
            }
            for (input, &producer) in plan.producer_of[reader.0].iter().enumerate() {
                if milp.shareable(producer) {
                    let instance = instance_of[&producer];
                    if let Some(&(_, read)) = variables
                        .reads
                        .iter()
                        .find(|(key, _)| *key == (reader, input, instance))
                    {
                        self.set(read);
                    }
                } else {
                    if let Some(&(_, link)) = variables
                        .links
                        .iter()
                        .find(|(key, _)| *key == (reader, input, producer))
                    {
                        self.set(link);
                    }
                    stack.push(producer);
                }
            }
        }
    }

    /// Every binary's value, the highest-shared-materialization indicators
    /// included: an instance reading another is reached by its one query.
    fn finish(mut self) -> Vec<(Variable, f64)> {
        let milp = self.milp;
        for &(reader, instance, single) in &milp.single_query_consumers {
            let owner = milp.instance_owner(reader);
            let reads = milp.owners[owner]
                .reads
                .iter()
                .any(|((_, _, read), variable)| {
                    *read == instance && self.values.contains_key(variable)
                });
            if reads {
                self.set(single);
            }
        }
        self.values.into_iter().collect()
    }
}
