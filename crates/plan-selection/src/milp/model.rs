//! #1095 §2.0: the group selection MILP's variables and rows over execution
//! contexts ([`crate::groups`]), one HiGHS solve, and the raw solution.
//!
//! ## Variables
//!
//! | Family | Meaning |
//! |---|---|
//! | `p[ctx, c]` | in context `ctx`, candidate `c` gives its result slot (binary): computed there, or read across the boundary (a shareable candidate, or a candidate of an ingestion pipeline `I_r`) |
//! | `x[m]` | shareable candidate `m` is selected (binary) |
//! | `a[q, m]` | query `q` depends on shareable `m` (continuous, exact at integral binaries) |
//! | `y[q, m, c]` | `AND(a[q, m], p[m, c])`, for reach from above and accuracy (continuous) |
//! | `L[ctx]`, `T[q, m]`, `K[q]` | latency of a query or of a query-time shareable candidate's run and context; `m`'s latency in `q`'s; compaction (continuous) |
//! | `N[q]`, `A[q, parts]` | approximate values `q` depends on; whether it depends on one sized for `parts` shares (continuous) |
//! | `g[m, c]` | highest shared materialization: `c` is read in context `m`, which at most one query reaches (binary) |
//! | `rank` | order ranks on a context's cyclic links (continuous) |
//!
//! The cost families are [`super::cost_terms`]'s.
//!
//! ## Rows (numbered as in #1095 §2.0)
//!
//! 1. Coverage: `Σ_{c ∈ Out(r)} p[q(r), c] = 1` per output reference.
//! 2. One candidate per result slot per context: `Σ_{c ∈ slot} p[ctx, c] ≤ 1`.
//! 3. Validity: `Σ_{c' ∈ compat(c, n)} p[ctx, c'] ≥ p[ctx, c]` for each `c`
//!    computed in `ctx`; `≥ x[m]` for `m`'s own inputs in its context;
//!    `p[ctx, m] ≤ x[m]`, `p[m, c] ≤ p[I_r(m), c]` for reads across a
//!    boundary (`r(m)` the refresh window of ingestion-time `m`).
//! 4. Used only if read, per context; a candidate of `I_r` is also read
//!    by `p[m, c]` for each `m` with `r(m) = r`.
//! 5. Sharing: `x[m] ≤ Σ_ctx p[ctx, m]`.
//! 6. Reach: `a[q, m] ≥ p[q, m]`, `a[q, c] ≥ a[q, m] + p[m, c] − 1`; from
//!    above, where a reader constraint counts queries.
//! 7. Latency (`plan_cost`'s: each query-time run it reaches counts once).
//! 8. Accuracy (A1), per run a query depends on.
//! 9. Reader constraints.
//!
//! A link from a candidate to one of its own result slot is dropped: with
//! one candidate per slot per context it can only be a self-loop (a
//! widened scan's `Project` lists its own memo group). Links inside a
//! larger cyclic component of a context get order ranks.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Instant;

use asap_cost::{CostInputs, CostModel};
use asap_types::stages::{
    output_references, PhysicalASAPMemoCandidates, PhysicalCandidateId, ReaderConstraint,
};
use asap_types::workload::QueryWorkloadEntry;
use good_lp::solvers::highs::highs;
use good_lp::{
    constraint, variable, Constraint, Expression, IntoAffineExpression, ProblemVariables,
    SolverModel, Variable,
};

use crate::groups::{AccuracyWeight, ContextKind, MilpInputs, ResultSlot, RunKind};

use super::cost_terms::{self, RunTerm};
use super::warm_start;

/// Why the selection MILP was not built or solved.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SelectionMilpError {
    /// The MILP implements cost model 1 only (§3); the name of the model given.
    UnsupportedCostModel(&'static str),
    /// `entries` and the query roots differ in number.
    EntryCount { roots: usize, entries: usize },
    /// No plan meets every row.
    Infeasible,
    /// HiGHS stopped (at the deadline) without an integral incumbent.
    NoSolution { status: String },
    /// The deadline passed before the solve.
    DeadlineReached,
    /// A rate or another number `asap-cost` could not compute.
    Cost(String),
    /// HiGHS failed.
    Solver(String),
}

/// How the MILP is built.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SelectionMilpOptions {
    /// Write the accuracy rows (row 8); off when the facade ignores
    /// accuracy guarantees.
    pub(crate) enforce_accuracy: bool,
    /// Solve each independent part alone ([`independent_parts`]); off only
    /// to compare with one solve in tests.
    pub(crate) split_into_parts: bool,
}

/// Which part of the model a variable or row belongs to, for the size
/// report: the query contexts, the `I_r`, the shareable candidates' contexts
/// (with their `x`, reach and schedule terms), or the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub(crate) enum Scope {
    Queries,
    Ingestion,
    SharedContexts,
    Global,
}

/// The size of one family (or scope) of variables and rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct FamilySize {
    pub(crate) variables: usize,
    pub(crate) binaries: usize,
    pub(crate) rows: usize,
    /// Nonzero coefficients of its rows.
    pub(crate) nonzeros: usize,
}

impl FamilySize {
    fn add(&mut self, other: FamilySize) {
        self.variables += other.variables;
        self.binaries += other.binaries;
        self.rows += other.rows;
        self.nonzeros += other.nonzeros;
    }
}

/// Variables and rows, counted per family and per scope.
pub(crate) struct ModelBuilder {
    pub(crate) variables: ProblemVariables,
    pub(crate) rows: Vec<Constraint>,
    /// The family of each row of `rows`.
    pub(crate) row_families: Vec<&'static str>,
    pub(crate) families: BTreeMap<&'static str, FamilySize>,
    pub(crate) scopes: BTreeMap<Scope, FamilySize>,
    /// Where new variables and rows are counted.
    pub(crate) scope: Scope,
    /// The family of each variable, in creation order.
    #[cfg(feature = "lp-diagnosis")]
    pub(crate) column_families: Vec<&'static str>,
}

impl ModelBuilder {
    fn new() -> Self {
        Self {
            variables: ProblemVariables::new(),
            rows: Vec::new(),
            row_families: Vec::new(),
            families: BTreeMap::new(),
            scopes: BTreeMap::new(),
            scope: Scope::Global,
            #[cfg(feature = "lp-diagnosis")]
            column_families: Vec::new(),
        }
    }

    fn count(&mut self, family: &'static str, size: FamilySize) {
        #[cfg(feature = "lp-diagnosis")]
        if size.variables > 0 {
            self.column_families.push(family);
        }
        self.families.entry(family).or_default().add(size);
        self.scopes.entry(self.scope).or_default().add(size);
    }

    pub(crate) fn binary(&mut self, family: &'static str) -> Variable {
        self.count(
            family,
            FamilySize {
                variables: 1,
                binaries: 1,
                ..FamilySize::default()
            },
        );
        self.variables.add(variable().binary())
    }

    /// A continuous variable in `[0, max]` (unbounded above for `None`).
    pub(crate) fn continuous(&mut self, family: &'static str, max: Option<f64>) -> Variable {
        self.count(
            family,
            FamilySize {
                variables: 1,
                ..FamilySize::default()
            },
        );
        let definition = variable().min(0.0);
        self.variables.add(match max {
            Some(max) => definition.max(max),
            None => definition,
        })
    }

    pub(crate) fn row(&mut self, family: &'static str, row: Constraint) {
        let nonzeros = row.expression().linear_coefficients().count();
        self.count(
            family,
            FamilySize {
                rows: 1,
                nonzeros,
                ..FamilySize::default()
            },
        );
        self.rows.push(row);
        self.row_families.push(family);
    }

    /// `and ≥ Σ terms − (n − 1)`, `and ∈ [0, 1]`: the AND of `n`
    /// indicators, tight under a cost or a `≤` row that charges it.
    pub(crate) fn and_at_least(&mut self, family: &'static str, terms: &[Expression]) -> Variable {
        let and = self.continuous(family, Some(1.0));
        let sum: Expression = terms.iter().cloned().sum();
        let count = terms.len() as f64;
        self.row(family, constraint!(and >= sum - (count - 1.0)));
        and
    }
}

/// One way a candidate's input is read in a context: `(reader, input)`
/// with the reader computed in that context, the query's output reference,
/// or the shareable candidate's own input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Reader {
    Candidate(PhysicalCandidateId, usize),
    Output(usize),
    OwnInput(usize),
}

/// One input of a candidate: `(reader, input)`.
type InputOf = (PhysicalCandidateId, usize);

/// The slot links of one multi-slot input: `(slot, e)`.
type SlotLinks = Vec<(ResultSlot, Variable)>;

/// The built model, before the solve.
pub(crate) struct SelectionMilp<'a> {
    pub(crate) builder: ModelBuilder,
    pub(crate) physical: &'a PhysicalASAPMemoCandidates,
    pub(crate) inputs: &'a MilpInputs,
    pub(crate) entries: &'a [QueryWorkloadEntry],
    pub(crate) cost_inputs: &'a CostInputs,
    /// Per context, `p[ctx, c]` per option.
    pub(crate) choice: Vec<BTreeMap<PhysicalCandidateId, Variable>>,
    /// Per context, per option, the reads of it in that context.
    pub(crate) readers: Vec<BTreeMap<PhysicalCandidateId, Vec<Reader>>>,
    /// `x[m]` per shareable candidate with a context.
    pub(crate) selected: BTreeMap<PhysicalCandidateId, Variable>,
    /// `a[q, m]`, for each query that may depend on shareable `m`.
    pub(crate) reach: HashMap<(usize, PhysicalCandidateId), Variable>,
    /// Per shareable candidate, the shareable candidates whose contexts may
    /// read it.
    pub(crate) shared_readers: BTreeMap<PhysicalCandidateId, Vec<PhysicalCandidateId>>,
    /// `L[q]`.
    pub(crate) latency: Vec<Variable>,
    /// An upper bound of each `L[q]` (its bound, or the most it can be), in
    /// latency units.
    pub(crate) latency_max: Vec<f64>,
    /// Milliseconds per latency unit: every latency variable (`L`, `T`, `K`
    /// and the held times) counts in these units, chosen so the largest
    /// possible latency is about 1,000 units (with sub-microsecond work per
    /// execution, milliseconds fall below HiGHS's absolute tolerances).
    pub(crate) ms_per_latency_unit: f64,
    /// The cost model 1 objective, in cost per second, without the request
    /// fees (constant for every plan, added after selection).
    pub(crate) objective_terms: Vec<(Variable, f64)>,
    /// The request fees per second (the same for every plan).
    pub(crate) request_cost_per_second: f64,
    /// Terms encoded inexactly, by reason.
    pub(crate) inexact: BTreeMap<&'static str, usize>,
    /// The terms of each run `(context, candidate)`, cached; a shareable
    /// candidate's own run is keyed by its own context.
    pub(crate) run_terms_cache: HashMap<(usize, PhysicalCandidateId), Vec<RunTerm>>,
    /// Runs whose terms are being built (cycles of links).
    pub(crate) terms_in_progress: HashSet<(usize, PhysicalCandidateId)>,
    /// Per run and outputs, the variable `≥` its terms giving them.
    pub(crate) producer_gives: HashMap<(usize, PhysicalCandidateId, Vec<usize>), Variable>,
    /// Per query-time scan run, its window terms
    /// ([`cost_terms::window_terms`]).
    pub(crate) window_cache: HashMap<(usize, PhysicalCandidateId), Option<Vec<RunTerm>>>,
    /// For each input whose producers lie in several result slots (a
    /// factored root over the private nodes of several implementations),
    /// per context: `e[ctx, r, n, slot]`, the slot input `n` of computed
    /// reader `r` reads (binary). Row 4 counts these reads, so a selected
    /// producer in the other slot is not justified by `r`.
    pub(crate) slot_links:
        BTreeMap<(usize, PhysicalCandidateId, usize), Vec<(ResultSlot, Variable)>>,
    /// [`SelectionMilpOptions::split_into_parts`].
    pub(crate) split_into_parts: bool,
}

/// Variables and rows of one model.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct SelectionMilpSize {
    pub(crate) variables: usize,
    pub(crate) binaries: usize,
    pub(crate) rows: usize,
    pub(crate) nonzeros: usize,
    pub(crate) families: BTreeMap<&'static str, FamilySize>,
    pub(crate) scopes: BTreeMap<Scope, FamilySize>,
    pub(crate) inexact: BTreeMap<&'static str, usize>,
}

/// How the solve ended with a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionMilpStatus {
    Optimal,
    /// HiGHS's time limit (the deadline), with an integral incumbent.
    TimeLimit,
}

/// The raw solution, ready to decode.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SelectionMilpSolution {
    pub(crate) status: SelectionMilpStatus,
    /// The objective: cost per second without the request fees.
    pub(crate) cost_per_second: f64,
    /// The request fees per second, the same for every plan.
    pub(crate) request_cost_per_second: f64,
    /// HiGHS's dual bound on `cost_per_second`.
    pub(crate) cost_lower_bound_per_second: f64,
    /// HiGHS's relative gap.
    pub(crate) relative_gap: f64,
    /// Per context, the options with `p[ctx, c] = 1`.
    pub(crate) chosen: Vec<BTreeSet<PhysicalCandidateId>>,
    /// The shareable candidates with `x[m] = 1`.
    pub(crate) selected: BTreeSet<PhysicalCandidateId>,
    /// Per multi-slot input `(context, reader, input)` of a used reader, the
    /// slot it reads.
    pub(crate) slot_reads: BTreeMap<(usize, PhysicalCandidateId, usize), ResultSlot>,
    pub(crate) size: SelectionMilpSize,
    /// Per independent part, how its solve went, in part order.
    pub(crate) parts: Vec<PartSolve>,
    pub(crate) build_ms: u64,
    /// Building the warm start, HiGHS's first incumbent.
    pub(crate) warm_start_ms: u64,
    /// The warm start's objective (like `cost_per_second`), when its
    /// completion costed it.
    pub(crate) warm_start_cost_per_second: Option<f64>,
    pub(crate) solve_ms: u64,
}

impl<'a> SelectionMilp<'a> {
    pub(crate) fn query_count(&self) -> usize {
        self.physical.query_roots.len()
    }

    /// The context where `context` reads a candidate of an `I_r`
    /// ([`MilpInputs::ingestion_context_read_by`]).
    pub(crate) fn ingestion_context_read_by(&self, context: usize) -> Option<usize> {
        self.inputs.ingestion_context_read_by(context)
    }

    /// The context of shareable candidate `m`.
    pub(crate) fn context_of(&self, shareable: PhysicalCandidateId) -> Option<usize> {
        self.inputs.context_of_shareable.get(&shareable).copied()
    }

    pub(crate) fn kind(&self, candidate: PhysicalCandidateId) -> RunKind {
        self.inputs.kind[candidate.0]
    }

    pub(crate) fn slot(&self, candidate: PhysicalCandidateId) -> ResultSlot {
        self.inputs.slot[candidate.0]
    }

    /// Whether `candidate` is computed in context `context`.
    pub(crate) fn computes(&self, context: usize, candidate: PhysicalCandidateId) -> bool {
        self.inputs.contexts[context].computes(self.kind(candidate))
    }

    /// The indicator of run `(context, candidate)`: `p[ctx, c]` for a
    /// candidate computed there, `x[m]` for a shareable candidate's own run
    /// in its own context.
    pub(crate) fn run_indicator(
        &self,
        context: usize,
        candidate: PhysicalCandidateId,
    ) -> Option<Variable> {
        if self.inputs.contexts[context].kind == ContextKind::Shared(candidate) {
            return self.selected.get(&candidate).copied();
        }
        if !self.computes(context, candidate) {
            return None;
        }
        self.choice[context].get(&candidate).copied()
    }

    /// The context where run `(context, candidate)` reads its inputs: its
    /// own (a shareable candidate's own run reads in its context too).
    pub(crate) fn runs_at_query_time(&self, context: usize) -> bool {
        self.inputs.contexts[context].at_query_time
    }

    pub(crate) fn size(&self) -> SelectionMilpSize {
        let families = self.builder.families.clone();
        SelectionMilpSize {
            variables: families.values().map(|f| f.variables).sum(),
            binaries: families.values().map(|f| f.binaries).sum(),
            rows: families.values().map(|f| f.rows).sum(),
            nonzeros: families.values().map(|f| f.nonzeros).sum(),
            families,
            scopes: self.builder.scopes.clone(),
            inexact: self.inexact.clone(),
        }
    }

    pub(crate) fn note_inexact(&mut self, reason: &'static str) {
        *self.inexact.entry(reason).or_default() += 1;
    }

    /// The scope of context `context`.
    pub(crate) fn scope_of(&self, context: usize) -> Scope {
        match self.inputs.contexts[context].kind {
            ContextKind::Query(_) => Scope::Queries,
            ContextKind::Ingestion(_) => Scope::Ingestion,
            ContextKind::Shared(_) => Scope::SharedContexts,
        }
    }

    /// Run `(context, candidate)`'s terms ([`cost_terms::run_terms`]),
    /// cached.
    pub(crate) fn run_terms(
        &mut self,
        context: usize,
        candidate: PhysicalCandidateId,
    ) -> Vec<RunTerm> {
        if let Some(terms) = self.run_terms_cache.get(&(context, candidate)) {
            return terms.clone();
        }
        let scope_here = self.scope_of(context);
        let scope = std::mem::replace(&mut self.builder.scope, scope_here);
        let terms = cost_terms::run_terms(self, context, candidate);
        self.builder.scope = scope;
        self.run_terms_cache
            .insert((context, candidate), terms.clone());
        terms
    }

    /// Every run of the model: `(context, candidate)` for each candidate
    /// computed in a context, and each shareable candidate's own run.
    pub(crate) fn runs(&self) -> Vec<(usize, PhysicalCandidateId)> {
        let mut runs = Vec::new();
        for (context, options) in self.choice.iter().enumerate() {
            for &candidate in options.keys() {
                if self.computes(context, candidate) {
                    runs.push((context, candidate));
                }
            }
            if let ContextKind::Shared(own) = self.inputs.contexts[context].kind {
                if self.selected.contains_key(&own) {
                    runs.push((context, own));
                }
            }
        }
        runs
    }

    /// The query contexts that may depend on shareable `m`, ascending.
    pub(crate) fn queries_reaching(&self, shareable: PhysicalCandidateId) -> Vec<usize> {
        let mut queries: Vec<usize> = (0..self.query_count())
            .filter(|&query| self.reach.contains_key(&(query, shareable)))
            .collect();
        queries.sort_unstable();
        queries
    }
}

/// Builds the MILP over `inputs` (§2.0): `entries[q]` is query `q`'s
/// workload entry.
pub(crate) fn build_selection_milp<'a>(
    physical: &'a PhysicalASAPMemoCandidates,
    inputs: &'a MilpInputs,
    entries: &'a [QueryWorkloadEntry],
    cost_inputs: &'a CostInputs,
    options: SelectionMilpOptions,
) -> Result<SelectionMilp<'a>, SelectionMilpError> {
    match cost_inputs.cost_model {
        CostModel::UsageOverTime(_) => {}
        CostModel::ProvisionedCapacity(_) => {
            return Err(SelectionMilpError::UnsupportedCostModel(
                "ProvisionedCapacity (cost model 2)",
            ))
        }
        CostModel::ClusterCapacity(_) => {
            return Err(SelectionMilpError::UnsupportedCostModel(
                "ClusterCapacity (cost model 3)",
            ))
        }
    }
    let query_count = physical.query_roots.len();
    if entries.len() != query_count {
        return Err(SelectionMilpError::EntryCount {
            roots: query_count,
            entries: entries.len(),
        });
    }
    let mut milp = SelectionMilp {
        builder: ModelBuilder::new(),
        physical,
        inputs,
        entries,
        cost_inputs,
        choice: Vec::new(),
        readers: Vec::new(),
        selected: BTreeMap::new(),
        reach: HashMap::new(),
        shared_readers: BTreeMap::new(),
        latency: Vec::new(),
        latency_max: Vec::new(),
        ms_per_latency_unit: 1.0,
        objective_terms: Vec::new(),
        request_cost_per_second: 0.0,
        inexact: BTreeMap::new(),
        run_terms_cache: HashMap::new(),
        terms_in_progress: HashSet::new(),
        producer_gives: HashMap::new(),
        window_cache: HashMap::new(),
        slot_links: BTreeMap::new(),
        split_into_parts: options.split_into_parts,
    };
    choice_variables(&mut milp);
    structural_rows(&mut milp);
    acyclic_rows(&mut milp);
    acyclic_context_rows(&mut milp);
    reach_rows(&mut milp);
    reader_constraint_rows(&mut milp);
    latency_rows(&mut milp)?;
    if options.enforce_accuracy {
        accuracy_rows(&mut milp);
    }
    cost_terms::encode_objective(&mut milp)?;
    Ok(milp)
}

/// `p`, `x` and, per context, the reads of each option.
fn choice_variables(milp: &mut SelectionMilp<'_>) {
    let inputs = milp.inputs;
    let output_refs = output_references(&milp.physical.query_roots);
    for (context, data) in inputs.contexts.iter().enumerate() {
        milp.builder.scope = milp.scope_of(context);
        let options: BTreeSet<PhysicalCandidateId> = data.options.iter().copied().collect();
        let mut readers: BTreeMap<PhysicalCandidateId, Vec<Reader>> = BTreeMap::new();
        let mut choices = BTreeMap::new();
        for &option in &options {
            choices.insert(option, milp.builder.binary("p (choices)"));
        }
        for &reader in &options {
            if !data.computes(inputs.kind[reader.0]) {
                continue;
            }
            for (input, producers) in inputs.producer_candidates_per_input[reader.0]
                .iter()
                .enumerate()
            {
                for &producer in producers {
                    // A read of its own result slot is a self-loop.
                    if options.contains(&producer)
                        && inputs.slot[producer.0] != inputs.slot[reader.0]
                    {
                        readers
                            .entry(producer)
                            .or_default()
                            .push(Reader::Candidate(reader, input));
                    }
                }
            }
        }
        match data.kind {
            ContextKind::Query(query) => {
                for (reference, (q, _)) in output_refs.iter().enumerate() {
                    if *q != query {
                        continue;
                    }
                    for &out in &inputs.output_candidates[reference] {
                        if options.contains(&out) {
                            readers
                                .entry(out)
                                .or_default()
                                .push(Reader::Output(reference));
                        }
                    }
                }
            }
            ContextKind::Shared(own) => {
                for (input, producers) in inputs.producer_candidates_per_input[own.0]
                    .iter()
                    .enumerate()
                {
                    for &producer in producers {
                        if options.contains(&producer) {
                            readers
                                .entry(producer)
                                .or_default()
                                .push(Reader::OwnInput(input));
                        }
                    }
                }
            }
            ContextKind::Ingestion(_) => {}
        }
        for &reader in &options {
            if !data.computes(inputs.kind[reader.0]) {
                continue;
            }
            for (input, producers) in inputs.producer_candidates_per_input[reader.0]
                .iter()
                .enumerate()
            {
                let slots: BTreeSet<ResultSlot> = producers
                    .iter()
                    .filter(|producer| options.contains(producer))
                    .map(|producer| inputs.slot[producer.0])
                    .filter(|&slot| slot != inputs.slot[reader.0])
                    .collect();
                if slots.len() > 1 {
                    let links = slots
                        .into_iter()
                        .map(|slot| (slot, milp.builder.binary("e (slot an input reads)")))
                        .collect();
                    milp.slot_links.insert((context, reader, input), links);
                }
            }
        }
        milp.choice.push(choices);
        milp.readers.push(readers);
    }
    milp.builder.scope = Scope::SharedContexts;
    for &shareable in inputs.context_of_shareable.keys() {
        let selected = milp.builder.binary("x (shareable candidates)");
        milp.selected.insert(shareable, selected);
    }
    for (context, data) in inputs.contexts.iter().enumerate() {
        if let ContextKind::Shared(reader) = data.kind {
            for &option in &data.options {
                if inputs.kind[option.0] == RunKind::Shareable {
                    milp.shared_readers.entry(option).or_default().push(reader);
                }
            }
            let _ = context;
        }
    }
    milp.builder.scope = Scope::Global;
}

/// Rows 1–5.
fn structural_rows(milp: &mut SelectionMilp<'_>) {
    let inputs = milp.inputs;
    let output_refs = output_references(&milp.physical.query_roots);
    // Per `I_r`, the contexts of the ingestion-time shareable candidates of
    // refresh window `r`.
    let mut readers_of_ingestion: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (m, &at) in &inputs.ingestion_context_of_shareable {
        readers_of_ingestion
            .entry(at)
            .or_default()
            .push(inputs.context_of_shareable[m]);
    }
    // 1. Coverage.
    milp.builder.scope = Scope::Queries;
    for (reference, (query, _)) in output_refs.iter().enumerate() {
        let sum: Expression = inputs.output_candidates[reference]
            .iter()
            .filter_map(|out| milp.choice[*query].get(out).copied())
            .map(Expression::from)
            .sum();
        milp.builder.row("1 coverage", constraint!(sum == 1.0));
    }
    for context in 0..inputs.contexts.len() {
        milp.builder.scope = milp.scope_of(context);
        let data = &inputs.contexts[context];
        // 2. One candidate per slot.
        let mut per_slot: BTreeMap<ResultSlot, Vec<Variable>> = BTreeMap::new();
        for (&option, &variable) in &milp.choice[context] {
            per_slot
                .entry(inputs.slot[option.0])
                .or_default()
                .push(variable);
        }
        for variables in per_slot.values().filter(|v| v.len() > 1) {
            let sum: Expression = variables.iter().copied().map(Expression::from).sum();
            milp.builder
                .row("2 one candidate per slot", constraint!(sum <= 1.0));
        }
        // 3. Validity.
        let choices = milp.choice[context].clone();
        let producers_sum = |reader: PhysicalCandidateId, input: usize| -> Expression {
            inputs.producer_candidates_per_input[reader.0][input]
                .iter()
                .filter(|producer| inputs.slot[producer.0] != inputs.slot[reader.0])
                .filter_map(|producer| choices.get(producer).copied())
                .map(Expression::from)
                .sum()
        };
        for (&option, &variable) in &choices {
            let kind = inputs.kind[option.0];
            if data.computes(kind) {
                for input in 0..inputs.producer_candidates_per_input[option.0].len() {
                    let sum = producers_sum(option, input);
                    milp.builder.row("3 validity", constraint!(sum >= variable));
                }
            } else {
                match kind {
                    RunKind::Shareable => {
                        let selected = milp.selected[&option];
                        milp.builder
                            .row("3 validity (boundary)", constraint!(variable <= selected));
                    }
                    RunKind::Ingestion => {
                        let source = milp
                            .ingestion_context_read_by(context)
                            .and_then(|ingestion| milp.choice[ingestion].get(&option).copied());
                        match source {
                            Some(source) => milp
                                .builder
                                .row("3 validity (boundary)", constraint!(variable <= source)),
                            None => milp
                                .builder
                                .row("3 validity (boundary)", constraint!(variable <= 0.0)),
                        }
                    }
                    RunKind::PerContext => milp
                        .builder
                        .row("3 validity (boundary)", constraint!(variable <= 0.0)),
                }
            }
        }
        if let ContextKind::Shared(own) = data.kind {
            let selected = milp.selected[&own];
            for input in 0..inputs.producer_candidates_per_input[own.0].len() {
                let sum: Expression = inputs.producer_candidates_per_input[own.0][input]
                    .iter()
                    .filter_map(|producer| choices.get(producer).copied())
                    .map(Expression::from)
                    .sum();
                milp.builder.row("3 validity", constraint!(sum >= selected));
            }
        }
        // The slot each multi-slot input reads: one, and only a slot the
        // context gives.
        let links: Vec<(InputOf, SlotLinks)> = milp
            .slot_links
            .range((context, PhysicalCandidateId(0), 0)..(context + 1, PhysicalCandidateId(0), 0))
            .map(|(&(_, reader, input), links)| ((reader, input), links.clone()))
            .collect();
        for ((reader, input), links) in links {
            let used = choices[&reader];
            let sum: Expression = links.iter().map(|&(_, link)| Expression::from(link)).sum();
            milp.builder
                .row("3 validity (slot links)", constraint!(sum == used));
            for (slot, link) in links {
                let given: Expression = inputs.producer_candidates_per_input[reader.0][input]
                    .iter()
                    .filter(|producer| inputs.slot[producer.0] == slot)
                    .filter_map(|producer| choices.get(producer).copied())
                    .map(Expression::from)
                    .sum();
                milp.builder
                    .row("3 validity (slot links)", constraint!(link <= given));
            }
        }
        // 4. Used only if read.
        for (&option, &variable) in &choices {
            let mut reads = Expression::from(0.0);
            for reader in milp.readers[context].get(&option).into_iter().flatten() {
                match *reader {
                    Reader::Candidate(reader, input) => {
                        match milp.slot_links.get(&(context, reader, input)) {
                            Some(links) => {
                                let slot = inputs.slot[option.0];
                                for &(at, link) in links {
                                    if at == slot {
                                        reads += link;
                                    }
                                }
                            }
                            None => reads += choices[&reader],
                        }
                    }
                    Reader::Output(_) => reads += 1.0,
                    Reader::OwnInput(_) => {
                        if let ContextKind::Shared(own) = data.kind {
                            reads += milp.selected[&own];
                        }
                    }
                }
            }
            // A candidate of `I_r` is also read by the contexts of the
            // ingestion-time shareable candidates of refresh window `r`.
            for &other in readers_of_ingestion.get(&context).into_iter().flatten() {
                if let Some(&read) = milp.choice[other].get(&option) {
                    reads += read;
                }
            }
            milp.builder
                .row("4 used only if read", constraint!(variable <= reads));
        }
    }
    // 5. Sharing.
    milp.builder.scope = Scope::SharedContexts;
    let mut reads_of: BTreeMap<PhysicalCandidateId, Expression> = BTreeMap::new();
    for choices in &milp.choice {
        for (&option, &variable) in choices {
            if inputs.kind[option.0] == RunKind::Shareable {
                *reads_of.entry(option).or_default() += variable;
            }
        }
    }
    let selected = milp.selected.clone();
    for (shareable, selected) in selected {
        let reads = reads_of.remove(&shareable).unwrap_or_default();
        milp.builder
            .row("5 sharing", constraint!(selected <= reads));
    }
    milp.builder.scope = Scope::Global;
}

/// Order ranks on the links inside each cyclic component of a context's
/// links between candidates it computes (none when the slot graph is
/// acyclic).
fn acyclic_rows(milp: &mut SelectionMilp<'_>) {
    for context in 0..milp.choice.len() {
        milp.builder.scope = milp.scope_of(context);
        let computed: Vec<PhysicalCandidateId> = milp.choice[context]
            .keys()
            .copied()
            .filter(|&c| milp.computes(context, c))
            .collect();
        let index: HashMap<PhysicalCandidateId, usize> =
            computed.iter().enumerate().map(|(i, &c)| (c, i)).collect();
        let mut edges: Vec<(usize, usize)> = Vec::new();
        for (&producer, readers) in &milp.readers[context] {
            let Some(&from) = index.get(&producer) else {
                continue;
            };
            for reader in readers {
                if let Reader::Candidate(reader, _) = reader {
                    edges.push((from, index[reader]));
                }
            }
        }
        let components = strongly_connected_components(computed.len(), &edges);
        let mut rank: HashMap<usize, Variable> = HashMap::new();
        for &(producer, reader) in &edges {
            let size = components.size_of(reader);
            if components.of[producer] != components.of[reader] || size < 2 {
                continue;
            }
            let n = size as f64;
            let [producer_rank, reader_rank] = [producer, reader].map(|node| {
                *rank
                    .entry(node)
                    .or_insert_with(|| milp.builder.continuous("acyclic ranks", Some(n - 1.0)))
            });
            // Both used: the reader ranks above the producer. (Within one
            // context a used reader reads the used candidate of the slot.)
            let (p, r) = (
                milp.choice[context][&computed[producer]],
                milp.choice[context][&computed[reader]],
            );
            milp.builder.row(
                "acyclic ranks",
                constraint!(producer_rank + 1.0 <= reader_rank + n * (2.0 - p - r)),
            );
        }
    }
    milp.builder.scope = Scope::Global;
}

/// Order ranks across contexts: on the graph whose nodes are the shareable
/// candidates' own runs and the runs of each `I_r`, with an edge from a run
/// to each shareable candidate or candidate of an `I_r` its context may
/// read (for a run of `I_r`, its producers there). Rows 3–5 alone admit a
/// cycle of reads across contexts that justifies itself with no output
/// above it (two shareable candidates of one memo group, each listed among
/// the other's producers); only edges inside a cyclic component get a row.
fn acyclic_context_rows(milp: &mut SelectionMilp<'_>) {
    let inputs = milp.inputs;
    // Nodes: (context of the run, candidate).
    let mut nodes: Vec<(usize, PhysicalCandidateId)> = Vec::new();
    for (&m, &context) in &inputs.context_of_shareable {
        nodes.push((context, m));
    }
    for (context, data) in inputs.contexts.iter().enumerate() {
        if let ContextKind::Ingestion(_) = data.kind {
            for &c in milp.choice[context].keys() {
                if milp.computes(context, c) {
                    nodes.push((context, c));
                }
            }
        }
    }
    let index: HashMap<(usize, PhysicalCandidateId), usize> =
        nodes.iter().enumerate().map(|(i, &run)| (run, i)).collect();
    // The node a read of `option` in `context` resolves to.
    let node_read = |context: usize, option: PhysicalCandidateId| -> Option<usize> {
        let run_context = match inputs.kind[option.0] {
            RunKind::Shareable => milp.context_of(option)?,
            RunKind::Ingestion => milp.ingestion_context_read_by(context)?,
            RunKind::PerContext => return None,
        };
        index.get(&(run_context, option)).copied()
    };
    // Edges with their activity: `(from, to, indicators that must all be 1)`.
    let mut edges: Vec<(usize, usize, Vec<Variable>)> = Vec::new();
    for (from, &(context, candidate)) in nodes.iter().enumerate() {
        if let ContextKind::Ingestion(_) = inputs.contexts[context].kind {
            let reader = milp.choice[context][&candidate];
            for producers in &inputs.producer_candidates_per_input[candidate.0] {
                for &producer in producers {
                    if let (Some(to), Some(&read)) = (
                        node_read(context, producer),
                        milp.choice[context].get(&producer),
                    ) {
                        if to != from {
                            edges.push((from, to, vec![reader, read]));
                        }
                    }
                }
            }
        } else {
            for (&option, &read) in &milp.choice[context] {
                if let Some(to) = node_read(context, option) {
                    edges.push((from, to, vec![read]));
                }
            }
        }
    }
    let plain: Vec<(usize, usize)> = edges.iter().map(|&(from, to, _)| (from, to)).collect();
    let components = strongly_connected_components(nodes.len(), &plain);
    milp.builder.scope = Scope::SharedContexts;
    let mut rank: HashMap<usize, Variable> = HashMap::new();
    for (from, to, active) in edges {
        let size = components.size_of(from);
        if components.of[from] != components.of[to] || size < 2 {
            continue;
        }
        let n = size as f64;
        let [reader_rank, read_rank] = [from, to].map(|node| {
            *rank.entry(node).or_insert_with(|| {
                milp.builder
                    .continuous("acyclic ranks (contexts)", Some(n - 1.0))
            })
        });
        let slack = active.len() as f64;
        let on: Expression = active.into_iter().map(Expression::from).sum();
        milp.builder.row(
            "acyclic ranks (contexts)",
            constraint!(read_rank + 1.0 <= reader_rank + n * (slack - on)),
        );
    }
    milp.builder.scope = Scope::Global;
}

/// The strongly connected components of a directed graph.
pub(crate) struct Components {
    /// Per node, its component.
    pub(crate) of: Vec<usize>,
    /// Per component, its number of nodes.
    pub(crate) sizes: Vec<usize>,
}

impl Components {
    pub(crate) fn size_of(&self, node: usize) -> usize {
        self.sizes[self.of[node]]
    }
}

/// The strongly connected components of the graph on nodes `0..count` with
/// `edges` `(from, to)` (Tarjan's algorithm, without recursion).
pub(crate) fn strongly_connected_components(count: usize, edges: &[(usize, usize)]) -> Components {
    let mut successors = vec![Vec::new(); count];
    for &(from, to) in edges {
        successors[from].push(to);
    }
    const UNVISITED: usize = usize::MAX;
    let mut order = vec![UNVISITED; count];
    let mut lowest = vec![0; count];
    let mut on_stack = vec![false; count];
    let mut stack = Vec::new();
    let mut of = vec![UNVISITED; count];
    let mut sizes = Vec::new();
    let mut next = 0;
    for root in 0..count {
        if order[root] != UNVISITED {
            continue;
        }
        let mut path = vec![(root, 0)];
        order[root] = next;
        lowest[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&mut (node, ref mut successor)) = path.last_mut() {
            if let Some(&to) = successors[node].get(*successor) {
                *successor += 1;
                if order[to] == UNVISITED {
                    order[to] = next;
                    lowest[to] = next;
                    next += 1;
                    stack.push(to);
                    on_stack[to] = true;
                    path.push((to, 0));
                } else if on_stack[to] {
                    lowest[node] = lowest[node].min(order[to]);
                }
                continue;
            }
            path.pop();
            if let Some(&(parent, _)) = path.last() {
                lowest[parent] = lowest[parent].min(lowest[node]);
            }
            if lowest[node] == order[node] {
                let component = sizes.len();
                let mut size = 0;
                loop {
                    let member = stack.pop().expect("the node is on the stack");
                    on_stack[member] = false;
                    of[member] = component;
                    size += 1;
                    if member == node {
                        break;
                    }
                }
                sizes.push(size);
            }
        }
    }
    Components { of, sizes }
}

/// The shareable candidates in order: each before every shareable
/// candidate its context may read (readers first), so `a[q, m]` exists
/// before the reach rows of what `m` reads. A cycle among contexts is
/// broken at the first candidate met.
pub(crate) fn shareables_readers_first(milp: &SelectionMilp<'_>) -> Vec<PhysicalCandidateId> {
    let inputs = milp.inputs;
    let mut order = Vec::new();
    let mut state: BTreeMap<PhysicalCandidateId, u8> = BTreeMap::new();
    // Post-order over "m reads c" (children = what m reads), reversed.
    for &start in inputs.context_of_shareable.keys() {
        if state.contains_key(&start) {
            continue;
        }
        let mut stack: Vec<(PhysicalCandidateId, bool)> = vec![(start, false)];
        while let Some((node, done)) = stack.pop() {
            if done {
                state.insert(node, 2);
                order.push(node);
                continue;
            }
            if state.contains_key(&node) {
                continue;
            }
            state.insert(node, 1);
            stack.push((node, true));
            let context = inputs.context_of_shareable[&node];
            for &option in &inputs.contexts[context].options {
                if inputs.kind[option.0] == RunKind::Shareable && !state.contains_key(&option) {
                    stack.push((option, false));
                }
            }
        }
    }
    order.reverse();
    order
}

/// Row 6: reach, from below for every `(q, m)`, from above where a reader
/// constraint counts queries.
fn reach_rows(milp: &mut SelectionMilp<'_>) {
    let inputs = milp.inputs;
    milp.builder.scope = Scope::SharedContexts;
    let order = shareables_readers_first(milp);
    // Which queries may depend on each shareable candidate: read in their
    // context, or in the context of one they may depend on.
    let mut may_reach: BTreeMap<PhysicalCandidateId, BTreeSet<usize>> = BTreeMap::new();
    for query in 0..milp.query_count() {
        for &option in &inputs.contexts[query].options {
            if inputs.kind[option.0] == RunKind::Shareable {
                may_reach.entry(option).or_default().insert(query);
            }
        }
    }
    for &shareable in &order {
        let queries = may_reach.get(&shareable).cloned().unwrap_or_default();
        let context = inputs.context_of_shareable[&shareable];
        for &option in &inputs.contexts[context].options {
            if inputs.kind[option.0] == RunKind::Shareable {
                may_reach
                    .entry(option)
                    .or_default()
                    .extend(queries.iter().copied());
            }
        }
    }
    for &shareable in &order {
        for &query in may_reach.get(&shareable).into_iter().flatten() {
            let reach = milp.builder.continuous("a (reach)", Some(1.0));
            milp.reach.insert((query, shareable), reach);
        }
    }
    for &shareable in &order {
        for &query in may_reach.get(&shareable).into_iter().flatten() {
            let reach = milp.reach[&(query, shareable)];
            if let Some(&direct) = milp.choice[query].get(&shareable) {
                milp.builder
                    .row("6 reach from below", constraint!(reach >= direct));
            }
            for &reader in milp.shared_readers.get(&shareable).into_iter().flatten() {
                let Some(&above) = milp.reach.get(&(query, reader)) else {
                    continue;
                };
                let context = inputs.context_of_shareable[&reader];
                let read = milp.choice[context][&shareable];
                milp.builder.row(
                    "6 reach from below",
                    constraint!(reach >= above + read - 1.0),
                );
            }
        }
    }
    // From above, where needed.
    let exact = shareables_needing_exact_reach(milp);
    for &shareable in &order {
        if !exact.contains(&shareable) {
            continue;
        }
        for &query in may_reach.get(&shareable).into_iter().flatten() {
            let reach = milp.reach[&(query, shareable)];
            let mut sources = Expression::from(0.0);
            if let Some(&direct) = milp.choice[query].get(&shareable) {
                sources += direct;
            }
            for &reader in milp
                .shared_readers
                .get(&shareable)
                .cloned()
                .unwrap_or_default()
                .iter()
            {
                let Some(&above) = milp.reach.get(&(query, reader)) else {
                    continue;
                };
                let context = inputs.context_of_shareable[&reader];
                let read = milp.choice[context][&shareable];
                let through = milp.builder.continuous("y (reach through)", Some(1.0));
                milp.builder
                    .row("6 reach from above", constraint!(through <= above));
                milp.builder
                    .row("6 reach from above", constraint!(through <= read));
                sources += through;
            }
            milp.builder
                .row("6 reach from above", constraint!(reach <= sources));
        }
    }
    milp.builder.scope = Scope::Global;
}

/// The reader constraints of each candidate.
pub(crate) fn reader_constraints_of_candidates<'a>(
    milp: &SelectionMilp<'a>,
) -> BTreeMap<PhysicalCandidateId, Vec<&'a ReaderConstraint>> {
    let physical: &'a PhysicalASAPMemoCandidates = milp.physical;
    let mut of_candidate: BTreeMap<PhysicalCandidateId, Vec<&'a ReaderConstraint>> =
        BTreeMap::new();
    for constraint in &physical.reader_constraints {
        of_candidate
            .entry(constraint.candidate())
            .or_default()
            .push(constraint);
    }
    of_candidate
}

/// The shareable candidates whose `a` must be exact from above: those a
/// counting reader constraint (minimum readers, highest shared
/// materialization) names or whose context holds a constrained candidate,
/// and transitively every shareable candidate whose context reads one.
fn shareables_needing_exact_reach(milp: &SelectionMilp<'_>) -> BTreeSet<PhysicalCandidateId> {
    let inputs = milp.inputs;
    let constraints = reader_constraints_of_candidates(milp);
    let mut stack: Vec<PhysicalCandidateId> = Vec::new();
    for (candidate, constraints) in &constraints {
        let counted = constraints.iter().any(|constraint| {
            matches!(
                constraint,
                ReaderConstraint::MinimumQueryReaders { .. }
                    | ReaderConstraint::HighestSharedQueryMaterialization { .. }
            )
        });
        if !counted {
            continue;
        }
        if inputs.kind[candidate.0] == RunKind::Shareable {
            stack.push(*candidate);
            // Highest shared materialization reads the reach of its
            // reading contexts.
            stack.extend(milp.shared_readers.get(candidate).into_iter().flatten());
        }
        for (context, data) in inputs.contexts.iter().enumerate() {
            if let ContextKind::Shared(own) = data.kind {
                if milp.choice[context].contains_key(candidate) {
                    stack.push(own);
                }
            }
        }
    }
    let mut exact = BTreeSet::new();
    while let Some(shareable) = stack.pop() {
        if exact.insert(shareable) {
            stack.extend(milp.shared_readers.get(&shareable).into_iter().flatten());
        }
    }
    exact
}

/// The queries of `query_outputs` buckets (one per query, each its output
/// references).
fn bucket_queries(query_of_reference: &[usize], query_outputs: &[Vec<usize>]) -> Vec<usize> {
    query_outputs
        .iter()
        .filter_map(|bucket| {
            bucket
                .first()
                .map(|&reference| query_of_reference[reference])
        })
        .collect()
}

/// Row 9: reader constraints, on every node of the constrained candidate.
/// A node in a query's context is reached by that query alone; a node in
/// shareable `m`'s context, and `m` itself, by the queries that depend on
/// `m`. Reads into an `I_r` and per-output-reference reach inside one query
/// are not modeled: the constraint is then applied to every reference of
/// the query (conservative, counted as inexact).
fn reader_constraint_rows(milp: &mut SelectionMilp<'_>) {
    let inputs = milp.inputs;
    let query_of_reference: Vec<usize> = output_references(&milp.physical.query_roots)
        .into_iter()
        .map(|(query, _)| query)
        .collect();
    let constraints = reader_constraints_of_candidates(milp);
    let query_count = milp.query_count();
    milp.builder.scope = Scope::Global;
    let reach_count = |milp: &SelectionMilp<'_>, queries: &[usize], m: PhysicalCandidateId| {
        let mut count = Expression::from(0.0);
        let mut most = 0.0f64;
        for &query in queries {
            if let Some(&reach) = milp.reach.get(&(query, m)) {
                count += reach;
                most += 1.0;
            }
        }
        (count, most)
    };
    for (candidate, constraints) in constraints {
        let shareable = inputs.kind[candidate.0] == RunKind::Shareable;
        // Its uses: (context, p) per context holding it as an option it
        // computes.
        let uses: Vec<(usize, Variable)> = (0..milp.choice.len())
            .filter(|&context| milp.computes(context, candidate))
            .filter_map(|context| {
                milp.choice[context]
                    .get(&candidate)
                    .map(|&variable| (context, variable))
            })
            .collect();
        for constraint in constraints {
            match constraint {
                ReaderConstraint::MinimumQueryReaders {
                    query_outputs,
                    minimum,
                    ..
                } => {
                    let queries = bucket_queries(&query_of_reference, query_outputs);
                    let minimum = *minimum as f64;
                    if shareable {
                        if let Some(&selected) = milp.selected.get(&candidate) {
                            let (count, _) = reach_count(milp, &queries, candidate);
                            milp.builder.row(
                                "9 reader constraints",
                                constraint!(count >= minimum * selected),
                            );
                        }
                    }
                    for &(context, used) in &uses {
                        let count = match inputs.contexts[context].kind {
                            ContextKind::Query(_) => Expression::from(1.0),
                            ContextKind::Shared(m) => reach_count(milp, &queries, m).0,
                            ContextKind::Ingestion(_) => {
                                milp.note_inexact(
                                    "minimum readers of a candidate of an I_r (forbidden)",
                                );
                                Expression::from(0.0)
                            }
                        };
                        milp.builder
                            .row("9 reader constraints", constraint!(count >= minimum * used));
                    }
                }
                ReaderConstraint::HighestSharedQueryMaterialization { query_outputs, .. } => {
                    let queries = bucket_queries(&query_of_reference, query_outputs);
                    if shareable {
                        let Some(&selected) = milp.selected.get(&candidate) else {
                            continue;
                        };
                        // Allowed: a read in a query's context (its readers
                        // there are reached by that query alone), or a read
                        // in the context of `m` that at most one query
                        // reaches.
                        let mut allowed = Expression::from(0.0);
                        for query in 0..query_count {
                            if let Some(&read) = milp.choice[query].get(&candidate) {
                                allowed += read;
                            }
                        }
                        for &reader in milp
                            .shared_readers
                            .get(&candidate)
                            .cloned()
                            .unwrap_or_default()
                            .iter()
                        {
                            let context = inputs.context_of_shareable[&reader];
                            let read = milp.choice[context][&candidate];
                            let (count, most) = reach_count(milp, &queries, reader);
                            let single = milp.builder.binary("g (single-query readers)");
                            milp.builder
                                .row("9 reader constraints", constraint!(single <= read));
                            let slack = (most - 1.0).max(0.0);
                            milp.builder.row(
                                "9 reader constraints",
                                constraint!(count <= 1.0 + slack * (1.0 - single)),
                            );
                            allowed += single;
                        }
                        milp.builder
                            .row("9 reader constraints", constraint!(selected <= allowed));
                    }
                    for &(context, used) in &uses {
                        match inputs.contexts[context].kind {
                            ContextKind::Query(_) => {}
                            ContextKind::Shared(m) => {
                                let (count, most) = reach_count(milp, &queries, m);
                                let slack = (most - 1.0).max(0.0);
                                milp.builder.row(
                                    "9 reader constraints",
                                    constraint!(count <= 1.0 + slack * (1.0 - used)),
                                );
                            }
                            ContextKind::Ingestion(_) => {
                                milp.note_inexact(
                                    "highest shared materialization in an I_r (forbidden)",
                                );
                                milp.builder
                                    .row("9 reader constraints", constraint!(used <= 0.0));
                            }
                        }
                    }
                }
                ReaderConstraint::AllowedOutputs { outputs, .. } => {
                    for query in 0..query_count {
                        let references: Vec<usize> = (0..query_of_reference.len())
                            .filter(|&reference| query_of_reference[reference] == query)
                            .collect();
                        let disallowed = references
                            .iter()
                            .filter(|reference| !outputs.contains(reference))
                            .count();
                        if disallowed == 0 {
                            continue;
                        }
                        if disallowed < references.len() {
                            milp.note_inexact("AllowedOutputs over part of a query's outputs");
                        }
                        forbid_reach(milp, query, candidate, &uses);
                    }
                }
                ReaderConstraint::IncompatibleOutputs { left, right, .. } => {
                    let (left, right) = (query_of_reference[*left], query_of_reference[*right]);
                    if left == right {
                        milp.note_inexact("IncompatibleOutputs within one query (forbidden)");
                        forbid_reach(milp, left, candidate, &uses);
                        continue;
                    }
                    if shareable {
                        if let (Some(&l), Some(&r)) = (
                            milp.reach.get(&(left, candidate)),
                            milp.reach.get(&(right, candidate)),
                        ) {
                            milp.builder
                                .row("9 reader constraints", constraint!(l + r <= 1.0));
                        }
                    }
                    for &(context, used) in &uses {
                        if let ContextKind::Shared(m) = inputs.contexts[context].kind {
                            if let (Some(&l), Some(&r)) =
                                (milp.reach.get(&(left, m)), milp.reach.get(&(right, m)))
                            {
                                milp.builder
                                    .row("9 reader constraints", constraint!(l + r + used <= 2.0));
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Query `query` reaches no node of `candidate`.
fn forbid_reach(
    milp: &mut SelectionMilp<'_>,
    query: usize,
    candidate: PhysicalCandidateId,
    uses: &[(usize, Variable)],
) {
    if let Some(&reach) = milp.reach.get(&(query, candidate)) {
        milp.builder
            .row("9 reader constraints", constraint!(reach <= 0.0));
    } else if let Some(&read) = milp.choice[query].get(&candidate) {
        milp.builder
            .row("9 reader constraints", constraint!(read <= 0.0));
    }
    for &(context, used) in uses {
        match milp.inputs.contexts[context].kind {
            ContextKind::Query(q) if q == query => {
                milp.builder
                    .row("9 reader constraints", constraint!(used <= 0.0));
            }
            ContextKind::Query(_) => {}
            ContextKind::Shared(m) => {
                if let Some(&reach) = milp.reach.get(&(query, m)) {
                    milp.builder
                        .row("9 reader constraints", constraint!(reach + used <= 1.0));
                }
            }
            ContextKind::Ingestion(_) => {
                milp.note_inexact("a reader constraint on a candidate of an I_r (forbidden)");
                milp.builder
                    .row("9 reader constraints", constraint!(used <= 0.0));
            }
        }
    }
}

/// Query `query`'s latency bound: the smaller of its `max_latency_ms` and
/// its fixed repeat interval (`checks::latency_violation`).
pub(crate) fn latency_bound_ms(entry: &QueryWorkloadEntry) -> Option<f64> {
    let interval = entry.recurrence.fixed_interval_ms().map(|ms| ms as f64);
    match (entry.requirements.max_latency_ms, interval) {
        (Some(bound), Some(interval)) => Some(bound.min(interval)),
        (bound, interval) => bound.or(interval),
    }
}

/// The latency terms of context `context`'s computed runs (and of `m`'s
/// own run in its own context): `Σ d · indicator`, and the most it can be.
fn context_work(milp: &mut SelectionMilp<'_>, context: usize) -> (Expression, f64) {
    let mut work = Expression::from(0.0);
    let mut most = 0.0;
    let mut runs: Vec<PhysicalCandidateId> = milp.choice[context]
        .keys()
        .copied()
        .filter(|&c| milp.computes(context, c))
        .collect();
    if let ContextKind::Shared(own) = milp.inputs.contexts[context].kind {
        runs.push(own);
    }
    // Per slot, only the slowest candidate can count (one per slot).
    let mut most_per_slot: BTreeMap<ResultSlot, f64> = BTreeMap::new();
    for candidate in runs {
        let mut run_most = 0.0;
        for term in milp.run_terms(context, candidate) {
            if term.numbers.latency_ms > 0.0 {
                let units = term.numbers.latency_ms / milp.ms_per_latency_unit;
                work += units * term.indicator.clone();
                run_most = f64::max(run_most, units);
            }
        }
        let slot = milp.slot(candidate);
        let entry = most_per_slot.entry(slot).or_default();
        *entry = entry.max(run_most);
    }
    most += most_per_slot.values().sum::<f64>();
    (work, most)
}

/// Row 7: each query's latency, `asap_cost::plan_cost`'s definition: its
/// request time, plus the work of every query-time run it reaches, each
/// once (its own context's, and each query-time shareable candidate's run
/// and context through `T[q, m]`), plus the longest compaction among the
/// ingestion-time states it reaches.
fn latency_rows(milp: &mut SelectionMilp<'_>) -> Result<(), SelectionMilpError> {
    let inputs = milp.inputs;
    let query_count = milp.query_count();
    let request_ms = milp.cost_inputs.measured_operation_costs.ns_per_evaluation * 1e-6;
    // The latency unit: the most any query's latency can be (every
    // candidate's work once, plus the request) is about 1,000 units.
    let most_ms = request_ms
        + inputs
            .candidate_data
            .iter()
            .filter_map(|data| data.usage.as_ref().ok())
            .map(|usage| usage.own_latency_ms)
            .sum::<f64>();
    if most_ms.is_finite() && most_ms > 0.0 {
        milp.ms_per_latency_unit = most_ms / 1_000.0;
    }
    let unit = milp.ms_per_latency_unit;
    let request = request_ms / unit;
    // `L[m]` per query-time shareable candidate.
    milp.builder.scope = Scope::SharedContexts;
    let mut shared_latency: BTreeMap<PhysicalCandidateId, (Variable, f64)> = BTreeMap::new();
    let shareables: Vec<(PhysicalCandidateId, usize)> = inputs
        .context_of_shareable
        .iter()
        .map(|(&m, &context)| (m, context))
        .collect();
    for (m, context) in shareables {
        if !milp.runs_at_query_time(context) {
            continue;
        }
        let (work, most) = context_work(milp, context);
        milp.builder.scope = Scope::SharedContexts;
        if most <= 0.0 {
            continue;
        }
        let latency = milp.builder.continuous("L (latency of m)", None);
        milp.builder.row("7 latency", constraint!(latency >= work));
        shared_latency.insert(m, (latency, most));
    }
    let compaction = cost_terms::compaction_ms(milp);
    for query in 0..query_count {
        let (mut work, mut most) = context_work(milp, query);
        milp.builder.scope = Scope::Queries;
        work += request;
        most += request;
        let mut compactions = Vec::new();
        let mut longest_compaction = 0.0f64;
        for (&m, &(shared, shared_most)) in &shared_latency {
            let Some(&reach) = milp.reach.get(&(query, m)) else {
                continue;
            };
            let through = milp.builder.continuous("T (latency of m in q)", None);
            milp.builder.row(
                "7 latency",
                constraint!(through >= shared - shared_most * (1.0 - reach)),
            );
            work += through;
            most += shared_most;
        }
        for (&m, &ms) in &compaction {
            if let Some(&reach) = milp.reach.get(&(query, m)) {
                if ms > 0.0 {
                    compactions.push((ms / unit, reach));
                    longest_compaction = longest_compaction.max(ms / unit);
                }
            }
        }
        if !compactions.is_empty() {
            let longest = milp.builder.continuous("K (compaction)", None);
            for (ms, reach) in compactions {
                milp.builder
                    .row("7 latency", constraint!(longest >= ms * reach));
            }
            work += longest;
            most += longest_compaction;
        }
        let latency = milp.builder.continuous("L (latency of q)", None);
        let bound = latency_bound_ms(&milp.entries[query])
            .map(|bound| bound / unit)
            .filter(|&bound| bound < most);
        // A binding bound's rows are divided by it, so HiGHS's absolute
        // feasibility tolerance is relative to the bound.
        let per_ms = bound.map_or(1.0, |bound| 1.0 / bound.max(1e-9));
        milp.builder
            .row("7 latency", constraint!(per_ms * latency >= per_ms * work));
        if let Some(bound) = bound {
            milp.builder
                .row("7 latency", constraint!(per_ms * latency <= per_ms * bound));
        }
        let bound = latency_bound_ms(&milp.entries[query]).map(|bound| bound / unit);
        milp.latency.push(latency);
        milp.latency_max
            .push(bound.map_or(most, |bound| bound.min(most)));
    }
    milp.builder.scope = Scope::Global;
    Ok(())
}

/// Row 8: accuracy (A1, revised 2026-10-31), per run `q` depends on: a run
/// unusable for `q` is never depended on, two whose pair is excluded are
/// not depended on together, and the shares depended on are at most each
/// such run's `parts`.
fn accuracy_rows(milp: &mut SelectionMilp<'_>) {
    let inputs = milp.inputs;
    milp.builder.scope = Scope::Global;
    let approximate_runs: Vec<(usize, PhysicalCandidateId)> = milp
        .runs()
        .into_iter()
        .filter(|&(_, candidate)| inputs.candidate_data[candidate.0].approximate)
        .collect();
    for query in 0..milp.query_count() {
        let exact_target = milp.entries[query].requirements.accuracy.is_exact();
        // (candidate, dep, parts) per run `q` may depend on.
        let mut counted: Vec<(PhysicalCandidateId, Expression, usize)> = Vec::new();
        // Where each counted run gives its value: (context, slot), `None`
        // for a shareable candidate's own run. One value per place.
        let mut places: BTreeSet<(usize, Option<ResultSlot>)> = BTreeSet::new();
        for &(context, candidate) in &approximate_runs {
            let Some(weight) = inputs.accuracy_weights[query].get(&candidate).copied() else {
                continue;
            };
            let dep: Option<Expression> = match inputs.contexts[context].kind {
                ContextKind::Query(q) if q == query => {
                    milp.choice[context].get(&candidate).map(|&p| p.into())
                }
                ContextKind::Query(_) => None,
                ContextKind::Shared(own) if own == candidate => {
                    milp.reach.get(&(query, candidate)).map(|&a| a.into())
                }
                ContextKind::Shared(own) => match milp.reach.get(&(query, own)) {
                    Some(&above) => {
                        let p = milp.choice[context][&candidate];
                        Some(
                            milp.builder
                                .and_at_least(
                                    "y (depends on a run in m)",
                                    &[above.into(), p.into()],
                                )
                                .into(),
                        )
                    }
                    None => None,
                },
                ContextKind::Ingestion(_) => {
                    milp.note_inexact("an approximate candidate of an I_r (forbidden)");
                    let p = milp.choice[context][&candidate];
                    milp.builder.row("8 accuracy", constraint!(p <= 0.0));
                    None
                }
            };
            let Some(dep) = dep else {
                continue;
            };
            let usable = match weight {
                _ if exact_target => false,
                AccuracyWeight::One => true,
                AccuracyWeight::Unusable(_) => false,
            };
            if usable {
                places.insert(match inputs.contexts[context].kind {
                    ContextKind::Shared(own) if own == candidate => (context, None),
                    _ => (context, Some(inputs.slot[candidate.0])),
                });
                counted.push((
                    candidate,
                    dep,
                    inputs.candidate_data[candidate.0].accuracy_budget_parts,
                ));
            } else {
                milp.builder.row("8 accuracy", constraint!(dep <= 0.0));
            }
        }
        let mut pairs: Vec<(Expression, Expression)> = Vec::new();
        for &(first, second) in &inputs.accuracy_excluded_pairs[query] {
            for (_, one, _) in counted.iter().filter(|(c, _, _)| *c == first) {
                for (_, other, _) in counted.iter().filter(|(c, _, _)| *c == second) {
                    pairs.push((one.clone(), other.clone()));
                }
            }
        }
        for (one, other) in pairs {
            milp.builder
                .row("8 accuracy (pair)", constraint!(one + other <= 1.0));
        }
        // The most values `q` can depend on together.
        let most = if tests_loose_big_m() {
            counted.len() as f64
        } else {
            places.len() as f64
        };
        if counted.iter().all(|&(_, _, parts)| parts as f64 >= most) {
            continue;
        }
        let shares = milp.builder.continuous("N (approximate values)", None);
        let sum: Expression = counted.iter().map(|(_, dep, _)| dep.clone()).sum();
        milp.builder.row("8 accuracy", constraint!(shares == sum));
        let mut by_parts: BTreeMap<usize, Vec<Expression>> = BTreeMap::new();
        for (_, dep, parts) in &counted {
            if (*parts as f64) < most {
                by_parts.entry(*parts).or_default().push(dep.clone());
            }
        }
        for (parts, deps) in by_parts {
            let reads = milp
                .builder
                .continuous("A (depends at a parts value)", Some(1.0));
            for dep in deps {
                milp.builder.row("8 accuracy", constraint!(reads >= dep));
            }
            let slack = most - parts as f64;
            milp.builder.row(
                "8 accuracy",
                constraint!(shares <= parts as f64 + slack * (1.0 - reads)),
            );
        }
    }
}

/// One independent part of the model (#1095 §5, 3a): a connected component
/// of the graph joining the variables of each row, or all components
/// without a query together. Parts share no variable and no row, and the
/// objective is a sum over variables, so solving each alone and joining
/// their solutions solves the whole model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndependentPart {
    /// The queries whose output references it covers, ascending.
    pub(crate) queries: Vec<usize>,
    /// Its columns (variables in creation order), ascending.
    pub(crate) columns: Vec<usize>,
    /// Its rows, ascending.
    pub(crate) rows: Vec<usize>,
}

/// The independent parts of `milp`, in order of their first query (the
/// part without a query last). Queries fall into one part when they may
/// share a shareable candidate, a run of an `I_r`, a source's raw retention or
/// anything else a row joins.
pub(crate) fn independent_parts(milp: &SelectionMilp<'_>) -> Vec<IndependentPart> {
    let column_of: HashMap<Variable, usize> = milp
        .builder
        .variables
        .iter_variables_with_def()
        .enumerate()
        .map(|(column, (variable, _))| (variable, column))
        .collect();
    let count = column_of.len();
    // Union-find over the columns, joined along each row.
    let mut parent: Vec<usize> = (0..count).collect();
    fn root(parent: &mut [usize], mut node: usize) -> usize {
        while parent[node] != node {
            parent[node] = parent[parent[node]];
            node = parent[node];
        }
        node
    }
    for row in &milp.builder.rows {
        let mut first: Option<usize> = None;
        for (variable, _) in row.expression().linear_coefficients() {
            let column = root(&mut parent, column_of[&variable]);
            match first {
                None => first = Some(column),
                Some(known) if known != column => parent[column] = known,
                Some(_) => {}
            }
        }
    }
    let output_refs = output_references(&milp.physical.query_roots);
    let mut query_of_component: BTreeMap<usize, usize> = BTreeMap::new();
    for (reference, &(query, _)) in output_refs.iter().enumerate() {
        for out in &milp.inputs.output_candidates[reference] {
            if let Some(variable) = milp.choice[query].get(out) {
                let component = root(&mut parent, column_of[variable]);
                query_of_component.entry(component).or_insert(query);
            }
        }
    }
    // The part of each component: its first query's, or the last.
    let part_of = |component: usize| query_of_component.get(&component).copied();
    let mut parts: BTreeMap<Option<usize>, IndependentPart> = BTreeMap::new();
    let mut part_of_column = Vec::with_capacity(count);
    for column in 0..count {
        let key = part_of(root(&mut parent, column));
        part_of_column.push(key);
        parts
            .entry(key)
            .or_insert_with(|| IndependentPart {
                queries: Vec::new(),
                columns: Vec::new(),
                rows: Vec::new(),
            })
            .columns
            .push(column);
    }
    for (reference, &(query, _)) in output_refs.iter().enumerate() {
        let Some(variable) = milp.inputs.output_candidates[reference]
            .iter()
            .find_map(|out| milp.choice[query].get(out))
        else {
            continue;
        };
        let key = part_of_column[column_of[variable]];
        let queries = &mut parts.get_mut(&key).expect("its column's part").queries;
        if !queries.contains(&query) {
            queries.push(query);
        }
    }
    for (index, row) in milp.builder.rows.iter().enumerate() {
        // A row without a variable is a constant check: kept in the part
        // without a query when there is one (`None` sorts first), else in
        // the first query's part.
        let key = row
            .expression()
            .linear_coefficients()
            .next()
            .map(|(variable, _)| part_of_column[column_of[&variable]])
            .unwrap_or_else(|| *parts.keys().next().expect("a part"));
        parts.get_mut(&key).expect("a part").rows.push(index);
    }
    // `None` sorts first; the part without a query goes last.
    let mut parts: Vec<(Option<usize>, IndependentPart)> = parts.into_iter().collect();
    parts.sort_by_key(|(key, _)| key.unwrap_or(usize::MAX));
    parts
        .into_iter()
        .map(|(_, mut part)| {
            part.queries.sort_unstable();
            part
        })
        .collect()
}

/// How one part's solve went, for the report.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(crate) struct PartSolve {
    pub(crate) queries: usize,
    pub(crate) variables: usize,
    pub(crate) binaries: usize,
    pub(crate) rows: usize,
    pub(crate) status: String,
    pub(crate) solve_ms: u64,
    /// HiGHS's dual bound on the part's objective (cost per second).
    pub(crate) dual_bound_per_second: f64,
}

/// One part's model ready for HiGHS: its variables, rows and objective
/// terms, renamed to its own columns when the model has several parts.
struct PartModel {
    variables: ProblemVariables,
    rows: Vec<Constraint>,
    objective_terms: Vec<(Variable, f64)>,
    /// Per column of the part, the column in the whole model.
    columns: Vec<usize>,
    queries: usize,
}

/// A part solved: its columns' values, the HiGHS objective and dual bound
/// (cost per second), and the status.
struct SolvedPart {
    status: SelectionMilpStatus,
    columns: Vec<f64>,
    objective_per_second: f64,
    dual_bound_per_second: f64,
    report: PartSolve,
}

/// Solves `milp` with HiGHS, one solve per independent part
/// ([`independent_parts`]) when `split_into_parts`, each part from the warm
/// start with its share of the time left before `deadline` (by variables).
pub(crate) fn solve_selection_milp(
    milp: SelectionMilp<'_>,
    deadline: Option<Instant>,
    build_ms: u64,
) -> Result<SelectionMilpSolution, SelectionMilpError> {
    let size = milp.size();
    let warm_started = Instant::now();
    let search_budget = Instant::now()
        + deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()) / 5)
            .unwrap_or(WARM_START_MOST_TIME)
            .min(WARM_START_MOST_TIME);
    let completion = super::completion::Completion::new(&milp);
    let start = warm_start::warm_start(&milp, &completion, Some(search_budget));
    let warm_start_ms = warm_started.elapsed().as_millis() as u64;
    let warm_start_cost_per_second = start.as_ref().map(|start| start.cost_per_second);
    // The diagnosis's LP solves do not count against the deadline: it is
    // pushed back by their time.
    #[cfg(feature = "lp-diagnosis")]
    let diagnosis_started = Instant::now();
    #[cfg(feature = "lp-diagnosis")]
    let diagnosis = {
        let column_of: HashMap<Variable, usize> = milp
            .builder
            .variables
            .iter_variables_with_def()
            .enumerate()
            .map(|(column, (variable, _))| (variable, column))
            .collect();
        let start_columns = start.as_ref().map(|start| {
            let mut columns = vec![0.0; column_of.len()];
            for &(variable, value) in &start.values {
                columns[column_of[&variable]] = value;
            }
            columns
        });
        super::lp_diagnosis::Diagnosis::run(&milp, start_columns.as_deref())
            .map(|diagnosis| (diagnosis, start_columns))
    };
    #[cfg(feature = "lp-diagnosis")]
    let deadline = deadline.map(|deadline| deadline + diagnosis_started.elapsed());
    let parts = if milp.split_into_parts {
        independent_parts(&milp)
    } else {
        Vec::new()
    };
    let SelectionMilp {
        builder,
        choice,
        selected,
        slot_links,
        request_cost_per_second,
        objective_terms,
        ..
    } = milp;
    let ModelBuilder {
        variables, rows, ..
    } = builder;
    let column_of: HashMap<Variable, usize> = variables
        .iter_variables_with_def()
        .enumerate()
        .map(|(column, (variable, _))| (variable, column))
        .collect();
    let column_count = column_of.len();
    let start_columns: Option<Vec<f64>> = start.map(|start| {
        let mut columns = vec![0.0; column_count];
        for (variable, value) in start.values {
            columns[column_of[&variable]] = value;
        }
        columns
    });
    let models: Vec<PartModel> = if parts.len() <= 1 {
        vec![PartModel {
            variables,
            rows,
            objective_terms,
            columns: (0..column_count).collect(),
            queries: parts.first().map_or(0, |part| part.queries.len()),
        }]
    } else {
        let definitions: Vec<_> = variables
            .iter_variables_with_def()
            .map(|(_, definition)| definition.clone())
            .collect();
        let mut part_of_column = vec![0; column_count];
        let mut renamed: Vec<Option<Variable>> = vec![None; column_count];
        let mut models: Vec<PartModel> = parts
            .iter()
            .enumerate()
            .map(|(index, part)| {
                let mut variables = ProblemVariables::new();
                for &column in &part.columns {
                    part_of_column[column] = index;
                    renamed[column] = Some(variables.add(definitions[column].clone()));
                }
                PartModel {
                    variables,
                    rows: Vec::with_capacity(part.rows.len()),
                    objective_terms: Vec::new(),
                    columns: part.columns.clone(),
                    queries: part.queries.len(),
                }
            })
            .collect();
        let rename = |variable: Variable| renamed[column_of[&variable]].expect("in a part");
        for (index, part) in parts.iter().enumerate() {
            for &row in &part.rows {
                let row = &rows[row];
                let mut expression = Expression::from(row.expression().constant());
                for (variable, coefficient) in row.expression().linear_coefficients() {
                    expression.add_mul(coefficient, rename(variable));
                }
                models[index].rows.push(if row.is_equality() {
                    constraint!(expression == 0.0)
                } else {
                    constraint!(expression <= 0.0)
                });
            }
        }
        for (variable, coefficient) in objective_terms {
            let column = column_of[&variable];
            models[part_of_column[column]]
                .objective_terms
                .push((rename(variable), coefficient));
        }
        models
    };
    // Smallest first: time a small part leaves goes to the larger ones.
    let mut order: Vec<usize> = (0..models.len()).collect();
    order.sort_by_key(|&index| models[index].columns.len());
    let mut remaining_columns: usize = models.iter().map(|model| model.columns.len()).sum();
    let mut models: Vec<Option<PartModel>> = models.into_iter().map(Some).collect();
    let mut columns = vec![0.0; column_count];
    let mut solved_parts: Vec<Option<SolvedPart>> = (0..models.len()).map(|_| None).collect();
    let started = Instant::now();
    for index in order {
        let model = models[index].take().expect("solved once");
        let share = model.columns.len().max(1) as f64 / remaining_columns.max(1) as f64;
        remaining_columns -= model.columns.len();
        let part_deadline = deadline.map(|deadline| {
            Instant::now()
                + deadline
                    .saturating_duration_since(Instant::now())
                    .mul_f64(share)
        });
        let start = start_columns.as_ref().map(|start| {
            model
                .columns
                .iter()
                .map(|&column| start[column])
                .collect::<Vec<f64>>()
        });
        let part_columns = model.columns.clone();
        let solved = solve_part(model, start, part_deadline)?;
        for (&column, &value) in part_columns.iter().zip(&solved.columns) {
            columns[column] = value;
        }
        solved_parts[index] = Some(solved);
    }
    let solve_ms = started.elapsed().as_millis() as u64;
    let solved_parts: Vec<SolvedPart> = solved_parts
        .into_iter()
        .map(|part| part.expect("every part solved"))
        .collect();
    let status = if solved_parts
        .iter()
        .all(|part| part.status == SelectionMilpStatus::Optimal)
    {
        SelectionMilpStatus::Optimal
    } else {
        SelectionMilpStatus::TimeLimit
    };
    let highs_objective: f64 = solved_parts
        .iter()
        .map(|part| part.objective_per_second)
        .sum();
    let dual_bound: f64 = solved_parts
        .iter()
        .map(|part| part.dual_bound_per_second)
        .sum();
    let relative_gap = match status {
        SelectionMilpStatus::Optimal => 0.0,
        SelectionMilpStatus::TimeLimit => {
            (highs_objective - dual_bound) / highs_objective.abs().max(f64::MIN_POSITIVE)
        }
    };
    // The objective of the integral plan: the incumbent's binaries,
    // rounded, with every continuous variable at its cheapest value (the
    // solver's own value carries its tolerances, relative to the scaled
    // objective). At the deadline, HiGHS's incumbent objective (an upper
    // bound) when the completion does not apply.
    let objective_value = match completion.complete_columns(&columns) {
        Ok(completed) => completed.cost_per_second,
        Err(reason) => {
            warm_start::debug(|| format!("completion of the incumbent failed: {reason}"));
            highs_objective
        }
    };
    warm_start::debug(|| {
        format!("objective: HiGHS {highs_objective:e}, completed {objective_value:e}")
    });
    #[cfg(feature = "lp-diagnosis")]
    if let Some((diagnosis, start_columns)) = diagnosis {
        let incumbent = completion.complete_columns(&columns).ok();
        diagnosis.write(
            start_columns.as_deref(),
            incumbent
                .as_ref()
                .map(|completed| completed.columns.as_slice()),
            objective_value,
            dual_bound,
            &format!("{status:?}"),
        );
    }
    let on = |variable: Variable| columns[column_of[&variable]] > 0.5;
    Ok(SelectionMilpSolution {
        status,
        cost_per_second: objective_value,
        request_cost_per_second,
        cost_lower_bound_per_second: dual_bound,
        relative_gap,
        chosen: choice
            .iter()
            .map(|options| {
                options
                    .iter()
                    .filter(|(_, &variable)| on(variable))
                    .map(|(&candidate, _)| candidate)
                    .collect()
            })
            .collect(),
        selected: selected
            .iter()
            .filter(|(_, &variable)| on(variable))
            .map(|(&candidate, _)| candidate)
            .collect(),
        slot_reads: slot_links
            .iter()
            .filter_map(|(&key, links)| {
                links
                    .iter()
                    .find(|(_, link)| on(*link))
                    .map(|&(slot, _)| (key, slot))
            })
            .collect(),
        size,
        parts: solved_parts.into_iter().map(|part| part.report).collect(),
        build_ms,
        warm_start_ms,
        warm_start_cost_per_second,
        solve_ms,
    })
}

/// Solves one part once with HiGHS, from `start` (its columns' values) when
/// given, with the time left before `deadline` as its time limit.
fn solve_part(
    model: PartModel,
    start: Option<Vec<f64>>,
    deadline: Option<Instant>,
) -> Result<SolvedPart, SelectionMilpError> {
    let PartModel {
        variables,
        rows,
        objective_terms,
        columns: part_columns,
        queries,
    } = model;
    let start_values: Option<Vec<(Variable, f64)>> = start.as_ref().map(|start| {
        variables
            .iter_variables_with_def()
            .map(|(variable, _)| variable)
            .zip(start.iter().copied())
            .collect()
    });
    let scale = objective_scale(&objective_terms, start_values.as_deref());
    let objective: Expression = objective_terms
        .iter()
        .map(|&(variable, coefficient)| coefficient * scale * variable)
        .sum();
    let binaries = variables
        .iter_variables_with_def()
        .filter(|(_, definition)| definition.is_integer())
        .count();
    let row_count = rows.len();
    let problem = variables.minimise(objective).using(highs).with_all(rows);
    let mut model = problem
        .try_into_inner()
        .map_err(|error| SelectionMilpError::Solver(error.to_string()))?;
    model.set_option("mip_rel_gap", 0.0);
    model.set_option("mip_abs_gap", 0.0);
    model.set_option("output_flag", false);
    // HiGHS's MIP search is mostly serial; a thread per core costs
    // gigabytes of address space on a large host (#1036 under 8 GB).
    model.set_option("threads", HIGHS_THREADS);
    let started_with_plan = start.is_some();
    if let Some(start) = &start {
        model
            .try_set_solution(Some(start), None, None, None)
            .map_err(|status| SelectionMilpError::Solver(format!("warm start: {status:?}")))?;
    }
    let mut only_read_back = false;
    if let Some(deadline) = deadline {
        let seconds = deadline
            .saturating_duration_since(Instant::now())
            .as_secs_f64();
        if seconds <= 0.0 {
            if !started_with_plan {
                return Err(SelectionMilpError::DeadlineReached);
            }
            only_read_back = true;
        }
        model.set_option("time_limit", seconds.max(MOMENT_TO_READ_THE_START_S));
    }
    let started = Instant::now();
    let solved = model
        .try_solve()
        .map_err(|status| SelectionMilpError::Solver(format!("{status:?}")))?;
    let solve_ms = started.elapsed().as_millis() as u64;
    use highs::{HighsModelStatus, HighsSolutionStatus};
    let status = match solved.status() {
        HighsModelStatus::Optimal if only_read_back => SelectionMilpStatus::TimeLimit,
        HighsModelStatus::Optimal => SelectionMilpStatus::Optimal,
        HighsModelStatus::Infeasible | HighsModelStatus::UnboundedOrInfeasible => {
            return Err(SelectionMilpError::Infeasible)
        }
        HighsModelStatus::ReachedTimeLimit
        | HighsModelStatus::ReachedInterrupt
        | HighsModelStatus::ReachedIterationLimit
        | HighsModelStatus::ReachedMemoryLimit
        | HighsModelStatus::ReachedSolutionLimit => SelectionMilpStatus::TimeLimit,
        other => return Err(SelectionMilpError::Solver(format!("{other:?}"))),
    };
    if solved.primal_solution_status() != HighsSolutionStatus::Feasible {
        return Err(SelectionMilpError::NoSolution {
            status: format!("{:?}", solved.status()),
        });
    }
    // HiGHS leaves `mip_dual_bound` at 0 for a part without binaries (an
    // LP): its optimal objective is its bound.
    let dual_bound = if binaries > 0 {
        solved
            .double_info_value(c"mip_dual_bound")
            .unwrap_or(f64::NAN)
    } else if status == SelectionMilpStatus::Optimal {
        solved.objective_value()
    } else {
        f64::NAN
    } / scale;
    Ok(SolvedPart {
        status,
        columns: solved.get_solution().columns().to_vec(),
        objective_per_second: solved.objective_value() / scale,
        dual_bound_per_second: dual_bound,
        report: PartSolve {
            queries,
            variables: part_columns.len(),
            binaries,
            rows: row_count,
            status: format!("{status:?}"),
            solve_ms,
            dual_bound_per_second: dual_bound,
        },
    })
}

/// HiGHS's threads (its `threads` option).
pub(crate) const HIGHS_THREADS: i32 = 4;

/// HiGHS's time limit when the warm start used up the time to the deadline.
const MOMENT_TO_READ_THE_START_S: f64 = 1e-3;

/// The most time the warm start takes.
const WARM_START_MOST_TIME: std::time::Duration = std::time::Duration::from_secs(60);

/// The factor the objective is multiplied by for HiGHS: the warm start's
/// cost becomes about 1,000, its largest coefficient at most 10⁷; without
/// a warm start cost, the largest coefficient becomes 1,000.
pub(crate) fn objective_scale(terms: &[(Variable, f64)], start: Option<&[(Variable, f64)]>) -> f64 {
    let largest = terms
        .iter()
        .map(|(_, coefficient)| coefficient.abs())
        .fold(0.0, f64::max);
    if !(largest > 0.0 && largest.is_finite()) {
        return 1.0;
    }
    let started: HashMap<Variable, f64> = start.into_iter().flatten().copied().collect();
    let start_cost: f64 = terms
        .iter()
        .map(|(variable, coefficient)| coefficient * started.get(variable).copied().unwrap_or(0.0))
        .sum();
    if start_cost > 0.0 && start_cost.is_finite() {
        (1_000.0 / start_cost).min(1e7 / largest)
    } else {
        1_000.0 / largest
    }
}

/// Whether this thread builds the big-M bounds as before #1095 step 1
/// (tests only: to compare the tightened bounds with the looser ones).
#[cfg(test)]
pub(crate) fn tests_loose_big_m() -> bool {
    tests::LOOSE_BIG_M.with(|loose| loose.get())
}

#[cfg(not(test))]
pub(crate) fn tests_loose_big_m() -> bool {
    false
}

#[cfg(test)]
pub(crate) mod tests;
