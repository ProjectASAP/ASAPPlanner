//! The memo's solver: one MILP over the memo's choices (design "The
//! solver: MILP over the memo").
//!
//! A choice is one candidate of one group with an option for each of its
//! private materialization slots ([`Choice`]); its coefficients come from
//! Stage 3's per-node pricing ([`ChoiceCoefficients`]). The model picks one
//! choice per needed group, a materialization for each shared group's own
//! node, and propagates which consumers (queries, materialized slots) reach
//! which group, so each private node is charged once per consumer reaching
//! it, at that consumer's rate. Terms the model omits (memory held while a
//! query runs, composed accuracy) are added back by the caller's lazy cuts:
//! the model's optimum is a lower bound on the plan's Stage 3 price.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use good_lp::solvers::highs::highs;
use good_lp::{
    constraint, variable, Expression, ProblemVariables, Solution, SolverModel, Variable, WithMipGap,
};

use super::{GroupId, Memo};
use crate::{PlanSelector, PlanningModels, Selection, SelectionError};
use asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidates;
use asap_physical_asap_optimization::PhysicalCandidate;
use asap_types::ir::QueryRoot;
use asap_types::physical::Materialization;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

/// How a child group of a choice is reached: by the consumers reaching the
/// choice's own node, or by one of its materialized private slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Via {
    Direct,
    Slot(usize),
}

/// What one part of a choice costs, from Stage 3's per-node pricing.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct PartCoefficients {
    /// Price per second per unit of the rate of a consumer reaching it.
    pub cost_per_unit_rate: f64,
    /// Work in ms per computation, which a query reaching it at query time
    /// adds to its latency.
    pub work_ms: f64,
}

/// One materialized private slot of a choice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SlotCoefficients {
    pub materialization: Materialization,
    /// Price per second that does not depend on a rate (ingestion-time
    /// work and kept state).
    pub fixed_cost: f64,
    /// Price per second per unit of the slot's own rate (query-time slots:
    /// the union rate of the queries reaching the group).
    pub cost_per_unit_rate: f64,
    /// Work in ms per computation at query time.
    pub work_ms: f64,
}

/// One choice's coefficients.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChoiceCoefficients {
    pub direct: PartCoefficients,
    pub slots: Vec<SlotCoefficients>,
    /// Per child group of the candidate, in order.
    pub child_via: Vec<Via>,
    /// The shareable nodes of the direct part (an ASAP node, or one whose
    /// schema has a unique key) of a group two or more queries read, other
    /// than its own node: each may be one node for every copy of the group
    /// (the identical-expression rule), and then built once at query time.
    pub shared_nodes: Vec<SharedNodeCoefficients>,
}

/// A shareable node of a choice's direct part ([`ChoiceCoefficients::shared_nodes`]).
/// Its price is in `direct.cost_per_unit_rate`, charged per consumer; built
/// once at query time it costs `cost_per_unit_rate` per unit of the union
/// rate of the queries reaching it instead, and holds `state_bytes` while
/// its longest reader runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SharedNodeCoefficients {
    pub cost_per_unit_rate: f64,
    pub work_ms: f64,
    pub state_bytes: u64,
}

/// One choice of a group: a candidate and an option per private slot.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Choice {
    pub group: GroupId,
    pub candidate: usize,
    /// The groups it implements (the first is `group`).
    pub implements: Vec<GroupId>,
    /// The child groups it reads.
    pub children: Vec<GroupId>,
    pub assignment: Vec<Materialization>,
    pub coefficients: ChoiceCoefficients,
    /// `None`: the group's one node, shared by every query reading it.
    /// `Some(q)`: query `q`'s own copy of it (the group apart); a group's
    /// copies are all apart or all shared.
    pub copy_of: Option<usize>,
    /// Each child group that is a scan: `None` when it is apart and its
    /// copy is this choice's own (not in `children`); `Some(h)` when it is
    /// the scan shared at horizon `h` ms (its choice's `scan_horizon_ms`).
    pub scan_children: Vec<(GroupId, Option<u64>)>,
    /// For a scan group's choice: the horizon it reads, the longest window
    /// among its readers.
    pub scan_horizon_ms: Option<u64>,
}

/// A shared group's own node, built once per evaluation when chosen.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GroupSlot {
    pub group: GroupId,
    /// Whether it may stay `NotMaterialized` (recomputed per consumer).
    pub may_recompute: bool,
}

/// One query of the model.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ModelQuery {
    /// Its evaluations per second.
    pub rate: f64,
    /// The groups its root reads directly (one for an operator root).
    pub roots: Vec<GroupId>,
    /// Its latency bound in ms, if any (the tighter of its own bound and
    /// its repeat interval).
    pub max_latency_ms: Option<f64>,
}

/// Everything the model reads.
pub(crate) struct ModelInput {
    pub group_count: usize,
    /// Per group, the queries whose plan may reach it.
    pub group_queries: Vec<Vec<usize>>,
    pub choices: Vec<Choice>,
    pub group_slots: Vec<GroupSlot>,
    pub queries: Vec<ModelQuery>,
    /// Per query, its schedule class: queries of one class run at the same
    /// instants (one recurrence and range evaluation).
    pub schedule: Vec<usize>,
    /// The union rate of the classes in each set, by sorted class set: how
    /// often a node built once for queries of those classes is computed.
    pub union_rate: HashMap<Vec<usize>, f64>,
    /// No-good cuts from earlier iterations: at most `bound` of these
    /// choices together (a plan whose every group took one of them).
    pub excluded: Vec<(Vec<usize>, f64)>,
    /// Lower bounds on the omitted cost, added as `extra ≥ bound` when all
    /// of a choice set is chosen (local cuts).
    pub extra_bounds: Vec<(Vec<usize>, f64)>,
}

/// The model's optimum.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ModelSolution {
    /// The chosen choices, by index in [`ModelInput::choices`].
    pub chosen: Vec<usize>,
    /// The shared groups built once per evaluation.
    pub built_once: BTreeSet<GroupId>,
    /// The model's objective: a lower bound on the plan's price.
    pub objective: f64,
    /// The model's size.
    pub variables: u64,
    pub constraints: u64,
}

/// A consumer of the model: a query, a materialized private slot of a
/// choice, or a shared group built once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Consumer {
    Query(usize),
    Slot { choice: usize, slot: usize },
    Group(GroupId),
}

/// Build and solve the model. `None` when it is infeasible.
pub(crate) fn solve(input: &ModelInput) -> Result<Option<ModelSolution>, String> {
    let mut vars = ProblemVariables::new();
    let n = input.group_count;
    let x: Vec<Variable> = input
        .choices
        .iter()
        .map(|_| vars.add(variable().binary()))
        .collect();
    let need: Vec<Variable> = (0..n).map(|_| vars.add(variable().binary())).collect();
    let once: BTreeMap<GroupId, Variable> = input
        .group_slots
        .iter()
        .map(|slot| (slot.group, vars.add(variable().binary())))
        .collect();
    // Which queries' plans reach each group at all (any materialization):
    // the union rate of a node built once reads them.
    let mut reaches_at_all: HashMap<(usize, usize), Variable> = HashMap::new();
    for (g, queries) in input.group_queries.iter().enumerate() {
        for &q in queries {
            reaches_at_all.insert((q, g), vars.add(variable().binary()));
        }
    }
    // Consumers that may reach each group's own node, and through it its
    // direct part.
    let consumers = possible_consumers(input);
    let mut reach: HashMap<(Consumer, usize), Variable> = HashMap::new();
    for (&g, ks) in &consumers {
        for &k in ks {
            reach.insert((k, g), vars.add(variable().min(0).max(1)));
        }
    }
    // A consumer passes through a group into its chosen direct part when
    // it reaches the group and the group is not built once (or it is the
    // group's own built-once consumer).
    let mut passes: HashMap<(Consumer, usize), Variable> = HashMap::new();
    for &(k, g) in reach.keys() {
        passes.insert((k, g), vars.add(variable().min(0).max(1)));
    }
    for &g in once.keys() {
        let k = Consumer::Group(g);
        passes
            .entry((k, g.0 as usize))
            .or_insert_with(|| vars.add(variable().min(0).max(1)));
    }
    // y[k, c]: consumer k passes into choice c's direct part.
    let mut y: HashMap<(Consumer, usize), Variable> = HashMap::new();
    for (c, choice) in input.choices.iter().enumerate() {
        let g = choice.group.0 as usize;
        for &(k, h) in passes.keys() {
            if h == g {
                y.insert((k, c), vars.add(variable().min(0).max(1)));
            }
        }
    }
    // Union-rate selectors for built-once consumers (shared groups and
    // query-time private slots): one per set of schedule classes among the
    // group's queries, exactly the classes of the queries reaching it.
    let mut rate_of: HashMap<usize, Vec<(Vec<usize>, Variable)>> = HashMap::new();
    let classes_of = |g: usize| -> Vec<usize> {
        let mut classes: Vec<usize> = input.group_queries[g]
            .iter()
            .map(|&q| input.schedule[q])
            .collect();
        classes.sort_unstable();
        classes.dedup();
        classes
    };
    let class_sets = |g: usize| -> Vec<Vec<usize>> {
        let classes = classes_of(g);
        (1u64..(1 << classes.len()))
            .map(|mask| {
                (0..classes.len())
                    .filter(|i| mask >> i & 1 == 1)
                    .map(|i| classes[i])
                    .collect()
            })
            .collect()
    };
    let mut groups_with_rates: BTreeSet<usize> = once.keys().map(|g| g.0 as usize).collect();
    for choice in &input.choices {
        if choice
            .coefficients
            .slots
            .iter()
            .any(|s| !matches!(s.materialization, Materialization::MatAtIngestion(_)))
        {
            groups_with_rates.insert(choice.group.0 as usize);
        }
    }
    for &g in &groups_with_rates {
        let sets = class_sets(g)
            .into_iter()
            .map(|set| (set, vars.add(variable().binary())))
            .collect();
        rate_of.insert(g, sets);
    }

    // The objective.
    let mut objective = Expression::from(0.0);
    // A consumer's constant rate, or `None` for one built once at query
    // time, whose rate is its group's union rate.
    let rate = |k: Consumer| -> Option<f64> {
        match k {
            Consumer::Query(q) => Some(input.queries[q].rate),
            Consumer::Slot { choice, slot } => {
                let m = input.choices[choice].coefficients.slots[slot].materialization;
                matches!(m, Materialization::MatAtIngestion(_)).then_some(1.0)
            }
            Consumer::Group(_) => None,
        }
    };
    // Products of a variable rate (a set selector) and a reach variable.
    let mut rated: Vec<(Variable, Variable, f64)> = Vec::new();
    for (&(k, c), &v) in &y {
        let cost = input.choices[c].coefficients.direct.cost_per_unit_rate;
        if cost == 0.0 {
            continue;
        }
        match rate(k) {
            Some(r) => objective += r * cost * v,
            None => {
                let g = match k {
                    Consumer::Group(g) => g.0 as usize,
                    Consumer::Slot { choice, .. } => input.choices[choice].group.0 as usize,
                    Consumer::Query(_) => unreachable!("a query's rate is constant"),
                };
                for (set, selector) in &rate_of[&g] {
                    rated.push((*selector, v, input.union_rate[set] * cost));
                }
            }
        }
    }
    for (c, choice) in input.choices.iter().enumerate() {
        for slot in &choice.coefficients.slots {
            objective += slot.fixed_cost * x[c];
            if slot.cost_per_unit_rate != 0.0
                && !matches!(slot.materialization, Materialization::MatAtIngestion(_))
            {
                for (set, selector) in &rate_of[&(choice.group.0 as usize)] {
                    rated.push((
                        *selector,
                        x[c],
                        input.union_rate[set] * slot.cost_per_unit_rate,
                    ));
                }
            }
        }
    }
    let mut products = Vec::new();
    for &(a, b, coefficient) in &rated {
        let z = vars.add(variable().min(0).max(1));
        objective += coefficient * z;
        products.push((z, a, b));
    }
    // Lower bounds on the omitted terms from earlier cuts.
    let extra: Vec<Variable> = input
        .extra_bounds
        .iter()
        .map(|_| vars.add(variable().min(0)))
        .collect();
    for &e in &extra {
        objective += e;
    }

    let variable_count = vars.len() as u64;
    let model = vars.minimise(objective.clone()).using(highs);
    let mut constraints = Vec::new();
    // Needed groups: every query's roots; children of chosen choices.
    for query in &input.queries {
        for root in &query.roots {
            constraints.push(constraint!(need[root.0 as usize] == 1));
        }
    }
    for (g, &needed) in need.iter().enumerate() {
        // One choice implementing it (its own, or a fused one covering
        // it) when needed, none otherwise.
        let mut implementing = Expression::from(0.0);
        for (c, choice) in input.choices.iter().enumerate() {
            if choice.implements.iter().any(|i| i.0 as usize == g) {
                implementing += x[c];
            }
        }
        constraints.push(constraint!(implementing == needed));
    }
    for (c, choice) in input.choices.iter().enumerate() {
        for child in &choice.children {
            constraints.push(constraint!(need[child.0 as usize] >= x[c]));
        }
    }
    for (&g, &v) in &once {
        constraints.push(constraint!(v <= need[g.0 as usize]));
        let slot = input.group_slots.iter().find(|s| s.group == g).unwrap();
        if !slot.may_recompute {
            constraints.push(constraint!(v == need[g.0 as usize]));
        }
    }
    // Reach at all: a query reaches its roots, and every child of a chosen
    // choice of a group it reaches.
    for (q, query) in input.queries.iter().enumerate() {
        for root in &query.roots {
            if let Some(&v) = reaches_at_all.get(&(q, root.0 as usize)) {
                constraints.push(constraint!(v == 1));
            }
        }
    }
    for (c, choice) in input.choices.iter().enumerate() {
        let g = choice.group.0 as usize;
        for child in &choice.children {
            for &q in &input.group_queries[g] {
                if let (Some(&above), Some(&below)) = (
                    reaches_at_all.get(&(q, g)),
                    reaches_at_all.get(&(q, child.0 as usize)),
                ) {
                    constraints.push(constraint!(below >= above + x[c] - 1));
                }
            }
        }
    }
    // Rate selectors: exactly the classes of the queries reaching the
    // group (at most one selector; a class is in it iff one of its
    // queries reaches the group).
    for (&g, sets) in &rate_of {
        let mut any = Expression::from(0.0);
        for (_, selector) in sets {
            any += *selector;
        }
        constraints.push(constraint!(any <= 1));
        for class in classes_of(g) {
            let mut containing = Expression::from(0.0);
            for (set, selector) in sets {
                if set.contains(&class) {
                    containing += *selector;
                }
            }
            for &q in &input.group_queries[g] {
                if input.schedule[q] == class {
                    constraints.push(constraint!(containing.clone() >= reaches_at_all[&(q, g)]));
                }
            }
            let mut members = Expression::from(0.0);
            for &q in &input.group_queries[g] {
                if input.schedule[q] == class {
                    members += reaches_at_all[&(q, g)];
                }
            }
            constraints.push(constraint!(containing <= members));
        }
    }
    // Consumer reach.
    for (q, query) in input.queries.iter().enumerate() {
        for root in &query.roots {
            if let Some(&v) = reach.get(&(Consumer::Query(q), root.0 as usize)) {
                constraints.push(constraint!(v >= need[root.0 as usize]));
            }
        }
    }
    for (&(k, g), &p) in &passes {
        match k {
            Consumer::Group(own) if own.0 as usize == g => {
                constraints.push(constraint!(p >= once[&own]));
            }
            _ => {
                let r = reach[&(k, g)];
                match once.get(&GroupId(g as u32)) {
                    Some(&o) => constraints.push(constraint!(p >= r - o)),
                    None => constraints.push(constraint!(p >= r)),
                }
            }
        }
    }
    for (&(k, c), &v) in &y {
        let g = input.choices[c].group.0 as usize;
        constraints.push(constraint!(v >= passes[&(k, g)] + x[c] - 1));
    }
    for (c, choice) in input.choices.iter().enumerate() {
        for (child, via) in choice.children.iter().zip(&choice.coefficients.child_via) {
            let h = child.0 as usize;
            match via {
                Via::Direct => {
                    for &(k, g) in passes.keys() {
                        if g != choice.group.0 as usize {
                            continue;
                        }
                        if let Some(&r) = reach.get(&(k, h)) {
                            constraints.push(constraint!(r >= y[&(k, c)]));
                        }
                    }
                }
                Via::Slot(slot) => {
                    let k = Consumer::Slot {
                        choice: c,
                        slot: *slot,
                    };
                    if let Some(&r) = reach.get(&(k, h)) {
                        constraints.push(constraint!(r >= x[c]));
                    }
                }
            }
        }
    }
    for &(z, a, b) in &products {
        constraints.push(constraint!(z >= a + b - 1));
    }
    // Latency bounds: the work each query reaches at query time.
    for (q, query) in input.queries.iter().enumerate() {
        let Some(bound) = query.max_latency_ms else {
            continue;
        };
        let mut latency = Expression::from(0.0);
        for (&(k, c), &v) in &y {
            if k == Consumer::Query(q) {
                latency += input.choices[c].coefficients.direct.work_ms * v;
            }
        }
        constraints.push(constraint!(latency <= bound));
    }
    // Cuts.
    for (set, bound) in &input.excluded {
        let mut chosen = Expression::from(0.0);
        for &c in set {
            chosen += x[c];
        }
        constraints.push(constraint!(chosen <= *bound));
    }
    for ((set, bound), &e) in input.extra_bounds.iter().zip(&extra) {
        let mut chosen = Expression::from(0.0);
        for &c in set {
            chosen += x[c];
        }
        constraints.push(constraint!(
            e >= *bound * (chosen - (set.len() as f64 - 1.0))
        ));
    }
    let constraint_count = constraints.len() as u64;
    let model = model
        .with_all(constraints)
        .with_mip_gap(0.0)
        .map_err(|e| format!("MIP gap: {e:?}"))?;
    let solution = match model.solve() {
        Ok(solution) => solution,
        Err(good_lp::ResolutionError::Infeasible) => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let chosen = x
        .iter()
        .enumerate()
        .filter(|(_, v)| solution.value(**v) > 0.5)
        .map(|(c, _)| c)
        .collect();
    let built_once = once
        .iter()
        .filter(|(_, v)| solution.value(**v) > 0.5)
        .map(|(g, _)| *g)
        .collect();
    Ok(Some(ModelSolution {
        chosen,
        built_once,
        objective: solution.eval(&objective),
        variables: variable_count,
        constraints: constraint_count,
    }))
}

/// For each group, the consumers that may reach its node: its queries
/// (that reach it without passing a materialized node), the materialized
/// private slots of choices of groups above it, and the shared groups
/// above it (or itself) that may be built once.
fn possible_consumers(input: &ModelInput) -> BTreeMap<usize, BTreeSet<Consumer>> {
    let mut consumers: BTreeMap<usize, BTreeSet<Consumer>> = BTreeMap::new();
    for (g, queries) in input.group_queries.iter().enumerate() {
        let entry = consumers.entry(g).or_default();
        for &q in queries {
            entry.insert(Consumer::Query(q));
        }
    }
    // Propagate downwards until nothing changes.
    let shared: BTreeSet<usize> = input
        .group_slots
        .iter()
        .map(|s| s.group.0 as usize)
        .collect();
    loop {
        let mut changed = false;
        for (c, choice) in input.choices.iter().enumerate() {
            let g = choice.group.0 as usize;
            let mut above: BTreeSet<Consumer> = consumers.get(&g).cloned().unwrap_or_default();
            if shared.contains(&g) {
                above.insert(Consumer::Group(GroupId(g as u32)));
            }
            for (child, via) in choice.children.iter().zip(&choice.coefficients.child_via) {
                let entry = consumers.entry(child.0 as usize).or_default();
                let before = entry.len();
                match via {
                    Via::Direct => entry.extend(above.iter().copied()),
                    Via::Slot(slot) => {
                        entry.insert(Consumer::Slot {
                            choice: c,
                            slot: *slot,
                        });
                    }
                }
                changed |= entry.len() != before;
            }
        }
        if !changed {
            break;
        }
    }
    consumers
}

/// A memo search's result: the selection, its physical candidate and its
/// logical roots, or why no plan is valid.
pub(crate) type MemoSearchResult =
    Result<(Selection, PhysicalCandidate, Vec<QueryRoot>), SelectionError>;

/// The cheapest valid plan of `per_computation`'s memo under cost model 1
/// (the caller routes cost models 2 and 3 to the legacy search).
///
/// Lazy loop: solve the model (a lower bound: it omits memory held while a
/// query runs, and checks composed guarantees only on its incumbents);
/// price the solution's group choice exactly, as the reference search
/// prices one ([`offer_group_choice`]: every threshold, identical-expression
/// variant and Stage 2 materialization), which updates the incumbent; add
/// a no-good cut excluding that group choice; stop when the model's bound
/// reaches the incumbent. Each group choice is priced at most once, so the
/// loop ends, and the incumbent is the reference search's optimum.
pub(crate) fn search<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> MemoSearchResult {
    let started = std::time::Instant::now();
    let memo = super::search::memo(per_computation, entries);
    let memo_stats = super::search::stats(&memo, started.elapsed().as_secs_f64() * 1e3);
    let scale = super::pricing::frozen_calibration(&memo, entries, data, models)?;
    let mut cache = super::pricing::PriceCache::new(scale);
    let mut choices: Vec<Choice> = Vec::new();
    for g in 0..memo.groups.len() {
        let (group_choices, _lazy) = super::coefficients::choices(
            &memo,
            GroupId(g as u32),
            entries,
            data,
            models,
            &mut cache,
        )?;
        choices.extend(group_choices);
    }
    let input = model_input(&memo, choices, entries, models)?;
    let mut input = input;
    let mut selector = PlanSelector::new(entries, data, models, false)?;
    let mut logical: HashMap<String, Vec<QueryRoot>> = HashMap::new();
    let mut number = 0usize;
    let mut iterations = 0u32;
    let mut bound = None;
    let mut first_bound = None;
    let mut solver_ms = 0.0;
    let mut size = (0, 0);
    loop {
        iterations += 1;
        let solve_started = std::time::Instant::now();
        let solved = solve(&input).map_err(SelectionError::Solver)?;
        solver_ms += solve_started.elapsed().as_secs_f64() * 1e3;
        let Some(solution) = solved else {
            break;
        };
        bound = Some(solution.objective);
        first_bound.get_or_insert(solution.objective);
        size = (solution.variables, solution.constraints);
        if let Some(incumbent) = selector.cheapest_price() {
            if solution.objective >= incumbent - 1e-9 * incumbent.abs() {
                break;
            }
        }
        // The solution's group choice: the candidate each chosen choice
        // takes for its group.
        let mut group_choice: super::search::GroupChoice = vec![None; memo.groups.len()];
        for &c in &solution.chosen {
            group_choice[input.choices[c].group.0 as usize] = Some(input.choices[c].candidate);
        }
        super::search::offer_group_choice(
            &memo,
            per_computation,
            &group_choice,
            entries,
            data,
            models,
            &mut selector,
            &mut logical,
            &mut number,
        )?;
        // Exclude it: every choice of each chosen (group, candidate).
        let groups = group_choice.iter().flatten().count();
        let excluded: Vec<usize> = input
            .choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| group_choice[choice.group.0 as usize] == Some(choice.candidate))
            .map(|(c, _)| c)
            .collect();
        input.excluded.push((excluded, groups as f64 - 1.0));
    }
    let (mut selection, selected) = selector.finish()?;
    let price = selection.costs[&selection.selected].cost_per_second;
    selection.search.memo = Some(memo_stats);
    selection.search.solver_iterations = iterations;
    selection.search.solver_time_ms = solver_ms as u64;
    (
        selection.search.solver_variables,
        selection.search.solver_constraints,
    ) = size;
    selection.search.solver_first_bound_cost_per_second = first_bound;
    selection.search.solver_gap = match (price, bound) {
        (Some(price), Some(bound)) if price > 0.0 => Some(((price - bound) / price).max(0.0)),
        (Some(_), Some(_)) => Some(0.0),
        _ => None,
    };
    let roots = logical
        .remove(&selected.from_logical)
        .expect("the selection's logical roots are kept");
    Ok((selection, selected, roots))
}

/// The model's input for `memo`'s `choices`.
fn model_input(
    memo: &Memo,
    choices: Vec<Choice>,
    entries: &[QueryWorkloadEntry],
    models: PlanningModels<'_>,
) -> Result<ModelInput, SelectionError> {
    let settings = models.plan_selection;
    let rate = |entries: &[&QueryWorkloadEntry]| {
        crate::shared_evaluation_rate(entries, settings).map_err(|error| SelectionError::Cost {
            candidate: "memo".to_string(),
            node: Default::default(),
            error,
        })
    };
    // Schedule classes: queries with one recurrence and range evaluation.
    let mut classes: Vec<&QueryWorkloadEntry> = Vec::new();
    let schedule: Vec<usize> = entries
        .iter()
        .map(|entry| {
            match classes.iter().position(|c| {
                c.recurrence == entry.recurrence && c.range_evaluation == entry.range_evaluation
            }) {
                Some(class) => class,
                None => {
                    classes.push(entry);
                    classes.len() - 1
                }
            }
        })
        .collect();
    let mut union_rate = HashMap::new();
    let mut queries = Vec::with_capacity(entries.len());
    for (q, entry) in entries.iter().enumerate() {
        let roots = memo
            .groups
            .iter()
            .enumerate()
            .filter(|(_, group)| group.roots_of.contains(&q))
            .map(|(g, _)| GroupId(g as u32))
            .collect();
        // A query's evaluation must fit its latency bound and its repeat
        // interval.
        let interval = entry.recurrence.fixed_interval_ms().map(|ms| ms as f64);
        let max_latency_ms = match (entry.requirements.max_latency_ms, interval) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        queries.push(ModelQuery {
            rate: rate(&[entry])?,
            roots,
            max_latency_ms,
        });
    }
    let group_queries: Vec<Vec<usize>> = memo.groups.iter().map(|g| g.queries.clone()).collect();
    for queries_of in &group_queries {
        let mut present: Vec<usize> = queries_of.iter().map(|&q| schedule[q]).collect();
        present.sort_unstable();
        present.dedup();
        for mask in 1u64..(1 << present.len()) {
            let set: Vec<usize> = (0..present.len())
                .filter(|i| mask >> i & 1 == 1)
                .map(|i| present[i])
                .collect();
            if let std::collections::hash_map::Entry::Vacant(slot) = union_rate.entry(set) {
                let members: Vec<&QueryWorkloadEntry> =
                    slot.key().iter().map(|&c| classes[c]).collect();
                slot.insert(rate(&members)?);
            }
        }
    }
    let group_slots = memo
        .groups
        .iter()
        .enumerate()
        .filter(|(_, group)| group.parents.len() > 1 || group.roots_of.len() > 1)
        .map(|(g, _)| {
            let allowed = &memo.candidates[g][0].materializations;
            GroupSlot {
                group: GroupId(g as u32),
                may_recompute: allowed.is_empty()
                    || allowed.contains(&Materialization::NotMaterialized),
            }
        })
        .collect();
    Ok(ModelInput {
        group_count: memo.groups.len(),
        group_queries,
        choices,
        group_slots,
        queries,
        schedule,
        union_rate,
        excluded: Vec::new(),
        extra_bounds: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(group: u32, children: &[u32], cost: f64) -> Choice {
        Choice {
            group: GroupId(group),
            candidate: 0,
            implements: vec![GroupId(group)],
            children: children.iter().map(|&c| GroupId(c)).collect(),
            assignment: Vec::new(),
            coefficients: ChoiceCoefficients {
                direct: PartCoefficients {
                    cost_per_unit_rate: cost,
                    work_ms: 1.0,
                },
                slots: Vec::new(),
                child_via: children.iter().map(|_| Via::Direct).collect(),
                shared_nodes: Vec::new(),
            },
            copy_of: None,
            scan_children: Vec::new(),
            scan_horizon_ms: None,
        }
    }

    /// A scan two queries read costs once per query recomputing it, or once
    /// per evaluation when built once: the model builds it once when that
    /// is cheaper, and its objective is the plan's price.
    #[test]
    fn a_shared_group_is_built_once_when_that_is_cheaper() {
        let input = ModelInput {
            group_count: 3,
            group_queries: vec![vec![0, 1], vec![0], vec![1]],
            choices: vec![
                choice(0, &[], 10.0),
                choice(1, &[0], 1.0),
                choice(2, &[0], 1.0),
            ],
            group_slots: vec![GroupSlot {
                group: GroupId(0),
                may_recompute: true,
            }],
            queries: vec![
                ModelQuery {
                    rate: 1.0,
                    roots: vec![GroupId(1)],
                    max_latency_ms: None,
                },
                ModelQuery {
                    rate: 1.0,
                    roots: vec![GroupId(2)],
                    max_latency_ms: None,
                },
            ],
            schedule: vec![0, 0],
            union_rate: [(vec![0], 1.0)].into(),
            excluded: Vec::new(),
            extra_bounds: Vec::new(),
        };
        let solution = solve(&input).unwrap().unwrap();
        assert_eq!(solution.built_once, [GroupId(0)].into());
        assert!(
            (solution.objective - 12.0).abs() < 1e-9,
            "{}",
            solution.objective
        );
        // Recomputing is cheaper when the scan is cheap.
        let mut cheap = input;
        cheap.choices[0].coefficients.direct.cost_per_unit_rate = 0.1;
        cheap.union_rate.insert(vec![0], 1.0);
        // Built once still costs its union rate × 0.1 = 0.1 < 0.2.
        let solution = solve(&cheap).unwrap().unwrap();
        assert!(
            (solution.objective - 2.1).abs() < 1e-9,
            "{}",
            solution.objective
        );
    }
}
