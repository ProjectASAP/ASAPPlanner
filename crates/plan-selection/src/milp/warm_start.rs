//! A feasible starting plan for the selection MILP, so HiGHS always has an
//! incumbent and a deadline yields a plan, never "no plan".
//!
//! **Unshared plan.** Each query is planned alone, with no sharing: from
//! each output reference down, every input takes the producer of the
//! cheapest subtree (a shortest derivation over the candidate graph, costs
//! at the query's rate, plus a tiny constant per node so the chosen
//! derivation is acyclic). A shareable candidate becomes the query's own
//! instance of it, the next unused index of that candidate, so instances
//! stay a prefix (symmetry breaking) and at most one per query (`m_s` is the
//! number of queries reaching it).
//!
//! Rows the derivation could break are checked per query and repaired:
//! - candidates the query may not use: approximate ones unusable for it
//!   (`AccuracyWeight::Unusable`), any approximate one for an exact target, candidates needing two or more readers
//!   (`MinimumQueryReaders`), candidates whose `AllowedOutputs` exclude the
//!   query;
//! - accuracy: if the plan reads `N` approximate instances and one has
//!   `parts < N`, candidates with `parts < N` are excluded and the query is
//!   planned again, down to an exact plan; if it reads both candidates of
//!   a pair excluded together, the second is excluded and it is planned
//!   again;
//! - latency: if the plan is over the query's bound, the query is planned
//!   again by latency alone; still over it, no plan exists for the query.
//!
//! **Sharing.** The unshared plan is then improved by local search. Every
//! plan it considers is costed exactly, by the MILP's own rows and objective
//! ([`Completion`]: the binaries of the plan and the cheapest values of the
//! continuous variables), and kept only when it is feasible and cheaper:
//! - *re-plan a query*: drop its own nodes and the instances only it read,
//!   and derive it again, where reading an instance another query already
//!   runs costs only what that read adds (nothing at ingestion time or for a
//!   materialized result of the same schedule);
//! - *move off a candidate*: re-plan, one after another, every query that
//!   reaches an instance of a candidate, none of them using it, so they
//!   move together onto another candidate (a materialized result two queries
//!   share costs more than either's not-materialized one alone, so neither
//!   moves alone);
//! - *merge two instances* of one operator: every reader of one reads the
//!   other, and the instances nothing reads any more are dropped.
//!
//! The search starts from three plans and keeps the cheapest result: the
//! unshared one; an amortized one, each query planned with each shareable
//! candidate's kept state (and ingestion-time work) divided by the number of
//! queries that can reach it, which favours what sharing would pay for; and
//! a sequential one, each query planned in turn reading what the queries
//! before it run. It stops when no move helps or at its budget, and returns
//! every variable's value, so HiGHS starts from a complete feasible
//! solution.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Instant;

use asap_cost::execution_demand::evaluation_rate;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::physical::Materialization;
use asap_types::stages::{PhysicalCandidateId, ReaderConstraint};
use good_lp::Variable;

use crate::owners::AccuracyWeight;

use super::completion::Completion;
use super::model::{
    latency_bound_ms, reader_constraints_of_candidates, SelectionMilp, SelectionMilpError,
};

/// The constant each node adds to a derivation's cost, so equal-cost
/// cycles are never chosen.
const NODE_TIE_BREAK: f64 = 1e-12;

/// A plan must be cheaper by this fraction to replace the current one.
const IMPROVEMENT: f64 = 1e-9;

/// What a derivation minimizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Objective {
    Cost,
    Latency,
}

/// The starting plan.
pub(crate) struct StartingPlan {
    /// Values of the variables: every variable when `cost_per_second` is
    /// known, else only the binaries set to 1 (HiGHS completes the rest).
    pub(crate) values: Vec<(Variable, f64)>,
    /// Its objective, when the completion costed it.
    pub(crate) cost_per_second: Option<f64>,
}

/// A feasible plan, improved by sharing until `budget` (module
/// documentation). Err names the first query with no plan alone meeting
/// its rows.
pub(crate) fn warm_start(
    milp: &SelectionMilp<'_>,
    budget: Option<Instant>,
) -> Result<StartingPlan, SelectionMilpError> {
    let queries = QueryData::new(milp);
    let mut unshared = Plan::new(milp);
    for query in 0..milp.query_count() {
        let derivation = derive_alone(milp, &queries, query, false)?;
        unshared
            .add(milp, query, &derivation)
            .expect("an unshared plan has an instance index per query");
    }
    let lookup = Lookup::new(milp);
    let only_binaries =
        |plan: &Plan| -> Vec<(Variable, f64)> { lookup.binaries(milp, plan).unwrap_or_default() };
    let completion = Completion::new(milp);
    let mut search = Search {
        milp,
        queries: &queries,
        lookup: &lookup,
        completion: &completion,
    };
    let Some((unshared_cost, _)) = search.cost(&unshared) else {
        return Ok(StartingPlan {
            values: only_binaries(&unshared),
            cost_per_second: None,
        });
    };
    let over = || budget.is_some_and(|budget| Instant::now() >= budget);
    // The other starting points: amortized, and sequential (each query in
    // turn reading what the queries before it run). A query with no
    // amortized plan meeting its rows keeps its unshared derivation.
    let mut seeds = vec![(unshared, unshared_cost)];
    let mut amortized = Some(Plan::new(milp));
    for query in 0..milp.query_count() {
        let derivation = match derive_alone(milp, &queries, query, true) {
            Ok(derivation) => derivation,
            Err(_) => derive_alone(milp, &queries, query, false)?,
        };
        amortized =
            amortized.and_then(|mut plan| plan.add(milp, query, &derivation).map(|()| plan));
    }
    if let Some(amortized) = amortized {
        if let Some((amortized_cost, _)) = search.cost(&amortized) {
            seeds.push((amortized, amortized_cost));
        }
    }
    let all: Vec<usize> = (0..milp.query_count()).collect();
    if let Some(sequential) = search.replan(&seeds[0].0, &all, None) {
        if let Some((sequential_cost, _)) = search.cost(&sequential) {
            seeds.push((sequential, sequential_cost));
        }
    }
    seeds.sort_by(|a, b| a.1.total_cmp(&b.1));
    let mut best: Option<(Plan, f64)> = None;
    for (seed, seed_cost) in seeds {
        if over() && best.is_some() {
            break;
        }
        let (plan, cost) = search.improve(seed, seed_cost, &over);
        if best.as_ref().is_none_or(|(_, known)| cost < *known) {
            best = Some((plan, cost));
        }
    }
    let (plan, _) = best.expect("the unshared seed");
    let (cost, columns) = search.cost(&plan).expect("the kept plan was costed");
    let values = milp
        .builder
        .variables
        .iter_variables_with_def()
        .zip(columns)
        .map(|((variable, _), value)| (variable, value))
        .collect();
    Ok(StartingPlan {
        values,
        cost_per_second: Some(cost),
    })
}

/// Every ordered pair of active instances whose candidates run the same
/// operator (of one candidate, or of two whose readers may read either):
/// merge the first into the second.
fn merge_pairs(milp: &SelectionMilp<'_>, plan: &Plan) -> Vec<(Key, Key)> {
    let kind = |key: &Key| milp.physical.physical_node(key.0).operator.kind_name();
    let keys: Vec<Key> = plan.instances.keys().copied().collect();
    let mut pairs = Vec::new();
    for &from in &keys {
        for &into in &keys {
            if from != into && kind(&from) == kind(&into) {
                pairs.push((from, into));
            }
        }
    }
    pairs
}

/// Per query: its rate and the candidates it may not use.
struct QueryData {
    rate: Vec<f64>,
    /// Alone (no other query reads what it runs).
    excluded: Vec<Vec<bool>>,
    /// With other queries: candidates needing several readers are allowed
    /// (the completion checks that they get them).
    excluded_shared: Vec<Vec<bool>>,
}

impl QueryData {
    fn new(milp: &SelectionMilp<'_>) -> Self {
        let inputs = milp.inputs;
        let count = milp.physical.candidates.len();
        let horizon = milp.cost_inputs.non_repeating_query_horizon_seconds;
        let constraints = reader_constraints_of_candidates(milp);
        let mut rate = Vec::new();
        let mut excluded_per_query = Vec::new();
        let mut excluded_shared_per_query = Vec::new();
        for query in 0..milp.query_count() {
            let entry = &milp.entries[query];
            rate.push(
                evaluation_rate([(&entry.recurrence, entry.range_evaluation)], horizon)
                    .unwrap_or(0.0),
            );
            let exact_target = entry.requirements.accuracy.is_exact();
            let mut excluded = vec![false; count];
            let mut needs_readers = vec![false; count];
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
                        || matches!(
                            inputs.accuracy_weights[query].get(&id),
                            Some(AccuracyWeight::Unusable(_))
                        );
                }
                for constraint in constraints.get(&id).into_iter().flatten() {
                    excluded[candidate] |= match constraint {
                        ReaderConstraint::MinimumQueryReaders { minimum, .. } => {
                            needs_readers[candidate] |= *minimum > 1;
                            false
                        }
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
            excluded_shared_per_query.push(excluded.clone());
            for (excluded, needs) in excluded.iter_mut().zip(needs_readers) {
                *excluded |= needs;
            }
            excluded_per_query.push(excluded);
        }
        Self {
            rate,
            excluded: excluded_per_query,
            excluded_shared: excluded_shared_per_query,
        }
    }
}

/// Query `query`'s derivation alone, repaired for accuracy and latency
/// (module documentation); with `amortized`, each shareable candidate's
/// cost divided by the number of queries that can reach it.
fn derive_alone(
    milp: &SelectionMilp<'_>,
    queries: &QueryData,
    query: usize,
    amortized: bool,
) -> Result<Derivation, SelectionMilpError> {
    let inputs = milp.inputs;
    let request_ms = milp.cost_inputs.measured_operation_costs.ns_per_evaluation * 1e-6;
    let mut excluded = queries.excluded[query].clone();
    let bound = latency_bound_ms(&milp.entries[query]);
    let mut objective = Objective::Cost;
    loop {
        let plan = derive(
            milp,
            queries.rate[query],
            objective,
            &excluded,
            &HashMap::new(),
            amortized,
            query,
        )
        .ok_or_else(|| no_plan(query, "no derivation over the candidates it may use"))?;
        // Accuracy: one share per approximate instance.
        let approximate: Vec<PhysicalCandidateId> = plan
            .new_shareable
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
        // A pair excluded together: drop its second candidate and plan again.
        if let Some(&(_, second)) = inputs.accuracy_excluded_pairs[query]
            .iter()
            .find(|(first, second)| approximate.contains(first) && approximate.contains(second))
        {
            excluded[second.0] = true;
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
            _ => return Ok(plan),
        }
    }
}

fn no_plan(query: usize, reason: &str) -> SelectionMilpError {
    SelectionMilpError::NoPlanForQuery {
        query,
        reason: reason.to_string(),
    }
}

/// An instance `(candidate, j)`.
type Key = (PhysicalCandidateId, usize);

/// What a derivation picks for an input or an output: a candidate it
/// derives (for a shareable one, a new instance), or an instance the plan
/// already runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pick {
    Derived(PhysicalCandidateId),
    Existing(Key),
}

/// One query's derivation: per candidate derived, its pick per input, and
/// the shareable candidates it derives (new instances) and their latency.
struct Derivation {
    /// The output reference's pick, per output of the query.
    outputs: Vec<(usize, Pick)>,
    picks: Vec<Vec<Pick>>,
    new_shareable: BTreeSet<PhysicalCandidateId>,
    latency_ms: f64,
}

/// The cheapest derivation for query `query` over the candidates not
/// `excluded` (by cost at `rate`, or by latency), where shareable candidate
/// `k` may instead read instance `existing[k].0` at cost `existing[k].1`;
/// or `None`.
fn derive(
    milp: &SelectionMilp<'_>,
    rate: f64,
    objective: Objective,
    excluded: &[bool],
    existing: &HashMap<PhysicalCandidateId, (usize, f64)>,
    amortized: bool,
    query: usize,
) -> Option<Derivation> {
    let inputs = milp.inputs;
    let count = milp.physical.candidates.len();
    let local: Vec<f64> = (0..count)
        .map(|candidate| {
            let Ok(usage) = &inputs.candidate_data[candidate].usage else {
                return f64::INFINITY;
            };
            let id = PhysicalCandidateId(candidate);
            let value = match objective {
                Objective::Cost => {
                    let execution = usage.cost_per_execution.unwrap_or(0.0) * rate;
                    let standing = usage.standing_cost_per_second.unwrap_or(0.0);
                    if amortized && milp.shareable(id) {
                        // What sharing divides: what is kept, and work at
                        // ingestion time; query-time work runs per reader
                        // schedule anyway.
                        let readers = inputs.queries_reaching[candidate].len().max(1) as f64;
                        if milp.physical.candidates[candidate].timing
                            == ExecutionTiming::IngestionTime
                        {
                            (execution + standing) / readers
                        } else {
                            execution + standing / readers
                        }
                    } else {
                        execution + standing
                    }
                }
                Objective::Latency => usage.own_latency_ms,
            };
            value.max(0.0) + NODE_TIE_BREAK
        })
        .collect();
    // Shortest derivations: a fixpoint, each candidate its own cost plus
    // its inputs' cheapest producers, or an existing instance's cost.
    let mut derived = vec![f64::INFINITY; count];
    let mut best = vec![f64::INFINITY; count];
    for (&candidate, &(_, cost)) in existing {
        best[candidate.0] = cost;
    }
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
                    .filter(|producer| !excluded[producer.0] || existing.contains_key(producer))
                    .map(|producer| best[producer.0])
                    .fold(f64::INFINITY, f64::min);
                total += cheapest;
            }
            if total < derived[candidate] * (1.0 - 1e-15) {
                derived[candidate] = total;
                if total < best[candidate] {
                    best[candidate] = total;
                }
                changed = true;
            }
        }
    }
    let pick = |candidate: PhysicalCandidateId| match existing.get(&candidate) {
        Some(&(index, cost)) if cost <= derived[candidate.0] => Pick::Existing((candidate, index)),
        _ => Pick::Derived(candidate),
    };
    let usable = |candidate: &PhysicalCandidateId| {
        best[candidate.0].is_finite() && best[candidate.0] < f64::MAX
    };
    let mut derivation = Derivation {
        outputs: Vec::new(),
        picks: vec![Vec::new(); count],
        new_shareable: BTreeSet::new(),
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
            .filter(usable)
            .min_by(|a, b| best[a.0].total_cmp(&best[b.0]))?;
        let chosen = pick(chosen);
        derivation.outputs.push((reference, chosen));
        if let Pick::Derived(candidate) = chosen {
            stack.push(candidate);
        }
    }
    let mut seen = vec![false; count];
    while let Some(candidate) = stack.pop() {
        if std::mem::replace(&mut seen[candidate.0], true) {
            continue;
        }
        if milp.shareable(candidate) {
            derivation.new_shareable.insert(candidate);
        }
        if let Ok(usage) = &inputs.candidate_data[candidate.0].usage {
            derivation.latency_ms += usage.own_latency_ms;
        }
        for producers in &inputs.producer_candidates_per_input[candidate.0] {
            let chosen = producers
                .iter()
                .copied()
                .filter(|producer| {
                    (!excluded[producer.0] || existing.contains_key(producer)) && usable(producer)
                })
                .min_by(|a, b| best[a.0].total_cmp(&best[b.0]))?;
            let chosen = pick(chosen);
            derivation.picks[candidate.0].push(chosen);
            if let Pick::Derived(producer) = chosen {
                stack.push(producer);
            }
        }
    }
    Some(derivation)
}

/// What produces an input or an output in a [`Plan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    Stateless(PhysicalCandidateId),
    Instance(Key),
}

/// One owner's nodes and links in a [`Plan`].
#[derive(Debug, Clone, Default)]
struct OwnerPlan {
    classes: BTreeSet<PhysicalCandidateId>,
    /// `(reader, input, source)`.
    links: BTreeSet<(PhysicalCandidateId, usize, Source)>,
}

/// A plan in the MILP's terms, before it becomes binaries.
#[derive(Debug, Clone)]
struct Plan {
    /// Per output reference.
    outputs: Vec<Option<Source>>,
    query_owners: Vec<OwnerPlan>,
    /// The active instances and their owners' choices.
    instances: BTreeMap<Key, OwnerPlan>,
}

impl Plan {
    fn new(milp: &SelectionMilp<'_>) -> Self {
        Self {
            outputs: vec![None; milp.query_of_reference.len()],
            query_owners: vec![OwnerPlan::default(); milp.query_count()],
            instances: BTreeMap::new(),
        }
    }

    /// Query `query`'s derivation, with a new instance of each shareable
    /// candidate it derives; `None` when a candidate has no instance left.
    fn add(
        &mut self,
        milp: &SelectionMilp<'_>,
        query: usize,
        derivation: &Derivation,
    ) -> Option<()> {
        let mut instance_of: HashMap<PhysicalCandidateId, usize> = HashMap::new();
        for &candidate in &derivation.new_shareable {
            let index = self
                .instances
                .range((candidate, 0)..=(candidate, usize::MAX))
                .count();
            if index >= milp.instances_of_candidate.get(&candidate)?.len() {
                return None;
            }
            self.instances
                .insert((candidate, index), OwnerPlan::default());
            instance_of.insert(candidate, index);
        }
        let source = |pick: Pick| match pick {
            Pick::Derived(candidate) if milp.shareable(candidate) => {
                Source::Instance((candidate, instance_of[&candidate]))
            }
            Pick::Derived(candidate) => Source::Stateless(candidate),
            Pick::Existing(key) => Source::Instance(key),
        };
        let fill = |owner: &mut OwnerPlan, top: PhysicalCandidateId, own: bool| {
            let mut stack = vec![top];
            let mut seen = BTreeSet::new();
            while let Some(reader) = stack.pop() {
                if !seen.insert(reader) {
                    continue;
                }
                if !(own && reader == top) {
                    owner.classes.insert(reader);
                }
                for (input, &pick) in derivation.picks[reader.0].iter().enumerate() {
                    let source = source(pick);
                    owner.links.insert((reader, input, source));
                    if let Source::Stateless(producer) = source {
                        stack.push(producer);
                    }
                }
            }
        };
        for &(reference, pick) in &derivation.outputs {
            let output = source(pick);
            self.outputs[reference] = Some(output);
            if let Source::Stateless(candidate) = output {
                fill(&mut self.query_owners[query], candidate, false);
            }
        }
        for (&candidate, &index) in &instance_of {
            let owner = self
                .instances
                .get_mut(&(candidate, index))
                .expect("added above");
            fill(owner, candidate, true);
        }
        Some(())
    }

    /// The instances every source of `owner` reads.
    fn reads(owner: &OwnerPlan) -> impl Iterator<Item = Key> + '_ {
        owner
            .links
            .iter()
            .filter_map(|&(_, _, source)| match source {
                Source::Instance(key) => Some(key),
                Source::Stateless(_) => None,
            })
    }

    /// The instances query `query` reaches.
    fn reached_by(&self, milp: &SelectionMilp<'_>, query: usize) -> BTreeSet<Key> {
        let mut stack: Vec<Key> = Self::reads(&self.query_owners[query]).collect();
        for (reference, output) in self.outputs.iter().enumerate() {
            if let (Some(Source::Instance(key)), true) =
                (output, milp.query_of_reference[reference] == query)
            {
                stack.push(*key);
            }
        }
        let mut reached = BTreeSet::new();
        while let Some(key) = stack.pop() {
            if reached.insert(key) {
                if let Some(owner) = self.instances.get(&key) {
                    stack.extend(Self::reads(owner));
                }
            }
        }
        reached
    }

    /// Drops the instances no query reaches and numbers each candidate's
    /// instances from 0; `None` when a read names a missing instance or the
    /// instances read each other in a cycle.
    fn tidy(&mut self, milp: &SelectionMilp<'_>) -> Option<()> {
        let mut reached = BTreeSet::new();
        for query in 0..self.query_owners.len() {
            reached.extend(self.reached_by(milp, query));
        }
        if reached.iter().any(|key| !self.instances.contains_key(key)) {
            return None;
        }
        self.instances.retain(|key, _| reached.contains(key));
        // Cycles: depth-first, with the instances on the path.
        fn visit(
            plan: &Plan,
            key: Key,
            done: &mut BTreeSet<Key>,
            path: &mut BTreeSet<Key>,
        ) -> bool {
            if done.contains(&key) {
                return true;
            }
            if !path.insert(key) {
                return false;
            }
            for read in Plan::reads(&plan.instances[&key]) {
                if !visit(plan, read, done, path) {
                    return false;
                }
            }
            path.remove(&key);
            done.insert(key);
            true
        }
        let mut done = BTreeSet::new();
        for &key in self.instances.keys() {
            if !visit(self, key, &mut done, &mut BTreeSet::new()) {
                return None;
            }
        }
        // Renumber.
        let mut renumbered: HashMap<Key, Key> = HashMap::new();
        let mut next: HashMap<PhysicalCandidateId, usize> = HashMap::new();
        for &(class, index) in self.instances.keys() {
            let new = next.entry(class).or_default();
            if *new != index {
                renumbered.insert((class, index), (class, *new));
            }
            *new += 1;
        }
        if !renumbered.is_empty() {
            self.rename(|key| renumbered.get(&key).copied().unwrap_or(key));
        }
        Some(())
    }

    /// Renames every instance by `rename`.
    fn rename(&mut self, rename: impl Fn(Key) -> Key) {
        let source = |source: Source| match source {
            Source::Instance(key) => Source::Instance(rename(key)),
            stateless => stateless,
        };
        let owner = |owner: &OwnerPlan| OwnerPlan {
            classes: owner.classes.clone(),
            links: owner
                .links
                .iter()
                .map(|&(reader, input, read)| (reader, input, source(read)))
                .collect(),
        };
        for output in self.outputs.iter_mut().flatten() {
            *output = source(*output);
        }
        for query_owner in &mut self.query_owners {
            *query_owner = owner(query_owner);
        }
        self.instances = self
            .instances
            .iter()
            .map(|(&key, instance_owner)| (rename(key), owner(instance_owner)))
            .collect();
    }

    /// Every reader of instance `from` reads `into` instead (of the same
    /// candidate, or of another its readers may read).
    fn merge(&mut self, from: Key, into: Key) -> Option<()> {
        self.instances.remove(&from)?;
        self.rename(|key| if key == from { into } else { key });
        Some(())
    }
}

/// The variable of each choice a [`Plan`] makes.
struct Lookup {
    /// `(owner, reader, input, producer)` → `e`.
    links: HashMap<(usize, PhysicalCandidateId, usize, PhysicalCandidateId), Variable>,
    /// `(owner, reader, input, instance)` → `b`.
    reads: HashMap<(usize, PhysicalCandidateId, usize, usize), Variable>,
    /// `(reference, candidate)` → `o`.
    outputs: HashMap<(usize, PhysicalCandidateId), Variable>,
    /// `(reference, instance)` → `ob`.
    output_reads: HashMap<(usize, usize), Variable>,
}

impl Lookup {
    fn new(milp: &SelectionMilp<'_>) -> Self {
        let mut links = HashMap::new();
        let mut reads = HashMap::new();
        for (owner, variables) in milp.owners.iter().enumerate() {
            for &((reader, input, producer), link) in &variables.links {
                links.insert((owner, reader, input, producer), link);
            }
            for &((reader, input, instance), read) in &variables.reads {
                reads.insert((owner, reader, input, instance), read);
            }
        }
        let mut outputs = HashMap::new();
        let mut output_reads = HashMap::new();
        for (reference, of_reference) in milp.outputs.iter().enumerate() {
            for &(class, output) in of_reference {
                outputs.insert((reference, class), output);
            }
        }
        for (reference, of_reference) in milp.output_reads.iter().enumerate() {
            for &(instance, read) in of_reference {
                output_reads.insert((reference, instance), read);
            }
        }
        Self {
            links,
            reads,
            outputs,
            output_reads,
        }
    }

    /// The binaries set to 1 by `plan`; `None` when the model has no
    /// variable for one of its choices.
    fn binaries(&self, milp: &SelectionMilp<'_>, plan: &Plan) -> Option<Vec<(Variable, f64)>> {
        let instance = |key: Key| -> Option<usize> {
            milp.instances_of_candidate.get(&key.0)?.get(key.1).copied()
        };
        let mut on = Vec::new();
        let mut owner_binaries = |owner: usize, choices: &OwnerPlan| -> Option<()> {
            for class in &choices.classes {
                on.push(*milp.owners[owner].used.get(class)?);
            }
            for &(reader, input, source) in &choices.links {
                on.push(match source {
                    Source::Stateless(producer) => {
                        *self.links.get(&(owner, reader, input, producer))?
                    }
                    Source::Instance(key) => {
                        *self.reads.get(&(owner, reader, input, instance(key)?))?
                    }
                });
            }
            Some(())
        };
        for (query, choices) in plan.query_owners.iter().enumerate() {
            owner_binaries(query, choices)?;
        }
        let mut active = BTreeSet::new();
        for (&key, choices) in &plan.instances {
            let global = instance(key)?;
            active.insert(global);
            owner_binaries(milp.instance_owner(global), choices)?;
        }
        for (reference, output) in plan.outputs.iter().enumerate() {
            on.push(match (*output)? {
                Source::Stateless(class) => *self.outputs.get(&(reference, class))?,
                Source::Instance(key) => *self.output_reads.get(&(reference, instance(key)?))?,
            });
        }
        for &global in &active {
            on.push(milp.active[global]);
        }
        // `g[i0, i]`: `i0` reads `i` and at most one query reaches `i0`.
        let mut reaching: HashMap<usize, usize> = HashMap::new();
        for query in 0..milp.query_count() {
            for key in plan.reached_by(milp, query) {
                *reaching.entry(instance(key)?).or_default() += 1;
            }
        }
        for &(reader, read, single) in &milp.single_query_consumers {
            if !active.contains(&reader) || reaching.get(&reader).copied().unwrap_or(0) > 1 {
                continue;
            }
            let data = milp.instances[reader];
            let reads = plan
                .instances
                .get(&(data.class, data.index))
                .is_some_and(|choices| Plan::reads(choices).any(|key| instance(key) == Some(read)));
            if reads {
                on.push(single);
            }
        }
        Some(on.into_iter().map(|variable| (variable, 1.0)).collect())
    }
}

/// The local search's state (module documentation, "Sharing").
struct Search<'s, 'a> {
    milp: &'s SelectionMilp<'a>,
    queries: &'s QueryData,
    lookup: &'s Lookup,
    completion: &'s Completion,
}

impl Search<'_, '_> {
    /// `plan` (of objective `cost`) after the local search's moves, until
    /// none helps or `over`.
    fn improve(&mut self, mut plan: Plan, mut cost: f64, over: &dyn Fn() -> bool) -> (Plan, f64) {
        let milp = self.milp;
        loop {
            let mut improved = false;
            for query in 0..milp.query_count() {
                if over() {
                    break;
                }
                let candidate = self.replan(&plan, &[query], None);
                improved |= self.keep_if_cheaper(candidate, &mut plan, &mut cost);
            }
            // Move every query reaching an instance of a candidate off it.
            let classes: BTreeSet<PhysicalCandidateId> =
                plan.instances.keys().map(|&(class, _)| class).collect();
            for class in classes {
                if over() {
                    break;
                }
                let readers: Vec<usize> = (0..milp.query_count())
                    .filter(|&query| {
                        plan.reached_by(milp, query)
                            .iter()
                            .any(|&(reached, _)| reached == class)
                    })
                    .collect();
                if readers.is_empty() {
                    continue;
                }
                let candidate = self.replan(&plan, &readers, Some(class));
                improved |= self.keep_if_cheaper(candidate, &mut plan, &mut cost);
            }
            // Merges, until none helps.
            'merges: while !over() {
                for (from, into) in merge_pairs(milp, &plan) {
                    if over() {
                        break 'merges;
                    }
                    let mut merged = plan.clone();
                    let candidate = merged
                        .merge(from, into)
                        .and_then(|()| merged.tidy(milp))
                        .map(|()| merged);
                    if self.keep_if_cheaper(candidate, &mut plan, &mut cost) {
                        improved = true;
                        continue 'merges;
                    }
                }
                break;
            }
            if !improved || over() {
                return (plan, cost);
            }
        }
    }

    /// Replaces `plan` (of objective `cost`) by `candidate` when that is
    /// feasible and cheaper.
    fn keep_if_cheaper(
        &mut self,
        candidate: Option<Plan>,
        plan: &mut Plan,
        cost: &mut f64,
    ) -> bool {
        let Some(candidate) = candidate else {
            return false;
        };
        match self.cost(&candidate) {
            Some((candidate_cost, _)) if candidate_cost < *cost * (1.0 - IMPROVEMENT) => {
                (*plan, *cost) = (candidate, candidate_cost);
                true
            }
            _ => false,
        }
    }

    /// `plan`'s objective and every variable's value, or `None` when it
    /// breaks a row.
    fn cost(&mut self, plan: &Plan) -> Option<(f64, Vec<f64>)> {
        let binaries = self.lookup.binaries(self.milp, plan)?;
        let completed = self.completion.complete(&binaries).ok()?;
        Some((completed.cost_per_second, completed.columns))
    }

    /// `plan` with queries `queries` derived again one after another, each
    /// reading the instances the plan already runs where that is cheaper,
    /// none of them using candidate `avoid`; `None` when a derivation is
    /// missing.
    fn replan(
        &self,
        plan: &Plan,
        queries: &[usize],
        avoid: Option<PhysicalCandidateId>,
    ) -> Option<Plan> {
        let milp = self.milp;
        let mut rest = plan.clone();
        for &query in queries {
            rest.query_owners[query] = OwnerPlan::default();
        }
        for (reference, output) in rest.outputs.iter_mut().enumerate() {
            if queries.contains(&milp.query_of_reference[reference]) {
                *output = None;
            }
        }
        rest.tidy(milp)?;
        for &query in queries {
            let mut excluded = self.queries.excluded_shared[query].clone();
            if let Some(avoid) = avoid {
                excluded[avoid.0] = true;
            }
            let derivation = self.derive_with_existing(&rest, query, &excluded)?;
            rest.add(milp, query, &derivation)?;
            rest.tidy(milp)?;
        }
        Some(rest)
    }

    /// Query `query`'s cheapest derivation over the candidates not
    /// `excluded`, where it may read the instances `rest` runs (those whose
    /// subtree holds no excluded candidate) at what that read adds.
    fn derive_with_existing(
        &self,
        rest: &Plan,
        query: usize,
        excluded: &[bool],
    ) -> Option<Derivation> {
        let milp = self.milp;
        let rate = self.queries.rate[query];
        // Per instance, the queries reaching it.
        let mut readers: HashMap<Key, Vec<usize>> = HashMap::new();
        for other in 0..milp.query_count() {
            if other != query {
                for key in rest.reached_by(milp, other) {
                    readers.entry(key).or_default().push(other);
                }
            }
        }
        // Per candidate, its cheapest instance to read.
        let mut existing: HashMap<PhysicalCandidateId, (usize, f64)> = HashMap::new();
        for (&key, choices) in &rest.instances {
            let (class, index) = key;
            let mut below = vec![class];
            let mut stack = vec![key];
            let mut seen = BTreeSet::new();
            while let Some(at) = stack.pop() {
                if !seen.insert(at) {
                    continue;
                }
                below.push(at.0);
                if let Some(owner) = rest.instances.get(&at) {
                    below.extend(owner.classes.iter().copied());
                    stack.extend(Plan::reads(owner));
                }
            }
            if below.iter().any(|candidate| excluded[candidate.0]) {
                continue;
            }
            let cost = self.read_cost(key, choices, rate, readers.get(&key), query);
            if existing.get(&class).is_none_or(|&(_, known)| cost < known) {
                existing.insert(class, (index, cost));
            }
        }
        derive(
            milp,
            rate,
            Objective::Cost,
            excluded,
            &existing,
            false,
            query,
        )
    }

    /// What query `query` reading instance `key` (owner choices `choices`,
    /// reached by `readers`) adds to the cost: nothing at ingestion time or
    /// for a materialized result some reader of the same schedule runs; else
    /// its and its region's executions at the query's rate.
    fn read_cost(
        &self,
        key: Key,
        choices: &OwnerPlan,
        rate: f64,
        readers: Option<&Vec<usize>>,
        query: usize,
    ) -> f64 {
        let milp = self.milp;
        let candidate = &milp.physical.candidates[key.0 .0];
        if candidate.timing == ExecutionTiming::IngestionTime {
            return NODE_TIE_BREAK;
        }
        let entry = &milp.entries[query];
        let same_schedule = readers.into_iter().flatten().any(|&other| {
            let other = &milp.entries[other];
            other.recurrence == entry.recurrence && other.range_evaluation == entry.range_evaluation
        });
        if candidate.materialization != Materialization::NotMaterialized && same_schedule {
            return NODE_TIE_BREAK;
        }
        let execution = |class: PhysicalCandidateId| {
            milp.inputs.candidate_data[class.0]
                .usage
                .as_ref()
                .map_or(0.0, |usage| usage.cost_per_execution.unwrap_or(0.0))
        };
        let own: f64 = std::iter::once(key.0)
            .chain(choices.classes.iter().copied())
            .map(execution)
            .sum();
        own * rate + NODE_TIE_BREAK
    }
}
