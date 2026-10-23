//! Term cuts (joint-planning-search design, "How we solve it", stronger
//! cuts): after a round, each term of the solution's plan that the model
//! underestimates gets a cut conditioned only on the choices that term
//! depends on, so it corrects every plan sharing those choices, not just
//! the one evaluated. Each cut's bound is a lower bound of the term in
//! every plan whose solution makes those choices; the no-good cut of the
//! solution stays as the fallback that ends the loop.
//!
//! The terms:
//!
//! - **A source's query-time scans.** The model charges the scans of one
//!   source as one widened scan may cost (the dearest, plus a pass per
//!   other scan). A plan reads them either apart, each at its own price,
//!   or widened to one label set, each widened scan at least as dear as a
//!   reader's scan widened to labels its plan's scans cover (Stage 3's
//!   price of the [`crate::search::memo::CandidateOrigin::WidenedScans`]
//!   twin at that label set). The cut charges the smaller of the two.
//! - **A query's held memory** (latency sum × memory sum): the choices a
//!   query reaches at query time fix its latency from below (each node key
//!   once, at the least work any choice the plan may have gives it) and
//!   the memory its parts hold; each held charge of those parts is at least
//!   its bytes times that latency. Builds several groups share (one node,
//!   left out of the model's held charges) get their held memory here.
//! - **The ingestion-time copy of a group built once**: a group built once
//!   per evaluation for its query-time readers that an ingestion-time slot
//!   also reads is computed twice, once at each timing (Stage 2 builds one
//!   copy per timing); the model charges only the query-time one.

use std::collections::{BTreeMap, BTreeSet};

use asap_types::physical::Materialization;

use crate::search::milp::model::ModelInput;

/// What the term cuts read about each choice of the model.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TermCuts {
    /// Per choice, the labels of each source's scans it reads (its own
    /// node, its private nodes and its child groups' nodes), by the
    /// source's raw-horizon key.
    pub scan_labels: Vec<Vec<(u64, BTreeSet<String>)>>,
    /// Per choice, the labels of each source's scans it runs at query
    /// time whenever it runs at query time: its own node's and its private
    /// nodes' (child groups' scans are their own choices'), none for a
    /// choice with an ingestion-time slot (whose scans below may run at
    /// ingestion time only).
    pub query_time_scan_labels: Vec<Vec<(u64, BTreeSet<String>)>>,
    /// Per choice, its scans widened: (source key, label set, the price per
    /// unit of rate of its query-time scans of that source widened to the
    /// label set), from its candidate's widened-scans twins.
    pub widened_scans: Vec<Vec<WidenedScan>>,
    /// Per choice, whether every consumer that may compute its direct part
    /// does so at query time (no ingestion-time slot above it): its
    /// query-time scans then run whenever it is chosen.
    pub always_at_query_time: Vec<bool>,
    /// Per choice, the slowest rate any consumer computes its direct part
    /// at: the least rate of its group's queries (a node built once runs at
    /// the union rate of the queries reaching it, at least each one's).
    pub rate_floor: Vec<f64>,
    /// Per choice, the held memory of its query-time builds several
    /// groups' choices share (one node, left out of
    /// [`ModelInput::held`]): (the build's shared key, price per ms of
    /// latency per unit of rate, whether each reader holds its own copy).
    pub shared_slot_held: Vec<Vec<SharedBuildHeld>>,
    /// Per choice, its direct part's price per second computed at
    /// ingestion time (under an ingestion-time slot; 0 when it cannot run
    /// there).
    pub at_ingestion_cost: Vec<f64>,
    /// The cuts so far.
    pub cuts: Vec<TermCut>,
}

/// A choice's query-time scans of one source widened to one label set.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WidenedScan {
    /// The source's raw-horizon key.
    pub source: u64,
    pub labels: BTreeSet<String>,
    pub cost_per_unit_rate: f64,
}

/// The memory a query-time build several groups' choices share holds
/// while an evaluation runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SharedBuildHeld {
    /// The build's shared key ([`crate::search::milp::model::SlotCoefficients::shared_site`]).
    pub key: (u64, usize),
    /// Price per ms of latency per unit of rate.
    pub cost_per_ms: f64,
    /// Whether each reader holds its own copy.
    pub per_reader: bool,
}

/// What makes a choice run at query time in a plan: other choices, and
/// the model's query-time reach (query, choice).
type Witness = (Vec<usize>, Vec<(usize, usize)>);

/// One term cut: when every choice of `choices` is chosen and each query
/// of `reached` reaches its choice at query time, the model's charge for
/// `term` is at least `bound`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TermCut {
    pub term: Term,
    pub choices: Vec<usize>,
    /// (query, choice): the model's query-time reach of the choice.
    pub reached: Vec<(usize, usize)>,
    /// Groups built once per evaluation.
    pub built_once: Vec<usize>,
    /// Nodes several groups' choices have, by key, whose charges the term
    /// includes ([`crate::search::milp::model::ChoiceCoefficients::cross_shared`]).
    pub nodes: Vec<u64>,
    pub bound: f64,
}

/// A term of the plan's price the model charges through its own variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Term {
    /// The query-time scans of the source with this raw-horizon key and
    /// the cut's nodes several groups' choices have: the model's
    /// per-source scan charge and its passes (a pass stands for a reader's
    /// node over a widened scan) and those nodes' charges.
    SourceScans(u64),
    /// The held memory of query `.0`'s own copy of choice `.1`'s parts.
    HeldPerReader(usize, usize),
    /// The held memory of choice `.0`'s parts over its longest reader.
    HeldLongest(usize),
    /// The held memory of a query-time build several groups' choices
    /// share, by its shared key (a variable of its own, charged only
    /// through these cuts).
    SharedBuildHeld((u64, usize)),
    /// The ingestion-time copy of a group built once per evaluation that an
    /// ingestion-time slot reads, by group (a variable of its own, charged
    /// only through these cuts). Its cut also holds only when some chosen
    /// choice's ingestion-time slot reads the group.
    IngestionCopy(usize),
}

impl Term {
    /// The term's kind, for reports and tests.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Term::SourceScans(_) => "source scans",
            Term::HeldPerReader(..) => "held per reader",
            Term::HeldLongest(_) => "held longest",
            Term::SharedBuildHeld(_) => "shared build held",
            Term::IngestionCopy(_) => "ingestion copy",
        }
    }
}

/// The per-choice facts [`TermCuts`] reads that the model's input already
/// has: rate floors and whether a choice always runs at query time.
pub(crate) fn fill_from_input(input: &ModelInput, cuts: &mut TermCuts) {
    let ingestion_slot_above = groups_below_ingestion_slots(input);
    cuts.always_at_query_time = input
        .choices
        .iter()
        .map(|choice| !ingestion_slot_above.contains(&(choice.group.0 as usize)))
        .collect();
    cuts.rate_floor = input
        .choices
        .iter()
        .map(|choice| {
            input.group_queries[choice.group.0 as usize]
                .iter()
                .map(|&q| input.queries[q].rate)
                .fold(f64::INFINITY, f64::min)
        })
        .map(|rate| if rate.is_finite() { rate } else { 0.0 })
        .collect();
}

/// The groups an ingestion-time slot of a choice may compute: its child
/// groups through that slot, and theirs read directly, transitively.
fn groups_below_ingestion_slots(input: &ModelInput) -> BTreeSet<usize> {
    let mut below: BTreeSet<usize> = BTreeSet::new();
    let mut stack: Vec<usize> = Vec::new();
    for choice in &input.choices {
        for (child, via) in choice.children.iter().zip(&choice.coefficients.child_via) {
            if let crate::search::milp::model::Via::Slot(i) = via {
                if matches!(
                    choice.coefficients.slots[*i].materialization,
                    Materialization::MatAtIngestion(_)
                ) {
                    stack.push(child.0 as usize);
                }
            }
        }
    }
    while let Some(g) = stack.pop() {
        if !below.insert(g) {
            continue;
        }
        for choice in input.choices.iter().filter(|c| c.group.0 as usize == g) {
            stack.extend(choice.children.iter().map(|h| h.0 as usize));
        }
    }
    below
}

/// The term cuts of the solution with `chosen` choices that are not yet in
/// `input`'s cuts and raise the model's charge.
pub(crate) fn term_cuts_of(input: &ModelInput, chosen: &[usize]) -> Vec<TermCut> {
    let facts = &input.cuts;
    // Each chosen choice that runs at query time in every plan with the
    // cut's choices: one that always does, or one a query reaches at query
    // time through choices the cut then includes (`witness`).
    let reach: Vec<Vec<usize>> = (0..input.queries.len())
        .map(|q| query_time_reach(input, chosen, q))
        .collect();
    // A witness: nothing, the model's query-time reach of the choice by a
    // query (when the model has that variable: the choice has query-time
    // work), or a query's whole query-time reach set.
    let has_reach_variable = |c: usize| {
        input.choices[c]
            .coefficients
            .query_work
            .iter()
            .any(|&(_, work)| work > 0.0)
    };
    let witness = |c: usize| -> Option<Witness> {
        if facts.always_at_query_time[c] {
            return Some((Vec::new(), Vec::new()));
        }
        let q = reach.iter().position(|r| r.contains(&c))?;
        // The model has the query's reach of the choice only for a query
        // of the choice's group (a fused choice of another group may be
        // reached through a group it implements).
        let reads = input.group_queries[input.choices[c].group.0 as usize].contains(&q);
        Some(match has_reach_variable(c) && reads {
            true => (Vec::new(), vec![(q, c)]),
            false => (reach[q].clone(), Vec::new()),
        })
    };
    let at_query_time: Vec<usize> = chosen
        .iter()
        .copied()
        .filter(|&c| witness(c).is_some())
        .collect();
    // Per source: its query-time scans in the plan, (choice, price per
    // unit of rate, pass per unit of rate).
    let mut scans: BTreeMap<u64, Vec<(usize, f64, f64)>> = BTreeMap::new();
    for &c in &at_query_time {
        let choice = &input.choices[c];
        for &(source, cost, pass) in &choice.coefficients.absorbed_scans {
            scans
                .entry(source)
                .or_default()
                .push((c, cost, pass.min(cost)));
        }
        let total = choice.coefficients.direct.cost_per_unit_rate;
        if let (Some(source), true) = (choice.scan_source, total > 0.0) {
            scans.entry(source).or_default().push((
                c,
                total,
                choice.scan_project_cost_per_unit_rate.min(total),
            ));
        }
    }
    let rate = |c: usize| facts.rate_floor[c];
    let has_ingestion_slot = |c: usize| {
        input.choices[c]
            .coefficients
            .slots
            .iter()
            .any(|slot| matches!(slot.materialization, Materialization::MatAtIngestion(_)))
    };
    let mut out = Vec::new();
    for (source, members) in scans {
        // The labels the plan's query-time scans of the source cover, at
        // least: a widened plan's scans cover them.
        let mut labels: BTreeSet<String> = BTreeSet::new();
        for &c in &at_query_time {
            for (s, read) in &facts.query_time_scan_labels[c] {
                if *s == source {
                    labels.extend(read.iter().cloned());
                }
            }
        }
        // Apart: each scan its own node at its own price (a plan makes
        // scans one node only when it widens them to one label set, or as
        // a scan group's one node, a single member).
        let apart: f64 = members.iter().map(|&(c, cost, _)| rate(c) * cost).sum();
        // Widened: at least the dearest scan, and at least any query-time
        // choice's scans widened to a label set the plan's scans cover.
        let mut widened = (0.0, None);
        for &(c, cost, _) in &members {
            if rate(c) * cost > widened.0 {
                widened = (rate(c) * cost, Some(c));
            }
        }
        for &c in &at_query_time {
            if has_ingestion_slot(c) {
                // Its widened scans may run at ingestion time only.
                continue;
            }
            for scan in &facts.widened_scans[c] {
                let charge = rate(c) * scan.cost_per_unit_rate;
                if scan.source == source && scan.labels.is_subset(&labels) && charge > widened.0 {
                    widened = (charge, Some(c));
                }
            }
        }
        // The nodes several groups' choices have among the choices reading
        // the source: in the plan at least once each, at least the least
        // charge of the choices having it (the model may charge a reader's
        // node as a pass of the scan instead).
        let mut nodes: BTreeMap<u64, f64> = BTreeMap::new();
        let mut readers: Vec<usize> = Vec::new();
        for &c in &at_query_time {
            let reads = members.iter().any(|m| m.0 == c)
                || facts.scan_labels[c].iter().any(|(s, _)| *s == source);
            if !reads {
                continue;
            }
            for &(key, cost) in &input.choices[c].coefficients.cross_shared {
                let charge = nodes.entry(key).or_insert(f64::INFINITY);
                *charge = charge.min(rate(c) * cost);
                readers.push(c);
            }
        }
        let bound = apart.min(widened.0) + nodes.values().sum::<f64>();
        // What the model charges these scans at least.
        let mut passes: BTreeMap<u64, f64> = BTreeMap::new();
        let mut excess: f64 = 0.0;
        for &(c, cost, pass) in &members {
            let p = passes.entry(cost.to_bits()).or_insert(f64::INFINITY);
            *p = p.min(rate(c) * pass);
            excess = excess.max(rate(c) * (cost - pass));
        }
        let charged = excess + passes.values().sum::<f64>();
        if bound <= charged * (1.0 + 1e-6) {
            continue;
        }
        let mut choices: Vec<usize> = members.iter().map(|&(c, _, _)| c).collect();
        choices.extend(widened.1);
        choices.extend(readers);
        let mut reached = Vec::new();
        for c in choices.clone() {
            let (more, by) = witness(c).expect("a query-time choice");
            choices.extend(more);
            reached.extend(by);
        }
        choices.sort_unstable();
        choices.dedup();
        reached.sort_unstable();
        reached.dedup();
        let cut = TermCut {
            term: Term::SourceScans(source),
            choices,
            reached,
            built_once: Vec::new(),
            nodes: nodes.keys().copied().collect(),
            bound,
        };
        if !facts.cuts.iter().any(|old| {
            old.term == cut.term
                && old.choices == cut.choices
                && old.reached == cut.reached
                && old.bound >= bound
        }) {
            out.push(cut);
        }
    }
    out
}

/// The choices query `q` reaches at query time in the plan of `chosen`:
/// from its roots through the chosen choices' children read directly or by
/// a query-time slot.
fn query_time_reach(input: &ModelInput, chosen: &[usize], q: usize) -> Vec<usize> {
    let mut choice_of: BTreeMap<usize, usize> = BTreeMap::new();
    for &c in chosen {
        for g in &input.choices[c].implements {
            choice_of.insert(g.0 as usize, c);
        }
    }
    let mut reached: BTreeSet<usize> = BTreeSet::new();
    let mut seen: BTreeSet<usize> = BTreeSet::new();
    let mut stack: Vec<usize> = input.queries[q]
        .roots
        .iter()
        .map(|g| g.0 as usize)
        .collect();
    while let Some(g) = stack.pop() {
        if !seen.insert(g) {
            continue;
        }
        let Some(&c) = choice_of.get(&g) else {
            continue;
        };
        reached.insert(c);
        let choice = &input.choices[c];
        for (child, via) in choice.children.iter().zip(&choice.coefficients.child_via) {
            let at_query = match via {
                crate::search::milp::model::Via::Direct => true,
                crate::search::milp::model::Via::Slot(i) => !matches!(
                    choice.coefficients.slots[*i].materialization,
                    Materialization::MatAtIngestion(_)
                ),
            };
            if at_query {
                stack.push(child.0 as usize);
            }
        }
    }
    reached.into_iter().collect()
}

/// The held-memory cuts of the solution with `chosen`
/// choices.
pub(crate) fn held_cuts_of(input: &ModelInput, chosen: &[usize]) -> Vec<TermCut> {
    let facts = &input.cuts;
    let mut out: Vec<TermCut> = Vec::new();
    let group_of = |c: usize| input.choices[c].group.0 as usize;
    let cut = |term: Term, choices: &[usize], bound: f64| TermCut {
        term,
        choices: choices.to_vec(),
        reached: Vec::new(),
        built_once: Vec::new(),
        nodes: Vec::new(),
        bound,
    };
    // Every query may read a part (nodes several groups share are one).
    let all_queries: Vec<usize> = (0..input.queries.len()).collect();
    // Per query: the choices it reaches at query time and the latency
    // they give it at least.
    let mut latency_of: Vec<(Vec<usize>, f64)> = Vec::new();
    for q in 0..input.queries.len() {
        let reached = query_time_reach(input, chosen, q);
        let groups: BTreeSet<usize> = reached.iter().map(|&c| group_of(c)).collect();
        // Each node key the query computes, at the least work a choice it
        // reaches gives it, or any choice of a group outside the reached
        // ones (whose node a plan may make one with it).
        let mut keys: BTreeMap<u64, f64> = BTreeMap::new();
        for &c in &reached {
            for &(key, work) in &input.choices[c].coefficients.query_work {
                if work > 0.0 {
                    let least = keys.entry(key).or_insert(f64::INFINITY);
                    *least = least.min(work);
                }
            }
        }
        let mut global: BTreeMap<u64, f64> = keys.clone();
        for (c, choice) in input.choices.iter().enumerate() {
            for &(key, work) in &choice.coefficients.query_work {
                if let Some(least) = global.get_mut(&key) {
                    *least = least.min(work);
                    if !groups.contains(&group_of(c)) {
                        let own = keys.get_mut(&key).expect("same keys");
                        *own = own.min(work);
                    }
                }
            }
        }
        let latency: f64 = keys.values().sum();
        let modeled: f64 = global.values().sum();
        let rate_q = input.queries[q].rate;
        // The model's own held charges already use the least latency per
        // key: a cut helps only when the reached choices give more.
        if latency > modeled * (1.0 + 1e-9) {
            for &c in &reached {
                let held = input.held.get(c).cloned().unwrap_or_default();
                // The model's held charge of a query's copy exists only for
                // a query of the choice's group.
                let reads = input.group_queries[group_of(c)].contains(&q);
                if reads && held.per_reader_per_ms > 0.0 {
                    let share = reader_share(input, &all_queries);
                    out.push(cut(
                        Term::HeldPerReader(q, c),
                        &reached,
                        held.per_reader_per_ms * rate_q * share * latency,
                    ));
                }
                if held.longest_per_ms > 0.0 {
                    out.push(cut(
                        Term::HeldLongest(c),
                        &reached,
                        held.longest_per_ms * facts.rate_floor[c] * latency,
                    ));
                }
            }
        }
        latency_of.push((reached, latency));
    }
    // A build several groups' choices share: held while its readers run,
    // at their union rate, for their rate-weighted mean latency (each
    // reader its own copy) or the longest. Readers a plan with more choices
    // may add lower the mean at most to the share of every query that may
    // read it. Its copies may also stay apart, each read by some of the
    // readers: then the copy a reader `q` reads holds at least at `q`'s
    // rate for `q`'s share of the mean (or `q`'s latency).
    let mut builds: BTreeMap<(u64, usize), (f64, bool, BTreeSet<usize>)> = BTreeMap::new();
    for (q, (reached, _)) in latency_of.iter().enumerate() {
        for &c in reached {
            for held in &facts.shared_slot_held[c] {
                let entry =
                    builds
                        .entry(held.key)
                        .or_insert((f64::INFINITY, false, BTreeSet::new()));
                entry.0 = entry.0.min(held.cost_per_ms);
                entry.1 |= held.per_reader;
                entry.2.insert(q);
            }
        }
    }
    for (key, (per_ms, per_reader, readers)) in builds {
        let mut possible: BTreeSet<usize> = BTreeSet::new();
        for (c, held) in facts.shared_slot_held.iter().enumerate() {
            if held.iter().any(|h| h.key == key) {
                possible.extend(input.group_queries[group_of(c)].iter().copied());
            }
        }
        let rate_all: f64 = possible.iter().map(|&q| input.queries[q].rate).sum();
        let mut classes: Vec<usize> = readers.iter().map(|&q| input.schedule[q]).collect();
        classes.sort_unstable();
        classes.dedup();
        let union = input.union_rate.get(&classes).copied().unwrap_or_else(|| {
            readers
                .iter()
                .map(|&q| input.queries[q].rate)
                .fold(0.0, f64::max)
        });
        let longest = readers.iter().map(|&q| latency_of[q].1).fold(0.0, f64::max);
        let weighted = match rate_all > 0.0 {
            true => {
                readers
                    .iter()
                    .map(|&q| input.queries[q].rate * latency_of[q].1)
                    .sum::<f64>()
                    / rate_all
            }
            false => 0.0,
        };
        let one_node = per_ms
            * union
            * match per_reader {
                true => weighted.min(longest),
                false => longest,
            };
        let apart = readers
            .iter()
            .map(|&q| {
                let (rate, latency) = (input.queries[q].rate, latency_of[q].1);
                per_ms
                    * rate
                    * match (per_reader, rate_all > 0.0) {
                        (true, true) => (rate * latency / rate_all).min(latency),
                        (true, false) => 0.0,
                        (false, _) => latency,
                    }
            })
            .fold(0.0, f64::max);
        let bound = one_node.min(apart);
        if bound > 0.0 {
            let mut choices: Vec<usize> = readers
                .iter()
                .flat_map(|&q| latency_of[q].0.iter().copied())
                .collect();
            choices.sort_unstable();
            choices.dedup();
            out.push(cut(Term::SharedBuildHeld(key), &choices, bound));
        }
    }
    out.retain(|cut| {
        !facts.cuts.iter().any(|old| {
            old.term == cut.term
                && old.choices == cut.choices
                && old.reached == cut.reached
                && old.bound >= cut.bound
        })
    });
    out
}

/// The least share of a part's held memory a reader among `queries` is
/// charged per unit of its rate: the part runs at the union rate of its
/// readers' classes, not their rates' sum, so a reader's share is `union /
/// sum` of its rate (Stage 3's rate-weighted mean latency at the part's
/// rate); the least ratio over the class sets of `queries` bounds it, and
/// readers in classes no union rate joins run at least at their fastest's
/// rate (`fastest / sum`).
pub(crate) fn reader_share(input: &ModelInput, queries: &[usize]) -> f64 {
    let fastest_over_sum = queries.iter().map(|&q| {
        let fastest = input.queries[q].rate;
        let sum: f64 = queries
            .iter()
            .map(|&r| input.queries[r].rate)
            .filter(|&rate| rate <= fastest)
            .sum();
        match sum > 0.0 {
            true => (fastest / sum).min(1.0),
            false => 1.0,
        }
    });
    input
        .union_rate
        .iter()
        .filter_map(|(set, &union)| {
            let rate_sum: f64 = queries
                .iter()
                .filter(|&&q| set.contains(&input.schedule[q]))
                .map(|&q| input.queries[q].rate)
                .sum();
            let covered = set
                .iter()
                .all(|class| queries.iter().any(|&q| input.schedule[q] == *class));
            (covered && rate_sum > 0.0).then_some((union / rate_sum).min(1.0))
        })
        .chain(fastest_over_sum)
        .fold(1.0, f64::min)
}

/// The ingestion-time copy cuts of the solution with `chosen` choices and
/// `built_once` groups: each group built once that a chosen choice's
/// ingestion-time slot reads costs at least its chosen choice's direct
/// part at ingestion time.
pub(crate) fn ingestion_copy_cuts_of(
    input: &ModelInput,
    chosen: &[usize],
    built_once: &BTreeSet<crate::search::memo::GroupId>,
) -> Vec<TermCut> {
    let facts = &input.cuts;
    let mut choice_of: BTreeMap<usize, usize> = BTreeMap::new();
    for &c in chosen {
        choice_of.insert(input.choices[c].group.0 as usize, c);
    }
    let mut out: Vec<TermCut> = Vec::new();
    for &p in chosen {
        let parent = &input.choices[p];
        for (child, via) in parent.children.iter().zip(&parent.coefficients.child_via) {
            let crate::search::milp::model::Via::Slot(i) = via else {
                continue;
            };
            if !matches!(
                parent.coefficients.slots[*i].materialization,
                Materialization::MatAtIngestion(_)
            ) || !built_once.contains(child)
            {
                continue;
            }
            let h = child.0 as usize;
            let Some(&c) = choice_of.get(&h) else {
                continue;
            };
            let bound = facts.at_ingestion_cost.get(c).copied().unwrap_or(0.0);
            if bound > 0.0 {
                out.push(TermCut {
                    term: Term::IngestionCopy(h),
                    choices: vec![c],
                    reached: Vec::new(),
                    built_once: vec![h],
                    nodes: Vec::new(),
                    bound,
                });
            }
        }
    }
    out.retain(|cut| !facts.cuts.contains(cut));
    out.dedup();
    out
}
