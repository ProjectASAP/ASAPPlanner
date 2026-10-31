//! #1095 §3: the cost model 1 objective of the group selection MILP. Every
//! number is `asap-cost`'s: a candidate's usage per execution and standing
//! cost ([`CandidateUsage`], from [`asap_cost::node_usage`] in
//! [`crate::groups`]), the schedule groups
//! ([`asap_cost::execution_demand::schedule_groups`]) and rates
//! ([`asap_cost::execution_demand::evaluation_rate`]) and the unit costs of
//! [`asap_cost::ResourceUnitCost`]. This module only turns them into
//! coefficients and the auxiliary rows that linearize them.
//!
//! A **run** is a candidate computed in an execution context (`p[ctx, c]`),
//! or a shareable candidate's own run (`x[m]`, keyed by its own context).
//! A run's usage is one or more [`RunTerm`]s, each an indicator and the
//! numbers that count when it is 1:
//! - one term on the run's own indicator when its usage is plan
//!   independent;
//! - per producer choice, when its usage depends on which candidate of an
//!   input's slot the context picks (a summary state's configuration, a
//!   scan's window): `AND(run, p[ctx, c'], producer gives that output)`;
//! - for a query-time scan read at several windows in one context: one
//!   term per set `S` of window kinds read, `δ_S`, the exact indicator that
//!   exactly the kinds in `S` are read (from `σ_W`, the exact OR of the
//!   reads at kind `W`), costed at the union of `S`'s windows
//!   (`PhysicalASAPDAG::scan_window`). This needs no monotonicity: every
//!   union is costed by `asap-cost` itself.
//!
//! **Executions per second** of a run: `rate(q)` in query context `q`; 1 in
//! `I` and for an ingestion-time shareable candidate; for a query-time
//! shareable candidate `m` and the runs in its context, `Σ rate · z[m, G,
//! ℓ]` over its schedule groups and their levels, `z ≥ a[q, m]` for each
//! reader at that level or faster (a group's rate is its fastest reader's).
//! A context's runs other than `m`'s plain own term are summed once, `E[m]
//! ≥ Σ cost · indicator`, and charged per component, `V ≥ E[m] − Ē[m] (x[m]
//! − z)`, with `Σ rate · V ≥ (slowest rate) · E[m]`.
//!
//! **Held memory**: working memory of a run in query context `q` for `q`'s
//! latency at `q`'s rate (`H_query ≥ L[q] − L̄[q] (1 − indicator)`); `m`'s
//! `MatAtQuery(Memory)` result and the working memory of the runs in its
//! context until `m`'s slowest reader finishes (`H[m] ≥ L[q] − L̄[q] (1 −
//! a[q, m])`, then `W ≥ H[m] − H̄[m] (slack − Σ indicators)` per rate
//! component).
//!
//! **Raw retention**: per source, levels at each raw-data size a query-time
//! scan run may keep, `v[src, ℓ] ≥` each scan term keeping at least that
//! much; charged `Δbytes · unit disk cost`.
//!
//! **Pane retention**: a kept pane chain's usage at the fewest panes any
//! reader needs on `x[m]`, and each larger retention's increase on `π[m,
//! ℓ] ≥` the reach (as `plan_cost` counts it: through query-time runs) of
//! each query needing at least that many panes.
//!
//! Terms `plan_cost` defines differently from the above are counted in
//! [`SelectionMilp::inexact`] by reason.
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

use crate::groups::{
    CandidateUsage, ContextKind, ResultSlot, RunKind, UsageAndOutput, UsageContext,
};

use super::model::{Reader, Scope, SelectionMilp, SelectionMilpError};

use crate::groups::MAX_WINDOW_KINDS;

/// The numbers of one run `asap-cost` computed, as the objective and the
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
    pub(crate) fn of(usage: &CandidateUsage) -> Self {
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

/// One term of a run: its numbers count, and it gives its readers output
/// `output`, when `indicator` is 1. At integral binaries exactly one term
/// of a used run is 1, except a kept pane chain's retention increases,
/// which add to its base term.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RunTerm {
    pub(crate) indicator: Expression,
    pub(crate) numbers: NodeNumbers,
    /// Whether `indicator` is the run's own variable (`p[ctx, c]` or
    /// `x[m]`).
    pub(crate) plain: bool,
    /// Its output (an index below `CandidateData::output_count`).
    pub(crate) output: usize,
}

/// Run `(context, candidate)`'s terms (module documentation). Empty when
/// the run does not exist.
pub(crate) fn run_terms(
    milp: &mut SelectionMilp<'_>,
    context: usize,
    candidate: PhysicalCandidateId,
) -> Vec<RunTerm> {
    let Some(run) = milp.run_indicator(context, candidate) else {
        return Vec::new();
    };
    let data = &milp.inputs.candidate_data[candidate.0];
    let Ok(base) = &data.usage else {
        return Vec::new();
    };
    let base = *base;
    for usage_context in data.usage_depends_on.clone() {
        match usage_context {
            UsageContext::ProducerOnACycle => milp.note_inexact("usage: a producer on a cycle"),
            UsageContext::TooManyContexts => milp.note_inexact("usage: too many contexts"),
            UsageContext::ProducerOutput
            | UsageContext::ScanWindowOfReader
            | UsageContext::PaneRetentionOfReachingQueries
            | UsageContext::UncostableContext => {}
        }
    }
    if milp.inputs.candidate_data[candidate.0].producer_usage_ambiguous {
        milp.note_inexact("usage: a producer's output depends on its own producers");
    }
    if !milp.inputs.candidate_data[candidate.0]
        .usage_per_window
        .is_empty()
    {
        if let Some(terms) = window_terms(milp, context, candidate, run) {
            return terms;
        }
    }
    if !milp.inputs.candidate_data[candidate.0]
        .inputs_changing_usage
        .is_empty()
    {
        return producer_terms(milp, context, candidate, run);
    }
    let mut terms = vec![RunTerm {
        indicator: run.into(),
        numbers: NodeNumbers::of(&base),
        plain: true,
        output: 0,
    }];
    if !milp.inputs.candidate_data[candidate.0]
        .usage_per_retained_panes
        .is_empty()
    {
        pane_retention_terms(milp, context, candidate, &mut terms);
    }
    terms
}

/// The indicator that, in `context`, the producer `producer` of a reader is
/// read and gives one of `outputs` (`None`: any output): `p[ctx, c']`, or
/// `AND(p[ctx, c'], t)` with `t ≥` the producer run's terms giving them.
/// `None` when the producer never gives them.
fn producer_gives(
    milp: &mut SelectionMilp<'_>,
    context: usize,
    producer: PhysicalCandidateId,
    outputs: Option<&BTreeSet<usize>>,
) -> Option<Expression> {
    let read = *milp.choice[context].get(&producer)?;
    let Some(outputs) = outputs else {
        return Some(read.into());
    };
    // The producer's run: computed here, its own run, or its run in `I`.
    let producer_run = match milp.kind(producer) {
        _ if milp.computes(context, producer) => context,
        RunKind::Shareable => milp.context_of(producer)?,
        RunKind::Ingestion => milp.ingestion_context(),
        RunKind::PerContext => return None,
    };
    if milp.terms_in_progress.contains(&(producer_run, producer)) {
        milp.note_inexact("usage: a producer's output on a cycle of links (every output kept)");
        return Some(read.into());
    }
    let producer_terms = milp.run_terms(producer_run, producer);
    if producer_terms
        .iter()
        .all(|term| outputs.contains(&term.output))
    {
        return Some(read.into());
    }
    let gives: Vec<&RunTerm> = producer_terms
        .iter()
        .filter(|term| outputs.contains(&term.output))
        .collect();
    if gives.is_empty() {
        return None;
    }
    let key = (
        producer_run,
        producer,
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
    if milp.computes(context, producer) {
        return Some(gives.into());
    }
    Some(
        milp.builder
            .and_at_least("t_producers (AND)", &[read.into(), gives.into()])
            .into(),
    )
}

/// A producer choice with the outputs it stands for (`None`: any output).
type OutputsOfProducer = (PhysicalCandidateId, Option<BTreeSet<usize>>);

/// Terms of a run whose usage or output depends on its producers (and
/// their outputs): one per choice, on `AND(run, producer gives)` over the
/// changing inputs.
fn producer_terms(
    milp: &mut SelectionMilp<'_>,
    context: usize,
    candidate: PhysicalCandidateId,
    run: Variable,
) -> Vec<RunTerm> {
    milp.terms_in_progress.insert((context, candidate));
    let data = &milp.inputs.candidate_data[candidate.0];
    let changing = data.inputs_changing_usage.clone();
    let base = *data.usage.as_ref().expect("costed");
    let same = |known: &Option<UsageAndOutput>, gives: &Option<UsageAndOutput>| match (known, gives)
    {
        (Some(known), Some(gives)) => known == gives,
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
    let producers_here: Vec<BTreeSet<PhysicalCandidateId>> = changing
        .iter()
        .map(|&input| {
            milp.inputs.producer_candidates_per_input[candidate.0][input]
                .iter()
                .copied()
                .filter(|producer| milp.choice[context].contains_key(producer))
                .filter(|producer| milp.slot(*producer) != milp.slot(candidate))
                .collect()
        })
        .collect();
    let mut covered: Vec<BTreeSet<PhysicalCandidateId>> = vec![BTreeSet::new(); changing.len()];
    let mut terms = Vec::new();
    for (choices, gives) in &table {
        let mut per_input: Vec<Expression> = Vec::with_capacity(choices.len());
        for (position, (producer, outputs)) in choices.iter().enumerate() {
            if !producers_here[position].contains(producer) {
                break;
            }
            covered[position].insert(*producer);
            match producer_gives(milp, context, *producer, outputs.as_ref()) {
                Some(indicator) => per_input.push(indicator),
                None => break,
            }
        }
        if per_input.len() < choices.len() {
            continue;
        }
        let mut all: Vec<Expression> = vec![run.into()];
        all.extend(per_input);
        // A multi-slot input reads the choice's slot only through its link.
        for (position, (producer, _)) in choices.iter().enumerate() {
            if let Some(links) = milp
                .slot_links
                .get(&(context, candidate, changing[position]))
            {
                let slot = milp.slot(*producer);
                if let Some(&(_, link)) = links.iter().find(|(at, _)| *at == slot) {
                    all.push(link.into());
                }
            }
        }
        let count = all.len() as f64;
        match gives {
            None => {
                let sum: Expression = all.into_iter().sum();
                milp.builder
                    .row("t_producers (AND)", constraint!(sum <= count - 1.0));
            }
            Some((usage, output)) => {
                let indicator = milp.builder.and_at_least("t_producers (AND)", &all);
                terms.push(RunTerm {
                    indicator: indicator.into(),
                    numbers: NodeNumbers::of(usage),
                    plain: false,
                    output: *output,
                });
            }
        }
    }
    // A producer missing from the table: its reads carry the first costed
    // usage.
    let mut missing = false;
    for (position, producers) in producers_here.iter().enumerate() {
        for &producer in producers {
            if covered[position].contains(&producer) {
                continue;
            }
            missing = true;
            let read = milp.choice[context][&producer];
            let indicator = milp
                .builder
                .and_at_least("t_producers (AND)", &[run.into(), read.into()]);
            terms.push(RunTerm {
                indicator: indicator.into(),
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
        milp.builder
            .row("t_producers (AND)", constraint!(run <= 0.0));
    }
    milp.terms_in_progress.remove(&(context, candidate));
    terms
}

/// The union of `windows`, as [`PhysicalASAPDAG::scan_window`] computes it
/// for one scan read at each: the nearest end and the farthest start.
pub(crate) fn window_union(windows: impl IntoIterator<Item = ScanWindow>) -> ScanWindow {
    let mut near = i64::MAX;
    let mut far = i64::MIN;
    for window in windows {
        match window {
            ScanWindow::Unbounded => return ScanWindow::Unbounded,
            ScanWindow::Within {
                end_offset_ms,
                width_ms,
            } => {
                near = near.min(end_offset_ms);
                far = far.max(end_offset_ms.saturating_add(width_ms));
            }
        }
    }
    if near > far {
        return ScanWindow::Unbounded;
    }
    ScanWindow::Within {
        end_offset_ms: near,
        width_ms: far.saturating_sub(near),
    }
}

/// The window a scan run read at `kinds` scans, as `asap_cost::plan_cost`
/// prices it: their union ([`window_union`]) when every kind is bounded;
/// with an unbounded read (a reader that is not a time range, a pane chain
/// or a time shift, or a read as an output), the trailing window of the
/// widest bounded kind (`plan_cost`'s `widest_window_read_ms`), or
/// `Unbounded` when there is none.
pub(crate) fn scan_window_read_at(
    kinds: impl IntoIterator<Item = ScanWindow> + Clone,
) -> ScanWindow {
    let unbounded = kinds
        .clone()
        .into_iter()
        .any(|kind| kind == ScanWindow::Unbounded);
    if !unbounded {
        return window_union(kinds);
    }
    let widest = kinds
        .into_iter()
        .filter_map(|kind| match kind {
            ScanWindow::Within { width_ms, .. } => Some(width_ms),
            ScanWindow::Unbounded => None,
        })
        .max();
    match widest {
        Some(width_ms) => ScanWindow::Within {
            end_offset_ms: 0,
            width_ms,
        },
        None => ScanWindow::Unbounded,
    }
}

/// The windows reader `reader` reads scan `scan` at (one per path through
/// time shifts); `Unbounded` when unknown.
fn reader_windows(
    milp: &SelectionMilp<'_>,
    scan: PhysicalCandidateId,
    reader: PhysicalCandidateId,
) -> Vec<ScanWindow> {
    milp.inputs.candidate_data[scan.0]
        .windows_per_reader
        .iter()
        .find(|(known, _)| *known == reader)
        .map(|(_, read)| read.clone())
        .unwrap_or_else(|| vec![ScanWindow::Unbounded])
}

/// Terms of query-time scan run `(context, scan)` whose usage depends on
/// the window its readers read (module documentation). `None` when its
/// readers read one window kind and it has no other dependence handled
/// here: the caller then uses its other terms.
fn window_terms(
    milp: &mut SelectionMilp<'_>,
    context: usize,
    scan: PhysicalCandidateId,
    run: Variable,
) -> Option<Vec<RunTerm>> {
    if let Some(cached) = milp.window_cache.get(&(context, scan)) {
        return cached.clone();
    }
    // Each read: (reading context, window kind, reader's indicator).
    let mut reads: Vec<(usize, ScanWindow, Expression)> = Vec::new();
    let reading_contexts: Vec<usize> = if milp.kind(scan) == RunKind::Shareable {
        (0..milp.choice.len())
            .filter(|&other| milp.choice[other].contains_key(&scan))
            .collect()
    } else {
        vec![context]
    };
    for &reading in &reading_contexts {
        for reader in milp.readers[reading]
            .get(&scan)
            .cloned()
            .unwrap_or_default()
        {
            let (window, indicator) = match reader {
                Reader::Candidate(reader, _) => {
                    let windows = reader_windows(milp, scan, reader);
                    if windows.len() > 1 {
                        milp.note_inexact("a scan read at several windows by one reader (union)");
                    }
                    (
                        window_union(windows),
                        Expression::from(milp.choice[reading][&reader]),
                    )
                }
                Reader::Output(_) => (ScanWindow::Unbounded, Expression::from(1.0)),
                Reader::OwnInput(_) => {
                    let ContextKind::Shared(own) = milp.inputs.contexts[reading].kind else {
                        continue;
                    };
                    // `m` itself reads the scan: at its own window.
                    (
                        window_union(reader_windows(milp, scan, own)),
                        Expression::from(milp.selected[&own]),
                    )
                }
            };
            reads.push((reading, window, indicator));
        }
    }
    let mut kinds: Vec<ScanWindow> = Vec::new();
    for (_, window, _) in &reads {
        if !kinds.contains(window) {
            kinds.push(*window);
        }
    }
    let usage_at = |milp: &SelectionMilp<'_>, window: ScanWindow| {
        let found = milp.inputs.candidate_data[scan.0]
            .usage_per_window
            .iter()
            .find(|(known, _, _)| *known == window)
            .map(|&(_, usage, output)| (NodeNumbers::of(&usage), output));
        if found.is_none() {
            super::warm_start::debug(|| {
                format!(
                    "scan {} has no usage at {window:?}; costed at {:?}",
                    scan.0,
                    milp.inputs.candidate_data[scan.0]
                        .usage_per_window
                        .iter()
                        .map(|(w, _, _)| *w)
                        .collect::<Vec<_>>()
                )
            });
        }
        found
    };
    let terms = match kinds.len() {
        0 => None,
        1 => match usage_at(milp, scan_window_read_at([kinds[0]])) {
            Some((numbers, output)) => Some(vec![RunTerm {
                indicator: run.into(),
                numbers,
                plain: true,
                output,
            }]),
            None => {
                // `asap-cost` cannot cost the scan so: no plan reads it so.
                milp.builder
                    .row("δ (scan window sets)", constraint!(run <= 0.0));
                Some(Vec::new())
            }
        },
        count if count <= MAX_WINDOW_KINDS => {
            // `σ_W`: the exact OR, over the reading contexts, of "the
            // context reads the scan and a reader at kind `W` is used"
            // (`w`, exact from both sides at integral binaries).
            let mut sigma = Vec::with_capacity(count);
            for kind in &kinds {
                let mut per_context: Vec<Variable> = Vec::new();
                for &reading in &reading_contexts {
                    let at_kind: Vec<&Expression> = reads
                        .iter()
                        .filter(|(at, window, _)| *at == reading && window == kind)
                        .map(|(_, _, indicator)| indicator)
                        .collect();
                    if at_kind.is_empty() {
                        continue;
                    }
                    let read_here = milp.choice[reading][&scan];
                    let used = milp.builder.continuous("σ (scan window kinds)", Some(1.0));
                    let mut upper = Expression::from(0.0);
                    for indicator in at_kind {
                        let indicator = indicator.clone();
                        upper += indicator.clone();
                        milp.builder.row(
                            "σ (scan window kinds)",
                            constraint!(used >= indicator + read_here - 1.0),
                        );
                    }
                    milp.builder
                        .row("σ (scan window kinds)", constraint!(used <= read_here));
                    milp.builder
                        .row("σ (scan window kinds)", constraint!(used <= upper));
                    per_context.push(used);
                }
                let or = match per_context.as_slice() {
                    [one] => *one,
                    many => {
                        let or = milp.builder.continuous("σ (scan window kinds)", Some(1.0));
                        let mut upper = Expression::from(0.0);
                        for &used in many {
                            milp.builder
                                .row("σ (scan window kinds)", constraint!(or >= used));
                            upper += used;
                        }
                        milp.builder
                            .row("σ (scan window kinds)", constraint!(or <= upper));
                        or
                    }
                };
                sigma.push(or);
            }
            let mut terms = Vec::new();
            for set in 1usize..(1 << count) {
                let window = scan_window_read_at(
                    (0..count)
                        .filter(|bit| set & (1 << bit) != 0)
                        .map(|bit| kinds[bit]),
                );
                let members = set.count_ones() as f64;
                let mut sum = Expression::from(0.0);
                for (bit, &or) in sigma.iter().enumerate() {
                    if set & (1 << bit) != 0 {
                        sum += or;
                    } else {
                        sum -= or;
                    }
                }
                let Some((numbers, output)) = usage_at(milp, window) else {
                    // `asap-cost` cannot cost the scan at this union: no
                    // plan reads exactly these kinds.
                    milp.builder
                        .row("δ (scan window sets)", constraint!(sum <= members - 1.0));
                    continue;
                };
                let exactly = milp.builder.continuous("δ (scan window sets)", Some(1.0));
                milp.builder.row(
                    "δ (scan window sets)",
                    constraint!(exactly >= sum - (members - 1.0)),
                );
                terms.push(RunTerm {
                    indicator: exactly.into(),
                    numbers,
                    plain: false,
                    output,
                });
            }
            Some(terms)
        }
        _ => {
            milp.note_inexact("a scan read at more window kinds than encoded (widest charged)");
            let widest = scan_window_read_at(kinds.iter().copied());
            usage_at(milp, widest).map(|(numbers, output)| {
                vec![RunTerm {
                    indicator: run.into(),
                    numbers,
                    plain: true,
                    output,
                }]
            })
        }
    };
    milp.window_cache.insert((context, scan), terms.clone());
    terms
}

/// For a shareable kept pane chain with usage depending on the panes it
/// retains: its base term at the fewest panes, plus one term per larger
/// retention, `π[m, ℓ] ≥` the reach of each query needing at least `ℓ`.
fn pane_retention_terms(
    milp: &mut SelectionMilp<'_>,
    context: usize,
    chain: PhysicalCandidateId,
    terms: &mut Vec<RunTerm>,
) {
    let data = &milp.inputs.candidate_data[chain.0];
    let mut levels = data.usage_per_retained_panes.clone();
    levels.sort_by_key(|(panes, _)| *panes);
    let per_query = data.retained_panes_per_query.clone();
    let Some(&(_, base)) = levels.first() else {
        return;
    };
    terms[0].numbers = NodeNumbers::of(&base);
    let at_query_time = milp.runs_at_query_time(context);
    // Per query, the indicator that it reaches the chain as `plan_cost`
    // counts reach (through query-time runs).
    let mut reached: Vec<(usize, Vec<Expression>)> = Vec::new();
    for &(query, _) in &per_query {
        let mut indicators = Vec::new();
        if let Some(&reach) = milp.reach.get(&(query, chain)) {
            if at_query_time
                || milp.physical.candidates[chain.0].timing == ExecutionTiming::QueryTime
            {
                indicators.push(Expression::from(reach));
                reached.push((query, indicators));
                continue;
            }
        }
        if let Some(&direct) = milp.choice[query].get(&chain) {
            indicators.push(direct.into());
        }
        for &reader in milp
            .shared_readers
            .get(&chain)
            .cloned()
            .unwrap_or_default()
            .iter()
        {
            let reader_context = milp.inputs.context_of_shareable[&reader];
            if !milp.runs_at_query_time(reader_context) {
                continue;
            }
            if let Some(&above) = milp.reach.get(&(query, reader)) {
                let read = milp.choice[reader_context][&chain];
                let and = milp
                    .builder
                    .and_at_least("π (pane retention levels)", &[above.into(), read.into()]);
                indicators.push(and.into());
            }
        }
        reached.push((query, indicators));
    }
    let mut below = NodeNumbers::of(&base);
    for &(panes, usage) in &levels[1..] {
        let numbers = NodeNumbers::of(&usage);
        let (increase, negative) = numbers.minus(below);
        below = numbers;
        if negative {
            milp.note_inexact("a longer pane retention with a smaller number");
        }
        if increase.execution_cost > 0.0 && at_query_time {
            milp.note_inexact("pane retention changing a query-time execution cost");
        }
        if increase.latency_ms > 0.0 || increase.held_bytes > 0.0 {
            milp.note_inexact("pane retention changing latency or held memory (not encoded)");
        }
        let level = milp
            .builder
            .continuous("π (pane retention levels)", Some(1.0));
        for (query, indicators) in &reached {
            let needs = per_query
                .iter()
                .any(|&(q, retained)| q == *query && retained >= panes);
            if !needs {
                continue;
            }
            for indicator in indicators {
                let indicator = indicator.clone();
                milp.builder
                    .row("π (pane retention levels)", constraint!(level >= indicator));
            }
        }
        terms.push(RunTerm {
            indicator: level.into(),
            numbers: NodeNumbers {
                latency_ms: 0.0,
                held_bytes: 0.0,
                ..increase
            },
            plain: false,
            output: terms[0].output,
        });
    }
}

/// Each ingestion-time shareable candidate's compaction term for its
/// readers' latency, ms, where `plan_cost` charges one: `(k − 1) · states ·
/// merge time`, `k` the cores its ingestion work takes. With at most one
/// second of ingestion work per second below it, `k = 1` and the term is 0;
/// a larger subtree is not encoded (counted as inexact).
pub(crate) fn compaction_ms(milp: &mut SelectionMilp<'_>) -> HashMap<PhysicalCandidateId, f64> {
    let inputs = milp.inputs;
    let mut compaction = HashMap::new();
    for &shareable in inputs.context_of_shareable.keys() {
        if milp.physical.candidates[shareable.0].timing != ExecutionTiming::IngestionTime {
            continue;
        }
        let mut seen = BTreeSet::new();
        let mut stack = vec![shareable];
        let mut most_work_ms = 0.0;
        while let Some(class) = stack.pop() {
            if !seen.insert(class) {
                continue;
            }
            if let Ok(usage) = &inputs.candidate_data[class.0].usage {
                most_work_ms += usage.work_ms_per_execution;
            }
            stack.extend(
                inputs.producer_candidates_per_input[class.0]
                    .iter()
                    .flatten()
                    .copied(),
            );
        }
        if most_work_ms > 1_000.0 {
            milp.note_inexact("compaction with more than one ingest worker (not encoded)");
        }
        compaction.insert(shareable, 0.0);
    }
    compaction
}

/// A schedule group's readers with their own rates, and its rate levels.
type GroupLevels = (Vec<(usize, f64)>, Vec<f64>);

/// How the runs of a context execute per second.
#[derive(Debug, Clone)]
enum ContextRate {
    /// A fixed rate (a query's, 1 at ingestion time, or a query-time
    /// shareable candidate whose possible readers run it at one rate).
    Fixed(f64),
    /// `Σ rate · z` over a query-time shareable candidate's schedule-group
    /// levels.
    Components(Vec<(Variable, f64)>),
}

/// Each context's rate (module documentation), and the queries' own
/// rates.
fn context_rates(
    milp: &mut SelectionMilp<'_>,
) -> Result<(Vec<ContextRate>, Vec<f64>), SelectionMilpError> {
    let horizon = milp.cost_inputs.non_repeating_query_horizon_seconds;
    let cost = |error: asap_cost::analytical::AnalyticalCostError| {
        SelectionMilpError::Cost(error.to_string())
    };
    let query_rate: Vec<f64> = milp
        .entries
        .iter()
        .map(|entry| evaluation_rate([(&entry.recurrence, entry.range_evaluation)], horizon))
        .collect::<Result<_, _>>()
        .map_err(cost)?;
    let mut rates = Vec::with_capacity(milp.inputs.contexts.len());
    milp.builder.scope = Scope::SharedContexts;
    for context in 0..milp.inputs.contexts.len() {
        let rate = match milp.inputs.contexts[context].kind {
            ContextKind::Query(query) => ContextRate::Fixed(query_rate[query]),
            ContextKind::Ingestion => ContextRate::Fixed(1.0),
            ContextKind::Shared(m) => {
                let per_request = match milp.physical.candidates[m.0].materialization {
                    Materialization::MatAtIngestion(_) | Materialization::NotMaterialized => {
                        rates.push(ContextRate::Fixed(1.0));
                        continue;
                    }
                    Materialization::MatAtQuery(_) => false,
                    Materialization::MatAtQueryKeptAcrossEvaluations(_) => true,
                };
                let readers = milp.queries_reaching(m);
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
                if unknown_phase_joins_several(
                    readers.iter().map(|&query| &milp.entries[query].recurrence),
                ) {
                    milp.note_inexact("schedule: an unknown phase among several known ones");
                }
                let groups = schedule_groups(readers.iter().map(|&query| demand(query)), horizon)
                    .map_err(cost)?;
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
                    let mut levels: Vec<f64> = alone.clone();
                    if (full - fastest).abs() > 1e-9 * full.abs().max(1e-12) {
                        milp.note_inexact("schedule: a group's rate is not its fastest reader's");
                        levels = vec![full];
                    }
                    levels.sort_by(f64::total_cmp);
                    levels.dedup();
                    levels_per_group.push((members.into_iter().zip(alone).collect(), levels));
                }
                if let [(_, levels)] = levels_per_group.as_slice() {
                    if let [level] = levels.as_slice() {
                        rates.push(ContextRate::Fixed(*level));
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
                                let reach = milp.reach[&(query, m)];
                                milp.builder
                                    .row("z (schedule groups)", constraint!(z >= reach));
                            }
                        }
                        components.push((z, level - below));
                        below = level;
                    }
                }
                ContextRate::Components(components)
            }
        };
        rates.push(rate);
    }
    milp.builder.scope = Scope::Global;
    Ok((rates, query_rate))
}

/// Whether some interval has readers of unknown phase and readers of two
/// or more known phases.
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

/// The objective (cost model 1, module documentation) and its rows.
pub(crate) fn encode_objective(milp: &mut SelectionMilp<'_>) -> Result<(), SelectionMilpError> {
    let CostModel::UsageOverTime(unit_cost) = milp.cost_inputs.cost_model.clone() else {
        return Err(SelectionMilpError::UnsupportedCostModel("not cost model 1"));
    };
    let horizon = milp.cost_inputs.non_repeating_query_horizon_seconds;
    let cost = |error: asap_cost::analytical::AnalyticalCostError| {
        SelectionMilpError::Cost(error.to_string())
    };
    let request_ms = milp.cost_inputs.measured_operation_costs.ns_per_evaluation * 1e-6;
    if request_ms > 0.0 {
        for entry in milp.entries {
            let requests = evaluation_rate([(&entry.recurrence, None)], horizon).map_err(cost)?;
            milp.request_cost_per_second += unit_cost.cost_per_cpu_op * request_ms
                / milp.cost_inputs.resource_speed.ms_per_cpu_op
                * requests;
        }
    }
    let (rates, query_rate) = context_rates(milp)?;
    let memory = unit_cost.cost_per_memory_byte_second;
    let mut objective: Vec<(Variable, f64)> = Vec::new();
    let mut charge = |expression: &Expression, coefficient: f64| {
        if coefficient != 0.0 {
            for (variable, factor) in expression.linear_coefficients() {
                objective.push((variable, factor * coefficient));
            }
        }
    };
    // Per query-time shareable context: the execution cost of its runs
    // other than `m`'s plain own term, and per slot (`None`: `m`'s own run)
    // the most it can be.
    let mut context_execution: BTreeMap<usize, (Expression, BTreeMap<Option<ResultSlot>, f64>)> =
        BTreeMap::new();
    // `H[m]` per query-time shareable context.
    let mut held_time: HashMap<usize, (Variable, f64)> = HashMap::new();
    let runs = milp.runs();
    for (context, candidate) in runs {
        let terms = milp.run_terms(context, candidate);
        milp.builder.scope = milp.scope_of(context);
        let candidate_data = &milp.physical.candidates[candidate.0];
        let (timing, materialization) = (candidate_data.timing, candidate_data.materialization);
        let own = milp.inputs.contexts[context].kind == ContextKind::Shared(candidate);
        // The most this run adds to its context's execution cost: all its
        // terms at 1.
        let mut run_execution = 0.0;
        for term in &terms {
            let numbers = term.numbers;
            charge(&term.indicator, numbers.standing_cost);
            match &rates[context] {
                ContextRate::Fixed(rate) => charge(&term.indicator, numbers.execution_cost * rate),
                ContextRate::Components(components) => {
                    if numbers.execution_cost == 0.0 {
                    } else if term.plain && own {
                        for &(component, rate) in components {
                            charge(&component.into(), numbers.execution_cost * rate);
                        }
                    } else {
                        let (sum, _) = context_execution
                            .entry(context)
                            .or_insert_with(|| (Expression::from(0.0), BTreeMap::new()));
                        *sum += numbers.execution_cost * term.indicator.clone();
                        run_execution += numbers.execution_cost;
                    }
                }
            }
            // Held memory.
            if numbers.held_bytes <= 0.0 || timing != ExecutionTiming::QueryTime {
                continue;
            }
            // Per byte held per latency unit.
            let per_byte_ms = memory * numbers.held_bytes / 1_000.0 * milp.ms_per_latency_unit;
            match milp.inputs.contexts[context].kind {
                ContextKind::Query(query) => {
                    let latency = milp.latency[query];
                    let most = milp.latency_max[query];
                    let held = milp
                        .builder
                        .continuous("H_query (held for a query's latency)", None);
                    let indicator = term.indicator.clone();
                    milp.builder.row(
                        "H_query (held for a query's latency)",
                        constraint!(held >= latency - most * (1.0 - indicator)),
                    );
                    charge(&held.into(), per_byte_ms * query_rate[query]);
                }
                ContextKind::Shared(m) => {
                    if own && materialization != Materialization::MatAtQuery(Medium::Memory) {
                        milp.note_inexact("held bytes of a shareable candidate not in memory");
                    }
                    let (longest, most_held) = *held_time.entry(context).or_insert_with(|| {
                        let longest = milp
                            .builder
                            .continuous("H (held until m's slowest reader)", None);
                        let mut most_held: f64 = 0.0;
                        for query in 0..milp.latency.len() {
                            if let Some(&reach) = milp.reach.get(&(query, m)) {
                                let latency = milp.latency[query];
                                let most = milp.latency_max[query];
                                most_held = most_held.max(most);
                                milp.builder.row(
                                    "H (held until m's slowest reader)",
                                    constraint!(longest >= latency - most * (1.0 - reach)),
                                );
                            }
                        }
                        (longest, most_held)
                    });
                    let components: Vec<(Option<Variable>, f64)> = match &rates[context] {
                        ContextRate::Fixed(rate) => vec![(None, *rate)],
                        ContextRate::Components(components) => components
                            .iter()
                            .map(|&(component, rate)| (Some(component), rate))
                            .collect(),
                    };
                    for (component, rate) in components {
                        let mut both = Expression::from(0.0);
                        let mut slack = 0.0;
                        if let Some(component) = component {
                            both += component;
                            slack += 1.0;
                        }
                        if !(term.plain && own) {
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
                }
                ContextKind::Ingestion => {}
            }
        }
        if let Some((_, most_per_slot)) = context_execution.get_mut(&context) {
            let slot = (!own).then(|| milp.inputs.slot[candidate.0]);
            let entry = most_per_slot.entry(slot).or_default();
            *entry = entry.max(run_execution);
        }
    }
    // A context's execution at its varying rate: `V ≥ E − Ē (x[m] − z)` per
    // component `z`, `Ē` the most `E` can be: per slot, its costliest run.
    milp.builder.scope = Scope::SharedContexts;
    for (context, (sum, most_per_slot)) in context_execution {
        let most: f64 = if super::model::tests_loose_big_m() {
            // Before: every term's cost.
            sum.clone()
                .linear_coefficients()
                .map(|(_, cost)| cost)
                .sum()
        } else {
            most_per_slot.values().sum()
        };
        let ContextRate::Components(components) = &rates[context] else {
            continue;
        };
        let ContextKind::Shared(m) = milp.inputs.contexts[context].kind else {
            continue;
        };
        let region = milp
            .builder
            .continuous("V (context execution at a rate component)", None);
        milp.builder.row(
            "V (context execution at a rate component)",
            constraint!(region >= sum),
        );
        let active = milp.selected[&m];
        let slowest = components
            .iter()
            .map(|&(_, rate)| rate)
            .fold(f64::INFINITY, f64::min);
        let mut at_rates = Expression::from(0.0);
        for &(component, rate) in components {
            let at_rate = milp
                .builder
                .continuous("V (context execution at a rate component)", None);
            milp.builder.row(
                "V (context execution at a rate component)",
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
    milp.builder.scope = Scope::Global;
    raw_retention(milp, unit_cost.cost_per_disk_byte_second, &mut charge);
    milp.objective_terms = objective;
    Ok(())
}

/// Raw retention: per source, the largest raw data a query-time scan run of
/// the plan keeps, by levels.
fn raw_retention(
    milp: &mut SelectionMilp<'_>,
    disk: f64,
    charge: &mut impl FnMut(&Expression, f64),
) {
    let mut kept: BTreeMap<String, Vec<(f64, Expression)>> = BTreeMap::new();
    for (context, candidate) in milp.runs() {
        let Operator::NonASAP(NonASAPOp::Scan { source, .. }) =
            &milp.physical.physical_node(candidate).operator
        else {
            continue;
        };
        if milp.physical.candidates[candidate.0].timing != ExecutionTiming::QueryTime {
            continue;
        }
        let source = source_name(source).to_string();
        for term in milp.run_terms(context, candidate) {
            let bytes = term.numbers.raw_kept_bytes;
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
