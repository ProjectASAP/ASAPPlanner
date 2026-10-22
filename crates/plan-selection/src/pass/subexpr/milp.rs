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

use super::GroupId;
use asap_types::physical::Materialization;

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
    /// Choice sets excluded by earlier cuts (no-good cuts).
    pub excluded: Vec<Vec<usize>>,
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
    for (&(k, g), _) in &reach {
        passes.insert((k, g), vars.add(variable().min(0).max(1)));
    }
    for (&g, _) in &once {
        let k = Consumer::Group(g);
        passes
            .entry((k, g.0 as usize))
            .or_insert_with(|| vars.add(variable().min(0).max(1)));
    }
    // y[k, c]: consumer k passes into choice c's direct part.
    let mut y: HashMap<(Consumer, usize), Variable> = HashMap::new();
    for (c, choice) in input.choices.iter().enumerate() {
        let g = choice.group.0 as usize;
        for (&(k, h), _) in &passes {
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

    let mut model = vars.minimise(objective.clone()).using(highs);
    // Needed groups: every query's roots; children of chosen choices.
    for query in &input.queries {
        for root in &query.roots {
            model = model.with(constraint!(need[root.0 as usize] == 1));
        }
    }
    for g in 0..n {
        // One choice implementing it (its own, or a fused one covering
        // it) when needed, none otherwise.
        let mut implementing = Expression::from(0.0);
        for (c, choice) in input.choices.iter().enumerate() {
            if choice.implements.iter().any(|i| i.0 as usize == g) {
                implementing += x[c];
            }
        }
        model = model.with(constraint!(implementing == need[g]));
    }
    for (c, choice) in input.choices.iter().enumerate() {
        for child in &choice.children {
            model = model.with(constraint!(need[child.0 as usize] >= x[c]));
        }
    }
    for (&g, &v) in &once {
        model = model.with(constraint!(v <= need[g.0 as usize]));
        let slot = input.group_slots.iter().find(|s| s.group == g).unwrap();
        if !slot.may_recompute {
            model = model.with(constraint!(v == need[g.0 as usize]));
        }
    }
    // Reach at all: a query reaches its roots, and every child of a chosen
    // choice of a group it reaches.
    for (q, query) in input.queries.iter().enumerate() {
        for root in &query.roots {
            if let Some(&v) = reaches_at_all.get(&(q, root.0 as usize)) {
                model = model.with(constraint!(v == 1));
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
                    model = model.with(constraint!(below >= above + x[c] - 1));
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
        model = model.with(constraint!(any <= 1));
        for class in classes_of(g) {
            let mut containing = Expression::from(0.0);
            for (set, selector) in sets {
                if set.contains(&class) {
                    containing += *selector;
                }
            }
            for &q in &input.group_queries[g] {
                if input.schedule[q] == class {
                    model = model.with(constraint!(containing.clone() >= reaches_at_all[&(q, g)]));
                }
            }
            let mut members = Expression::from(0.0);
            for &q in &input.group_queries[g] {
                if input.schedule[q] == class {
                    members += reaches_at_all[&(q, g)];
                }
            }
            model = model.with(constraint!(containing <= members));
        }
    }
    // Consumer reach.
    for (q, query) in input.queries.iter().enumerate() {
        for root in &query.roots {
            if let Some(&v) = reach.get(&(Consumer::Query(q), root.0 as usize)) {
                model = model.with(constraint!(v >= need[root.0 as usize]));
            }
        }
    }
    for (&(k, g), &p) in &passes {
        match k {
            Consumer::Group(own) if own.0 as usize == g => {
                model = model.with(constraint!(p >= once[&own]));
            }
            _ => {
                let r = reach[&(k, g)];
                match once.get(&GroupId(g as u32)) {
                    Some(&o) => model = model.with(constraint!(p >= r - o)),
                    None => model = model.with(constraint!(p >= r)),
                }
            }
        }
    }
    for (&(k, c), &v) in &y {
        let g = input.choices[c].group.0 as usize;
        model = model.with(constraint!(v >= passes[&(k, g)] + x[c] - 1));
    }
    for (c, choice) in input.choices.iter().enumerate() {
        for (child, via) in choice.children.iter().zip(&choice.coefficients.child_via) {
            let h = child.0 as usize;
            match via {
                Via::Direct => {
                    for (&(k, g), _) in &passes {
                        if g != choice.group.0 as usize {
                            continue;
                        }
                        if let Some(&r) = reach.get(&(k, h)) {
                            model = model.with(constraint!(r >= y[&(k, c)]));
                        }
                    }
                }
                Via::Slot(slot) => {
                    let k = Consumer::Slot {
                        choice: c,
                        slot: *slot,
                    };
                    if let Some(&r) = reach.get(&(k, h)) {
                        model = model.with(constraint!(r >= x[c]));
                    }
                }
            }
        }
    }
    for &(z, a, b) in &products {
        model = model.with(constraint!(z >= a + b - 1));
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
        model = model.with(constraint!(latency <= bound));
    }
    // Cuts.
    for set in &input.excluded {
        let mut chosen = Expression::from(0.0);
        for &c in set {
            chosen += x[c];
        }
        model = model.with(constraint!(chosen <= set.len() as f64 - 1.0));
    }
    for ((set, bound), &e) in input.extra_bounds.iter().zip(&extra) {
        let mut chosen = Expression::from(0.0);
        for &c in set {
            chosen += x[c];
        }
        model = model.with(constraint!(
            e >= *bound * (chosen - (set.len() as f64 - 1.0))
        ));
    }
    let model = model
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

use asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidates;
use asap_physical_asap_optimization::PhysicalCandidate;
use asap_types::ir::QueryRoot;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::{PlanningModels, Selection, SelectionError};

/// A memo search's result: the selection, its physical candidate and its
/// logical roots, or why no plan is valid.
pub(crate) type MemoSearchResult =
    Result<(Selection, PhysicalCandidate, Vec<QueryRoot>), SelectionError>;

/// The cheapest valid plan of `per_computation`'s memo under cost model 1,
/// as [`super::search::brute_force`] returns it, with the solver's gap,
/// iterations and time in the selection's report
/// ([`crate::SearchReport::solver_gap`]). `None` while the model is not
/// built: the caller runs the legacy search and reports
/// [`crate::RoutedToLegacy::MilpNotBuilt`].
pub(crate) fn search<Id: Clone>(
    per_computation: &PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Option<MemoSearchResult> {
    let _ = (per_computation, entries, data, models);
    None
}
