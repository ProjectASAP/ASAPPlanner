//! A feasible starting plan for the group selection MILP (#1095 §2.0,
//! "Solving strategy" step 1), so HiGHS starts with an incumbent and a
//! deadline yields a plan.
//!
//! **Each query's cheapest plan.** Queries are planned one after another
//! into one plan. A query's plan is derived from its output references
//! down: in each context, each slot an input needs takes the candidate
//! with the cheapest estimated subtree (its usage at the context's rate
//! plus its inputs' cheapest subtrees), unless the context already chose a
//! candidate of that slot, which every reader must then accept (one
//! candidate per slot per context); a dead end backtracks to the next
//! candidate. A shareable candidate's context is derived once, when it is
//! first selected; a candidate of an `I_r` is chosen in the `I_r` of the
//! reading context's refresh window.
//!
//! **Repairs per query.** If the query's plan depends on more approximate
//! values than one of them is sized for, on a pair excluded together, or
//! on a candidate unusable for it, the query is planned again with exact
//! candidates only; if its estimated latency is over its bound, it is
//! planned again by latency alone.
//!
//! **Greedy sharing.** A second plan prices what the queries planned
//! before already selected (a shareable candidate, a candidate of an `I_r`) at
//! zero, so later queries read it. Both plans, in workload order and
//! reversed, are costed exactly by the MILP's own rows and objective
//! ([`Completion`]); the cheapest feasible one is the start.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Instant;

use asap_cost::execution_demand::evaluation_rate;
use asap_types::stages::{output_references, PhysicalCandidateId, ReaderConstraint};
use good_lp::Variable;

use crate::groups::{AccuracyWeight, ContextKind, ResultSlot, RunKind};

use super::completion::Completion;
use super::cost_terms::NodeNumbers;
use super::model::{latency_bound_ms, SelectionMilp};

/// The most derivation steps one query's plan takes before giving up.
const MOST_STEPS_PER_QUERY: usize = 200_000;

/// The starting plan.
pub(crate) struct StartingPlan {
    /// Every variable's value.
    pub(crate) values: Vec<(Variable, f64)>,
    /// Its objective (cost per second without the request fees).
    pub(crate) cost_per_second: f64,
}

/// What a derivation minimizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Objective {
    Cost,
    Latency,
}

/// The candidates a query's derivation may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Approximate {
    /// Approximate candidates usable for the query.
    Usable,
    /// Those sized for at least this many shares.
    SizedFor(usize),
    /// Exact candidates only.
    None,
}

/// A plan under construction: per context, the candidate of each slot, and
/// the selected shareable candidates.
#[derive(Debug, Clone, Default)]
struct Plan {
    chosen: Vec<BTreeMap<ResultSlot, PhysicalCandidateId>>,
    selected: BTreeSet<PhysicalCandidateId>,
    /// Per multi-slot input `(context, reader, input)`, the slot it reads.
    slot_reads: BTreeMap<(usize, PhysicalCandidateId, usize), ResultSlot>,
}

/// One change to a [`Plan`], to undo.
enum Undo {
    Chose(usize, ResultSlot),
    Selected(PhysicalCandidateId),
    SlotRead(usize, PhysicalCandidateId, usize),
}

/// A feasible plan (module documentation), or `None` when none was found
/// before `budget`.
pub(crate) fn warm_start(
    milp: &SelectionMilp<'_>,
    completion: &Completion,
    budget: Option<Instant>,
) -> Option<StartingPlan> {
    let estimates = Estimates::new(milp);
    let mut best: Option<StartingPlan> = None;
    let count = milp.query_count();
    let mut orders: Vec<Vec<usize>> = vec![(0..count).collect(), (0..count).rev().collect()];
    // More orders, while the budget lasts: a fixed pseudo-random sequence
    // of permutations, so runs are reproducible.
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..MORE_ORDERS {
        let mut order: Vec<usize> = (0..count).collect();
        for at in (1..count).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            order.swap(at, (state % (at as u64 + 1)) as usize);
        }
        orders.push(order);
    }
    // (share, amortized): alone; reading what earlier queries selected; and
    // that with each shareable candidate's own cost divided among the
    // queries that may reach it.
    let strategies = [(false, false), (true, false), (true, true)];
    let mut tried = 0;
    'orders: for order in &orders {
        for &(share, amortized) in &strategies {
            if budget.is_some_and(|budget| Instant::now() >= budget) && best.is_some() {
                break 'orders;
            }
            tried += 1;
            let Some(plan) =
                sequential_plan(milp, &estimates, order, share, amortized, &BTreeSet::new())
            else {
                debug(|| format!("warm start: no plan (share {share}, amortized {amortized})"));
                continue;
            };
            let binaries = binaries_of(milp, &plan);
            let completed = completion.complete(&binaries);
            if let Err(reason) = &completed {
                debug(|| format!("warm start: completion failed (share {share}): {reason}"));
            }
            if let Ok(completed) = completed {
                debug(|| {
                    format!(
                        "warm start: plan (share {share}, amortized {amortized}) costs {:e}, {} shareable selected",
                        completed.cost_per_second,
                        plan.selected.len()
                    )
                });
                if best
                    .as_ref()
                    .is_none_or(|known| completed.cost_per_second < known.cost_per_second)
                {
                    best = Some(StartingPlan {
                        values: completion.values(&completed),
                        cost_per_second: completed.cost_per_second,
                    });
                }
            }
        }
    }
    // Greedy sharing: prefer one more shareable candidate that several
    // queries may read (most readers first), kept when the exact cost
    // falls.
    let mut sharable: Vec<(usize, PhysicalCandidateId)> = milp
        .inputs
        .context_of_shareable
        .keys()
        .map(|&m| (estimates.readers[m.0], m))
        .filter(|&(readers, _)| readers >= 2)
        .collect();
    sharable.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut preferred: BTreeSet<PhysicalCandidateId> = BTreeSet::new();
    for (_, m) in sharable {
        if best.is_none() || budget.is_some_and(|budget| Instant::now() >= budget) {
            break;
        }
        let mut trial = preferred.clone();
        trial.insert(m);
        tried += 1;
        let Some(plan) = sequential_plan(milp, &estimates, &orders[0], true, false, &trial) else {
            continue;
        };
        if let Ok(completed) = completion.complete(&binaries_of(milp, &plan)) {
            if best
                .as_ref()
                .is_some_and(|known| completed.cost_per_second < known.cost_per_second)
            {
                preferred = trial;
                best = Some(StartingPlan {
                    values: completion.values(&completed),
                    cost_per_second: completed.cost_per_second,
                });
            }
        }
    }
    debug(|| {
        format!(
            "warm start: {tried} plans tried, best {:?}",
            best.as_ref().map(|best| best.cost_per_second)
        )
    });
    best
}

/// The pseudo-random orders tried after workload order and its reverse,
/// within the budget.
const MORE_ORDERS: usize = 30;

/// The plan's binaries: `p[ctx, c]` and `x[m]` set to 1.
fn binaries_of(milp: &SelectionMilp<'_>, plan: &Plan) -> Vec<(Variable, f64)> {
    let mut binaries = Vec::new();
    for (context, chosen) in plan.chosen.iter().enumerate() {
        for candidate in chosen.values() {
            if let Some(&variable) = milp.choice[context].get(candidate) {
                binaries.push((variable, 1.0));
            }
        }
    }
    for shareable in &plan.selected {
        if let Some(&variable) = milp.selected.get(shareable) {
            binaries.push((variable, 1.0));
        }
    }
    for (key, slot) in &plan.slot_reads {
        if let Some(links) = milp.slot_links.get(key) {
            for &(at, link) in links {
                if at == *slot {
                    binaries.push((link, 1.0));
                }
            }
        }
    }
    binaries
}

/// Every query planned in `order` into one plan; with `share`, what is
/// already selected costs nothing to read.
fn sequential_plan(
    milp: &SelectionMilp<'_>,
    estimates: &Estimates,
    order: &[usize],
    share: bool,
    amortized: bool,
    preferred: &BTreeSet<PhysicalCandidateId>,
) -> Option<Plan> {
    let mut plan = Plan {
        chosen: vec![BTreeMap::new(); milp.choice.len()],
        selected: BTreeSet::new(),
        slot_reads: BTreeMap::new(),
    };
    for &query in order {
        let attempts = [
            (Objective::Cost, Approximate::Usable),
            (Objective::Cost, Approximate::SizedFor(2)),
            (Objective::Cost, Approximate::SizedFor(4)),
            (Objective::Cost, Approximate::None),
            (Objective::Latency, Approximate::Usable),
            (Objective::Latency, Approximate::SizedFor(2)),
            (Objective::Latency, Approximate::SizedFor(4)),
            (Objective::Latency, Approximate::None),
        ];
        // The first attempt meeting accuracy and the latency estimate; else
        // the last meeting accuracy (the latency estimate is from each
        // candidate's first costed usage, so it may be pessimistic: the
        // completion checks the exact latency).
        let mut planned = false;
        let mut fallback = None;
        for (attempt, (objective, approximate)) in attempts.into_iter().enumerate() {
            let mut deriving = Derivation {
                milp,
                estimates,
                query,
                objective,
                approximate,
                share,
                amortized,
                preferred,
                undo: Vec::new(),
                steps: 0,
                references: output_references(&milp.physical.query_roots)
                    .into_iter()
                    .enumerate()
                    .filter(|(_, (q, _))| *q == query)
                    .map(|(reference, _)| reference)
                    .collect(),
            };
            let derived = deriving.derive_query(&mut plan).is_ok();
            let accurate = derived && meets_accuracy(milp, &plan, query);
            let fast = accurate && meets_latency(milp, &plan, query);
            debug(|| {
                format!(
                    "warm start: q{query} {objective:?}/{approximate:?}: derived {derived}, \
                     accuracy {accurate}, latency {fast}, steps {}",
                    deriving.steps
                )
            });
            if fast {
                planned = true;
                break;
            }
            if accurate {
                fallback = Some(attempt);
            }
            let undo = std::mem::take(&mut deriving.undo);
            undo_all(&mut plan, undo);
        }
        if let (false, Some(attempt)) = (planned, fallback) {
            let (objective, approximate) = attempts[attempt];
            let mut deriving = Derivation {
                milp,
                estimates,
                query,
                objective,
                approximate,
                share,
                amortized,
                preferred,
                undo: Vec::new(),
                steps: 0,
                references: output_references(&milp.physical.query_roots)
                    .into_iter()
                    .enumerate()
                    .filter(|(_, (q, _))| *q == query)
                    .map(|(reference, _)| reference)
                    .collect(),
            };
            planned = deriving.derive_query(&mut plan).is_ok();
        }
        if !planned {
            return None;
        }
    }
    Some(plan)
}

fn undo_all(plan: &mut Plan, undo: Vec<Undo>) {
    for change in undo.into_iter().rev() {
        match change {
            Undo::Chose(context, slot) => {
                plan.chosen[context].remove(&slot);
            }
            Undo::Selected(shareable) => {
                plan.selected.remove(&shareable);
            }
            Undo::SlotRead(context, reader, input) => {
                plan.slot_reads.remove(&(context, reader, input));
            }
        }
    }
}

/// Per context and candidate, the estimated cost and latency of the
/// cheapest subtree, per query (the rate of a query context; shared
/// contexts at the slowest rate of the queries that may reach them).
struct Estimates {
    /// Per context, its rate (executions per second).
    rate: Vec<f64>,
    /// Per candidate, its numbers (first costed context).
    numbers: Vec<Option<NodeNumbers>>,
    /// Per candidate, the queries that may depend on it (shareable ones).
    readers: Vec<usize>,
}

impl Estimates {
    fn new(milp: &SelectionMilp<'_>) -> Self {
        let horizon = milp.cost_inputs.non_repeating_query_horizon_seconds;
        let query_rate: Vec<f64> = milp
            .entries
            .iter()
            .map(|entry| {
                evaluation_rate([(&entry.recurrence, entry.range_evaluation)], horizon)
                    .unwrap_or(1.0)
            })
            .collect();
        let rate = milp
            .inputs
            .contexts
            .iter()
            .map(|context| match context.kind {
                ContextKind::Query(query) => query_rate[query],
                ContextKind::Ingestion(_) => 1.0,
                ContextKind::Shared(m) if context.at_query_time => {
                    let readers = milp.queries_reaching(m);
                    readers
                        .iter()
                        .map(|&query| query_rate[query])
                        .fold(f64::INFINITY, f64::min)
                        .min(1e9)
                }
                ContextKind::Shared(_) => 1.0,
            })
            .collect();
        let numbers = milp
            .inputs
            .candidate_data
            .iter()
            .map(|data| data.usage.as_ref().ok().map(NodeNumbers::of))
            .collect();
        let mut readers = vec![0; milp.inputs.candidate_data.len()];
        for &m in milp.inputs.context_of_shareable.keys() {
            readers[m.0] = milp.queries_reaching(m).len();
        }
        Self {
            rate,
            numbers,
            readers,
        }
    }
}

/// One query's derivation into a plan.
struct Derivation<'m, 'a> {
    milp: &'m SelectionMilp<'a>,
    estimates: &'m Estimates,
    query: usize,
    objective: Objective,
    approximate: Approximate,
    share: bool,
    /// Divide a shareable candidate's own cost among the queries that may
    /// reach it.
    amortized: bool,
    /// Shareable candidates estimated free to read (greedy sharing).
    preferred: &'m BTreeSet<PhysicalCandidateId>,
    undo: Vec<Undo>,
    steps: usize,
    /// The query's output references.
    references: Vec<usize>,
}

impl Derivation<'_, '_> {
    /// Whether the query may use `candidate`.
    fn allowed(&self, candidate: PhysicalCandidateId) -> bool {
        let inputs = self.milp.inputs;
        if inputs.candidate_data[candidate.0].approximate {
            if self.approximate == Approximate::None
                || self.milp.entries[self.query]
                    .requirements
                    .accuracy
                    .is_exact()
            {
                return false;
            }
            if let Some(AccuracyWeight::Unusable(_)) =
                inputs.accuracy_weights[self.query].get(&candidate)
            {
                return false;
            }
            if let Approximate::SizedFor(shares) = self.approximate {
                if inputs.candidate_data[candidate.0].accuracy_budget_parts < shares {
                    return false;
                }
            }
        }
        // A candidate needing two or more reading queries is not planned
        // alone; one some of the query's outputs may not reach is not used.
        let references = &self.references;
        !self
            .milp
            .physical
            .reader_constraints
            .iter()
            .any(|constraint| match constraint {
                ReaderConstraint::MinimumQueryReaders {
                    candidate: c,
                    minimum,
                    ..
                } => *c == candidate && *minimum > 1,
                ReaderConstraint::AllowedOutputs {
                    candidate: c,
                    outputs,
                } => *c == candidate && references.iter().any(|r| !outputs.contains(r)),
                ReaderConstraint::IncompatibleOutputs {
                    candidate: c,
                    left,
                    right,
                } => *c == candidate && references.contains(left) && references.contains(right),
                ReaderConstraint::HighestSharedQueryMaterialization { .. } => false,
            })
    }

    /// The estimated cost (or latency) of `candidate`'s own run read in
    /// `context`, plus the cheapest subtrees of its inputs.
    fn estimate(
        &self,
        plan: &Plan,
        memo: &mut HashMap<(usize, PhysicalCandidateId), f64>,
        context: usize,
        candidate: PhysicalCandidateId,
    ) -> f64 {
        if let Some(&known) = memo.get(&(context, candidate)) {
            return known;
        }
        memo.insert((context, candidate), f64::INFINITY);
        let milp = self.milp;
        let inputs = milp.inputs;
        let value = (|| {
            if !self.allowed(candidate) {
                return f64::INFINITY;
            }
            let Some(numbers) = self.estimates.numbers[candidate.0] else {
                return f64::INFINITY;
            };
            let kind = inputs.kind[candidate.0];
            // Where its own run is and where it reads its inputs.
            let (run_context, already) = if milp.computes(context, candidate) {
                (context, false)
            } else {
                match kind {
                    RunKind::Shareable => match milp.context_of(candidate) {
                        Some(own) => (own, plan.selected.contains(&candidate)),
                        None => return f64::INFINITY,
                    },
                    RunKind::Ingestion => {
                        let Some(ingestion) = milp
                            .ingestion_context_read_by(context)
                            .filter(|&at| milp.choice[at].contains_key(&candidate))
                        else {
                            return f64::INFINITY;
                        };
                        let chosen = plan.chosen[ingestion].get(&inputs.slot[candidate.0]);
                        (ingestion, chosen == Some(&candidate))
                    }
                    RunKind::PerContext => return f64::INFINITY,
                }
            };
            if (self.share && already) || self.preferred.contains(&candidate) {
                return 0.0;
            }
            let mut own = match self.objective {
                Objective::Cost => {
                    numbers.execution_cost * self.estimates.rate[run_context]
                        + numbers.standing_cost
                }
                Objective::Latency => numbers.latency_ms,
            };
            if self.amortized && kind == RunKind::Shareable && self.objective == Objective::Cost {
                own /= self.estimates.readers[candidate.0].max(1) as f64;
            }
            let mut total = own;
            for producers in &inputs.producer_candidates_per_input[candidate.0] {
                let best = producers
                    .iter()
                    .filter(|producer| milp.choice[run_context].contains_key(producer))
                    .filter(|producer| {
                        kind == RunKind::Shareable
                            || inputs.slot[producer.0] != inputs.slot[candidate.0]
                    })
                    .map(|&producer| self.estimate(plan, memo, run_context, producer))
                    .fold(f64::INFINITY, f64::min);
                total += best;
            }
            total
        })();
        memo.insert((context, candidate), value);
        value
    }

    fn derive_query(&mut self, plan: &mut Plan) -> Result<(), ()> {
        let milp = self.milp;
        let mut memo = HashMap::new();
        for (reference, (query, _)) in output_references(&milp.physical.query_roots)
            .into_iter()
            .enumerate()
        {
            if query != self.query {
                continue;
            }
            let allowed: Vec<PhysicalCandidateId> = milp.inputs.output_candidates[reference]
                .iter()
                .copied()
                .filter(|candidate| milp.choice[query].contains_key(candidate))
                .collect();
            self.read(plan, &mut memo, query, &allowed)?;
        }
        Ok(())
    }

    /// Makes `context` give one of `allowed` (one input's producers, or an
    /// output's candidates).
    fn read(
        &mut self,
        plan: &mut Plan,
        memo: &mut HashMap<(usize, PhysicalCandidateId), f64>,
        context: usize,
        allowed: &[PhysicalCandidateId],
    ) -> Result<PhysicalCandidateId, ()> {
        self.steps += 1;
        if self.steps > MOST_STEPS_PER_QUERY {
            return Err(());
        }
        let milp = self.milp;
        let inputs = milp.inputs;
        // A slot already chosen here with an allowed candidate.
        for &candidate in allowed {
            if plan.chosen[context].get(&inputs.slot[candidate.0]) == Some(&candidate) {
                return Ok(candidate);
            }
        }
        let mut options: Vec<(f64, PhysicalCandidateId)> = allowed
            .iter()
            .copied()
            .filter(|candidate| milp.choice[context].contains_key(candidate))
            .filter(|candidate| !plan.chosen[context].contains_key(&inputs.slot[candidate.0]))
            .map(|candidate| (self.estimate(plan, memo, context, candidate), candidate))
            .collect();
        options.retain(|(estimate, _)| estimate.is_finite());
        options.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        for (_, candidate) in options {
            let mark = self.undo.len();
            if self.choose(plan, memo, context, candidate).is_ok() {
                return Ok(candidate);
            }
            let undo = self.undo.split_off(mark);
            undo_all(plan, undo);
        }
        Err(())
    }

    /// Chooses `candidate` for its slot in `context` and derives what it
    /// needs.
    fn choose(
        &mut self,
        plan: &mut Plan,
        memo: &mut HashMap<(usize, PhysicalCandidateId), f64>,
        context: usize,
        candidate: PhysicalCandidateId,
    ) -> Result<(), ()> {
        let milp = self.milp;
        let inputs = milp.inputs;
        let slot = inputs.slot[candidate.0];
        plan.chosen[context].insert(slot, candidate);
        self.undo.push(Undo::Chose(context, slot));
        if milp.computes(context, candidate) {
            for (input, producers) in inputs.producer_candidates_per_input[candidate.0]
                .iter()
                .enumerate()
            {
                let producers: Vec<PhysicalCandidateId> = producers
                    .iter()
                    .copied()
                    .filter(|producer| inputs.slot[producer.0] != slot)
                    .collect();
                let read = self.read(plan, memo, context, &producers)?;
                if milp.slot_links.contains_key(&(context, candidate, input)) {
                    plan.slot_reads
                        .insert((context, candidate, input), inputs.slot[read.0]);
                    self.undo.push(Undo::SlotRead(context, candidate, input));
                }
            }
            return Ok(());
        }
        match inputs.kind[candidate.0] {
            RunKind::Shareable => {
                if plan.selected.contains(&candidate) {
                    return Ok(());
                }
                plan.selected.insert(candidate);
                self.undo.push(Undo::Selected(candidate));
                let own = milp.context_of(candidate).ok_or(())?;
                for producers in &inputs.producer_candidates_per_input[candidate.0] {
                    self.read(plan, memo, own, producers)?;
                }
                Ok(())
            }
            RunKind::Ingestion => {
                let ingestion = milp.ingestion_context_read_by(context).ok_or(())?;
                if context == ingestion {
                    return Err(());
                }
                self.read(plan, memo, ingestion, &[candidate]).map(|_| ())
            }
            RunKind::PerContext => Err(()),
        }
    }
}

/// The runs query `query` depends on in `plan`: `(context, candidate)` for
/// candidates computed in its context and in the contexts of the
/// shareable candidates it reaches, and each such shareable candidate
/// `(its context, itself)`.
fn runs_reached(
    milp: &SelectionMilp<'_>,
    plan: &Plan,
    query: usize,
) -> BTreeSet<(usize, PhysicalCandidateId)> {
    let inputs = milp.inputs;
    let mut reached = BTreeSet::new();
    let mut stack: Vec<(usize, PhysicalCandidateId)> = Vec::new();
    for (reference, (q, _)) in output_references(&milp.physical.query_roots)
        .into_iter()
        .enumerate()
    {
        if q != query {
            continue;
        }
        for &candidate in &inputs.output_candidates[reference] {
            if plan.chosen[query].get(&inputs.slot[candidate.0]) == Some(&candidate) {
                stack.push((query, candidate));
            }
        }
    }
    while let Some((context, candidate)) = stack.pop() {
        let (run_context, own) = if milp.computes(context, candidate) {
            (context, false)
        } else {
            match inputs.kind[candidate.0] {
                RunKind::Shareable => match milp.context_of(candidate) {
                    Some(own) => (own, true),
                    None => continue,
                },
                RunKind::Ingestion => match milp.ingestion_context_read_by(context) {
                    Some(ingestion) => (ingestion, false),
                    None => continue,
                },
                RunKind::PerContext => continue,
            }
        };
        if !reached.insert((run_context, candidate)) {
            continue;
        }
        let slot = inputs.slot[candidate.0];
        for producers in &inputs.producer_candidates_per_input[candidate.0] {
            for &producer in producers {
                if !own && inputs.slot[producer.0] == slot {
                    continue;
                }
                if plan.chosen[run_context].get(&inputs.slot[producer.0]) == Some(&producer) {
                    stack.push((run_context, producer));
                    break;
                }
            }
        }
    }
    reached
}

/// Whether query `query`'s approximate runs in `plan` meet the A1 rows.
fn meets_accuracy(milp: &SelectionMilp<'_>, plan: &Plan, query: usize) -> bool {
    let inputs = milp.inputs;
    let runs: Vec<PhysicalCandidateId> = runs_reached(milp, plan, query)
        .into_iter()
        .map(|(_, candidate)| candidate)
        .filter(|candidate| inputs.candidate_data[candidate.0].approximate)
        .filter(|candidate| inputs.accuracy_weights[query].contains_key(candidate))
        .collect();
    if runs.is_empty() {
        return true;
    }
    if milp.entries[query].requirements.accuracy.is_exact() {
        return false;
    }
    let count = runs.len();
    for &candidate in &runs {
        if matches!(
            inputs.accuracy_weights[query].get(&candidate),
            Some(AccuracyWeight::Unusable(_))
        ) {
            return false;
        }
        if inputs.candidate_data[candidate.0].accuracy_budget_parts < count {
            return false;
        }
    }
    !inputs.accuracy_excluded_pairs[query]
        .iter()
        .any(|(first, second)| runs.contains(first) && runs.contains(second))
}

/// Whether query `query`'s estimated latency in `plan` is within its bound.
fn meets_latency(milp: &SelectionMilp<'_>, plan: &Plan, query: usize) -> bool {
    let Some(bound) = latency_bound_ms(&milp.entries[query]) else {
        return true;
    };
    let request_ms = milp.cost_inputs.measured_operation_costs.ns_per_evaluation * 1e-6;
    let latency: f64 = runs_reached(milp, plan, query)
        .into_iter()
        .filter(|&(context, _)| milp.runs_at_query_time(context))
        .filter_map(|(_, candidate)| milp.inputs.candidate_data[candidate.0].usage.as_ref().ok())
        .map(|usage| usage.own_latency_ms)
        .sum();
    debug(|| {
        format!(
            "warm start: q{query} estimated latency {} ms, bound {bound} ms",
            latency + request_ms
        )
    });
    latency + request_ms <= bound
}

/// Prints `message` to stderr when `ASAP_STAGE3_DEBUG` is set.
pub(crate) fn debug(message: impl FnOnce() -> String) {
    if std::env::var_os("ASAP_STAGE3_DEBUG").is_some() {
        eprintln!("{}", message());
    }
}
