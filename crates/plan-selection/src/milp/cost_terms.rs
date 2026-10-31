//! #1095 §3: the cost model 1 objective of the selection MILP. Every number is
//! `asap-cost`'s: a candidate's usage per execution and standing cost
//! ([`CandidateUsage`], from [`asap_cost::node_usage`] in `owners.rs`), the
//! schedule groups ([`asap_cost::execution_demand::schedule_groups`]) and
//! rates ([`asap_cost::execution_demand::evaluation_rate`]) and the unit
//! costs of [`asap_cost::ResourceUnitCost`]. This module only turns them
//! into coefficients and the auxiliary rows that linearize them; it holds
//! no usage formula, schedule rule or unit cost of its own.
//!
//! `asap_cost::plan_cost` charges, per node of a plan, under cost model 1:
//!
//! | Term | `plan_cost` | Encoding |
//! |---|---|---|
//! | execution | cost of one execution × executions per second | below |
//! | standing | kept memory and disk per second, whatever runs it | `standing · indicator` |
//! | held memory | query-time bytes × the evaluation's latency × executions per second | below |
//! | raw retention | per source, the largest raw data a query-time scan keeps, on disk | levels `v[src, ℓ]` |
//! | request fees | per query, a fixed time per request | none: the same for every plan, reported apart |
//!
//! **Indicators.** A node's numbers and its output (the statistics its
//! readers are costed from) may depend on which producer an input reads,
//! and on that producer's own output (`CandidateData::usage_per_producers`,
//! tabulated per producer output). Its terms then sit on `AND(link,
//! t_outputs)`, where `t_outputs ≥` the producer node's terms giving that
//! output (one variable per producer node and outputs, shared by its
//! readers), or on the link alone when every output of the producer gives
//! the same; for several such inputs, on an AND over them (`t_producers`,
//! `t ≥ Σ − (n − 1)`). A query-time scan whose usage depends on the window
//! its readers read gets one level `λ[i, ℓ] ≥` each read at a window of
//! level `ℓ` or wider; the scan reads the widest one, and a reader of it
//! is costed at that window through `AND(read, λ[i, low] − λ[i, high + 1])`.
//!
//! The ANDs are kept only where the usage really differs: a reader's
//! choices of a producer's outputs with equal usage are one choice when no
//! reader of the reader tells its own outputs apart
//! ([`outputs_that_matter`]), and the scan levels below a reader's own
//! widest window (which its read already rules out) count as its own
//! level, so a reader whose usage does not change from its own window up
//! needs none.
//!
//! **Executions per second** of instance `i`:
//! - a fixed `rate · x[i]`: at ingestion time (once per second), for a
//!   materialized instance whose possible readers form one schedule group
//!   at one rate, and for a not-materialized one with one consumer query;
//! - otherwise `Σ rate · u` over components `u ∈ [0, 1]`:
//!   - `MatAtQuery`, kept across evaluations: one `z[i, G, ℓ]` per schedule
//!     group `G` of its possible readers and per level `ℓ` of the readers'
//!     own rates in it, `z ≥ a[q, i]` for each reader `q` at that level or
//!     above: the group's rate is its fastest reader's;
//!   - not materialized: once per consumer, as `plan_cost`'s
//!     `computations` counts it: the queries reaching it through nodes that
//!     are not materialized (`y[q, i]`, or `a[q, i]` when no materialized
//!     node can be in between) at their own rates, plus each materialized
//!     instance `m` reaching it so, at `m`'s rate (`AND(z_m, y[m, i])` per
//!     component of `m`).
//!
//! A query owner's node runs at its query's rate. An instance owner's nodes
//! other than the instance's own plain term are summed once, `E[i] ≥ Σ
//! cost · indicator`, and charged per rate component, `V ≥ E[i] − Ē[i] (x[i]
//! − u)`, with `Σ rate · V ≥ (slowest rate) · E[i]` (an active instance runs
//! at one rate component at least).
//!
//! Rows written `− x[i]` where `− 1` would do (the AND rows of `y`, `V`) are
//! the same at every plan, since what they combine is 0 when `x[i]` is; they
//! are tighter in the LP relaxation.
//!
//! **Held memory** (`plan_cost`'s `held`): a `MatAtQuery` result is held
//! for the longest reader's latency, `H[i] ≥ L[q] − L̄[q] (1 − a[q, i])`
//! with each reader's own most latency `L̄[q]`, once per execution: `H` at a
//! fixed rate, or `W ≥ H − max_q L̄[q] (1 − u)` per rate component. A
//! not-materialized node's working memory is held for each consumer query's
//! latency at its rate: `G[q, ·] ≥ L[q] − M (1 − reach)`, charged `rate(q) ·
//! bytes · unit memory cost`.
//!
//! **Raw retention**: per source, levels at each distinct raw-data size a
//! scan may keep, `v[src, ℓ] ≥` each scan's indicator of keeping at least
//! that much; charged `Δbytes · unit disk cost`.
//!
//! Terms `plan_cost` defines differently from the above are counted in
//! [`SelectionMilp::inexact`](super::model::SelectionMilp) by reason (see the
//! notes at each).
use std::collections::{BTreeMap, BTreeSet, HashMap};

use asap_cost::execution_demand::{evaluation_rate, schedule_groups};
use asap_cost::CostModel;
use asap_types::ir::physical_export::ScanWindow;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{NonASAPOp, Operator};
use asap_types::physical::{Materialization, Medium};
use asap_types::stages::PhysicalCandidateId;
use asap_types::workload::bounds::source_name;
use asap_types::workload::{QueryRecurrence, RepeatedDemand};
use good_lp::{constraint, Expression, IntoAffineExpression, Variable};

use crate::owners::{CandidateUsage, UsageAndOutput, UsageContext};

use super::model::{candidates_below, SelectionMilp, SelectionMilpError};

/// The numbers of one node `asap-cost` computed, as the objective and the
/// latency rows read them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct NodeNumbers {
    /// Cost of one execution.
    pub(crate) execution_cost: f64,
    /// Cost per second of what it keeps whatever runs it.
    pub(crate) standing_cost: f64,
    /// What it adds to a reaching query's latency, ms.
    pub(crate) latency_ms: f64,
    /// Query-time memory held while an evaluation reading it runs.
    pub(crate) held_bytes: f64,
    /// Raw data a query-time scan keeps on disk.
    pub(crate) raw_kept_bytes: f64,
}

impl NodeNumbers {
    fn of(usage: &CandidateUsage) -> Self {
        Self {
            execution_cost: usage.cost_per_execution.unwrap_or(0.0),
            standing_cost: usage.standing_cost_per_second.unwrap_or(0.0),
            latency_ms: usage.own_latency_ms,
            held_bytes: usage.held_bytes as f64,
            raw_kept_bytes: usage.raw_data_kept_bytes as f64,
        }
    }

    /// `self − other`, each number at least 0, and whether one was below.
    fn minus(self, other: Self) -> (Self, bool) {
        let fields = [
            self.execution_cost - other.execution_cost,
            self.standing_cost - other.standing_cost,
            self.latency_ms - other.latency_ms,
            self.held_bytes - other.held_bytes,
            self.raw_kept_bytes - other.raw_kept_bytes,
        ];
        let negative = fields
            .iter()
            .any(|&field| field < -1e-12 * field.abs().max(1.0));
        let [execution_cost, standing_cost, latency_ms, held_bytes, raw_kept_bytes] =
            fields.map(|field| field.max(0.0));
        (
            Self {
                execution_cost,
                standing_cost,
                latency_ms,
                held_bytes,
                raw_kept_bytes,
            },
            negative,
        )
    }
}

/// One term of a node: its numbers count, and it gives its readers output
/// `output`, when `indicator` is 1. At integral binaries exactly one term
/// of a used node is 1, except a windowed scan's, whose levels add up
/// (`window_terms`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodeTerm {
    pub(crate) indicator: Expression,
    pub(crate) numbers: NodeNumbers,
    /// Whether `indicator` is the node's own variable (`s[o, k]` or `x[i]`).
    pub(crate) plain: bool,
    /// Its output (an index below `CandidateData::output_count`).
    pub(crate) output: usize,
}

/// Owner `owner`'s use of `class` as terms (module documentation,
/// "Indicators"). Empty when the owner cannot use it.
pub(crate) fn node_terms(
    milp: &mut SelectionMilp<'_>,
    owner: usize,
    class: PhysicalCandidateId,
) -> Vec<NodeTerm> {
    let Some(node) = milp.node_variable(owner, class) else {
        return Vec::new();
    };
    let data = &milp.inputs.candidate_data[class.0];
    let Ok(base) = &data.usage else {
        return Vec::new();
    };
    for context in &data.usage_depends_on {
        match context {
            UsageContext::ProducerOnACycle => milp.note_inexact("usage: a producer on a cycle"),
            UsageContext::TooManyContexts => milp.note_inexact("usage: too many contexts"),
            UsageContext::PaneRetentionOfReachingQueries => {
                milp.note_inexact("usage: pane retention of reaching queries (not encoded)")
            }
            UsageContext::ProducerOutput
            | UsageContext::ScanWindowOfReader
            | UsageContext::UncostableContext => {}
        }
    }
    if data.producer_usage_ambiguous {
        milp.note_inexact("usage: a context option other than producers or windows");
    }
    let own_instance = milp
        .instance_of_owner(owner)
        .filter(|&instance| milp.instances[instance].class == class);
    if let (Some(instance), false) = (own_instance, data.usage_per_window.is_empty()) {
        return window_terms(milp, instance, class);
    }
    if !data.inputs_changing_usage.is_empty() {
        return producer_terms(milp, owner, class, node);
    }
    vec![NodeTerm {
        indicator: node.into(),
        numbers: NodeNumbers::of(base),
        plain: true,
        output: 0,
    }]
}

/// Per candidate, whether some reader's usage (or an output that matters)
/// depends on which of its outputs it gives: otherwise its readers' choices
/// of its outputs with equal usage are one choice, and need no AND with the
/// producer's terms. A fixpoint from "no candidate's outputs matter"; a
/// reader with several changing inputs tells every producer output apart.
pub(crate) fn outputs_that_matter(inputs: &crate::owners::MilpInputs) -> Vec<bool> {
    let count = inputs.candidate_data.len();
    let mut matters = vec![false; count];
    let mut changed = true;
    while changed {
        changed = false;
        for (reader, data) in inputs.candidate_data.iter().enumerate() {
            let mut gives_per_choice: HashMap<PhysicalCandidateId, Vec<Option<UsageAndOutput>>> =
                HashMap::new();
            for (choices, gives) in &data.usage_per_producers {
                if data.inputs_changing_usage.len() != 1 {
                    for choice in choices {
                        if choice.output.is_some() && !matters[choice.class.0] {
                            matters[choice.class.0] = true;
                            changed = true;
                        }
                    }
                    continue;
                }
                let Some(choice) = choices.first() else {
                    continue;
                };
                let gives =
                    gives.map(|(usage, output)| (usage, if matters[reader] { output } else { 0 }));
                gives_per_choice
                    .entry(choice.class)
                    .or_default()
                    .push(gives);
            }
            for (producer, gives) in gives_per_choice {
                if !matters[producer.0] && gives.windows(2).any(|pair| pair[0] != pair[1]) {
                    matters[producer.0] = true;
                    changed = true;
                }
            }
        }
    }
    matters
}

/// One link of a node's input: the link's variable and the producer node
/// (its owner and candidate).
#[derive(Debug, Clone, Copy)]
struct Link {
    variable: Variable,
    producer_owner: usize,
    producer: PhysicalCandidateId,
    /// The instance read, for a read of one.
    instance: Option<usize>,
}

/// The links input `input` of `class` in owner `owner` may take.
fn links_of(
    milp: &SelectionMilp<'_>,
    owner: usize,
    class: PhysicalCandidateId,
    input: usize,
) -> Vec<Link> {
    let variables = &milp.owners[owner];
    let mut links = Vec::new();
    for &((reader, at, producer), variable) in &variables.links {
        if reader == class && at == input {
            links.push(Link {
                variable,
                producer_owner: owner,
                producer,
                instance: None,
            });
        }
    }
    for &((reader, at, instance), variable) in &variables.reads {
        if reader == class && at == input {
            links.push(Link {
                variable,
                producer_owner: milp.instance_owner(instance),
                producer: milp.instances[instance].class,
                instance: Some(instance),
            });
        }
    }
    links
}

/// The indicator that `link` is taken and its producer gives `output`
/// (`None`: any output): the link itself, or `AND(link, the producer's
/// terms giving that output)`. `None` when the producer never gives it, or
/// its terms are being built (a cycle of links; then the link stands for
/// every output, counted as inexact).
fn link_output_indicator(
    milp: &mut SelectionMilp<'_>,
    link: Link,
    outputs: Option<&BTreeSet<usize>>,
) -> Option<Expression> {
    let Some(outputs) = outputs else {
        return Some(link.variable.into());
    };
    if milp
        .terms_in_progress
        .contains(&(link.producer_owner, link.producer))
    {
        milp.note_inexact("usage: a producer's output on a cycle of links (every output kept)");
        return Some(link.variable.into());
    }
    let producer_terms = milp.node_terms(link.producer_owner, link.producer);
    if producer_terms
        .iter()
        .all(|term| outputs.contains(&term.output))
    {
        return Some(link.variable.into());
    }
    let gives: Vec<&NodeTerm> = producer_terms
        .iter()
        .filter(|term| outputs.contains(&term.output))
        .collect();
    if gives.is_empty() {
        return None;
    }
    // `gives` as one variable per producer node and outputs, `≥` the sum of
    // its terms giving them, shared by every reader of that node.
    let key = (
        link.producer_owner,
        link.producer,
        outputs.iter().copied().collect::<Vec<_>>(),
    );
    let gives = match milp.producer_gives.get(&key) {
        Some(&gives) => gives,
        None => {
            let sum: Expression = gives.iter().map(|term| term.indicator.clone()).sum();
            let variable = milp
                .builder
                .continuous("t_outputs (a producer gives outputs)", Some(1.0));
            milp.builder.row(
                "t_outputs (a producer gives outputs)",
                constraint!(variable >= sum),
            );
            milp.producer_gives.insert(key, variable);
            variable
        }
    };
    Some(
        milp.builder
            .and_at_least("t_producers (AND)", &[link.variable.into(), gives.into()])
            .into(),
    )
}

/// A schedule group's readers with their own rates, and its rate levels.
type GroupLevels = (Vec<(usize, f64)>, Vec<f64>);

/// A producer choice with the outputs it stands for (`None`: any output).
type OutputsOfProducer = (PhysicalCandidateId, Option<BTreeSet<usize>>);

/// Terms of a node whose usage or output depends on its producers (and
/// their outputs): one per choice, on `AND(link, producer's output)` for
/// one changing input, or on an AND over the inputs for several.
fn producer_terms(
    milp: &mut SelectionMilp<'_>,
    owner: usize,
    class: PhysicalCandidateId,
    node: Variable,
) -> Vec<NodeTerm> {
    milp.terms_in_progress.insert((owner, class));
    let data = &milp.inputs.candidate_data[class.0];
    let changing = data.inputs_changing_usage.clone();
    let base = *data.usage.as_ref().expect("costed");
    // The choices, each with the outputs it stands for; for one changing
    // input, the outputs of a producer that give the same usage and output
    // are one choice (the same usage alone, when no reader of `class`
    // tells its outputs apart).
    let output_matters = milp.output_matters[class.0];
    let same = |known: &Option<UsageAndOutput>, gives: &Option<UsageAndOutput>| match (known, gives)
    {
        (Some((known_usage, known_output)), Some((usage, output))) => {
            known_usage == usage && (!output_matters || known_output == output)
        }
        (None, None) => true,
        _ => false,
    };
    let mut table: Vec<(Vec<OutputsOfProducer>, Option<UsageAndOutput>)> = Vec::new();
    for (choices, gives) in &data.usage_per_producers {
        let choices: Vec<OutputsOfProducer> = choices
            .iter()
            .map(|choice| {
                (
                    choice.class,
                    choice.output.map(|output| BTreeSet::from([output])),
                )
            })
            .collect();
        if changing.len() == 1 {
            if let Some((known, _)) = table
                .iter_mut()
                .find(|(known, known_gives)| known[0].0 == choices[0].0 && same(known_gives, gives))
            {
                if let (Some(outputs), Some(more)) = (&mut known[0].1, &choices[0].1) {
                    outputs.extend(more);
                }
                continue;
            }
        }
        table.push((choices, *gives));
    }
    let links: Vec<Vec<Link>> = changing
        .iter()
        .map(|&input| links_of(milp, owner, class, input))
        .collect();
    let mut terms = Vec::new();
    let mut covered: Vec<BTreeSet<PhysicalCandidateId>> = vec![BTreeSet::new(); changing.len()];
    for (choices, gives) in &table {
        // Per changing input, the indicator that it takes this choice.
        let mut per_input = Vec::with_capacity(choices.len());
        for (position, choice) in choices.iter().enumerate() {
            let mut sum = Expression::from(0.0);
            let mut any = false;
            let (class_chosen, outputs) = choice;
            for &link in links[position]
                .iter()
                .filter(|link| link.producer == *class_chosen)
            {
                covered[position].insert(*class_chosen);
                let indicator = match scan_window_of(milp, class, link, outputs.as_ref()) {
                    Some(window_indicator) => window_indicator,
                    None => link_output_indicator(milp, link, outputs.as_ref()),
                };
                if let Some(indicator) = indicator {
                    sum += indicator;
                    any = true;
                }
            }
            if !any {
                break;
            }
            per_input.push(sum);
        }
        if per_input.len() < choices.len() {
            continue;
        }
        let count = per_input.len() as f64;
        match gives {
            None => {
                // `asap-cost` cannot cost this choice: no plan makes it.
                let sum: Expression = per_input.into_iter().sum();
                milp.builder
                    .row("t_producers (AND)", constraint!(sum <= count - 1.0));
            }
            Some((usage, output)) => {
                let indicator = match per_input.len() {
                    1 => per_input.into_iter().next().expect("one"),
                    _ => milp
                        .builder
                        .and_at_least("t_producers (AND)", &per_input)
                        .into(),
                };
                terms.push(NodeTerm {
                    indicator,
                    numbers: NodeNumbers::of(usage),
                    plain: false,
                    output: *output,
                });
            }
        }
    }
    // A producer missing from the table (not costed with this class): its
    // links carry the first costed usage.
    let mut missing = false;
    for (position, links) in links.iter().enumerate() {
        for link in links {
            if covered[position].contains(&link.producer) {
                continue;
            }
            missing = true;
            terms.push(NodeTerm {
                indicator: link.variable.into(),
                numbers: NodeNumbers::of(&base),
                plain: false,
                output: 0,
            });
        }
    }
    if missing {
        milp.note_inexact("usage: a producer missing from its usage table");
    }
    if terms.is_empty() {
        // No producer choice can be costed: the node cannot be used.
        milp.builder
            .row("t_producers (AND)", constraint!(node <= 0.0));
    }
    milp.terms_in_progress.remove(&(owner, class));
    terms
}

/// For a read of a windowed scan instance: the indicator that the read is
/// taken and the scan gives `output`, i.e. its widest window read is one of
/// the levels giving that output: `AND(read, λ[i, low] − λ[i, high + 1])`
/// (`λ[i, 0] = x[i]`, `window_terms`). Exact when a reader's numbers do not
/// fall as the scan's window widens (the levels are lower bounds). `None`
/// unless `link` reads a windowed scan; `Some(None)` when the scan never
/// gives `output`.
fn scan_window_of(
    milp: &mut SelectionMilp<'_>,
    reader: PhysicalCandidateId,
    link: Link,
    outputs: Option<&BTreeSet<usize>>,
) -> Option<Option<Expression>> {
    let instance = link.instance?;
    let scan = link.producer;
    if milp.inputs.candidate_data[scan.0]
        .usage_per_window
        .is_empty()
    {
        return None;
    }
    let outputs = outputs?;
    // Per level (narrowest first), whether the scan gives one of `outputs`.
    let windows = sorted_windows(milp, scan);
    let mut gives: Vec<bool> = windows
        .iter()
        .map(|(_, _, given)| outputs.contains(given))
        .collect();
    // The read itself makes the scan read at least the reader's own widest
    // window (`window_terms`): the levels below it never hold when the read
    // is taken, so they count as the reader's own level does.
    if let Some(own) = reader_level(milp, scan, reader, &windows) {
        let at_own = gives[own];
        for level in &mut gives[..own] {
            *level = at_own;
        }
    }
    if gives.iter().all(|&gives| gives) {
        return Some(Some(link.variable.into()));
    }
    let scan_terms = milp.node_terms(milp.instance_owner(instance), scan);
    // Per run of levels `low..=high` giving one of them, `λ_low −
    // λ_{high+1}` (`λ_0 = x[i]`): the widest window read is in the run.
    let mut within_runs = Expression::from(0.0);
    let mut any = false;
    let mut level = 0;
    while level < gives.len() {
        if !gives[level] {
            level += 1;
            continue;
        }
        let low = level;
        while level < gives.len() && gives[level] {
            level += 1;
        }
        within_runs += scan_terms.get(low)?.indicator.clone();
        if let Some(above) = scan_terms.get(level) {
            within_runs -= above.indicator.clone();
        }
        any = true;
    }
    if !any {
        return Some(None);
    }
    Some(Some(
        milp.builder
            .and_at_least("t_producers (AND)", &[link.variable.into(), within_runs])
            .into(),
    ))
}

/// The level (in `windows`, [`sorted_windows`] of `scan`) of the widest
/// window `reader` reads `scan` at, as [`window_terms`] places its reads;
/// `None` when it reads a window `asap-cost` did not cost.
fn reader_level(
    milp: &SelectionMilp<'_>,
    scan: PhysicalCandidateId,
    reader: PhysicalCandidateId,
    windows: &[(ScanWindow, NodeNumbers, usize)],
) -> Option<usize> {
    let read_windows = milp.inputs.candidate_data[scan.0]
        .windows_per_reader
        .iter()
        .find(|(known, _)| *known == reader)
        .map(|(_, read)| read.clone())
        .unwrap_or_else(|| vec![ScanWindow::Unbounded]);
    read_windows
        .iter()
        .map(|window| windows.iter().position(|(known, _, _)| known == window))
        .collect::<Option<Vec<usize>>>()
        .and_then(|levels| levels.into_iter().max())
}

/// A windowed scan class's windows with their numbers and outputs,
/// narrowest first: the levels of [`window_terms`].
fn sorted_windows(
    milp: &SelectionMilp<'_>,
    scan: PhysicalCandidateId,
) -> Vec<(ScanWindow, NodeNumbers, usize)> {
    let mut windows: Vec<(ScanWindow, NodeNumbers, usize)> = milp.inputs.candidate_data[scan.0]
        .usage_per_window
        .iter()
        .map(|(window, usage, output)| (*window, NodeNumbers::of(usage), *output))
        .collect();
    windows.sort_by(|a, b| {
        (a.1.raw_kept_bytes, a.1.execution_cost, a.1.latency_ms)
            .partial_cmp(&(b.1.raw_kept_bytes, b.1.execution_cost, b.1.latency_ms))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    windows
}

/// Whether window `inner` lies within `outer`, so a scan read at both
/// reads `outer`.
fn within(inner: ScanWindow, outer: ScanWindow) -> bool {
    match (inner, outer) {
        (_, ScanWindow::Unbounded) => true,
        (ScanWindow::Unbounded, _) => false,
        (
            ScanWindow::Within {
                end_offset_ms: inner_end,
                width_ms: inner_width,
            },
            ScanWindow::Within {
                end_offset_ms: outer_end,
                width_ms: outer_width,
            },
        ) => {
            outer_end <= inner_end
                && outer_end.saturating_add(outer_width) >= inner_end.saturating_add(inner_width)
        }
    }
}

/// Terms of query-time scan instance `instance` whose usage depends on the
/// window its readers read: the narrowest window's numbers on `x[i]`, and
/// each wider level's increase on `λ[i, ℓ]`, where `λ[i, ℓ] ≥` every read
/// at level `ℓ` or wider (the scan reads the widest window read).
fn window_terms(
    milp: &mut SelectionMilp<'_>,
    instance: usize,
    class: PhysicalCandidateId,
) -> Vec<NodeTerm> {
    let data = &milp.inputs.candidate_data[class.0];
    let windows = sorted_windows(milp, class);
    let windows_per_reader = data.windows_per_reader.clone();
    if windows.windows(2).any(|pair| !within(pair[0].0, pair[1].0)) {
        milp.note_inexact("scan windows that are not nested");
    }
    // Each read's level: the widest window its reader may read.
    let reads = milp.reads_of_instance[instance].clone();
    let mut level_reads: Vec<Vec<Variable>> = vec![Vec::new(); windows.len()];
    for (owner, read) in reads {
        let reader = milp.owners[owner]
            .reads
            .iter()
            .find(|(_, variable)| *variable == read)
            .map(|((reader, _, _), _)| *reader);
        let read_windows: Vec<ScanWindow> = match reader {
            Some(reader) => windows_per_reader
                .iter()
                .find(|(known, _)| *known == reader)
                .map(|(_, read)| read.clone())
                .unwrap_or_else(|| vec![ScanWindow::Unbounded]),
            None => vec![ScanWindow::Unbounded],
        };
        if read_windows.len() > 1 {
            milp.note_inexact("a scan read at several windows by one reader (widest kept)");
        }
        let level = read_windows
            .iter()
            .map(|window| windows.iter().position(|(known, _, _)| known == window))
            .collect::<Option<Vec<usize>>>()
            .and_then(|levels| levels.into_iter().max());
        match level {
            Some(level) => level_reads[level].push(read),
            None => {
                // A window `asap-cost` cannot cost: no plan reads it so.
                milp.builder
                    .row("λ (scan window levels)", constraint!(read <= 0.0));
            }
        }
    }
    let mut terms = vec![NodeTerm {
        indicator: milp.active[instance].into(),
        numbers: windows.first().map_or_else(NodeNumbers::default, |w| w.1),
        plain: true,
        output: windows.first().map_or(0, |w| w.2),
    }];
    for level in 1..windows.len() {
        let (increase, negative) = windows[level].1.minus(windows[level - 1].1);
        if negative {
            milp.note_inexact("a wider scan window with a smaller number");
        }
        let at_least = milp.builder.continuous("λ (scan window levels)", Some(1.0));
        for &read in level_reads[level..].iter().flatten() {
            milp.builder
                .row("λ (scan window levels)", constraint!(at_least >= read));
        }
        terms.push(NodeTerm {
            indicator: at_least.into(),
            numbers: increase,
            plain: false,
            output: windows[level].2,
        });
    }
    terms
}

/// Each instance's compaction term for its readers' latency, ms, where
/// `plan_cost` charges one: `(k − 1) · states · merge time`, `k` the cores
/// its ingestion work takes. With at most one second of ingestion work per
/// second below it, `k = 1` and the term is 0; a larger subtree is not
/// encoded (counted as inexact).
pub(crate) fn compaction_ms(milp: &mut SelectionMilp<'_>) -> HashMap<usize, f64> {
    let inputs = milp.inputs;
    let mut compaction = HashMap::new();
    for instance in 0..milp.instances.len() {
        let class = milp.instances[instance].class;
        let candidate = &milp.physical.candidates[class.0];
        if candidate.timing != ExecutionTiming::IngestionTime {
            continue;
        }
        let most_work_ms: f64 = candidates_below(class, &inputs.producer_candidates_per_input)
            .into_iter()
            .filter_map(|below| inputs.candidate_data[below.0].usage.as_ref().ok())
            .map(|usage| usage.work_ms_per_execution)
            .sum();
        if most_work_ms > 1_000.0 {
            milp.note_inexact("compaction with more than one ingest worker (not encoded)");
            compaction.insert(instance, 0.0);
        }
    }
    compaction
}

/// How an instance's executions per second are encoded.
#[derive(Debug, Clone)]
enum InstanceRate {
    /// `rate · x[i]`: ingestion time (once per second), or readers that
    /// all run it at one rate.
    Fixed(f64),
    /// `Σ rate · u` over indicators `u ∈ [0, 1]`, each tight under the
    /// costs it carries: a materialized instance's schedule-group levels
    /// `z`, or a not-materialized one's consumers (`y`, `a`, or
    /// `AND(z_m, y_m)` for a materialized consumer `m`).
    Components(Vec<(Variable, f64)>),
}

/// Who may read a class at an owner boundary: a query owner, or the owners
/// of a class's instances.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum BoundaryReader {
    Query(usize),
    Class(PhysicalCandidateId),
}

/// The objective (cost model 1, module documentation) and its rows.
pub(crate) fn encode_objective(milp: &mut SelectionMilp<'_>) -> Result<(), SelectionMilpError> {
    let CostModel::UsageOverTime(unit_cost) = milp.cost_inputs.cost_model.clone() else {
        return Err(SelectionMilpError::UnsupportedCostModel("not cost model 1"));
    };
    let horizon = milp.cost_inputs.non_repeating_query_horizon_seconds;
    let cost = |error: asap_cost::analytical::AnalyticalCostError| {
        SelectionMilpError::Cost(error.to_string())
    };
    // Each query's own rate, with its range evaluation.
    let query_rate: Vec<f64> = milp
        .entries
        .iter()
        .map(|entry| evaluation_rate([(&entry.recurrence, entry.range_evaluation)], horizon))
        .collect::<Result<_, _>>()
        .map_err(cost)?;
    // Request fees: a fixed time per request, the same for every plan.
    let request_ms = milp.cost_inputs.measured_operation_costs.ns_per_evaluation * 1e-6;
    if request_ms > 0.0 {
        for entry in milp.entries {
            let requests = evaluation_rate([(&entry.recurrence, None)], horizon).map_err(cost)?;
            milp.request_cost_per_second += unit_cost.cost_per_cpu_op * request_ms
                / milp.cost_inputs.resource_speed.ms_per_cpu_op
                * requests;
        }
    }
    let rates = instance_rates(milp, &query_rate)?;
    let memory = unit_cost.cost_per_memory_byte_second;
    let mut objective: Vec<(Variable, f64)> = Vec::new();
    let mut charge = |expression: &Expression, coefficient: f64| {
        if coefficient != 0.0 {
            for (variable, factor) in expression.linear_coefficients() {
                objective.push((variable, factor * coefficient));
            }
        }
    };
    let mut held_time: HashMap<usize, (Variable, f64)> = HashMap::new();
    // Per instance at a varying rate, the execution cost of its nodes other
    // than its own plain term, and the most it can be.
    let mut region_execution: BTreeMap<usize, (Expression, f64)> = BTreeMap::new();
    let readers = boundary_readers(milp);
    let mut pure: HashMap<PhysicalCandidateId, bool> = HashMap::new();
    for owner in 0..milp.owners.len() {
        let instance = milp.instance_of_owner(owner);
        for class in milp.nodes_of_owner(owner) {
            let terms = milp.node_terms(owner, class);
            let candidate = &milp.physical.candidates[class.0];
            let (timing, materialization) = (candidate.timing, candidate.materialization);
            let own_class = instance.is_some_and(|i| milp.instances[i].class == class);
            for term in &terms {
                let numbers = term.numbers;
                charge(&term.indicator, numbers.standing_cost);
                // Execution.
                match instance.map(|i| (i, &rates[i])) {
                    None => charge(&term.indicator, numbers.execution_cost * query_rate[owner]),
                    Some((_, InstanceRate::Fixed(rate))) => {
                        charge(&term.indicator, numbers.execution_cost * rate)
                    }
                    Some((instance, InstanceRate::Components(components))) => {
                        if numbers.execution_cost == 0.0 {
                        } else if term.plain && own_class {
                            for &(component, rate) in components {
                                charge(&component.into(), numbers.execution_cost * rate);
                            }
                        } else {
                            let (sum, most) = region_execution
                                .entry(instance)
                                .or_insert_with(|| (Expression::from(0.0), 0.0));
                            *sum += numbers.execution_cost * term.indicator.clone();
                            *most += numbers.execution_cost;
                        }
                    }
                }
                // Held memory.
                if numbers.held_bytes <= 0.0 || timing != ExecutionTiming::QueryTime {
                    continue;
                }
                let per_byte_ms = memory * numbers.held_bytes / 1_000.0;
                let Some(instance) = instance else {
                    // A query owner's node: its working memory, for its
                    // query's latency at its rate.
                    let latency = milp.latency[owner];
                    let most = milp.latency_max_ms[owner];
                    let held = milp
                        .builder
                        .continuous("G (held for a query's latency)", None);
                    let indicator = term.indicator.clone();
                    milp.builder.row(
                        "G (held for a query's latency)",
                        constraint!(held >= latency - most * (1.0 - indicator)),
                    );
                    charge(&held.into(), per_byte_ms * query_rate[owner]);
                    continue;
                };
                let instance_class = milp.instances[instance].class;
                let instance_candidate = &milp.physical.candidates[instance_class.0];
                let materialized =
                    instance_candidate.materialization != Materialization::NotMaterialized;
                if own_class && materialization == Materialization::MatAtQuery(Medium::Memory)
                    || !own_class && materialized
                {
                    // Held for the longest reader's latency, once per
                    // execution of the instance.
                    let components = match &rates[instance] {
                        InstanceRate::Fixed(rate) => vec![(None, *rate)],
                        InstanceRate::Components(components) => components
                            .iter()
                            .map(|&(component, rate)| (Some(component), rate))
                            .collect(),
                    };
                    if !own_class && milp.inputs.queries_reaching[instance_class.0].len() > 1 {
                        milp.note_inexact(
                            "working memory below a shared materialization (longest reader, \
                             not the rate-weighted mean)",
                        );
                    }
                    // `H ≥ L[q] − L̄[q] (1 − a[q, i])`, with each query's own
                    // most latency `L̄[q]`; `H ≤ max_q L̄[q]`.
                    let (longest, most_held) = *held_time.entry(instance).or_insert_with(|| {
                        let longest = milp
                            .builder
                            .continuous("H (held time of an instance)", None);
                        let mut most_held: f64 = 0.0;
                        for &query in &milp.inputs.queries_reaching[instance_class.0] {
                            if let Some(&reach) = milp.reach.get(&(query, instance)) {
                                let latency = milp.latency[query];
                                let most = milp.latency_max_ms[query];
                                most_held = most_held.max(most);
                                milp.builder.row(
                                    "H (held time of an instance)",
                                    constraint!(longest >= latency - most * (1.0 - reach)),
                                );
                            }
                        }
                        (longest, most_held)
                    });
                    for (component, rate) in components {
                        // `held ≥ H − M (slack − Σ indicators)` over the
                        // component and the term (when not `x[i]` itself).
                        let mut both = Expression::from(0.0);
                        let mut slack = 0.0;
                        if let Some(component) = component {
                            both += component;
                            slack += 1.0;
                        }
                        if !(term.plain && own_class) {
                            both += term.indicator.clone();
                            slack += 1.0;
                        }
                        if slack == 0.0 {
                            charge(&longest.into(), per_byte_ms * rate);
                            continue;
                        }
                        let held = milp
                            .builder
                            .continuous("W (held time × schedule group)", None);
                        milp.builder.row(
                            "W (held time × schedule group)",
                            constraint!(held >= longest - most_held * (slack - both)),
                        );
                        charge(&held.into(), per_byte_ms * rate);
                    }
                } else {
                    // A not-materialized instance or a node below one: each
                    // consumer query holds it for its latency at its rate.
                    let is_pure = *pure
                        .entry(instance_class)
                        .or_insert_with(|| consumers(milp, &readers, instance_class).1.is_empty());
                    if !is_pure {
                        milp.note_inexact(
                            "working memory of a node a materialized instance also consumes",
                        );
                    }
                    for &query in &milp.inputs.queries_reaching[instance_class.0] {
                        let Some(&reach) = milp.reach.get(&(query, instance)) else {
                            continue;
                        };
                        let latency = milp.latency[query];
                        let most = milp.latency_max_ms[query];
                        let held = milp
                            .builder
                            .continuous("G (held for a query's latency)", None);
                        let both = Expression::from(reach) + term.indicator.clone();
                        milp.builder.row(
                            "G (held for a query's latency)",
                            constraint!(held >= latency - most * (2.0 - both)),
                        );
                        charge(&held.into(), per_byte_ms * query_rate[query]);
                    }
                }
            }
        }
    }
    // A region's execution at its instance's varying rate:
    // `V ≥ E_i − Ē_i (1 − u)` per rate component `u`.
    for (instance, (sum, most)) in region_execution {
        let InstanceRate::Components(components) = &rates[instance] else {
            continue;
        };
        let region = milp
            .builder
            .continuous("V (region execution at a rate component)", None);
        milp.builder.row(
            "V (region execution at a rate component)",
            constraint!(region >= sum),
        );
        // `V_u ≥ E_i − Ē_i (x[i] − u)`: `E_i ≤ Ē_i x[i]` and `u ≤ x[i]` at
        // every plan, so this is `E_i · u`, tighter than `(1 − u)` at a
        // fractional `x[i]`. An active instance runs at its slowest
        // component's rate at least: `Σ rate · V_u ≥ min rate · E_i`.
        let active = milp.active[instance];
        let slowest = components
            .iter()
            .map(|&(_, rate)| rate)
            .fold(f64::INFINITY, f64::min);
        let mut at_rates = Expression::from(0.0);
        for &(component, rate) in components {
            let at_rate = milp
                .builder
                .continuous("V (region execution at a rate component)", None);
            milp.builder.row(
                "V (region execution at a rate component)",
                constraint!(at_rate >= region - most * (active - component)),
            );
            charge(&at_rate.into(), rate);
            at_rates += rate * at_rate;
        }
        if slowest.is_finite() && slowest > 0.0 {
            milp.builder.row(
                "V (slowest rate)",
                constraint!(at_rates >= slowest * region),
            );
        }
    }
    raw_retention(milp, unit_cost.cost_per_disk_byte_second, &mut charge);
    milp.objective_terms = objective;
    Ok(())
}

/// The boundary readers of each class: the query owners and the classes
/// whose instance regions may read it.
fn boundary_readers(
    milp: &SelectionMilp<'_>,
) -> HashMap<PhysicalCandidateId, BTreeSet<BoundaryReader>> {
    let mut readers: HashMap<PhysicalCandidateId, BTreeSet<BoundaryReader>> = HashMap::new();
    for (query, region) in milp.inputs.query_owners.iter().enumerate() {
        for &(_, _, read) in &region.boundary_reads {
            readers
                .entry(read)
                .or_default()
                .insert(BoundaryReader::Query(query));
        }
        for &(_, read) in &region.output_reads {
            readers
                .entry(read)
                .or_default()
                .insert(BoundaryReader::Query(query));
        }
    }
    for owners in &milp.inputs.instance_owners {
        for &(_, _, read) in &owners.region.boundary_reads {
            readers
                .entry(read)
                .or_default()
                .insert(BoundaryReader::Class(owners.class));
        }
    }
    readers
}

fn materialization_of(milp: &SelectionMilp<'_>, class: PhysicalCandidateId) -> Materialization {
    milp.physical.candidates[class.0].materialization
}

/// The consumers of a not-materialized shareable `class`, as
/// `plan_cost`'s `computations` counts them: the queries and the
/// materialized classes reaching it through not-materialized classes.
fn consumers(
    milp: &SelectionMilp<'_>,
    readers: &HashMap<PhysicalCandidateId, BTreeSet<BoundaryReader>>,
    class: PhysicalCandidateId,
) -> (BTreeSet<usize>, BTreeSet<PhysicalCandidateId>) {
    let mut queries = BTreeSet::new();
    let mut materialized = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut stack = vec![class];
    while let Some(class) = stack.pop() {
        if !seen.insert(class) {
            continue;
        }
        for reader in readers.get(&class).into_iter().flatten() {
            match *reader {
                BoundaryReader::Query(query) => {
                    queries.insert(query);
                }
                BoundaryReader::Class(reader) => {
                    if materialization_of(milp, reader) == Materialization::NotMaterialized {
                        stack.push(reader);
                    } else {
                        materialized.insert(reader);
                    }
                }
            }
        }
    }
    (queries, materialized)
}

/// Each instance's executions per second (module documentation).
// Instance indexes also key `reach`, `active` and the owners.
#[allow(clippy::needless_range_loop)]
fn instance_rates(
    milp: &mut SelectionMilp<'_>,
    query_rate: &[f64],
) -> Result<Vec<InstanceRate>, SelectionMilpError> {
    let horizon = milp.cost_inputs.non_repeating_query_horizon_seconds;
    let cost = |error: asap_cost::analytical::AnalyticalCostError| {
        SelectionMilpError::Cost(error.to_string())
    };
    let mut rates: Vec<Option<InstanceRate>> = vec![None; milp.instances.len()];
    // Materialized instances first: their rates depend on reach only.
    for instance in 0..milp.instances.len() {
        let class = milp.instances[instance].class;
        let candidate = &milp.physical.candidates[class.0];
        let per_request = match candidate.materialization {
            Materialization::NotMaterialized => continue,
            Materialization::MatAtIngestion(_) => {
                rates[instance] = Some(InstanceRate::Fixed(1.0));
                continue;
            }
            Materialization::MatAtQuery(_) => false,
            Materialization::MatAtQueryKeptAcrossEvaluations(_) => true,
        };
        let readers: Vec<usize> = milp.inputs.queries_reaching[class.0]
            .iter()
            .copied()
            .filter(|query| milp.reach.contains_key(&(*query, instance)))
            .collect();
        let demand = |query: usize| {
            let entry = &milp.entries[query];
            (
                &entry.recurrence,
                if per_request {
                    None
                } else {
                    entry.range_evaluation
                },
            )
        };
        if unknown_phase_joins_several(readers.iter().map(|&query| &milp.entries[query].recurrence))
        {
            milp.note_inexact("schedule: an unknown phase among several known ones");
        }
        let groups =
            schedule_groups(readers.iter().map(|&query| demand(query)), horizon).map_err(cost)?;
        // Per group, its members with their own rates, and its levels.
        let mut levels_per_group: Vec<GroupLevels> = Vec::new();
        for group in groups {
            let members: Vec<usize> = group
                .readers
                .iter()
                .map(|&position| readers[position])
                .collect();
            let alone: Vec<f64> = members
                .iter()
                .map(|&query| evaluation_rate([demand(query)], horizon))
                .collect::<Result<_, _>>()
                .map_err(cost)?;
            let fastest = alone.iter().copied().fold(0.0, f64::max);
            let full = group.evaluations_per_second;
            // Levels of the members' own rates; the group runs at its
            // fastest member's when that is its rate.
            let mut levels: Vec<f64> = alone.clone();
            if (full - fastest).abs() > 1e-9 * full.abs().max(1e-12) {
                milp.note_inexact("schedule: a group's rate is not its fastest reader's");
                levels = vec![full];
            }
            levels.sort_by(f64::total_cmp);
            levels.dedup();
            levels_per_group.push((members.into_iter().zip(alone).collect(), levels));
        }
        // One group at one level: every reader runs it at that rate.
        if let [(_, levels)] = levels_per_group.as_slice() {
            if let [level] = levels.as_slice() {
                rates[instance] = Some(InstanceRate::Fixed(*level));
                continue;
            }
        }
        let mut components = Vec::new();
        for (members, levels) in levels_per_group {
            let mut below = 0.0;
            for &level in &levels {
                let z = milp.builder.continuous("z (schedule groups)", Some(1.0));
                for &(query, own) in &members {
                    if own >= level || levels.len() == 1 {
                        let reach = milp.reach[&(query, instance)];
                        milp.builder
                            .row("z (schedule groups)", constraint!(z >= reach));
                    }
                }
                components.push((z, level - below));
                below = level;
            }
        }
        rates[instance] = Some(InstanceRate::Components(components));
    }
    // Not-materialized instances: once per consumer.
    let readers = boundary_readers(milp);
    let mut consumers_of_class: HashMap<
        PhysicalCandidateId,
        (BTreeSet<usize>, BTreeSet<PhysicalCandidateId>),
    > = HashMap::new();
    for instance in 0..milp.instances.len() {
        if rates[instance].is_some() {
            continue;
        }
        let class = milp.instances[instance].class;
        consumers_of_class
            .entry(class)
            .or_insert_with(|| consumers(milp, &readers, class));
    }
    // `y[c, i]` for the instances with a materialized consumer.
    let mut through: HashMap<(Consumer, usize), Variable> = HashMap::new();
    for instance in 0..milp.instances.len() {
        if rates[instance].is_some() {
            continue;
        }
        let class = milp.instances[instance].class;
        let (queries, materialized) = &consumers_of_class[&class];
        if materialized.is_empty() {
            continue;
        }
        for &query in queries {
            let y = milp
                .builder
                .continuous("y (consumers through not-materialized nodes)", Some(1.0));
            through.insert((Consumer::Query(query), instance), y);
        }
        for class in materialized {
            for &consumer in milp.instances_of_candidate.get(class).into_iter().flatten() {
                let y = milp
                    .builder
                    .continuous("y (consumers through not-materialized nodes)", Some(1.0));
                through.insert((Consumer::Instance(consumer), instance), y);
            }
        }
    }
    // y from below: direct reads, then through not-materialized instances.
    let mut readers_of_instance: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); milp.instances.len()];
    for reader in 0..milp.instances.len() {
        for &((_, _, read), _) in &milp.owners[milp.instance_owner(reader)].reads {
            readers_of_instance[read].insert(reader);
        }
    }
    let consumer_of =
        |milp: &SelectionMilp<'_>, consumer: Consumer, instance: usize| -> Option<Variable> {
            through
                .get(&(consumer, instance))
                .copied()
                .or_else(|| match consumer {
                    Consumer::Query(query) => milp.reach.get(&(query, instance)).copied(),
                    Consumer::Instance(_) => None,
                })
        };
    let keys: Vec<(Consumer, usize)> = through.keys().copied().collect();
    for (consumer, instance) in keys {
        let y = through[&(consumer, instance)];
        let reading_owner = match consumer {
            Consumer::Query(query) => query,
            Consumer::Instance(reader) => milp.instance_owner(reader),
        };
        for &(owner, read) in &milp.reads_of_instance[instance].clone() {
            if owner == reading_owner {
                milp.builder.row(
                    "y (consumers through not-materialized nodes)",
                    constraint!(y >= read),
                );
            }
        }
        for &reader in &readers_of_instance[instance] {
            // Through not-materialized instances only.
            if rates[reader].is_some() || reader == instance {
                continue;
            }
            let Some(above) = consumer_of(milp, consumer, reader) else {
                continue;
            };
            let owner = milp.instance_owner(reader);
            let reads: Vec<Variable> = milp.owners[owner]
                .reads
                .iter()
                .filter(|((_, _, read), _)| *read == instance)
                .map(|(_, variable)| *variable)
                .collect();
            // `y ≥ above ∧ read`, as `above + read − x[reader]` (both are at
            // most `x[reader]` at every plan).
            let active = milp.active[reader];
            for read in reads {
                milp.builder.row(
                    "y (consumers through not-materialized nodes)",
                    constraint!(y >= above + read - active),
                );
            }
        }
    }
    for instance in 0..milp.instances.len() {
        if rates[instance].is_some() {
            continue;
        }
        let class = milp.instances[instance].class;
        let (queries, materialized) = consumers_of_class[&class].clone();
        if materialized.is_empty() && queries.len() == 1 {
            // Its one consumer query's rate.
            let query = *queries.first().expect("one");
            rates[instance] = Some(InstanceRate::Fixed(query_rate[query]));
            continue;
        }
        let mut components = Vec::new();
        for &query in &queries {
            if let Some(y) = consumer_of(milp, Consumer::Query(query), instance) {
                components.push((y, query_rate[query]));
            }
        }
        for class in &materialized {
            for &consumer in milp.instances_of_candidate.get(class).into_iter().flatten() {
                let Some(&y) = through.get(&(Consumer::Instance(consumer), instance)) else {
                    continue;
                };
                match rates[consumer].clone() {
                    Some(InstanceRate::Fixed(rate)) => components.push((y, rate)),
                    Some(InstanceRate::Components(consumer_components)) => {
                        for (z, rate) in consumer_components {
                            let both = milp.builder.and_at_least(
                                "P (materialized consumers' rates, AND)",
                                &[z.into(), y.into()],
                            );
                            components.push((both, rate));
                        }
                    }
                    None => {}
                }
            }
        }
        rates[instance] = Some(InstanceRate::Components(components));
    }
    Ok(rates
        .into_iter()
        .map(|rate| rate.expect("every instance has a rate"))
        .collect())
}

/// A consumer of a not-materialized instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Consumer {
    Query(usize),
    Instance(usize),
}

/// Whether some interval has readers of unknown phase and readers of two
/// or more known phases: which known group the unknown ones join then
/// depends on which readers are present.
fn unknown_phase_joins_several<'r>(recurrences: impl Iterator<Item = &'r QueryRecurrence>) -> bool {
    let mut phases: BTreeMap<u32, (bool, BTreeSet<u64>)> = BTreeMap::new();
    for recurrence in recurrences {
        match recurrence {
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(interval)) => {
                phases.entry(interval.0).or_default().0 = true;
            }
            QueryRecurrence::Repeated(RepeatedDemand::FixedIntervalAt {
                interval,
                evaluation_phase,
            }) => {
                phases
                    .entry(interval.0)
                    .or_default()
                    .1
                    .insert(evaluation_phase.0 % u64::from(interval.0.max(1)));
            }
            _ => {}
        }
    }
    phases
        .values()
        .any(|(unknown, known)| *unknown && known.len() > 1)
}

/// Raw retention: per source, the largest raw data a query-time scan of
/// the plan keeps, by levels.
fn raw_retention(
    milp: &mut SelectionMilp<'_>,
    disk: f64,
    charge: &mut impl FnMut(&Expression, f64),
) {
    // Per source, each scan instance's (cumulative bytes, indicator).
    let mut kept: BTreeMap<String, Vec<(f64, Expression)>> = BTreeMap::new();
    for instance in 0..milp.instances.len() {
        let class = milp.instances[instance].class;
        let copy = class;
        let candidate = &milp.physical.candidates[copy.0];
        let Operator::NonASAP(NonASAPOp::Scan { source, .. }) =
            &milp.physical.physical_node(copy).operator
        else {
            continue;
        };
        if candidate.timing != ExecutionTiming::QueryTime {
            continue;
        }
        let source = source_name(source).to_string();
        let owner = milp.instance_owner(instance);
        // A scan's terms are its narrowest window and each wider level's
        // increase (`window_terms`), or its one term.
        let mut bytes = 0.0;
        for term in milp.node_terms(owner, class) {
            bytes += term.numbers.raw_kept_bytes;
            if bytes > 0.0 {
                kept.entry(source.clone())
                    .or_default()
                    .push((bytes, term.indicator));
            }
        }
    }
    for (_, scans) in kept {
        let mut levels: Vec<f64> = scans.iter().map(|(bytes, _)| *bytes).collect();
        levels.sort_by(f64::total_cmp);
        levels.dedup();
        let mut below = 0.0;
        for level in levels {
            let at_least = milp
                .builder
                .continuous("v (raw retention levels)", Some(1.0));
            for (bytes, indicator) in &scans {
                if *bytes >= level {
                    let indicator = indicator.clone();
                    milp.builder.row(
                        "v (raw retention levels)",
                        constraint!(at_least >= indicator),
                    );
                }
            }
            charge(&at_least.into(), disk * (level - below));
            below = level;
        }
    }
}
