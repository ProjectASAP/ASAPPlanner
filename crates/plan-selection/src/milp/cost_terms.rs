//! #1095 §3: the cost model 1 objective of the selection MILP. Every number is
//! `asap-cost`'s: a class's usage per execution and standing cost
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
//! **Indicators.** A node's numbers may depend on which producer class an
//! input reads (`CandidateData::usage_per_producers`): its terms then sit on
//! the links of that input, `Σ` over the producer's links (one producer per
//! used input), or, for several such inputs, on an AND binary over them
//! (`t_producers`, `t ≥ Σ links − (n − 1)`). A query-time scan whose usage
//! depends on the window its readers read gets one level `λ[i, ℓ] ≥` each
//! read at a window of level `ℓ` or wider; the scan reads the widest one.
//!
//! **Executions per second** `R[i]` of instance `i`:
//! - ingestion time: 1 (`R` is `x[i]` itself);
//! - `MatAtQuery`, kept across evaluations: one `z[i, G, ℓ]` per schedule
//!   group `G` of its possible readers and per level `ℓ` of the readers'
//!   own rates in it, `z ≥ a[q, i]` for each reader `q` at that level or
//!   above: the group's rate is its fastest reader's (`R ≥ Σ Δrate · z`);
//! - not materialized: once per consumer, as `plan_cost`'s `computations`
//!   counts it: the queries reaching it through nodes that are not
//!   materialized (`y[q, i]`, or `a[q, i]` when no materialized node can be
//!   in between) at their own rates, plus each materialized instance `m`
//!   reaching it so, at `R[m]` (`P[m, i] ≥ R[m] − R̄[m] (1 − y[m, i])`).
//!
//! A query owner's node runs at its query's rate; an instance owner's node
//! at its instance's `R` (`Q ≥ R − R̄ (1 − indicator)`, with `R̄` the most
//! `R` can be).
//!
//! **Held memory** (`plan_cost`'s `held`): a `MatAtQuery` result is held
//! for the longest reader's latency, `H[i] ≥ L[q] − M (1 − a[q, i])`, once
//! per execution: `W[i, G, ℓ] ≥ H[i] − M (1 − z[i, G, ℓ])`, charged
//! `Δrate · bytes · unit memory cost`. A not-materialized node's working
//! memory is held for each consumer query's latency at its rate:
//! `G[q, ·] ≥ L[q] − M (1 − reach)`, charged `rate(q) · bytes · unit
//! memory cost`.
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

use crate::owners::{CandidateUsage, ProducerChoice, UsageContext};

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

/// A reader's usage per window of the scan it reads (`None`: no window).
type UsagePerWindow = Vec<(Option<ScanWindow>, Option<CandidateUsage>)>;

/// One term of a node: its numbers count when `indicator` is 1. A node's
/// numbers are the sum of its terms' at integral binaries.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodeTerm {
    pub(crate) indicator: Expression,
    pub(crate) numbers: NodeNumbers,
    /// Whether `indicator` is the node's own variable (`s[o, k]` or `x[i]`).
    pub(crate) plain: bool,
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
        milp.note_inexact("usage: a producer whose own output depends on its context");
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
    }]
}

/// The indicator that input `input` of `class` in owner `owner` reads
/// producer class `producer`: the sum of those links; `None` when the owner
/// has none.
fn link_indicator(
    milp: &SelectionMilp<'_>,
    owner: usize,
    class: PhysicalCandidateId,
    input: usize,
    producer: PhysicalCandidateId,
) -> Option<Expression> {
    let variables = &milp.owners[owner];
    let mut sum = Expression::from(0.0);
    let mut any = false;
    for &((reader, at, stateless), link) in &variables.links {
        if reader == class && at == input && stateless == producer {
            sum += link;
            any = true;
        }
    }
    for &((reader, at, instance), read) in &variables.reads {
        if reader == class && at == input && milp.instances[instance].class == producer {
            sum += read;
            any = true;
        }
    }
    any.then_some(sum)
}

/// Terms of a node whose usage depends on its producers.
fn producer_terms(
    milp: &mut SelectionMilp<'_>,
    owner: usize,
    class: PhysicalCandidateId,
    node: Variable,
) -> Vec<NodeTerm> {
    let data = &milp.inputs.candidate_data[class.0];
    let changing = data.inputs_changing_usage.clone();
    let mut table = data.usage_per_producers.clone();
    let base = *data.usage.as_ref().expect("costed");
    let mut terms = Vec::new();
    let mut covered: Vec<BTreeSet<PhysicalCandidateId>> = vec![BTreeSet::new(); changing.len()];
    if changing.len() == 1 {
        // On the links of the one input, per producer class; for a scan
        // whose output depends on its window, per level of the scan
        // instance read.
        let input = changing[0];
        let mut by_class: BTreeMap<PhysicalCandidateId, UsagePerWindow> = BTreeMap::new();
        for (choices, usage) in &table {
            by_class
                .entry(choices[0].class)
                .or_default()
                .push((choices[0].window, *usage));
        }
        for (producer, entries) in by_class {
            let Some(indicator) = link_indicator(milp, owner, class, input, producer) else {
                continue;
            };
            covered[0].insert(producer);
            if entries.iter().all(|(window, _)| window.is_none()) {
                match entries[0].1 {
                    None => milp
                        .builder
                        .row("t_producers (AND)", constraint!(indicator <= 0.0)),
                    Some(usage) => terms.push(NodeTerm {
                        indicator,
                        numbers: NodeNumbers::of(&usage),
                        plain: false,
                    }),
                }
                continue;
            }
            terms.extend(scan_reader_terms(
                milp, owner, class, input, producer, &entries,
            ));
        }
    } else {
        // Several inputs: windows are merged, the costliest kept.
        if table
            .iter()
            .any(|(choices, _)| choices.iter().any(|choice| choice.window.is_some()))
        {
            milp.note_inexact(
                "usage: several changing inputs, one a windowed scan (costliest kept)",
            );
            let mut merged: Vec<(Vec<ProducerChoice>, Option<CandidateUsage>)> = Vec::new();
            for (choices, usage) in table {
                let key: Vec<ProducerChoice> = choices
                    .iter()
                    .map(|choice| ProducerChoice {
                        class: choice.class,
                        window: None,
                    })
                    .collect();
                match merged.iter_mut().find(|(known, _)| *known == key) {
                    Some((_, known)) => {
                        let total = |usage: &Option<CandidateUsage>| {
                            usage.map_or(f64::INFINITY, |usage| {
                                NodeNumbers::of(&usage).execution_cost
                                    + NodeNumbers::of(&usage).standing_cost
                            })
                        };
                        if total(&usage) > total(known) {
                            *known = usage;
                        }
                    }
                    None => merged.push((key, usage)),
                }
            }
            table = merged;
        }
        for (choices, usage) in &table {
            let indicators: Option<Vec<Expression>> = changing
                .iter()
                .zip(choices)
                .map(|(&input, choice)| link_indicator(milp, owner, class, input, choice.class))
                .collect();
            let Some(indicators) = indicators else {
                continue;
            };
            for (position, choice) in choices.iter().enumerate() {
                covered[position].insert(choice.class);
            }
            let count = indicators.len() as f64;
            match usage {
                None => {
                    // `asap-cost` cannot cost this choice: no plan makes it.
                    let sum: Expression = indicators.into_iter().sum();
                    milp.builder
                        .row("t_producers (AND)", constraint!(sum <= count - 1.0));
                }
                Some(usage) => {
                    let indicator = milp
                        .builder
                        .and_at_least("t_producers (AND)", &indicators)
                        .into();
                    terms.push(NodeTerm {
                        indicator,
                        numbers: NodeNumbers::of(usage),
                        plain: false,
                    });
                }
            }
        }
    }
    // A producer missing from the table (not costed with this class): its
    // links carry the first costed usage.
    let mut missing = false;
    for (position, &input) in changing.iter().enumerate() {
        let producers: BTreeSet<PhysicalCandidateId> = milp.owners[owner]
            .links
            .iter()
            .filter(|((reader, at, _), _)| *reader == class && *at == input)
            .map(|&((_, _, producer), _)| producer)
            .chain(
                milp.owners[owner]
                    .reads
                    .iter()
                    .filter(|((reader, at, _), _)| *reader == class && *at == input)
                    .map(|&((_, _, instance), _)| milp.instances[instance].class),
            )
            .collect();
        for producer in producers.difference(&covered[position]) {
            missing = true;
            if let Some(indicator) = link_indicator(milp, owner, class, input, *producer) {
                terms.push(NodeTerm {
                    indicator,
                    numbers: NodeNumbers::of(&base),
                    plain: false,
                });
            }
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
    terms
}

/// The terms of `class` (input `input`) in `owner` reading instances of
/// windowed scan class `scan`: per instance read, the reader's numbers at
/// the scan's narrowest window on the read, and each wider level's increase
/// on `AND(read, λ[i, ℓ])`. `entries` are the reader's usages per window.
fn scan_reader_terms(
    milp: &mut SelectionMilp<'_>,
    owner: usize,
    class: PhysicalCandidateId,
    input: usize,
    scan: PhysicalCandidateId,
    entries: &UsagePerWindow,
) -> Vec<NodeTerm> {
    let levels: Vec<ScanWindow> = sorted_windows(milp, scan)
        .into_iter()
        .map(|(window, _)| window)
        .collect();
    let reads: Vec<(usize, Variable)> = milp.owners[owner]
        .reads
        .iter()
        .filter(|((reader, at, instance), _)| {
            *reader == class && *at == input && milp.instances[*instance].class == scan
        })
        .map(|&((_, _, instance), read)| (instance, read))
        .collect();
    let usage_at = |window: ScanWindow| {
        entries
            .iter()
            .find(|(known, _)| *known == Some(window))
            .and_then(|(_, usage)| *usage)
    };
    let mut terms = Vec::new();
    for (instance, read) in reads {
        let scan_terms = milp.node_terms(milp.instance_owner(instance), scan);
        let mut previous: Option<NodeNumbers> = None;
        for (level, &window) in levels.iter().enumerate() {
            let Some(usage) = usage_at(window) else {
                milp.note_inexact("usage: a reader not costed at one of its scan's windows");
                continue;
            };
            let numbers = NodeNumbers::of(&usage);
            let (numbers, negative) = match previous {
                None => (numbers, false),
                Some(previous) => numbers.minus(previous),
            };
            if negative {
                milp.note_inexact("usage: a reader cheaper over a wider scan window");
            }
            previous = Some(NodeNumbers::of(&usage));
            let indicator = match level {
                0 => read.into(),
                _ => match scan_terms.get(level) {
                    Some(term) => milp
                        .builder
                        .and_at_least("t_producers (AND)", &[read.into(), term.indicator.clone()])
                        .into(),
                    None => continue,
                },
            };
            terms.push(NodeTerm {
                indicator,
                numbers,
                plain: false,
            });
        }
    }
    terms
}

/// A windowed scan class's windows with their numbers, narrowest first:
/// the levels of [`window_terms`].
fn sorted_windows(
    milp: &SelectionMilp<'_>,
    scan: PhysicalCandidateId,
) -> Vec<(ScanWindow, NodeNumbers)> {
    let mut windows: Vec<(ScanWindow, NodeNumbers)> = milp.inputs.candidate_data[scan.0]
        .usage_per_window
        .iter()
        .map(|(window, usage)| (*window, NodeNumbers::of(usage)))
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
/// each wider level's increase on `λ[i, ℓ]`.
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
            .map(|window| windows.iter().position(|(known, _)| known == window))
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
    /// Ingestion time: once per second while active (`x[i]`).
    Ingestion,
    /// `rate` ≤ `most`; `groups` are the `(z, Δrate)` of a materialized
    /// instance's schedule groups (empty for a not-materialized one).
    Variable {
        rate: Variable,
        most: f64,
        groups: Vec<(Variable, f64)>,
    },
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
    // The longest latency any query can have, for products with a latency.
    let most_latency = milp.latency_max_ms.iter().copied().fold(0.0, f64::max);
    let mut held_time: HashMap<usize, Variable> = HashMap::new();
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
                match instance.map(|i| &rates[i]) {
                    None => charge(&term.indicator, numbers.execution_cost * query_rate[owner]),
                    Some(InstanceRate::Ingestion) => {
                        charge(&term.indicator, numbers.execution_cost)
                    }
                    Some(InstanceRate::Variable { rate, most, .. }) => {
                        if numbers.execution_cost == 0.0 {
                        } else if term.plain && own_class {
                            charge(&(*rate).into(), numbers.execution_cost);
                        } else {
                            let at_rate = milp
                                .builder
                                .continuous("Q (node at its instance's rate)", None);
                            let indicator = term.indicator.clone();
                            milp.builder.row(
                                "Q (node at its instance's rate)",
                                constraint!(at_rate >= *rate - *most * (1.0 - indicator)),
                            );
                            charge(&at_rate.into(), numbers.execution_cost);
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
                    let Some(InstanceRate::Variable { groups, .. }) = rates.get(instance) else {
                        continue;
                    };
                    let groups = groups.clone();
                    if !own_class && milp.inputs.queries_reaching[instance_class.0].len() > 1 {
                        milp.note_inexact(
                            "working memory below a shared materialization (longest reader, \
                             not the rate-weighted mean)",
                        );
                    }
                    let longest = *held_time.entry(instance).or_insert_with(|| {
                        let longest = milp
                            .builder
                            .continuous("H (held time of an instance)", None);
                        for &query in &milp.inputs.queries_reaching[instance_class.0] {
                            if let Some(&reach) = milp.reach.get(&(query, instance)) {
                                let latency = milp.latency[query];
                                milp.builder.row(
                                    "H (held time of an instance)",
                                    constraint!(longest >= latency - most_latency * (1.0 - reach)),
                                );
                            }
                        }
                        longest
                    });
                    for (group, increase) in groups {
                        let held = milp
                            .builder
                            .continuous("W (held time × schedule group)", None);
                        let indicator = term.indicator.clone();
                        let slack = if term.plain && own_class { 1.0 } else { 2.0 };
                        let both = if term.plain && own_class {
                            Expression::from(group)
                        } else {
                            group + indicator
                        };
                        milp.builder.row(
                            "W (held time × schedule group)",
                            constraint!(held >= longest - most_latency * (slack - both)),
                        );
                        charge(&held.into(), per_byte_ms * increase);
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
                rates[instance] = Some(InstanceRate::Ingestion);
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
        let rate = milp.builder.continuous("R (instance rates)", None);
        let mut sum = Expression::from(0.0);
        let mut most = 0.0;
        let mut group_terms = Vec::new();
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
            most += full;
            // Levels of the members' own rates; the group runs at its
            // fastest member's when that is its rate.
            let mut levels: Vec<f64> = alone.clone();
            if (full - fastest).abs() > 1e-9 * full.abs().max(1e-12) {
                milp.note_inexact("schedule: a group's rate is not its fastest reader's");
                levels = vec![full];
            }
            levels.sort_by(f64::total_cmp);
            levels.dedup();
            let mut below = 0.0;
            for &level in &levels {
                let z = milp.builder.continuous("z (schedule groups)", Some(1.0));
                for (&query, &own) in members.iter().zip(&alone) {
                    if own >= level || levels.len() == 1 {
                        let reach = milp.reach[&(query, instance)];
                        milp.builder
                            .row("z (schedule groups)", constraint!(z >= reach));
                    }
                }
                sum += (level - below) * z;
                group_terms.push((z, level - below));
                below = level;
            }
        }
        milp.builder
            .row("R (instance rates)", constraint!(rate >= sum));
        rates[instance] = Some(InstanceRate::Variable {
            rate,
            most,
            groups: group_terms,
        });
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
            for read in reads {
                milp.builder.row(
                    "y (consumers through not-materialized nodes)",
                    constraint!(y >= above + read - 1.0),
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
        let rate = milp.builder.continuous("R (instance rates)", None);
        let mut sum = Expression::from(0.0);
        let mut most = 0.0;
        for &query in &queries {
            if let Some(y) = consumer_of(milp, Consumer::Query(query), instance) {
                sum += query_rate[query] * y;
                most += query_rate[query];
            }
        }
        for class in &materialized {
            for &consumer in milp.instances_of_candidate.get(class).into_iter().flatten() {
                let Some(&y) = through.get(&(Consumer::Instance(consumer), instance)) else {
                    continue;
                };
                match &rates[consumer] {
                    Some(InstanceRate::Ingestion) => {
                        sum += y;
                        most += 1.0;
                    }
                    Some(InstanceRate::Variable {
                        rate: consumer_rate,
                        most: consumer_most,
                        ..
                    }) => {
                        let (consumer_rate, consumer_most) = (*consumer_rate, *consumer_most);
                        let at_rate = milp
                            .builder
                            .continuous("P (materialized consumers' rates)", None);
                        milp.builder.row(
                            "P (materialized consumers' rates)",
                            constraint!(at_rate >= consumer_rate - consumer_most * (1.0 - y)),
                        );
                        sum += at_rate;
                        most += consumer_most;
                    }
                    None => {}
                }
            }
        }
        milp.builder
            .row("R (instance rates)", constraint!(rate >= sum));
        rates[instance] = Some(InstanceRate::Variable {
            rate,
            most,
            groups: Vec::new(),
        });
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
