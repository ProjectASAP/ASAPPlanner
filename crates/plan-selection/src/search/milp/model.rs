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

use crate::search::memo::{GroupId, Memo};
use crate::search::milp::coefficients::LazyTerms;
use crate::{CostModel, PlanSelector, PlanningModels, ResourceUnitCost, Selection, SelectionError};
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
#[derive(Debug, Clone, PartialEq)]
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
    /// The structural key of the slot's build over its inputs (and a
    /// threshold index, 0): the slots of chosen choices with the same key
    /// and materialization are one node when shared (a site's build at one
    /// threshold, identical expressions), so the model charges them once.
    pub shared_site: Option<(u64, usize)>,
    /// A query-time slot's raw scans, out of `cost_per_unit_rate`: like
    /// [`ChoiceCoefficients::absorbed_scans`], charged as scans of their
    /// source (another slot's or part's identical scan is one node).
    pub scans: Vec<(u64, f64, f64)>,
}

/// A cross-group node's variables: its once charge, its charge, the
/// binary choosing between per consumer and once, its members' once
/// charges, its per-consumer charge and their big M.
type CrossNodeVars = (
    Variable,
    Variable,
    Variable,
    Vec<(usize, f64)>,
    Expression,
    f64,
);

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
    /// Shareable nodes another group's choices may also have (equal keys):
    /// one node once shared, so charged once per key, at the dearest of
    /// the chosen ones (key, price per unit of rate). Moved out of
    /// `direct` and `shared_nodes` by the model's input.
    pub cross_shared: Vec<(u64, f64)>,
    /// The query-time scans of the direct part (apart: this choice's own
    /// copies), as (source key, price, a pass over its output) per unit of
    /// rate. Scans of one source may be one widened scan: like the scan
    /// choices (`Choice::scan_source`), each is charged its pass, and the
    /// largest excess once per source. Moved out of `direct` by the
    /// model's input.
    pub absorbed_scans: Vec<(u64, f64, f64)>,
    /// Each query-time node of the direct part but scans, as (key up to
    /// scan widening, price per unit of rate). Nodes of several groups'
    /// choices with one key are one node once shared (identical
    /// expressions, scans widened to one label set): the model's input
    /// moves them to `cross_shared`.
    pub direct_nodes: Vec<(u64, f64)>,
    /// Each node a query reaching this choice at query time computes (its
    /// direct part and its query-time slots), as (node key, work in ms per
    /// computation). Nodes of several choices with one key may be one node
    /// in the plan (identical expressions, scans of one source widened),
    /// which a query's latency counts once: the model counts each key
    /// once per query, at the least work any choice gives it.
    pub query_work: Vec<(u64, f64)>,
}

/// A shareable node of a choice's direct part ([`ChoiceCoefficients::shared_nodes`]).
/// Its price is in `direct.cost_per_unit_rate`, charged per consumer; built
/// once at query time it costs `cost_per_unit_rate` per unit of the union
/// rate of the queries reaching it instead, and holds `state_bytes` while
/// its longest reader runs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SharedNodeCoefficients {
    pub cost_per_unit_rate: f64,
    pub work_ms: f64,
    pub state_bytes: u64,
    /// The widened keys of the nodes it covers ([`ChoiceCoefficients::direct_nodes`]),
    /// itself first.
    pub members: Vec<u64>,
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
    /// For a scan group's choice: its source's raw-horizon key. Scans of
    /// one source may be one widened scan (the identical-expression rule
    /// over scans widened to one label set), which costs at least the
    /// dearest of them: the model charges the most expensive scan choice
    /// of each source, not their sum.
    pub scan_source: Option<u64>,
    /// For a scan group's choice: what a pass over its output costs per
    /// unit of rate, at most its own price. A widened scan shared by a
    /// source's scan groups costs at least the dearest scan, and each other
    /// reader at least such a pass: the model charges each distinct scan's
    /// pass once (readers of one scan share its node), plus the largest
    /// excess over it.
    pub scan_project_cost_per_unit_rate: f64,
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
    /// With the groups that plan does not need: a plan needing any of them
    /// is another plan, not cut.
    pub excluded: Vec<(Vec<usize>, Vec<usize>, f64)>,
    /// Lower bounds on the omitted cost, added as `extra ≥ bound` when all
    /// of a choice set is chosen (local cuts).
    pub extra_bounds: Vec<(Vec<usize>, f64)>,
    /// Requirements of a choice on another group's choice: when the first
    /// is chosen, one of the second (an exact implementation, or one
    /// carrying a coupling site) is.
    pub requires: Vec<(usize, Vec<usize>)>,
    /// Choices that may not be chosen together with any of the second set
    /// (panes and an exact top-k reuse in one plan).
    pub exclusive: (Vec<usize>, Vec<usize>),
    /// The rollup requirements ([`crate::search::memo::rules::windows::rollup_rules`]): when a
    /// choice of each set of the first list is chosen (each set a
    /// candidate's choices), one of the second is.
    pub rollup_rules: Vec<(Vec<Vec<usize>>, Vec<usize>)>,
    /// Each query-time raw scan of a choice's part: each source's longest
    /// one is kept on disk.
    pub raw_scans: Vec<RawScan>,
    /// Whether the deployment keeps raw data: without it, no part with a
    /// raw scan may run at query time.
    pub raw_data_retained: bool,
    /// Per choice, the memory its parts hold while a query's evaluation
    /// runs ([`HeldAtQueryTime`]).
    pub held: Vec<HeldAtQueryTime>,
    /// The solver's time limit in seconds, if any.
    pub time_limit_s: Option<f64>,
    /// Solve the linear relaxation (every binary in [0, 1]): a lower bound
    /// on the model's optimum.
    pub relaxed: bool,
}

/// The memory a choice's parts hold while an evaluation runs, priced per
/// ms of latency: Stage 3 charges `bytes · rate · E / 1000` at the memory
/// price, `E` the latency of the queries reaching the part.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct HeldAtQueryTime {
    /// Per unit of a reading query's rate: each reader holds its own copy
    /// over its own latency.
    pub per_reader_per_ms: f64,
    /// Per unit of rate: one copy held over the longest reader's latency.
    pub longest_per_ms: f64,
}

/// A raw scan of one part of a choice, when that part runs at query time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RawScan {
    pub choice: usize,
    /// Index of its source.
    pub source: usize,
    /// The price per second of keeping its look-back on disk.
    pub cost: f64,
    pub via: Via,
}

/// The memory `choice`'s parts hold while an evaluation runs
/// ([`LazyTerms::held`]), but its ingestion-time slots'. Its own node,
/// when built once, holds the smaller of its working memory and its built
/// state.
fn held_at_query_time(
    choice: &Choice,
    lazy: &LazyTerms,
    prices: &ResourceUnitCost,
    shared_keys: &BTreeSet<(u64, usize)>,
) -> HeldAtQueryTime {
    let mut held = HeldAtQueryTime::default();
    for term in &lazy.held {
        if let Via::Slot(i) = term.via {
            let slot = &choice.coefficients.slots[i];
            if matches!(slot.materialization, Materialization::MatAtIngestion(_))
                || slot
                    .shared_site
                    .is_some_and(|key| shared_keys.contains(&key))
            {
                continue;
            }
        }
        let bytes = match term.own_node {
            true => term.bytes.min(lazy.built_once_state_bytes),
            false => term.bytes,
        };
        let per_ms = prices.cost_per_memory_byte_second * bytes as f64 / 1000.0;
        match term.per_reader {
            true => held.per_reader_per_ms += per_ms,
            false => held.longest_per_ms += per_ms,
        }
    }
    held
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
    /// Whether the solver proved it optimal (not stopped at its time
    /// limit): only then is `objective` a lower bound.
    pub optimal: bool,
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
    // Prices per second are tiny (1e-6 and less) next to the solver's
    // feasibility tolerance (1e-7): solve in units where the largest
    // per-choice charge is about 1, and convert the objective back.
    let scale = price_scale(input);
    let scaled = scaled(input, scale);
    let solution = solve_scaled(&scaled)?;
    Ok(solution.map(|mut solution| {
        solution.objective /= scale;
        solution
    }))
}

/// How far below its objective a solve stopped at the solver's gap
/// tolerance (a relative gap of about 1e-6 at most) may have its optimum.
const GAP_TOLERANCE: f64 = 1e-4;

/// The factor that brings `input`'s largest charge per evaluation to about
/// 1: each choice's charges at the fastest query's rate, and raw scans.
/// A slot's fixed cost (its kept state) is left out: a huge one would
/// shrink every other charge below the solver's tolerances (about 1e-7),
/// where it no longer tells charges apart and stops above the optimum.
fn price_scale(input: &ModelInput) -> f64 {
    let fastest = input.queries.iter().map(|q| q.rate).fold(0.0, f64::max);
    let largest = input
        .choices
        .iter()
        .flat_map(|choice| {
            let k = &choice.coefficients;
            std::iter::once(k.direct.cost_per_unit_rate * fastest)
                .chain(k.slots.iter().map(move |s| s.cost_per_unit_rate * fastest))
        })
        .chain(input.raw_scans.iter().map(|s| s.cost))
        .filter(|v| v.is_finite())
        .fold(0.0, f64::max);
    if largest > 0.0 {
        1.0 / largest
    } else {
        1.0
    }
}

/// `input` with every price multiplied by `scale`.
fn scaled(input: &ModelInput, scale: f64) -> ModelInput {
    let mut scaled = model_input_clone(input);
    for choice in &mut scaled.choices {
        let k = &mut choice.coefficients;
        k.direct.cost_per_unit_rate *= scale;
        for slot in &mut k.slots {
            slot.fixed_cost *= scale;
            slot.cost_per_unit_rate *= scale;
            for (_, cost, pass) in &mut slot.scans {
                *cost *= scale;
                *pass *= scale;
            }
        }
        for node in &mut k.shared_nodes {
            node.cost_per_unit_rate *= scale;
        }
        choice.scan_project_cost_per_unit_rate *= scale;
        for (_, cost) in &mut choice.coefficients.cross_shared {
            *cost *= scale;
        }
        for (_, cost, pass) in &mut choice.coefficients.absorbed_scans {
            *cost *= scale;
            *pass *= scale;
        }
    }
    for scan in &mut scaled.raw_scans {
        scan.cost *= scale;
    }
    for held in &mut scaled.held {
        held.per_reader_per_ms *= scale;
        held.longest_per_ms *= scale;
    }
    for (_, bound) in &mut scaled.extra_bounds {
        *bound *= scale;
    }
    scaled
}

fn solve_scaled(input: &ModelInput) -> Result<Option<ModelSolution>, String> {
    let mut vars = ProblemVariables::new();
    let n = input.group_count;
    let relaxed = input.relaxed;
    let binary = || {
        if relaxed {
            variable().min(0).max(1)
        } else {
            variable().binary()
        }
    };
    let x: Vec<Variable> = input.choices.iter().map(|_| vars.add(binary())).collect();
    let need: Vec<Variable> = (0..n).map(|_| vars.add(binary())).collect();
    let once: BTreeMap<GroupId, Variable> = input
        .group_slots
        .iter()
        .map(|slot| (slot.group, vars.add(binary())))
        .collect();
    // Which queries' plans reach each group at all (any materialization):
    // the union rate of a node built once reads them.
    let mut reaches_at_all: HashMap<(usize, usize), Variable> = HashMap::new();
    for (g, queries) in input.group_queries.iter().enumerate() {
        for &q in queries {
            reaches_at_all.insert((q, g), vars.add(binary()));
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
        if !choice.coefficients.shared_nodes.is_empty()
            || choice
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
            .map(|set| (set, vars.add(binary())))
            .collect();
        rate_of.insert(g, sets);
    }

    // The objective.
    let mut objective = Expression::from(0.0);
    let mut products_pending: Vec<(Variable, Variable, Variable)> = Vec::new();
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
    // 0 for a group no query reads (no rate is charged linearly then).
    let slowest_rate = |g: usize| -> f64 {
        rate_of
            .get(&g)
            .into_iter()
            .flatten()
            .map(|(set, _)| input.union_rate[set])
            .reduce(f64::min)
            .unwrap_or(0.0)
    };
    // Products of a variable rate (a set selector) and a reach variable.
    let mut rated: Vec<(Variable, Variable, f64)> = Vec::new();
    // The shareable nodes of a choice's direct part cost, per unit of
    // rate, `shared_cost[c]`, charged either per consumer (the rest of the
    // direct part's charge) or once at the union rate of the queries
    // reaching the group when built once: the model charges the smaller
    // (`per_consumer_shared[c]` against `built_once_shared[c]`).
    let shared_cost: Vec<f64> = input
        .choices
        .iter()
        .map(|choice| {
            choice
                .coefficients
                .shared_nodes
                .iter()
                .map(|node| node.cost_per_unit_rate)
                .sum()
        })
        .collect();
    let mut per_consumer_shared: Vec<Vec<(Variable, f64)>> = vec![Vec::new(); input.choices.len()];
    let mut built_once_shared: Vec<Vec<(Variable, f64)>> = vec![Vec::new(); input.choices.len()];
    let mut product = |vars: &mut ProblemVariables, a: Variable, b: Variable| {
        let z = vars.add(variable().min(0).max(1));
        products_pending.push((z, a, b));
        z
    };
    // Each scan choice's direct charge, per source (`Choice::scan_source`).
    let mut scan_charges: BTreeMap<u64, BTreeMap<(usize, usize), Expression>> = BTreeMap::new();
    // Each scan choice's pass, per identical scan (its source and price):
    // readers of one scan share its node and pay its pass once.
    let mut pass_charges: BTreeMap<(u64, u64), BTreeMap<(usize, usize), Expression>> =
        BTreeMap::new();
    // Each choice's direct charge per schedule class of its query
    // consumers: one node serves the class's queries.
    let mut class_charges: BTreeMap<(usize, usize), Vec<Expression>> = BTreeMap::new();
    // Each cross-group node's charges per consumer, and their largest sum.
    let mut cross_per_consumer: BTreeMap<u64, (Expression, f64)> = BTreeMap::new();
    for (&(k, c), &v) in &y {
        let total = input.choices[c].coefficients.direct.cost_per_unit_rate;
        let rate_or_slowest = match rate(k) {
            Some(r) => r,
            None => slowest_rate(match k {
                Consumer::Group(g) => g.0 as usize,
                Consumer::Slot { choice, .. } => input.choices[choice].group.0 as usize,
                Consumer::Query(_) => unreachable!("a query's rate is constant"),
            }),
        };
        for &(key, cost) in &input.choices[c].coefficients.cross_shared {
            let entry = cross_per_consumer
                .entry(key)
                .or_insert_with(|| (Expression::from(0.0), 0.0));
            entry.0 += rate_or_slowest * cost * v;
            entry.1 += rate_or_slowest * cost;
        }
        // A scan's charge at its consumer's rate: for a consumer whose rate
        // is the reaching queries' union rate (a node built once or a
        // slot), the slowest class's rate plus the excess of the union
        // its rate selector picks.
        let scans_here = !input.choices[c].coefficients.absorbed_scans.is_empty()
            || (total != 0.0 && input.choices[c].scan_source.is_some());
        let mut above_slowest: Vec<(f64, Variable)> = Vec::new();
        if scans_here && rate(k).is_none() {
            let g = match k {
                Consumer::Group(g) => g.0 as usize,
                Consumer::Slot { choice, .. } => input.choices[choice].group.0 as usize,
                Consumer::Query(_) => unreachable!("a query's rate is constant"),
            };
            for (set, selector) in &rate_of[&g] {
                let excess = input.union_rate[set] - rate_or_slowest;
                if excess > 0.0 {
                    above_slowest.push((excess, product(&mut vars, *selector, v)));
                }
            }
        }
        let at_rate = |cost: f64| -> Expression {
            let mut charge = rate_or_slowest * cost * v;
            for &(excess, z) in &above_slowest {
                charge += excess * cost * z;
            }
            charge
        };
        for &(source, cost, pass) in &input.choices[c].coefficients.absorbed_scans {
            let pass = pass.min(cost);
            *pass_charges
                .entry((source, cost.to_bits()))
                .or_default()
                .entry((c, 0))
                .or_insert_with(|| Expression::from(0.0)) += at_rate(pass);
            *scan_charges
                .entry(source)
                .or_default()
                .entry((c, 0))
                .or_insert_with(|| Expression::from(0.0)) += at_rate(cost - pass);
        }
        if total == 0.0 {
            continue;
        }
        if let Some(source) = input.choices[c].scan_source {
            let pass = input.choices[c].scan_project_cost_per_unit_rate.min(total);
            *pass_charges
                .entry((source, total.to_bits()))
                .or_default()
                .entry((c, 0))
                .or_insert_with(|| Expression::from(0.0)) += at_rate(pass);
            *scan_charges
                .entry(source)
                .or_default()
                .entry((c, 0))
                .or_insert_with(|| Expression::from(0.0)) += at_rate(total - pass);
            continue;
        }
        let shared = shared_cost[c].min(total);
        let cost = total - shared;
        match rate(k) {
            Some(r) => {
                match k {
                    // Queries of one class read one node of the choice.
                    Consumer::Query(q) => class_charges
                        .entry((c, input.schedule[q]))
                        .or_default()
                        .push(r * cost * v),
                    _ => objective += r * cost * v,
                }
                if shared != 0.0 {
                    per_consumer_shared[c].push((v, r * shared));
                }
            }
            None => {
                let g = match k {
                    Consumer::Group(g) => g.0 as usize,
                    Consumer::Slot { choice, .. } => input.choices[choice].group.0 as usize,
                    Consumer::Query(_) => unreachable!("a query's rate is constant"),
                };
                // At least the slowest class's rate is charged linearly
                // (some query reaches the group), the rest through the
                // selector products: a tighter relaxation.
                let slowest = slowest_rate(g);
                objective += slowest * cost * v;
                for (set, selector) in &rate_of[&g] {
                    let union = input.union_rate[set];
                    if cost != 0.0 && union > slowest {
                        rated.push((*selector, v, (union - slowest) * cost));
                    }
                    if shared != 0.0 {
                        let z = product(&mut vars, *selector, v);
                        per_consumer_shared[c].push((z, union * shared));
                    }
                }
            }
        }
    }
    // Query-time slots' scans, at the slot's group's slowest rate, each
    // slot's apart from its choice's other parts.
    for (c, choice) in input.choices.iter().enumerate() {
        let g = choice.group.0 as usize;
        let slowest = slowest_rate(g);
        let with_scans = choice.coefficients.slots.iter().any(|slot| {
            !slot.scans.is_empty()
                && !matches!(slot.materialization, Materialization::MatAtIngestion(_))
        });
        if !with_scans {
            continue;
        }
        // At the slot's rate: the slowest class's, plus the excess of the
        // union its group's rate selector picks.
        let mut above_slowest: Vec<(f64, Variable)> = Vec::new();
        for (set, selector) in &rate_of[&g] {
            let excess = input.union_rate[set] - slowest;
            if excess > 0.0 {
                above_slowest.push((excess, product(&mut vars, *selector, x[c])));
            }
        }
        let at_rate = |cost: f64| -> Expression {
            let mut charge = slowest * cost * x[c];
            for &(excess, z) in &above_slowest {
                charge += excess * cost * z;
            }
            charge
        };
        for (i, slot) in choice.coefficients.slots.iter().enumerate() {
            if matches!(slot.materialization, Materialization::MatAtIngestion(_)) {
                continue;
            }
            for &(source, cost, pass) in &slot.scans {
                let pass = pass.min(cost);
                *pass_charges
                    .entry((source, cost.to_bits()))
                    .or_default()
                    .entry((c, i + 1))
                    .or_insert_with(|| Expression::from(0.0)) += at_rate(pass);
                *scan_charges
                    .entry(source)
                    .or_default()
                    .entry((c, i + 1))
                    .or_insert_with(|| Expression::from(0.0)) += at_rate(cost - pass);
            }
        }
    }
    let mut scan_maxima: Vec<(Variable, Vec<Expression>)> = Vec::new();
    for charges in class_charges.into_values() {
        let m = vars.add(variable().min(0));
        objective += m;
        scan_maxima.push((m, charges));
    }
    for charges in scan_charges.into_values().chain(pass_charges.into_values()) {
        let m = vars.add(variable().min(0));
        objective += m;
        scan_maxima.push((m, charges.into_values().collect()));
    }
    for (c, choice) in input.choices.iter().enumerate() {
        if shared_cost[c] == 0.0 {
            continue;
        }
        for (set, selector) in &rate_of[&(choice.group.0 as usize)] {
            let z = product(&mut vars, *selector, x[c]);
            built_once_shared[c].push((z, input.union_rate[set] * shared_cost[c]));
        }
    }
    // min(A, B) as a + b with a ≥ A − M·u, b ≥ B − M·(1 − u), u binary.
    let mut cheaper_of: Vec<(Variable, Variable, Variable, usize, f64)> = Vec::new();
    for c in 0..input.choices.len() {
        if per_consumer_shared[c].is_empty() && built_once_shared[c].is_empty() {
            continue;
        }
        let big_m: f64 = per_consumer_shared[c].iter().map(|(_, w)| w).sum::<f64>()
            + built_once_shared[c].iter().map(|(_, w)| w).sum::<f64>();
        let a = vars.add(variable().min(0));
        let b = vars.add(variable().min(0));
        let u = vars.add(binary());
        objective += a + b;
        objective += shared_cost[c] * slowest_rate(input.choices[c].group.0 as usize) * x[c];
        cheaper_of.push((a, b, u, c, big_m));
    }
    // Slots that may be one shared node (equal builds over equal inputs):
    // charged once, at the dearest of the chosen ones' charges with the
    // slowest class's rate. One node shared by them costs at least each
    // (it reads the longest of their windows), and apart they cost their
    // sum.
    let mut shared_slots: BTreeMap<(u64, usize, String), Vec<(usize, f64)>> = BTreeMap::new();
    for (c, choice) in input.choices.iter().enumerate() {
        for slot in &choice.coefficients.slots {
            let Some((site, threshold)) = slot.shared_site else {
                continue;
            };
            let at_query = !matches!(slot.materialization, Materialization::MatAtIngestion(_));
            let charge = slot.fixed_cost
                + match at_query {
                    true => slowest_rate(choice.group.0 as usize) * slot.cost_per_unit_rate,
                    false => 0.0,
                };
            shared_slots
                .entry((site, threshold, format!("{:?}", slot.materialization)))
                .or_default()
                .push((c, charge));
        }
    }
    // A node several groups' choices have (`cross_shared`) costs, in the
    // plan, its charge per consumer (one node computed per consumer, or a
    // copy per group) or, built once, at least the dearest chosen one's
    // charge at its group's slowest rate: the model charges the smaller.
    let mut cross_once: BTreeMap<u64, Vec<(usize, f64)>> = BTreeMap::new();
    for (c, choice) in input.choices.iter().enumerate() {
        for &(key, cost) in &choice.coefficients.cross_shared {
            cross_once
                .entry(key)
                .or_default()
                .push((c, slowest_rate(choice.group.0 as usize) * cost));
        }
    }
    let mut cross_vars: Vec<CrossNodeVars> = Vec::new();
    for (key, members) in cross_once {
        let (per_consumer, per_consumer_max) = cross_per_consumer
            .remove(&key)
            .unwrap_or_else(|| (Expression::from(0.0), 0.0));
        let big_m = per_consumer_max + members.iter().map(|(_, w)| *w).fold(0.0, f64::max);
        let once = vars.add(variable().min(0));
        let charged = vars.add(variable().min(0));
        let u = vars.add(binary());
        objective += charged;
        cross_vars.push((once, charged, u, members, per_consumer, big_m));
    }
    let mut shared_slot_vars: Vec<(Variable, Vec<(usize, f64)>)> = Vec::new();
    for members in shared_slots.into_values() {
        let m = vars.add(variable().min(0));
        objective += m;
        shared_slot_vars.push((m, members));
    }
    for (c, choice) in input.choices.iter().enumerate() {
        for slot in &choice.coefficients.slots {
            if slot.shared_site.is_some() {
                continue;
            }
            objective += slot.fixed_cost * x[c];
            if slot.cost_per_unit_rate != 0.0
                && !matches!(slot.materialization, Materialization::MatAtIngestion(_))
            {
                let g = choice.group.0 as usize;
                let slowest = slowest_rate(g);
                objective += slowest * slot.cost_per_unit_rate * x[c];
                for (set, selector) in &rate_of[&g] {
                    let union = input.union_rate[set];
                    if union > slowest {
                        rated.push((*selector, x[c], (union - slowest) * slot.cost_per_unit_rate));
                    }
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

    // Query-time reach and latency (for held memory and latency bounds):
    // `at_query_time[q, g]`, query `q` reaches group `g` without passing an
    // ingestion-time slot; `work_reached[q, c]`, it reaches choice `c` so.
    let mut at_query_time: HashMap<(usize, usize), Variable> = HashMap::new();
    for (g, queries) in input.group_queries.iter().enumerate() {
        for &q in queries {
            at_query_time.insert((q, g), vars.add(variable().min(0).max(1)));
        }
    }
    // A choice's query-time work as (node key, work): each node a query
    // reaching the choice computes ([`ChoiceCoefficients::query_work`]);
    // without that list, its parts' total under a key of its own, its
    // shareable nodes and shared slots left out (Stage 3 counts a shared
    // node once per query).
    let query_time_work = |c: usize| -> f64 {
        let coefficients = &input.choices[c].coefficients;
        let shared: f64 = coefficients.shared_nodes.iter().map(|n| n.work_ms).sum();
        (coefficients.direct.work_ms - shared).max(0.0)
            + coefficients
                .slots
                .iter()
                .filter(|s| {
                    !matches!(s.materialization, Materialization::MatAtIngestion(_))
                        && s.shared_site.is_none()
                })
                .map(|s| s.work_ms)
                .sum::<f64>()
    };
    let work_of = |c: usize| -> Vec<(u64, f64)> {
        let listed = &input.choices[c].coefficients.query_work;
        match listed.is_empty() {
            false => listed.clone(),
            // A key no node key takes: node keys are 32-bit, scan keys
            // have the top bit.
            true => vec![((1 << 62) | c as u64, query_time_work(c))],
        }
    };
    // Per node key, the least and the most work any choice gives it: a
    // query's latency counts each key once, at least at the least.
    let mut key_work: HashMap<u64, (f64, f64)> = HashMap::new();
    for c in 0..input.choices.len() {
        for (key, work) in work_of(c) {
            let entry = key_work.entry(key).or_insert((work, work));
            entry.0 = entry.0.min(work);
            entry.1 = entry.1.max(work);
        }
    }
    let held_of = |c: usize| input.held.get(c).copied().unwrap_or_default();
    let mut work_reached: HashMap<(usize, usize), Variable> = HashMap::new();
    for (c, choice) in input.choices.iter().enumerate() {
        let held = held_of(c);
        let works = work_of(c).iter().any(|&(_, work)| work > 0.0);
        if !works && held.per_reader_per_ms <= 0.0 && held.longest_per_ms <= 0.0 {
            continue;
        }
        for &q in &input.group_queries[choice.group.0 as usize] {
            work_reached.insert((q, c), vars.add(variable().min(0).max(1)));
        }
    }
    // `key_reached[q, key]`: query `q` computes the node of `key` at
    // query time, through any choice it reaches.
    let mut key_reached: HashMap<(usize, u64), Variable> = HashMap::new();
    for &(q, c) in work_reached.keys() {
        for (key, work) in work_of(c) {
            if work > 0.0 {
                key_reached
                    .entry((q, key))
                    .or_insert_with(|| vars.add(variable().min(0).max(1)));
            }
        }
    }
    // Held memory per (choice, query) for a reader's own copy, and per
    // choice for one copy over the longest reader.
    let mut held_per_reader: HashMap<(usize, usize), Variable> = HashMap::new();
    let mut held_longest: HashMap<usize, Variable> = HashMap::new();
    for &(q, c) in work_reached.keys() {
        let held = held_of(c);
        if held.per_reader_per_ms > 0.0 {
            let h = vars.add(variable().min(0));
            objective += h;
            held_per_reader.insert((q, c), h);
        }
        if held.longest_per_ms > 0.0 && !held_longest.contains_key(&c) {
            let h = vars.add(variable().min(0));
            objective += h;
            held_longest.insert(c, h);
        }
    }
    let vars_exclusive = (vars.add(binary()), vars.add(binary()));
    let source_count = input
        .raw_scans
        .iter()
        .map(|s| s.source + 1)
        .max()
        .unwrap_or(0);
    let kept_on_disk: Vec<Variable> = (0..source_count)
        .map(|_| vars.add(variable().min(0)))
        .collect();
    for &v in &kept_on_disk {
        objective += v;
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
    // Aggregated over a group's choices (one is chosen when it is
    // needed), which keeps the LP relaxation tight.
    let mut choices_of: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (c, choice) in input.choices.iter().enumerate() {
        choices_of[choice.group.0 as usize].push(c);
    }
    for (g, of_g) in choices_of.iter().enumerate() {
        let mut children: BTreeSet<usize> = BTreeSet::new();
        for &c in of_g {
            children.extend(input.choices[c].children.iter().map(|h| h.0 as usize));
        }
        for h in children {
            let mut with_child = Expression::from(0.0);
            for &c in of_g {
                if input.choices[c].children.iter().any(|k| k.0 as usize == h) {
                    with_child += x[c];
                }
            }
            for &q in &input.group_queries[g] {
                if let (Some(&above), Some(&below)) =
                    (reaches_at_all.get(&(q, g)), reaches_at_all.get(&(q, h)))
                {
                    constraints.push(constraint!(below >= above + with_child.clone() - 1));
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
    for (&(k, g), &p) in &passes {
        let mut into = Expression::from(0.0);
        for &c in &choices_of[g] {
            into += y[&(k, c)];
        }
        // Unless a fused choice of another group implements it.
        for (c, choice) in input.choices.iter().enumerate() {
            if choice.group.0 as usize != g && choice.implements.iter().any(|i| i.0 as usize == g) {
                into += x[c];
            }
        }
        constraints.push(constraint!(into >= p));
    }
    // A slot's consumer exists only when its choice is chosen, and a
    // group's own consumer only when the group is built once.
    let exists = |k: Consumer| -> Option<Variable> {
        match k {
            Consumer::Slot { choice, .. } => Some(x[choice]),
            Consumer::Group(g) => once.get(&g).copied(),
            Consumer::Query(_) => None,
        }
    };
    for (&(k, _), &r) in &reach {
        if let Some(e) = exists(k) {
            constraints.push(constraint!(r <= e));
        }
    }
    for (&(k, _), &p) in &passes {
        if let Some(e) = exists(k) {
            constraints.push(constraint!(p <= e));
        }
    }
    for (&(k, _), &v) in &y {
        if let Some(e) = exists(k) {
            constraints.push(constraint!(v <= e));
        }
    }
    for (c, choice) in input.choices.iter().enumerate() {
        for (child, via) in choice.children.iter().zip(&choice.coefficients.child_via) {
            let h = child.0 as usize;
            match via {
                Via::Direct => {
                    // Aggregated below, over the group's choices.
                    let _ = h;
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
    for (g, of_g) in choices_of.iter().enumerate() {
        let mut children: BTreeSet<usize> = BTreeSet::new();
        for &c in of_g {
            for (child, via) in input.choices[c]
                .children
                .iter()
                .zip(&input.choices[c].coefficients.child_via)
            {
                if *via == Via::Direct {
                    children.insert(child.0 as usize);
                }
            }
        }
        for h in children {
            for &(k, pg) in passes.keys() {
                if pg != g {
                    continue;
                }
                let Some(&r) = reach.get(&(k, h)) else {
                    continue;
                };
                let mut into = Expression::from(0.0);
                for &c in of_g {
                    let choice = &input.choices[c];
                    if choice
                        .children
                        .iter()
                        .zip(&choice.coefficients.child_via)
                        .any(|(child, via)| child.0 as usize == h && *via == Via::Direct)
                    {
                        into += y[&(k, c)];
                    }
                }
                constraints.push(constraint!(r >= into));
            }
        }
    }
    for &(z, a, b) in products.iter().chain(&products_pending) {
        constraints.push(constraint!(z >= a + b - 1));
    }
    for &(a, b, u, c, big_m) in &cheaper_of {
        let mut per_consumer = Expression::from(0.0);
        for &(v, w) in &per_consumer_shared[c] {
            per_consumer += w * v;
        }
        let mut built_once = Expression::from(0.0);
        for &(v, w) in &built_once_shared[c] {
            built_once += w * v;
        }
        // Both are at least the slowest class's rate (some query reaches
        // the chosen choice): charged linearly, which keeps the relaxation
        // tight; only the excess goes through the binary choice.
        let floor = shared_cost[c] * slowest_rate(input.choices[c].group.0 as usize);
        constraints.push(constraint!(a >= per_consumer - floor * x[c] - big_m * u));
        constraints.push(constraint!(
            b >= built_once - floor * x[c] - big_m * (1 - u)
        ));
    }
    // Query-time reach: a query reaches its roots, and through a chosen
    // choice its children read directly or by a query-time slot.
    for (q, query) in input.queries.iter().enumerate() {
        for root in &query.roots {
            if let Some(&v) = at_query_time.get(&(q, root.0 as usize)) {
                constraints.push(constraint!(v == 1));
            }
        }
    }
    for (g, of_g) in choices_of.iter().enumerate() {
        let mut children: BTreeSet<usize> = BTreeSet::new();
        for &c in of_g {
            children.extend(input.choices[c].children.iter().map(|h| h.0 as usize));
        }
        for h in children {
            let mut with_child = Expression::from(0.0);
            for &c in of_g {
                let choice = &input.choices[c];
                let at_query = choice
                    .children
                    .iter()
                    .zip(&choice.coefficients.child_via)
                    .any(|(child, via)| {
                        child.0 as usize == h
                            && match via {
                                Via::Direct => true,
                                Via::Slot(i) => !matches!(
                                    choice.coefficients.slots[*i].materialization,
                                    Materialization::MatAtIngestion(_)
                                ),
                            }
                    });
                if at_query {
                    with_child += x[c];
                }
            }
            for &q in &input.group_queries[g] {
                if let (Some(&above), Some(&below)) =
                    (at_query_time.get(&(q, g)), at_query_time.get(&(q, h)))
                {
                    constraints.push(constraint!(below >= above + with_child.clone() - 1));
                }
            }
        }
    }
    for (&(q, c), &w) in &work_reached {
        let g = input.choices[c].group.0 as usize;
        constraints.push(constraint!(w >= at_query_time[&(q, g)] + x[c] - 1));
        for (key, work) in work_of(c) {
            if work > 0.0 {
                constraints.push(constraint!(key_reached[&(q, key)] >= w));
            }
        }
    }
    // Each query's latency: the work of each node it computes at query
    // time, counted once per node key at the least work any choice gives
    // it (a lower bound: compaction is left out).
    let latency_of = |q: usize| {
        let mut latency = Expression::from(0.0);
        for (&(r, key), &v) in &key_reached {
            if r == q {
                latency += key_work[&key].0 * v;
            }
        }
        latency
    };
    for (q, query) in input.queries.iter().enumerate() {
        if let Some(bound) = query.max_latency_ms {
            constraints.push(constraint!(latency_of(q) <= bound));
        }
    }
    // Held memory, linearized: h ≥ coefficient · (latency − L · (1 − w)),
    // L the longest the query's latency can be; exact when w is 0 or 1.
    let longest_latency: Vec<f64> = (0..input.queries.len())
        .map(|q| {
            key_reached
                .keys()
                .filter(|&&(r, _)| r == q)
                .map(|(_, key)| key_work[key].1)
                .sum()
        })
        .collect();
    // A part read by several queries runs at their classes' union rate,
    // not their rates' sum: per reader, its held memory is charged at
    // `union / sum` of that reader's rate (Stage 3's rate-weighted mean
    // latency at the part's rate). The least ratio over the group's class
    // sets bounds it from below.
    let per_reader_share: Vec<f64> = input
        .group_queries
        .iter()
        .map(|queries_of| {
            input
                .union_rate
                .iter()
                .filter_map(|(set, &union)| {
                    let rate_sum: f64 = queries_of
                        .iter()
                        .filter(|&&q| set.contains(&input.schedule[q]))
                        .map(|&q| input.queries[q].rate)
                        .sum();
                    let covered = set
                        .iter()
                        .all(|class| queries_of.iter().any(|&q| input.schedule[q] == *class));
                    (covered && rate_sum > 0.0).then_some((union / rate_sum).min(1.0))
                })
                .fold(1.0, f64::min)
        })
        .collect();
    for (&(q, c), &w) in &work_reached {
        let held = held_of(c);
        let longest = longest_latency[q];
        if let Some(&h) = held_per_reader.get(&(q, c)) {
            let share = per_reader_share[input.choices[c].group.0 as usize];
            let coefficient = held.per_reader_per_ms * input.queries[q].rate * share;
            constraints.push(constraint!(
                h >= coefficient * (latency_of(q) - longest * (1 - w))
            ));
        }
        if let Some(&h) = held_longest.get(&c) {
            let g = input.choices[c].group.0 as usize;
            let rate = input.group_queries[g]
                .iter()
                .map(|&r| input.queries[r].rate)
                .reduce(f64::min)
                .unwrap_or(0.0);
            let coefficient = held.longest_per_ms * rate;
            constraints.push(constraint!(
                h >= coefficient * (latency_of(q) - longest * (1 - w))
            ));
        }
    }
    for (m, charges) in scan_maxima {
        for charge in charges {
            constraints.push(constraint!(m >= charge));
        }
    }
    for (once, charged, u, members, per_consumer, big_m) in cross_vars {
        for (c, charge) in members {
            constraints.push(constraint!(once >= charge * x[c]));
        }
        constraints.push(constraint!(charged >= per_consumer - big_m * u));
        constraints.push(constraint!(charged >= once - big_m * (1 - u)));
    }
    for (m, members) in &shared_slot_vars {
        for &(c, charge) in members {
            constraints.push(constraint!(*m >= charge * x[c]));
        }
    }
    // Cuts.
    for (set, outside, bound) in &input.excluded {
        let mut chosen = Expression::from(0.0);
        for &c in set {
            chosen += x[c];
        }
        for &g in outside {
            chosen -= need[g];
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
    for (c, allowed) in &input.requires {
        let mut any = Expression::from(0.0);
        for &a in allowed {
            any += x[a];
        }
        constraints.push(constraint!(x[*c] <= any));
    }
    for (all_of, one_of) in &input.rollup_rules {
        let mut chosen = Expression::from(0.0);
        for set in all_of {
            for &c in set {
                chosen += x[c];
            }
        }
        for &c in one_of {
            chosen -= x[c];
        }
        constraints.push(constraint!(chosen <= all_of.len() as f64 - 1.0));
    }
    let (first, second) = &input.exclusive;
    if !first.is_empty() && !second.is_empty() {
        let (p, t) = (vars_exclusive.0, vars_exclusive.1);
        for &c in first {
            constraints.push(constraint!(x[c] <= p));
        }
        for &c in second {
            constraints.push(constraint!(x[c] <= t));
        }
        constraints.push(constraint!(p + t <= 1));
    }
    // Raw data: each source's longest query-time scan is kept on disk; a
    // deployment without raw data runs no raw scan at query time. A part
    // runs at query time for every consumer but an ingestion-time slot.
    let at_query_time = |k: Consumer| match k {
        Consumer::Slot { choice, slot } => !matches!(
            input.choices[choice].coefficients.slots[slot].materialization,
            Materialization::MatAtIngestion(_)
        ),
        _ => true,
    };
    for scan in &input.raw_scans {
        let c = scan.choice;
        let mut runs: Vec<Variable> = Vec::new();
        match scan.via {
            Via::Direct => {
                for (&(k, d), &v) in &y {
                    if d == c && at_query_time(k) {
                        runs.push(v);
                    }
                }
            }
            Via::Slot(i) => {
                let m = input.choices[c].coefficients.slots[i].materialization;
                if !matches!(m, Materialization::MatAtIngestion(_)) {
                    runs.push(x[c]);
                }
            }
        }
        for v in runs {
            if !input.raw_data_retained {
                constraints.push(constraint!(v == 0));
            } else if scan.cost > 0.0 {
                constraints.push(constraint!(kept_on_disk[scan.source] >= scan.cost * v));
            }
        }
    }
    let constraint_count = constraints.len() as u64;
    let mut model = model
        .with_all(constraints)
        .with_mip_gap(0.0)
        .map_err(|e| format!("MIP gap: {e:?}"))?;
    // No absolute gap either: prices are scaled to about 1.
    model = model.set_option("mip_abs_gap", 0.0);
    if let Some(limit) = input.time_limit_s {
        model = model.set_time_limit(limit.max(0.001));
    }
    let solution = match model.solve() {
        Ok(solution) => solution,
        Err(good_lp::ResolutionError::Infeasible) => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let chosen: Vec<usize> = x
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
        // Stopped within the solver's own tolerance of a zero gap, the
        // objective is lowered by more than that tolerance to stay a bound.
        objective: match solution.status() {
            good_lp::solvers::SolutionStatus::GapLimit => {
                let value = solution.eval(&objective);
                value - value.abs() * GAP_TOLERANCE
            }
            _ => solution.eval(&objective),
        },
        optimal: matches!(
            solution.status(),
            good_lp::solvers::SolutionStatus::Optimal | good_lp::solvers::SolutionStatus::GapLimit
        ),
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
/// The memo is split into independent clusters ([`crate::search::milp::clusters::split`]):
/// queries linked through no shared group, coupling site or charged raw
/// horizon. Each cluster is its own workload with its own model and lazy
/// loop ([`ClusterLoop`]), the clusters' solves running in parallel; each
/// loop stops once its incumbent is certified. Under cost model 1 the
/// workload's cost is the sum of the clusters' costs, so the clusters'
/// plans together are the workload's cheapest plan
/// ([`crate::search::milp::clusters::combine`], which checks the sum).
pub(crate) fn search<Id: Clone + std::fmt::Debug>(
    per_computation: &PerComputationCandidates<Id>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    time_limit_ms: Option<u64>,
) -> MemoSearchResult {
    let started = std::time::Instant::now();
    let memo = crate::search::memo::brute_force::memo(per_computation, entries);
    let memo_stats =
        crate::search::memo::brute_force::stats(&memo, started.elapsed().as_secs_f64() * 1e3);
    let roots: Vec<QueryRoot> = per_computation
        .roots
        .iter()
        .map(|(_, root)| root.clone())
        .collect();
    let clusters = crate::search::milp::clusters::split(&memo, &roots, &models);
    if clusters.len() > 1 {
        if let Some(found) = search_clusters(
            per_computation,
            &clusters,
            entries,
            data,
            models,
            started,
            time_limit_ms,
        ) {
            let mut found = found?;
            found.0.search.memo = Some(memo_stats);
            return Ok(found);
        }
    }
    // One cluster, or clusters whose plans do not add up: the whole memo,
    // unless the clusters spent the time limit.
    if clusters.len() > 1
        && time_limit_ms.is_some_and(|limit| started.elapsed().as_millis() >= u128::from(limit))
    {
        return Err(SelectionError::Solver(
            "the MILP search reached its time limit: the clusters' plans did not combine into one plan whose cost is their sum, and no time is left to search the memo as one".to_string(),
        ));
    }
    let all: Vec<usize> = (0..entries.len()).collect();
    let mut cluster = ClusterLoop::new(
        memo,
        per_computation,
        &all,
        entries,
        data,
        models,
        started,
        time_limit_ms,
    )?;
    run_rounds(std::slice::from_mut(&mut cluster))?;
    let (mut selection, selected, roots, report) = cluster.finish()?;
    selection.search.memo = Some(memo_stats);
    if let Some(milp) = &mut selection.search.milp {
        milp.clusters = vec![report];
    }
    Ok((selection, selected, roots))
}

/// [`search`] over `clusters` (each a list of the workload's queries), each
/// searched as its own workload, their plans combined into the workload's.
/// `None` when a cluster's Pass 1 fails or the combined plan's cost is not
/// the sum of the clusters' (the caller then searches the memo as one).
fn search_clusters<Id: Clone + std::fmt::Debug>(
    per_computation: &PerComputationCandidates<Id>,
    clusters: &[Vec<usize>],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    started: std::time::Instant,
    time_limit_ms: Option<u64>,
) -> Option<MemoSearchResult> {
    // Each cluster's workload: its queries' roots through Pass 1, pruned
    // for the deployment as the caller pruned the workload's.
    let mut workloads = Vec::with_capacity(clusters.len());
    for queries in clusters {
        let roots: Vec<(Id, QueryRoot)> = queries
            .iter()
            .map(|&q| per_computation.roots[q].clone())
            .collect();
        let inventory =
            asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates(roots)
                .ok()?
                .without_unsupported_summaries(models.deployment_capabilities);
        let cluster_entries: Vec<QueryWorkloadEntry> =
            queries.iter().map(|&q| entries[q].clone()).collect();
        workloads.push((inventory, cluster_entries));
    }
    let mut loops = Vec::with_capacity(clusters.len());
    for (queries, (inventory, cluster_entries)) in clusters.iter().zip(&workloads) {
        let memo = crate::search::memo::brute_force::memo(inventory, cluster_entries);
        match ClusterLoop::new(
            memo,
            inventory,
            queries,
            cluster_entries,
            data,
            models,
            started,
            time_limit_ms,
        ) {
            Ok(cluster) => loops.push(cluster),
            Err(error) => return Some(Err(error)),
        }
    }
    if let Err(error) = run_rounds(&mut loops) {
        return Some(Err(error));
    }
    let mut found = Vec::with_capacity(loops.len());
    for (queries, cluster) in clusters.iter().zip(loops) {
        match cluster.finish() {
            Ok(result) => found.push(result),
            Err(SelectionError::NoValidCandidate(rejected)) => {
                return Some(Err(SelectionError::NoValidCandidate(
                    rejected
                        .into_iter()
                        .map(|r| crate::search::legacy::components::renumber(r, queries))
                        .collect(),
                )))
            }
            Err(error) => return Some(Err(error)),
        }
    }
    crate::search::milp::clusters::combine(found, clusters, entries, data, models).map(Ok)
}

/// Solve every cluster's model until each loop stops: up to one solver
/// thread per core, each solving one cluster's model (HiGHS keeps one
/// state per model), while this thread runs the incumbent evaluation of
/// the solutions that come back. A loop's next solve starts as soon as
/// its last solution is evaluated.
fn run_rounds<Id: Clone>(loops: &mut [ClusterLoop<'_, Id>]) -> Result<(), SelectionError> {
    /// A solver thread's stack: HiGHS recurses in presolve.
    const SOLVER_STACK_BYTES: usize = 256 << 20;
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(loops.len())
        .max(1);
    std::thread::scope(|scope| {
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut ready: std::collections::VecDeque<usize> = (0..loops.len())
            .filter(|&k| loops[k].next_round())
            .collect();
        let mut solving = 0usize;
        loop {
            while solving < threads {
                let Some(k) = ready.pop_front() else {
                    break;
                };
                // A queued solve's time limit counts from when it starts.
                if !loops[k].start_solve() {
                    continue;
                }
                let input = loops[k]
                    .input
                    .take()
                    .expect("a loop's input between solves");
                let sender = sender.clone();
                std::thread::Builder::new()
                    .stack_size(SOLVER_STACK_BYTES)
                    .spawn_scoped(scope, move || {
                        let solve_started = std::time::Instant::now();
                        let solved = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            solve(&input)
                        }))
                        .unwrap_or_else(|_| Err("the solver panicked".to_string()));
                        // The receiver is gone only once the search failed.
                        let _ = sender.send((k, input, solved, solve_started.elapsed()));
                    })
                    .map_err(|e| SelectionError::Solver(format!("no solver thread: {e}")))?;
                solving += 1;
            }
            if solving == 0 {
                return Ok(());
            }
            let (k, input, solved, solve_time) = receiver
                .recv()
                .expect("every solver thread sends its result");
            solving -= 1;
            loops[k].input = Some(input);
            loops[k].solver_ms += solve_time.as_secs_f64() * 1e3;
            loops[k].take_solution(solved)?;
            if loops[k].next_round() {
                ready.push_back(k);
            }
        }
    })
}

/// One cluster's lazy loop: solve the model (a lower bound: it omits memory held while a
/// query runs, and checks composed guarantees only on its incumbents);
/// evaluate the solution's group choice exactly, as the reference search
/// does ([`crate::search::memo::brute_force::offer_group_choice`]: every threshold,
/// identical-expression variant and Stage 2 materialization), which
/// updates the incumbent; add a no-good cut excluding that group choice;
/// stop when the MILP lower bound reaches the incumbent cost. Each group
/// choice is evaluated at most once, so the loop ends, and the incumbent is
/// the reference search's optimum.
///
/// A round is [`Self::next_round`], [`solve`] of [`Self::input`] (on a
/// solver thread, [`run_rounds`]), then [`Self::take_solution`].
struct ClusterLoop<'a, Id> {
    memo: Memo,
    per_computation: &'a PerComputationCandidates<Id>,
    /// Its queries, by index in the whole workload; query `i` of
    /// `entries` is `queries[i]`.
    queries: &'a [usize],
    entries: &'a [QueryWorkloadEntry],
    data: &'a DataWorkload,
    models: PlanningModels<'a>,
    started: std::time::Instant,
    time_limit_ms: Option<u64>,
    /// The model; `None` while a solver thread has it.
    input: Option<ModelInput>,
    selector: PlanSelector<'a>,
    logical: HashMap<String, Vec<QueryRoot>>,
    number: usize,
    iterations: u32,
    bound: Option<f64>,
    first_bound: Option<f64>,
    solver_ms: f64,
    size: (u64, u64),
    time_limit_reached: bool,
    /// The loop has stopped: certified, out of plans, or out of time.
    done: bool,
}

impl<'a, Id: Clone> ClusterLoop<'a, Id> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        memo: Memo,
        per_computation: &'a PerComputationCandidates<Id>,
        queries: &'a [usize],
        entries: &'a [QueryWorkloadEntry],
        data: &'a DataWorkload,
        models: PlanningModels<'a>,
        started: std::time::Instant,
        time_limit_ms: Option<u64>,
    ) -> Result<Self, SelectionError> {
        let input = model_input_of(&memo, entries, data, models)?;
        Ok(Self {
            memo,
            per_computation,
            queries,
            entries,
            data,
            models,
            started,
            time_limit_ms,
            input: Some(input),
            selector: PlanSelector::new(entries, data, models, false)?,
            logical: HashMap::new(),
            number: 0,
            iterations: 0,
            bound: None,
            first_bound: None,
            solver_ms: 0.0,
            size: (0, 0),
            time_limit_reached: false,
            done: false,
        })
    }

    /// Whether the loop has another round; `false` when it has stopped.
    fn next_round(&mut self) -> bool {
        if self.done || self.time_limit_reached {
            self.done = true;
            return false;
        }
        true
    }

    /// Start the round's solve: set its time limit to the time left now;
    /// `false` (the loop stops at its time limit) when none is left.
    fn start_solve(&mut self) -> bool {
        let remaining_s = self
            .time_limit_ms
            .map(|limit| limit as f64 / 1e3 - self.started.elapsed().as_secs_f64());
        if remaining_s.is_some_and(|s| s <= 0.0) {
            self.time_limit_reached = true;
            self.done = true;
            return false;
        }
        let input = self.input.as_mut().expect("the loop's input");
        input.time_limit_s = remaining_s;
        self.iterations += 1;
        true
    }

    /// The rest of a round, given its solve's result: the solution's
    /// incumbent evaluation and its cut, or the end of the loop.
    fn take_solution(
        &mut self,
        solved: Result<Option<ModelSolution>, String>,
    ) -> Result<(), SelectionError> {
        let memo = &self.memo;
        let input = self.input.as_mut().expect("the loop's input");
        let solved = match solved {
            Ok(solved) => solved,
            // The solver stopped at the time limit before finding any
            // solution: the incumbent so far stands.
            Err(error) if input.time_limit_s.is_some() && error.contains("NoSolutionFound") => {
                self.time_limit_reached = true;
                self.done = true;
                return Ok(());
            }
            Err(error) => return Err(SelectionError::Solver(error)),
        };
        // The solver stopped at the time limit: its plan is no bound, but
        // it is still evaluated, as a last round, and may become the
        // incumbent.
        let last_round = solved.as_ref().is_some_and(|s| !s.optimal);
        self.time_limit_reached |= last_round;
        let Some(solution) = solved else {
            // No plan left below the incumbent: it is optimal.
            if let Some(incumbent) = self.selector.cheapest_price() {
                self.bound = Some(incumbent);
            }
            self.done = true;
            return Ok(());
        };
        if !last_round {
            self.bound = Some(solution.objective);
            self.first_bound.get_or_insert(solution.objective);
        }
        self.size = (solution.variables, solution.constraints);
        if let Some(incumbent) = self.selector.cheapest_price() {
            if !last_round && solution.objective >= incumbent - 1e-9 * incumbent.abs() {
                self.done = true;
                return Ok(());
            }
        }
        // The solution's group choice: the candidate each chosen choice
        // takes for its group.
        let mut group_choice: crate::search::memo::brute_force::GroupChoice =
            vec![None; memo.groups.len()];
        for &c in &solution.chosen {
            group_choice[input.choices[c].group.0 as usize] = Some(input.choices[c].candidate);
        }
        // A group choice that fails a requirement the model does not
        // encode is evaluated as any other: the incumbent evaluation finds
        // no valid plan in it, and the cut below removes exactly it.
        crate::search::memo::brute_force::offer_group_choice(
            memo,
            self.per_computation,
            &group_choice,
            self.entries,
            self.data,
            self.models,
            &mut self.selector,
            &mut self.logical,
            &mut self.number,
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
        let mut covered = vec![false; memo.groups.len()];
        for (g, c) in group_choice.iter().enumerate() {
            if let Some(c) = c {
                for i in &memo.candidates[g][*c].implements {
                    covered[i.0 as usize] = true;
                }
            }
        }
        let outside = (0..memo.groups.len()).filter(|&g| !covered[g]).collect();
        input
            .excluded
            .push((excluded, outside, groups as f64 - 1.0));
        Ok(())
    }

    /// The stopped loop's selection, physical candidate and logical roots,
    /// with its MILP report, and its cluster's report.
    fn finish(
        self,
    ) -> Result<
        (
            Selection,
            PhysicalCandidate,
            Vec<QueryRoot>,
            crate::MilpClusterReport,
        ),
        SelectionError,
    > {
        let Self {
            memo,
            per_computation: _,
            queries,
            entries,
            data: _,
            models,
            input,
            selector,
            mut logical,
            iterations,
            mut bound,
            first_bound,
            solver_ms,
            size,
            time_limit_reached,
            ..
        } = self;
        let mut input = input.expect("the loop's input");
        if time_limit_reached && bound.is_none() {
            // No round solved to optimality: the relaxation's optimum (all
            // cuts kept) is still a lower bound for the gap.
            input.relaxed = true;
            input.time_limit_s = None;
            // A failed relaxation leaves the gap unknown, never the
            // incumbent lost.
            if let Ok(Some(relaxation)) = solve(&input) {
                bound = Some(relaxation.objective);
            }
        }
        if !time_limit_reached && iterations == 1 && selector.cheapest_price().is_none() {
            // Infeasible from the first round: name the queries with no plan
            // of their own.
            let mut without_plan = Vec::new();
            for (q, entry) in entries.iter().enumerate().take(input.queries.len()) {
                let mut alone = model_input_clone(&input);
                for (r, query) in alone.queries.iter_mut().enumerate() {
                    if r != q {
                        query.roots.clear();
                    }
                }
                if solve(&alone).map_err(SelectionError::Solver)?.is_none() {
                    without_plan.push(format!("query {} ({})", queries[q], entry.query.0));
                }
            }
            if !without_plan.is_empty() {
                return Err(SelectionError::Solver(format!(
                    "no valid plan for {} (raw data {})",
                    without_plan.join(", "),
                    match models.deployment_capabilities.raw_data_retained {
                        true => "retained",
                        false => "not retained",
                    }
                )));
            }
        }
        if time_limit_reached && selector.cheapest_price().is_none() {
            return Err(SelectionError::Solver(format!(
                "the MILP search reached its time limit after {iterations} rounds before finding a valid plan"
            )));
        }
        let (mut selection, selected) = selector.finish()?;
        let price = selection.costs[&selection.selected].cost_per_second;
        // Once no plan below the incumbent is left, the bound is the
        // incumbent.
        let lower_bound = bound.map(|bound| match price {
            Some(price) => bound.min(price),
            None => bound,
        });
        selection.search.incumbent_cost = price;
        let report = crate::MilpReport {
            lower_bound,
            first_lower_bound: first_bound,
            gap: gap(price, lower_bound),
            rounds: iterations,
            variable_count: size.0,
            constraint_count: size.1,
            solver_time_ms: solver_ms as u64,
            time_limit_reached,
            clusters: Vec::new(),
        };
        let cluster = crate::MilpClusterReport {
            queries: queries.to_vec(),
            groups: memo.groups.len(),
            rounds: report.rounds,
            gap: report.gap,
            lower_bound: report.lower_bound,
            incumbent_cost: price,
            solver_time_ms: report.solver_time_ms,
            time_limit_reached,
        };
        selection.search.milp = Some(report);
        let roots = logical
            .remove(&selected.from_logical)
            .expect("the selection's logical roots are kept");
        Ok((selection, selected, roots, cluster))
    }
}

/// (incumbent cost − lower bound) / incumbent cost, at least 0; `None`
/// without both.
pub(super) fn gap(incumbent_cost: Option<f64>, lower_bound: Option<f64>) -> Option<f64> {
    match (incumbent_cost, lower_bound) {
        (Some(price), Some(bound)) if price > 0.0 => Some(((price - bound) / price).max(0.0)),
        (Some(_), Some(_)) => Some(0.0),
        _ => None,
    }
}

/// The model's input for `memo`: each group's shared choices (copies and
/// widened scans are left to the exact pricing) with their lazy lower
/// bounds, and the raw scans.
pub(crate) fn model_input_of(
    memo: &Memo,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<ModelInput, SelectionError> {
    let scale = crate::cost::per_node::frozen_calibration(memo, entries, data, models)?;
    let mut cache = crate::cost::per_node::PriceCache::new(scale);
    let mut choices: Vec<Choice> = Vec::new();
    let mut raw_scans: Vec<(usize, String, u64, Via)> = Vec::new();
    let mut lazies: Vec<LazyTerms> = Vec::new();
    let CostModel::UsageOverTime(prices) = &models.plan_selection.cost_model else {
        unreachable!("the caller routes cost models 2 and 3 to the legacy search");
    };
    for g in 0..memo.groups.len() {
        let (group_choices, lazy) = crate::search::milp::coefficients::choices(
            memo,
            GroupId(g as u32),
            entries,
            data,
            models,
            &mut cache,
        )?;
        // A group's copies (apart) cost at least its shared choice with its
        // node not built once: each copy is charged per consumer as the
        // shared node is, its own scans read at least the shortest horizon,
        // and its builds are sized for fewer readers but built per copy. So
        // the model keeps the shared choices (a lower bound), and the exact
        // pricing of a group choice prices both.
        // Widened scans are reached through the identical-expression
        // variants of the exact pricing, never chosen directly.
        for (choice, lazy) in group_choices.into_iter().zip(lazy) {
            if choice.copy_of.is_some()
                || memo.candidates[g][choice.candidate].origin
                    == crate::search::memo::CandidateOrigin::WidenedScans
            {
                continue;
            }
            lazies.push(lazy.clone());
            raw_scans.extend(
                lazy.raw_scans
                    .iter()
                    .map(|(source, bytes, via)| (choices.len(), source.clone(), *bytes, *via)),
            );
            choices.push(choice);
        }
    }
    let mut input = model_input(memo, choices, entries, data, models)?;
    let mut sources: Vec<String> = Vec::new();
    for (c, source, bytes, via) in raw_scans {
        let index = match sources.iter().position(|s| *s == source) {
            Some(i) => i,
            None => {
                sources.push(source);
                sources.len() - 1
            }
        };
        input.raw_scans.push(RawScan {
            choice: c,
            source: index,
            cost: prices.cost_per_disk_byte_second * bytes as f64,
            via,
        });
    }
    input.raw_data_retained = models.deployment_capabilities.raw_data_retained;
    // Nodes several groups' choices have, equal up to scan widening: one
    // node once shared, charged once per key at the dearest chosen one
    // (`ChoiceCoefficients::cross_shared`).
    let mut groups_of_node: HashMap<u64, BTreeSet<GroupId>> = HashMap::new();
    for choice in &input.choices {
        for &(key, _) in &choice.coefficients.direct_nodes {
            groups_of_node.entry(key).or_default().insert(choice.group);
        }
    }
    for choice in &mut input.choices {
        let k = &mut choice.coefficients;
        for &(_, cost, _) in &k.absorbed_scans {
            k.direct.cost_per_unit_rate -= cost;
        }
        let cross: Vec<(u64, f64)> = k
            .direct_nodes
            .iter()
            .copied()
            .filter(|(key, _)| groups_of_node[key].len() > 1)
            .collect();
        for &(key, cost) in &cross {
            k.direct.cost_per_unit_rate -= cost;
            k.cross_shared.push((key, cost));
            for node in &mut k.shared_nodes {
                if node.members.contains(&key) {
                    node.cost_per_unit_rate -= cost;
                }
            }
        }
        k.shared_nodes.retain(|node| node.cost_per_unit_rate > 0.0);
        k.direct.cost_per_unit_rate = k.direct.cost_per_unit_rate.max(0.0);
    }
    // A slot whose build several groups' choices may share is one node
    // charged once (`SlotCoefficients::shared_site`): its held memory is
    // left out rather than charged per choice.
    let mut groups_of_key: HashMap<(u64, usize), BTreeSet<GroupId>> = HashMap::new();
    for choice in &input.choices {
        for slot in &choice.coefficients.slots {
            if let Some(key) = slot.shared_site {
                groups_of_key.entry(key).or_default().insert(choice.group);
            }
        }
    }
    let shared_keys: BTreeSet<(u64, usize)> = groups_of_key
        .into_iter()
        .filter(|(_, groups)| groups.len() > 1)
        .map(|(key, _)| key)
        .collect();
    input.held = input
        .choices
        .iter()
        .zip(&lazies)
        .map(|(choice, lazy)| held_at_query_time(choice, lazy, prices, &shared_keys))
        .collect();
    Ok(input)
}

/// Every group choice of `memo` with its exact price (the cheapest plan
/// [`crate::search::memo::brute_force::offer_group_choice`] finds) and the model's value
/// restricted to it, which should be a lower bound of the price; group
/// choices without a valid plan are left out.
pub(crate) fn lower_bound_check<Id: Clone>(
    memo: &Memo,
    per_computation: &PerComputationCandidates<Id>,
    input: &ModelInput,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<
    Vec<(
        crate::search::memo::brute_force::GroupChoice,
        f64,
        Option<f64>,
    )>,
    SelectionError,
> {
    let mut out = Vec::new();
    for group_choice in crate::search::memo::brute_force::group_choices(memo).unwrap_or_default() {
        let mut one = PlanSelector::new(entries, data, models, false)?;
        let mut scratch = HashMap::new();
        let mut k = 0usize;
        crate::search::memo::brute_force::offer_group_choice(
            memo,
            per_computation,
            &group_choice,
            entries,
            data,
            models,
            &mut one,
            &mut scratch,
            &mut k,
        )?;
        let Some(exact) = one.cheapest_price() else {
            continue;
        };
        let mut restricted = model_input_clone(input);
        restricted.excluded.clear();
        let outside: Vec<usize> = (0..restricted.choices.len())
            .filter(|&c| {
                group_choice[restricted.choices[c].group.0 as usize]
                    != Some(restricted.choices[c].candidate)
            })
            .collect();
        restricted.excluded.push((outside, Vec::new(), 0.0));
        let value = solve(&restricted)
            .map_err(SelectionError::Solver)?
            .map(|s| s.objective);
        out.push((group_choice, exact, value));
    }
    Ok(out)
}

fn model_input_clone(input: &ModelInput) -> ModelInput {
    ModelInput {
        group_count: input.group_count,
        group_queries: input.group_queries.clone(),
        choices: input.choices.clone(),
        group_slots: input.group_slots.clone(),
        queries: input.queries.clone(),
        schedule: input.schedule.clone(),
        union_rate: input.union_rate.clone(),
        excluded: input.excluded.clone(),
        extra_bounds: input.extra_bounds.clone(),
        requires: input.requires.clone(),
        exclusive: input.exclusive.clone(),
        raw_scans: input.raw_scans.clone(),
        raw_data_retained: input.raw_data_retained,
        held: input.held.clone(),
        time_limit_s: input.time_limit_s,
        relaxed: input.relaxed,
        rollup_rules: input.rollup_rules.clone(),
    }
}

/// Which choices keep their queries' accuracy targets reachable, as
/// requirements on child groups' choices ([`ModelInput::requires`]). A
/// plan's failure probability only grows up the plan (composition adds
/// its inputs' failure probabilities, and a root's guarantee over a whole
/// result is at least its per-value one) unless a node is exact whatever
/// its input (a count of rows), so a group's node composed over its
/// candidate and one child candidate, every other child exact, bounds it
/// from below. With its summaries at the largest size Stage 3 can give
/// them ([`crate::resized_to_the_largest`]), its data-workload statistics
/// bound and every other unknown term at zero (the optimistic floor), its
/// failure probability above the largest target of the queries a plan may
/// lead to the group rules the pair out, under every size threshold of the
/// candidate's and the child's coupling sites. No guarantee at all (an
/// approximate input that no rule composes) counts as failing surely. A
/// candidate that fails with every child exact is ruled out alone. A group
/// a plan may read through a node exact whatever its input gets no rule.
/// Nothing is ruled out unless the built-in model's semantics hold: the
/// deployment's accuracy model may accept an output whatever its guarantee
/// (as when guarantees are ignored), or its evidence may supply one. A
/// failure only deeper compositions show is left to the exact pricing (the
/// no-good cut of its group choice).
fn guarantee_requirements(
    memo: &Memo,
    choices: &[Choice],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Vec<(usize, Vec<usize>)> {
    use asap_logical_asap_optimization::per_computation_candidates::recompose_guarantees;
    use asap_types::ir::properties::{AccuracyGuarantee, AccuracyMetric};
    use asap_types::ir::OperatorNode;
    use std::rc::Rc;
    // Only the built-in model's semantics: a model that may accept an
    // output whatever its guarantee (also the ignore setting), or evidence,
    // may accept what the guarantee misses.
    if models
        .effective_accuracy()
        .may_accept_regardless_of_guarantee()
        || models.evidence.may_provide_evidence()
    {
        return Vec::new();
    }
    // `node`'s guarantee, composed again over its inputs; `Err` when that
    // fails.
    let guarantee_of = |roots: &[QueryRoot]| -> Result<Option<AccuracyGuarantee>, ()> {
        let roots = recompose_guarantees(roots).map_err(|_| ())?;
        match &roots[0] {
            QueryRoot::Operator(root) => Ok(root.guarantee.clone()),
            _ => Err(()),
        }
    };
    let exact = |node: &Rc<OperatorNode>| {
        guarantee_of(&[QueryRoot::Operator(Rc::clone(node))])
            .is_ok_and(|g| g.is_some_and(|g| g.is_exact()))
    };
    // The optimistic floor of the failure probability of `node`'s
    // guarantee with its summaries at the largest size Stage 3's resizing
    // can give them; `Some(1.0)` for no guarantee at its own size, which
    // no resizing or node above repairs (an approximate input no rule
    // composes); `None` when unknown. A guarantee lost only at the largest
    // size (a family whose size cap lets its error grow there, e.g. a
    // UnivMon heap at a tiny failure probability) is unknown, never 1.0.
    let floor = |node: &Rc<OperatorNode>| -> Option<f64> {
        let largest =
            crate::resized_to_the_largest(&[QueryRoot::Operator(Rc::clone(node))]).ok()?;
        let Some(guarantee) = guarantee_of(&largest).ok()? else {
            return guarantee_of(&[QueryRoot::Operator(Rc::clone(node))])
                .ok()?
                .is_none()
                .then_some(1.0);
        };
        crate::bind_statistics(&guarantee, &|statistic| {
            crate::workload_statistic(statistic, None, data)
        })
        .optimistic_floor()
        .spec
        .failure_probability
        .evaluate()
    };
    // The queries a plan may lead to each group: its roots' queries, and
    // those of every group with a candidate reading it. A group is free
    // when a plan may read it with no target: a candidate's output is
    // exact over one of its approximate candidates (a count of rows), or a
    // free group reads it.
    let mut reaching: Vec<BTreeSet<usize>> = memo
        .groups
        .iter()
        .map(|group| group.roots_of.iter().copied().collect())
        .collect();
    let mut free = vec![false; memo.groups.len()];
    for candidates in &memo.candidates {
        for candidate in candidates {
            for &GroupId(h) in &candidate.children {
                let h = h as usize;
                let child_node = Rc::as_ptr(&memo.groups[h].node);
                free[h] |= memo.candidates[h].iter().any(|d| {
                    let replaced = HashMap::from([(child_node, Rc::clone(&d.node))]);
                    let composed = crate::search::memo::brute_force::substituted(
                        &candidate.node,
                        &replaced,
                        &mut HashMap::new(),
                    );
                    !exact(&d.node) && exact(&composed)
                });
            }
        }
    }
    loop {
        let mut changed = false;
        for (g, candidates) in memo.candidates.iter().enumerate() {
            let above = reaching[g].clone();
            let children = candidates
                .iter()
                .flat_map(|c| c.children.iter())
                .chain(&memo.groups[g].children);
            for child in children {
                let h = child.0 as usize;
                let before = (reaching[h].len(), free[h]);
                reaching[h].extend(&above);
                free[h] |= free[g];
                changed |= (reaching[h].len(), free[h]) != before;
            }
        }
        if !changed {
            break;
        }
    }
    // The largest target of those queries: a plan needing the group meets
    // at least one of them through it. None when one has no target, or
    // the group is free.
    let target = |g: usize| -> Option<f64> {
        if free[g] {
            return None;
        }
        let targets: Option<Vec<f64>> = reaching[g]
            .iter()
            .map(|&q| entries[q].requirements.accuracy.failure_probability)
            .collect();
        targets?.into_iter().max_by(f64::total_cmp)
    };
    let misses = |fp: Option<f64>, target: f64| fp.is_some_and(|fp| fp > target * (1.0 + 1e-9));
    // Each candidate with a choice, at each threshold of its sites.
    let mut choices_of: BTreeMap<(usize, usize), Vec<usize>> = BTreeMap::new();
    for (c, choice) in choices.iter().enumerate() {
        choices_of
            .entry((choice.group.0 as usize, choice.candidate))
            .or_default()
            .push(c);
    }
    let versions: HashMap<(usize, usize), Vec<Rc<OperatorNode>>> = choices_of
        .keys()
        .map(|&(g, c)| {
            let nodes =
                crate::search::milp::coefficients::at_site_thresholds(memo, &memo.candidates[g][c])
                    .into_iter()
                    .map(|(version, _)| version.node)
                    .collect();
            ((g, c), nodes)
        })
        .collect();
    let mut requires = Vec::new();
    for (&(g, c), own) in &choices_of {
        let Some(target) = target(g) else {
            continue;
        };
        let nodes = &versions[&(g, c)];
        if nodes.iter().all(|node| misses(floor(node), target)) {
            requires.extend(own.iter().map(|&choice| (choice, Vec::new())));
            continue;
        }
        for child in &memo.candidates[g][c].children {
            let h = child.0 as usize;
            let child_node = Rc::as_ptr(&memo.groups[h].node);
            let of_child: Vec<(usize, usize)> = choices_of
                .keys()
                .copied()
                .filter(|&(k, _)| k == h)
                .collect();
            let allowed: Vec<(usize, usize)> = of_child
                .iter()
                .copied()
                .filter(|d| {
                    nodes.iter().any(|node| {
                        versions[d].iter().any(|child_version| {
                            let replaced = HashMap::from([(child_node, Rc::clone(child_version))]);
                            let composed = crate::search::memo::brute_force::substituted(
                                node,
                                &replaced,
                                &mut HashMap::new(),
                            );
                            !misses(floor(&composed), target)
                        })
                    })
                })
                .collect();
            if allowed.len() == of_child.len() {
                continue;
            }
            let allowed: Vec<usize> = allowed
                .iter()
                .flat_map(|d| choices_of[d].iter().copied())
                .collect();
            requires.extend(own.iter().map(|&choice| (choice, allowed.clone())));
        }
    }
    // The accuracy metric: a query's root guarantee must be in its target's
    // metric (or one comparable directly), whatever its size. A root
    // group's candidate decides that metric when everything below it that
    // may approximate is in it or in one child candidate whose children
    // only compute exactly (a group whose candidates and their children's
    // are all exact): that pair's guarantee is the root's.
    let mut exact_only = vec![false; memo.groups.len()];
    if let Some(order) = memo.topological_order() {
        for GroupId(h) in order {
            let h = h as usize;
            exact_only[h] = memo.candidates[h].iter().all(|candidate| {
                crate::search::memo::rules::windows::is_exact(candidate)
                    && candidate.children.iter().all(|c| exact_only[c.0 as usize])
            });
        }
    }
    let metric_of = |node: &Rc<OperatorNode>| {
        let guarantee = guarantee_of(&[QueryRoot::Operator(Rc::clone(node))]).ok()??;
        (!guarantee.is_exact()).then_some(guarantee.spec.accuracy_metric)
    };
    let metric_meets = |metric: AccuracyMetric, target: AccuracyMetric| {
        metric == target
            || (metric == AccuracyMetric::ABSOLUTE_STREAM_L2
                && target == AccuracyMetric::ABSOLUTE_STREAM_L1)
    };
    for (&(g, c), own) in &choices_of {
        let targets: Vec<AccuracyMetric> = memo.groups[g]
            .roots_of
            .iter()
            .filter_map(|&q| {
                let target = &entries[q].requirements.accuracy;
                (!target.is_exact())
                    .then_some(target.accuracy_metric)
                    .flatten()
            })
            .collect();
        if targets.is_empty() {
            continue;
        }
        let misses = |node: &Rc<OperatorNode>| {
            metric_of(node).is_some_and(|m| !targets.iter().all(|&t| metric_meets(m, t)))
        };
        let candidate = &memo.candidates[g][c];
        let open: Vec<usize> = candidate
            .children
            .iter()
            .map(|h| h.0 as usize)
            .filter(|&h| !exact_only[h])
            .collect();
        match open[..] {
            [] => {
                if misses(&candidate.node) {
                    requires.extend(own.iter().map(|&choice| (choice, Vec::new())));
                }
            }
            [h] => {
                let child_node = Rc::as_ptr(&memo.groups[h].node);
                let of_child: Vec<(usize, usize)> = choices_of
                    .keys()
                    .copied()
                    .filter(|&(k, _)| k == h)
                    .collect();
                let allowed: Vec<(usize, usize)> = of_child
                    .iter()
                    .copied()
                    .filter(|&(_, d)| {
                        let child = &memo.candidates[h][d];
                        // A child reading approximate groups decides nothing.
                        if !child.children.iter().all(|k| exact_only[k.0 as usize]) {
                            return true;
                        }
                        let replaced = HashMap::from([(child_node, Rc::clone(&child.node))]);
                        let composed = crate::search::memo::brute_force::substituted(
                            &candidate.node,
                            &replaced,
                            &mut HashMap::new(),
                        );
                        !misses(&composed)
                    })
                    .collect();
                if allowed.len() < of_child.len() {
                    let allowed: Vec<usize> = allowed
                        .iter()
                        .flat_map(|d| choices_of[d].iter().copied())
                        .collect();
                    requires.extend(own.iter().map(|&choice| (choice, allowed.clone())));
                }
            }
            // Several approximate children: no pair decides the metric.
            _ => {}
        }
    }

    requires
}

/// The model's input for `memo`'s `choices`.
fn model_input(
    memo: &Memo,
    choices: Vec<Choice>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
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
    // Requirements the reference search checks on a group choice
    // ([`crate::search::memo::rules::windows::requirements_met`]); rollups below.
    let candidate_of = |c: &Choice| &memo.candidates[c.group.0 as usize][c.candidate];
    let mut requires = Vec::new();
    for (c, choice) in choices.iter().enumerate() {
        let candidate = candidate_of(choice);
        for &g in &candidate.requires_exact_children {
            let allowed = (0..choices.len())
                .filter(|&a| {
                    choices[a].group == g
                        && crate::search::memo::rules::windows::is_exact(candidate_of(&choices[a]))
                })
                .collect();
            requires.push((c, allowed));
        }
        for (g, site) in &candidate.requires_sites {
            let allowed = (0..choices.len())
                .filter(|&a| {
                    choices[a].group == *g
                        && candidate_of(&choices[a]).coupling_sites.contains(site)
                })
                .collect();
            requires.push((c, allowed));
        }
    }
    requires.extend(guarantee_requirements(
        memo, &choices, entries, data, models,
    ));
    // A reader of a scan shared at horizon `h` reads the scan group's
    // choice at that horizon, so every reader of one scan agrees on it.
    for (c, choice) in choices.iter().enumerate() {
        for &(scan, horizon) in &choice.scan_children {
            let Some(h) = horizon else { continue };
            let allowed = (0..choices.len())
                .filter(|&a| choices[a].group == scan && choices[a].scan_horizon_ms == Some(h))
                .collect();
            requires.push((c, allowed));
        }
    }
    // Rollups: every rule over the candidates with a choice, each
    // candidate standing for its choices (one per group is chosen).
    let mut choices_of: BTreeMap<(usize, usize), Vec<usize>> = BTreeMap::new();
    for (c, choice) in choices.iter().enumerate() {
        choices_of
            .entry((choice.group.0 as usize, choice.candidate))
            .or_default()
            .push(c);
    }
    let present: BTreeSet<(usize, usize)> = choices_of.keys().copied().collect();
    let rollup_rules = crate::search::memo::rules::windows::rollup_rules(memo, &present)
        .into_iter()
        .map(|rule| {
            (
                rule.all_of.iter().map(|k| choices_of[k].clone()).collect(),
                rule.one_of
                    .iter()
                    .flat_map(|k| choices_of[k].clone())
                    .collect(),
            )
        })
        .collect();
    let of_origin = |origin: crate::search::memo::CandidateOrigin| -> Vec<usize> {
        (0..choices.len())
            .filter(|&c| candidate_of(&choices[c]).origin == origin)
            .collect()
    };
    let exclusive = (
        of_origin(crate::search::memo::CandidateOrigin::Panes),
        of_origin(crate::search::memo::CandidateOrigin::TopKReuse),
    );
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
        requires,
        exclusive,
        rollup_rules,
        raw_scans: Vec::new(),
        raw_data_retained: true,
        held: Vec::new(),
        time_limit_s: None,
        relaxed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_types::ir::properties::AccuracyTarget;

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
                cross_shared: Vec::new(),
                absorbed_scans: Vec::new(),
                direct_nodes: Vec::new(),
                query_work: Vec::new(),
            },
            copy_of: None,
            scan_children: Vec::new(),
            scan_horizon_ms: None,
            scan_source: None,
            scan_project_cost_per_unit_rate: 0.0,
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
            requires: Vec::new(),
            exclusive: (Vec::new(), Vec::new()),
            rollup_rules: Vec::new(),
            raw_scans: Vec::new(),
            raw_data_retained: true,
            held: Vec::new(),
            time_limit_s: None,
            relaxed: false,
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

    /// A group choice: per group, its candidate.
    type Plan = Vec<Option<usize>>;

    /// `queries`' memo (each query every minute, at `targets`, over a
    /// gauge `m` of 10,000 series) and its model's input: every choice
    /// but copies and widened scans.
    fn memo_and_input(queries: &[&str], targets: &[AccuracyTarget]) -> (Memo, ModelInput) {
        memo_and_input_over(queries, targets, None)
    }

    /// [`memo_and_input`] with `m`'s distinct items per evaluation bound
    /// to `max_distinct`.
    fn memo_and_input_over(
        queries: &[&str],
        targets: &[AccuracyTarget],
        max_distinct: Option<u64>,
    ) -> (Memo, ModelInput) {
        use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
        use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
        use asap_types::ir::schema::schema_support::with_promql_series_identity;
        use asap_types::workload::{
            DataArrival, Predictability, QueryRecurrence, RepeatedDemand, RepetitionInterval,
            SourceFacts,
        };
        let roots: Vec<(usize, QueryRoot)> = queries
            .iter()
            .zip(targets)
            .map(|(q, target)| {
                let root = lower_promql(q, target.clone());
                QueryRoot::Operator(with_promql_series_identity(&root).unwrap())
            })
            .enumerate()
            .collect();
        let per_computation = generate_per_computation_candidates(roots).unwrap();
        let entries: Vec<QueryWorkloadEntry> = targets
            .iter()
            .map(|target| {
                let mut entry = entry_with(
                    target.clone(),
                    QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
                        60_000,
                    ))),
                );
                entry.predictability = Predictability::Predictable { known_at: None };
                entry
            })
            .collect();
        let data = DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 10_000.0,
            },
            sources: [(
                "m".to_string(),
                SourceFacts {
                    series_count: Some(10_000),
                    max_distinct_per_evaluation: max_distinct,
                    ..SourceFacts::default()
                },
            )]
            .into_iter()
            .collect(),
        };
        let models = PlanningModels::builtin();
        let memo = crate::search::memo::brute_force::memo(&per_computation, &entries);
        let scale =
            crate::cost::per_node::frozen_calibration(&memo, &entries, &data, models).unwrap();
        let mut cache = crate::cost::per_node::PriceCache::new(scale);
        let mut choices = Vec::new();
        for g in 0..memo.groups.len() {
            let (group_choices, _) = crate::search::milp::coefficients::choices(
                &memo,
                GroupId(g as u32),
                &entries,
                &data,
                models,
                &mut cache,
            )
            .unwrap();
            choices.extend(group_choices.into_iter().filter(|choice| {
                choice.copy_of.is_none()
                    && memo.candidates[g][choice.candidate].origin
                        != crate::search::memo::CandidateOrigin::WidenedScans
            }));
        }
        let input = model_input(&memo, choices, &entries, &data, models).unwrap();
        (memo, input)
    }

    /// The plans of `queries`' memo ([`memo_and_input`]) the model may
    /// return, as group choices: the model of every choice with no latency
    /// bound, solved with a no-good cut on each group choice it returns
    /// until it is infeasible; and the group choices of the same
    /// candidates the reference search accepts
    /// ([`crate::search::memo::rules::windows::requirements_met`]).
    fn model_and_reference_plans(
        queries: &[&str],
        targets: &[AccuracyTarget],
    ) -> (BTreeSet<Plan>, BTreeSet<Plan>) {
        let (memo, mut input) = memo_and_input(queries, targets);
        assert!(
            input
                .choices
                .iter()
                .any(|c| memo.candidates[c.group.0 as usize][c.candidate]
                    .rollup_of
                    .is_some()),
            "a rollup is a choice"
        );
        for query in &mut input.queries {
            query.max_latency_ms = None;
        }
        let present: BTreeSet<(usize, usize)> = input
            .choices
            .iter()
            .map(|c| (c.group.0 as usize, c.candidate))
            .collect();
        let mut model: BTreeSet<Vec<Option<usize>>> = BTreeSet::new();
        while let Some(solution) = solve(&input).unwrap() {
            let mut plan = vec![None; memo.groups.len()];
            for &c in &solution.chosen {
                plan[input.choices[c].group.0 as usize] = Some(input.choices[c].candidate);
            }
            let excluded: Vec<usize> = (0..input.choices.len())
                .filter(|&c| {
                    plan[input.choices[c].group.0 as usize] == Some(input.choices[c].candidate)
                })
                .collect();
            let groups = plan.iter().flatten().count();
            let mut covered = vec![false; memo.groups.len()];
            for (g, c) in plan.iter().enumerate() {
                if let Some(c) = c {
                    for i in &memo.candidates[g][*c].implements {
                        covered[i.0 as usize] = true;
                    }
                }
            }
            let outside = (0..memo.groups.len()).filter(|&g| !covered[g]).collect();
            input
                .excluded
                .push((excluded, outside, groups as f64 - 1.0));
            assert!(model.insert(plan), "a group choice returned twice");
        }
        // Only groups with a choice to make: one with a single candidate
        // is in a plan or not by how its readers reach it (a scan read
        // apart is each reader's own copy), not by a choice.
        let decided: Vec<bool> = (0..memo.groups.len())
            .map(|g| present.iter().filter(|(h, _)| *h == g).count() > 1)
            .collect();
        let project = |plan: Vec<Option<usize>>| -> Vec<Option<usize>> {
            plan.into_iter()
                .zip(&decided)
                .map(|(c, &decided)| c.filter(|_| decided))
                .collect()
        };
        let model = model.into_iter().map(project).collect();
        let reference = crate::search::memo::brute_force::group_choices(&memo)
            .unwrap()
            .into_iter()
            .filter(|plan| {
                plan.iter()
                    .enumerate()
                    .all(|(g, c)| c.is_none_or(|c| present.contains(&(g, c))))
            })
            .filter(|plan| {
                let chosen: Vec<(GroupId, &crate::search::memo::ImplementationCandidate)> = plan
                    .iter()
                    .enumerate()
                    .filter_map(|(g, c)| Some((GroupId(g as u32), &memo.candidates[g][(*c)?])))
                    .collect();
                crate::search::memo::rules::windows::requirements_met(&chosen)
            })
            .map(project)
            .collect();
        (model, reference)
    }

    /// Exact sums by `job`, `(job, instance)` and `(job, instance, zone)`:
    /// a coarser sum may roll up from either finer one, but the joint rule
    /// rolls it up from the coarsest finer build chosen, which must be exact.
    /// The group choices the model may return are exactly those the
    /// reference search accepts.
    #[test]
    fn the_model_admits_exactly_the_plans_whose_rollups_meet_the_joint_rule() {
        let (model, reference) = model_and_reference_plans(
            &[
                "sum by (job) (m)",
                "sum by (job, instance) (m)",
                "sum by (job, instance, zone) (m)",
            ],
            &[
                AccuracyTarget::exact(),
                AccuracyTarget::exact(),
                AccuracyTarget::exact(),
            ],
        );
        assert!(!reference.is_empty());
        let only_model: Vec<_> = model.difference(&reference).collect();
        let only_reference: Vec<_> = reference.difference(&model).collect();
        assert!(
            only_model.is_empty() && only_reference.is_empty(),
            "model only {only_model:?}, reference only {only_reference:?} ({} plans)",
            reference.len()
        );
    }

    /// The average of per-series medians over 10 minutes, within 1%: the
    /// average's exact sum and count over the medians have a guarantee only
    /// when a rule composes the medians' error: an exact median, or a
    /// relative error (DDSketch), but not a KLL's rank error. So the model
    /// requires the average's choices that read the medians' group to read
    /// one of those, never a KLL one.
    #[test]
    fn a_choice_requires_child_choices_that_keep_its_guarantee() {
        let (memo, input) = memo_and_input(
            &["avg by (instance) (quantile_over_time(0.5, m[10m]))"],
            &[AccuracyTarget::max_error(0.01).failure_probability(0.01)],
        );
        let candidate = |c: usize| {
            &memo.candidates[input.choices[c].group.0 as usize][input.choices[c].candidate]
        };
        let builds = |c: usize, algorithm: &str| {
            asap_types::ir::OperatorNode::reachable(&candidate(c).node)
                .iter()
                .any(|n| {
                    matches!(n.asap(), Some(asap_types::ir::ASAPOp::SummaryAgg { family, .. })
                        if format!("{family:?}").contains(algorithm))
                })
        };
        let exact = |c: usize| crate::search::memo::rules::windows::is_exact(candidate(c));
        let mut checked = 0;
        for (c, allowed) in &input.requires {
            let child_group = |d: usize| candidate(*c).children.contains(&input.choices[d].group);
            let kll_children: Vec<usize> = (0..input.choices.len())
                .filter(|&d| child_group(d) && builds(d, "Kll"))
                .collect();
            if kll_children.is_empty() || !exact(*c) {
                continue;
            }
            checked += 1;
            assert!(
                allowed.iter().all(|d| !kll_children.contains(d)),
                "{allowed:?}"
            );
            assert!(allowed.iter().any(|&d| exact(d)), "{allowed:?}");
        }
        assert!(
            checked > 0,
            "the average's choices require a composable median"
        );
        // Solved, no exact average reads a KLL median.
        let solution = solve(&input).unwrap().unwrap();
        for &c in &solution.chosen {
            for &d in &solution.chosen {
                let reads = candidate(c).children.contains(&input.choices[d].group);
                assert!(!(reads && exact(c) && builds(d, "Kll")), "{c} reads {d}");
            }
        }
    }

    /// Classic HLL's failure probability over more distinct items than its
    /// contract covers is 1 at every precision: no size threshold (a larger
    /// shared precision) repairs it, so ruling such a candidate out under
    /// every threshold of its sites is ruling it out for good.
    #[test]
    fn an_uncertified_distinct_count_fails_at_every_precision() {
        use asap_logical_asap_optimization::accuracy::estimators::hll::classic_failure_probability;
        for precision in 4..=18 {
            assert_eq!(classic_failure_probability(precision, 0.05, 1_000_000), 1.0);
        }
        // Within the contract, a precision large enough certifies it.
        assert_eq!(classic_failure_probability(4, 0.05, 1_000), 1.0);
        assert!(classic_failure_probability(18, 0.05, 1_000) < 1.0);
    }

    /// A 0.9 quantile over 5 minutes within a rank error of 1%: a DDSketch
    /// estimate guarantees a relative error, which no rule compares with a
    /// rank target, whatever its size. The model never chooses a plan
    /// whose root reads a DDSketch, and still has one reading a KLL.
    #[test]
    fn a_choice_whose_guarantee_is_in_another_metric_is_ruled_out() {
        use asap_types::ir::properties::AccuracyMetric;
        let (memo, mut input) = memo_and_input(
            &["quantile_over_time(0.9, m[5m])"],
            &[AccuracyTarget::max_error(0.01)
                .failure_probability(0.01)
                .accuracy_metric(AccuracyMetric::Rank)],
        );
        let builds = |c: usize, algorithm: &str| {
            let choice = &input.choices[c];
            asap_types::ir::OperatorNode::reachable(
                &memo.candidates[choice.group.0 as usize][choice.candidate].node,
            )
            .iter()
            .any(|n| {
                matches!(n.asap(), Some(asap_types::ir::ASAPOp::SummaryAgg { family, .. })
                    if format!("{family:?}").contains(algorithm))
            })
        };
        assert!((0..input.choices.len()).any(|c| builds(c, "DDSketch")));
        for query in &mut input.queries {
            query.max_latency_ms = None;
        }
        // Every plan the model may return reads no DDSketch.
        let mut plans = 0;
        while let Some(solution) = solve(&input).unwrap() {
            plans += 1;
            assert!(
                solution.chosen.iter().all(|&c| !builds(c, "DDSketch")),
                "{:?}",
                solution.chosen
            );
            let groups = solution
                .chosen
                .iter()
                .map(|&c| input.choices[c].group)
                .collect::<BTreeSet<_>>()
                .len();
            input
                .excluded
                .push((solution.chosen.clone(), Vec::new(), groups as f64 - 1.0));
        }
        assert!(plans > 0);
    }
    /// The classic HLL `params` of every summary `c`'s candidate builds.
    fn hll_precisions(memo: &Memo, input: &ModelInput, c: usize) -> Vec<(u8, Option<f64>)> {
        let choice = &input.choices[c];
        asap_types::ir::OperatorNode::reachable(
            &memo.candidates[choice.group.0 as usize][choice.candidate].node,
        )
        .iter()
        .filter_map(|n| match n.asap() {
            Some(asap_types::ir::ASAPOp::SummaryAgg {
                family: asap_types::ir::schema::FieldDataType::Sketch(kind, _),
                ..
            }) => match kind.params() {
                asap_types::ir::schema::SketchParams::Hll {
                    precision,
                    relative_error,
                } => Some((*precision, *relative_error)),
                _ => None,
            },
            _ => None,
        })
        .collect()
    }

    /// Distinct items per minute within 5% with probability 0.99, over at
    /// most 2,000 distinct items: classic HLL at its Pass 1 precision does
    /// not certify that, and Stage 3 enlarges its precision until it does.
    /// So no HLL choice is ruled out.
    #[test]
    fn a_summary_stage_3_enlarges_to_certify_is_kept() {
        use asap_logical_asap_optimization::accuracy::estimators::hll;
        let (memo, input) = memo_and_input_over(
            &["distinct_over_time(m[1m])"],
            &[AccuracyTarget::max_error(0.05).failure_probability(0.01)],
            Some(2_000),
        );
        let hll_choices: Vec<usize> = (0..input.choices.len())
            .filter(|&c| !hll_precisions(&memo, &input, c).is_empty())
            .collect();
        assert!(!hll_choices.is_empty());
        // Its Pass 1 precision misses the target; Stage 3's meets it.
        assert!(hll_choices.iter().any(|&c| {
            hll_precisions(&memo, &input, c)
                .iter()
                .any(|&(precision, relative_error)| {
                    let relative_error = hll::classic_relative_error(precision, relative_error);
                    hll::classic_failure_probability(precision, relative_error, 2_000) > 0.01
                        && hll::classic_failure_probability(18, relative_error, 2_000) <= 0.01
                })
        }));
        for (c, allowed) in &input.requires {
            assert!(
                !(hll_choices.contains(c) && allowed.is_empty()),
                "HLL choice {c} ruled out"
            );
        }
    }

    /// The number of series over 1,000,000 distinct items each: a count
    /// of rows is exact whatever the per-series distinct estimates are, so
    /// its target asks nothing of them, and an HLL that certifies no
    /// failure probability over that many items is not ruled out.
    #[test]
    fn a_count_of_rows_asks_nothing_of_its_input() {
        let (memo, input) = memo_and_input_over(
            &["count(distinct_over_time(m[1m]))"],
            &[AccuracyTarget::max_error(0.05).failure_probability(0.01)],
            Some(1_000_000),
        );
        let hll_choices: Vec<usize> = (0..input.choices.len())
            .filter(|&c| !hll_precisions(&memo, &input, c).is_empty())
            .collect();
        assert!(!hll_choices.is_empty());
        for (c, allowed) in &input.requires {
            assert!(
                !(hll_choices.contains(c) && allowed.is_empty()),
                "HLL choice {c} ruled out"
            );
        }
    }
    /// Samples per distinct value per series, within 5% with probability
    /// 0.99: the distinct count is the divisor. A UnivMon divisor sized for
    /// a vanishing failure probability hits its heap cap, and its relative
    /// error grows past what a division composes, so the quotient has no
    /// guarantee at the largest size only. That is unknown, not a sure
    /// failure: no choice building a UnivMon is ruled out.
    #[test]
    fn a_guarantee_lost_only_at_the_largest_size_rules_nothing_out() {
        let (memo, input) = memo_and_input(
            &["count_over_time(m[1m]) / distinct_over_time(m[1m])"],
            &[AccuracyTarget::max_error(0.05).failure_probability(0.01)],
        );
        let univmon = |c: usize| {
            let choice = &input.choices[c];
            asap_types::ir::OperatorNode::reachable(
                &memo.candidates[choice.group.0 as usize][choice.candidate].node,
            )
            .iter()
            .any(|n| {
                matches!(n.asap(), Some(asap_types::ir::ASAPOp::SummaryAgg { family, .. })
                    if format!("{family:?}").contains("UnivMon"))
            })
        };
        let univmons: Vec<usize> = (0..input.choices.len()).filter(|&c| univmon(c)).collect();
        assert!(!univmons.is_empty());
        for (c, allowed) in &input.requires {
            assert!(
                !(univmons.contains(c) && allowed.is_empty()),
                "UnivMon choice {c} ruled out"
            );
            // A requirement on a group with UnivMon choices allows them all.
            for &d in &univmons {
                let same_group = allowed
                    .iter()
                    .any(|&a| input.choices[a].group == input.choices[d].group);
                assert!(
                    !same_group || allowed.contains(&d),
                    "choice {c} rules out UnivMon choice {d}"
                );
            }
        }
    }
}
