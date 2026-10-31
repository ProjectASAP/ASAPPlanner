//! #1095 §2.0: the owner-scoped selection MILP's variables and structural rows,
//! one HiGHS solve, and the raw solution.
//!
//! ## Variables
//!
//! | Family | Meaning |
//! |---|---|
//! | `x[i]` | instance `i = (s, j)` of shareable candidate `s` is active (binary) |
//! | `s[o, k]` | owner `o` uses stateless candidate `k` of its region (binary) |
//! | `e[o, k, n, k']` | in `o`, input `n` of `k` reads stateless `k'` (binary) |
//! | `b[o, k, n, i]` | in `o`, input `n` of `k` reads instance `i` (binary) |
//! | `o[r, k]` | output reference `r` is stateless `k` of its query owner (binary) |
//! | `ob[r, i]` | output reference `r` is instance `i` (binary) |
//! | `a[q, i]` | query `q` reaches instance `i` (continuous, integral at integral binaries) |
//! | `t[q, i0, i]` | `q` reaches `i` through `i0`: the upper bounds of `a` (continuous) |
//! | `u[i]`, `u[q, i]` | the work of instance `i`'s nodes other than its own, and that work when `q` reaches `i` (latency, continuous) |
//! | `g[i0, i]` | instance `i0`, reading `i`, is reached by at most one query (binary) |
//! | `rank[o, k]`, `rank[i]` | order ranks on cyclic components of links (row 5, continuous) |
//! | `N[q]`, `A[q, p]` | approximate values `q` reads; whether it reads one sized for `p` parts (continuous) |
//! | `L[q]`, `C[q]` | query `q`'s latency and its compaction term, ms (continuous) |
//!
//! The cost families are [`super::cost_terms`]'s.
//!
//! A query owner's or an instance owner's use of a stateless class is one
//! node of the plan; an instance is one node. Reach, latency and accuracy
//! are therefore written per node, as `asap_cost::plan_cost` measures
//! them: a query reaching two instances of one candidate reads two
//! approximate values and adds both instances' work to its latency.
//!
//! ## Rows
//!
//! 1. Outputs: `Σ_k o[r, k] + Σ_i ob[r, i] = 1`; `o[r, k] ≤ s[q, k]`.
//! 2. One producer per used input: `Σ e + Σ b = s[o, k]` (or `x[i]` for the
//!    instance's own class).
//! 3. Used only if read: `e[o, k, n, k'] ≤ s[o, k']`;
//!    `s[o, k] ≤ Σ e[o, ·, ·, k] + Σ_r o[r, k]`.
//! 4. Instances: `b ≤ x[i]`, `ob ≤ x[i]`; `x[i] ≤` its reads; symmetry
//!    `x[s, j] ≥ x[s, j + 1]`.
//! 5. Acyclic (row 5 of the issue, "copies suffice", was deleted; the number
//!    is reused): no class reads itself in an owner, and order ranks on the
//!    links inside each cyclic component of an owner's links and of the
//!    instances' reads ([`acyclic_rows`]).
//! 6. Reach: `a[q, i] ≥ b` for `q`'s own reads of `i`;
//!    `a[q, i] ≥ a[q, i0] + b − 1` when `i0`'s owner reads `i`. Where a
//!    reader constraint needs it to be exact from above too (minimum
//!    readers, highest shared materialization), and for every instance
//!    reading such an instance: `a[q, i] ≤ Σ own reads + Σ_{i0} t[q, i0, i]`,
//!    `t ≤ a[q, i0]`, `t ≤ Σ b(i0 reads i)`.
//! 7. Reader constraints on the nodes of the constrained class.
//! 8. Latency: `L[q] ≥ eval + Σ d · s[q, k] + Σ_i (d_i · a[q, i] +
//!    u[q, i]) + C[q]`, `u[q, i] ≥ u[i] − ū[i] (1 − a[q, i])`, `C[q] ≥ c_i ·
//!    a[q, i]`, `L[q] ≤ bound(q)`, where `bound(q)` is the smaller of
//!    `max_latency_ms` and the fixed repeat interval; a binding bound's two
//!    rows are divided by the bound, so HiGHS's absolute tolerance is
//!    relative to it.
//! 9. Accuracy (A1, revised 2026-10-30): `N[q] = Σ a[q, i]` over the
//!    approximate instances `q` may reach; per value `p` of `parts`, `A[q,
//!    p] ≥ a[q, i]` for its instances and `N[q] ≤ p + M (1 − A[q, p])`;
//!    `a[q, i] = 0` for an instance unusable for `q`, and `a[q, i] + a[q, j]
//!    ≤ 1` for a pair excluded together (both from the composition, see
//!    [`crate::owners`]).
//!
//! The solve starts from [`super::warm_start`]'s plan (with every variable's
//! value), so a deadline ends with a plan.
//!
//! Every cost row is a lower bound that minimization makes tight; every
//! structural row is exact at integral binaries.
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

use crate::owners::{held_state, AccuracyWeight, MilpInputs, OwnerRegion};

use super::cost_terms::{self, NodeTerm};
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
    /// No plan meets every row, and query `query` has none even alone (the
    /// warm start found no derivation, or none within its latency bound).
    NoPlanForQuery { query: usize, reason: String },
    /// A rate or another number `asap-cost` could not compute.
    Cost(String),
    /// HiGHS failed.
    Solver(String),
}

/// How the MILP is built.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SelectionMilpOptions {
    /// Write the accuracy rows (constraint 9); off when the facade ignores
    /// accuracy guarantees.
    pub(crate) enforce_accuracy: bool,
}

/// One possible instance `(s, j)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Instance {
    pub(crate) class: PhysicalCandidateId,
    /// `j`, below `m_s`.
    pub(crate) index: usize,
    /// Index into [`MilpInputs::instance_owners`].
    pub(crate) owners: usize,
}

/// One owner's variables. Owners `0..Q` are the query owners, `Q + i` the
/// owner of instance `i`.
#[derive(Debug, Default)]
pub(crate) struct OwnerVariables {
    /// `s[o, k]` per stateless class of its region.
    pub(crate) used: BTreeMap<PhysicalCandidateId, Variable>,
    /// `e[o, k, n, k']`: `(reader, input, producer)`.
    pub(crate) links: Vec<((PhysicalCandidateId, usize, PhysicalCandidateId), Variable)>,
    /// `b[o, k, n, i]`: `(reader, input, instance)`.
    pub(crate) reads: Vec<((PhysicalCandidateId, usize, usize), Variable)>,
}

/// The size of one family of variables and rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FamilySize {
    pub(crate) variables: usize,
    pub(crate) binaries: usize,
    pub(crate) rows: usize,
    /// Nonzero coefficients of its rows.
    pub(crate) nonzeros: usize,
}

/// Variables and rows, counted per family.
pub(crate) struct ModelBuilder {
    pub(crate) variables: ProblemVariables,
    pub(crate) rows: Vec<Constraint>,
    /// The family of each row of `rows`.
    pub(crate) row_families: Vec<&'static str>,
    /// Every binary variable, in the order added.
    pub(crate) binaries: Vec<Variable>,
    pub(crate) families: BTreeMap<&'static str, FamilySize>,
}

impl ModelBuilder {
    fn new() -> Self {
        Self {
            variables: ProblemVariables::new(),
            rows: Vec::new(),
            row_families: Vec::new(),
            binaries: Vec::new(),
            families: BTreeMap::new(),
        }
    }

    pub(crate) fn binary(&mut self, family: &'static str) -> Variable {
        let size = self.families.entry(family).or_default();
        size.variables += 1;
        size.binaries += 1;
        let binary = self.variables.add(variable().binary());
        self.binaries.push(binary);
        binary
    }

    /// A continuous variable in `[0, max]` (unbounded above for `None`).
    pub(crate) fn continuous(&mut self, family: &'static str, max: Option<f64>) -> Variable {
        self.families.entry(family).or_default().variables += 1;
        let definition = variable().min(0.0);
        self.variables.add(match max {
            Some(max) => definition.max(max),
            None => definition,
        })
    }

    pub(crate) fn row(&mut self, family: &'static str, row: Constraint) {
        let size = self.families.entry(family).or_default();
        size.rows += 1;
        size.nonzeros += row.expression().linear_coefficients().count();
        self.rows.push(row);
        self.row_families.push(family);
    }

    /// `and ≥ Σ terms − (n − 1)`, `and ≥ 0`: the AND of `n` indicators,
    /// tight under a cost or a `≤` row that charges it.
    pub(crate) fn and_at_least(&mut self, family: &'static str, terms: &[Expression]) -> Variable {
        let and = self.continuous(family, Some(1.0));
        let sum: Expression = terms.iter().cloned().sum();
        let count = terms.len() as f64;
        self.row(family, constraint!(and >= sum - (count - 1.0)));
        and
    }
}

/// The built model, before the solve.
pub(crate) struct SelectionMilp<'a> {
    pub(crate) builder: ModelBuilder,
    pub(crate) physical: &'a PhysicalASAPMemoCandidates,
    /// Per candidate, whether it holds state (its nodes are instances).
    pub(crate) shareable: Vec<bool>,
    pub(crate) inputs: &'a MilpInputs,
    pub(crate) entries: &'a [QueryWorkloadEntry],
    pub(crate) cost_inputs: &'a CostInputs,
    pub(crate) instances: Vec<Instance>,
    /// Per class, its instances (indexes into `instances`), by `j`.
    pub(crate) instances_of_candidate: HashMap<PhysicalCandidateId, Vec<usize>>,
    /// Indexed by owner (`0..Q` queries, `Q + i` instances).
    pub(crate) owners: Vec<OwnerVariables>,
    /// `x[i]`.
    pub(crate) active: Vec<Variable>,
    /// Per output reference, `o[r, k]` for its stateless classes.
    pub(crate) outputs: Vec<Vec<(PhysicalCandidateId, Variable)>>,
    /// Per output reference, `ob[r, i]`.
    pub(crate) output_reads: Vec<Vec<(usize, Variable)>>,
    /// The query of each output reference.
    pub(crate) query_of_reference: Vec<usize>,
    /// `a[q, i]`, for each query that may reach `i`.
    pub(crate) reach: HashMap<(usize, usize), Variable>,
    /// Per instance, every variable that reads it: `(owner, variable)`,
    /// `ob` reads under their query's owner.
    pub(crate) reads_of_instance: Vec<Vec<(usize, Variable)>>,
    /// `L[q]`.
    pub(crate) latency: Vec<Variable>,
    /// An upper bound of each `L[q]` (its bound, or the most it can be).
    pub(crate) latency_max_ms: Vec<f64>,
    /// The cost model 1 objective, in cost per second, without the request
    /// fees (constant for every plan, added after selection).
    pub(crate) objective_terms: Vec<(Variable, f64)>,
    /// The request fees per second (`plan_cost` charges each query's
    /// request to its first node; the same for every plan).
    pub(crate) request_cost_per_second: f64,
    /// Terms encoded inexactly, by reason (open questions on #1095).
    pub(crate) inexact: BTreeMap<&'static str, usize>,
    /// `g[i0, i]`: instance `i0`, which reads instance `i`, is reached by at
    /// most one query (highest shared materialization).
    pub(crate) single_query_consumers: Vec<(usize, usize, Variable)>,
    /// Per producer node and outputs, the variable `≥` its terms giving
    /// them (`cost_terms::link_output_indicator`).
    pub(crate) producer_gives: HashMap<(usize, PhysicalCandidateId, Vec<usize>), Variable>,
    /// The nodes whose terms are being built (cycles of links).
    pub(crate) terms_in_progress: HashSet<(usize, PhysicalCandidateId)>,
    /// The nodes' terms, cached per `(owner, class)`.
    pub(crate) node_terms_cache: HashMap<(usize, PhysicalCandidateId), Vec<NodeTerm>>,
    /// Per candidate, whether a reader tells its outputs apart
    /// ([`cost_terms::outputs_that_matter`]).
    pub(crate) output_matters: Vec<bool>,
}

/// Variables and rows of one model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SelectionMilpSize {
    pub(crate) variables: usize,
    pub(crate) binaries: usize,
    pub(crate) rows: usize,
    pub(crate) nonzeros: usize,
    pub(crate) families: BTreeMap<&'static str, FamilySize>,
    /// AND rows of usage that depends on more than one input's producer.
    pub(crate) producer_and_rows: usize,
    pub(crate) inexact: BTreeMap<&'static str, usize>,
}

/// How the solve ended with a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionMilpStatus {
    Optimal,
    /// HiGHS's time limit (the deadline), with an integral incumbent; also
    /// when the warm start used up the time to the deadline and HiGHS only
    /// read it back.
    TimeLimit,
}

/// What produces one input or one output: a stateless class in the same
/// owner, or an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChosenProducer {
    Stateless(PhysicalCandidateId),
    Instance {
        class: PhysicalCandidateId,
        index: usize,
    },
}

/// One owner's chosen nodes and links.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ChosenOwner {
    /// The stateless classes it uses, ascending.
    pub(crate) classes: Vec<PhysicalCandidateId>,
    /// `(reader, input, producer)`; the reader is a class it uses or the
    /// instance's own class.
    pub(crate) links: Vec<(PhysicalCandidateId, usize, ChosenProducer)>,
}

/// The raw solution, ready to decode (#1095 C5).
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
    /// Per output reference, what produces it.
    pub(crate) outputs: Vec<ChosenProducer>,
    /// Per query, its owner's choices.
    pub(crate) query_owners: Vec<ChosenOwner>,
    /// The active instances `(class, j)` with their owners' choices.
    pub(crate) instances: Vec<(PhysicalCandidateId, usize, ChosenOwner)>,
    pub(crate) size: SelectionMilpSize,
    pub(crate) build_ms: u64,
    /// Building the warm start, HiGHS's first incumbent.
    pub(crate) warm_start_ms: u64,
    /// The warm start's objective (cost per second without the request
    /// fees, like `cost_per_second`), when its completion costed it: `None`
    /// when some query has no plan alone or the completion found the warm
    /// start infeasible, and HiGHS started without it. `cost_per_second` is
    /// at most this.
    pub(crate) warm_start_cost_per_second: Option<f64>,
    pub(crate) solve_ms: u64,
}

impl<'a> SelectionMilp<'a> {
    pub(crate) fn query_count(&self) -> usize {
        self.inputs.query_owners.len()
    }

    /// The owner of instance `i`.
    pub(crate) fn instance_owner(&self, instance: usize) -> usize {
        self.query_count() + instance
    }

    /// Owner `owner`'s region.
    pub(crate) fn region(&self, owner: usize) -> &'a OwnerRegion {
        let inputs: &'a MilpInputs = self.inputs;
        match owner.checked_sub(self.query_count()) {
            None => &inputs.query_owners[owner],
            Some(instance) => &inputs.instance_owners[self.instances[instance].owners].region,
        }
    }

    /// The instance owner `owner`'s instance, `None` for a query owner.
    pub(crate) fn instance_of_owner(&self, owner: usize) -> Option<usize> {
        owner.checked_sub(self.query_count())
    }

    /// Whether `candidate` holds state (its nodes are instances).
    pub(crate) fn shareable(&self, candidate: PhysicalCandidateId) -> bool {
        self.shareable[candidate.0]
    }

    /// The variable that says owner `owner` uses `class`: `s[o, k]`, or
    /// `x[i]` for an instance owner's own class.
    pub(crate) fn node_variable(
        &self,
        owner: usize,
        class: PhysicalCandidateId,
    ) -> Option<Variable> {
        if let Some(instance) = self.instance_of_owner(owner) {
            if self.instances[instance].class == class {
                return Some(self.active[instance]);
            }
        }
        self.owners[owner].used.get(&class).copied()
    }

    /// The nodes owner `owner` may use: its region's classes and, for an
    /// instance owner, its own class.
    pub(crate) fn nodes_of_owner(&self, owner: usize) -> Vec<PhysicalCandidateId> {
        let mut nodes: Vec<PhysicalCandidateId> = self.owners[owner].used.keys().copied().collect();
        if let Some(instance) = self.instance_of_owner(owner) {
            nodes.push(self.instances[instance].class);
        }
        nodes
    }

    pub(crate) fn size(&self) -> SelectionMilpSize {
        let families = self.builder.families.clone();
        SelectionMilpSize {
            variables: families.values().map(|f| f.variables).sum(),
            binaries: families.values().map(|f| f.binaries).sum(),
            rows: families.values().map(|f| f.rows).sum(),
            nonzeros: families.values().map(|f| f.nonzeros).sum(),
            producer_and_rows: families.get("t_producers (AND)").map_or(0, |f| f.rows),
            families,
            inexact: self.inexact.clone(),
        }
    }

    pub(crate) fn note_inexact(&mut self, reason: &'static str) {
        *self.inexact.entry(reason).or_default() += 1;
    }

    /// The nodes' terms of owner `owner`'s use of `class`
    /// ([`cost_terms::node_terms`]), cached.
    pub(crate) fn node_terms(&mut self, owner: usize, class: PhysicalCandidateId) -> Vec<NodeTerm> {
        if let Some(terms) = self.node_terms_cache.get(&(owner, class)) {
            return terms.clone();
        }
        let terms = cost_terms::node_terms(self, owner, class);
        self.node_terms_cache.insert((owner, class), terms.clone());
        terms
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
    if entries.len() != query_count || inputs.query_owners.len() != query_count {
        return Err(SelectionMilpError::EntryCount {
            roots: query_count,
            entries: entries.len(),
        });
    }
    let usable = |class: PhysicalCandidateId| inputs.candidate_data[class.0].usage.is_ok();
    let mut instances = Vec::new();
    let mut instances_of_candidate: HashMap<PhysicalCandidateId, Vec<usize>> = HashMap::new();
    for (owners, instance_owners) in inputs.instance_owners.iter().enumerate() {
        if !usable(instance_owners.class) {
            continue;
        }
        for index in 0..instance_owners.instance_count {
            instances_of_candidate
                .entry(instance_owners.class)
                .or_default()
                .push(instances.len());
            instances.push(Instance {
                class: instance_owners.class,
                index,
                owners,
            });
        }
    }
    let query_of_reference: Vec<usize> = output_references(&physical.query_roots)
        .into_iter()
        .map(|(query, _)| query)
        .collect();
    let mut milp = SelectionMilp {
        builder: ModelBuilder::new(),
        physical,
        shareable: (0..physical.candidates.len())
            .map(|id| {
                let candidate = &physical.candidates[id];
                held_state(
                    physical.physical_node(PhysicalCandidateId(id)),
                    candidate.timing,
                    candidate.materialization,
                )
                .is_some()
            })
            .collect(),
        inputs,
        entries,
        cost_inputs,
        instances_of_candidate,
        owners: Vec::new(),
        active: Vec::new(),
        outputs: vec![Vec::new(); query_of_reference.len()],
        output_reads: vec![Vec::new(); query_of_reference.len()],
        query_of_reference,
        reach: HashMap::new(),
        reads_of_instance: vec![Vec::new(); instances.len()],
        latency: Vec::new(),
        latency_max_ms: Vec::new(),
        objective_terms: Vec::new(),
        request_cost_per_second: 0.0,
        inexact: BTreeMap::new(),
        single_query_consumers: Vec::new(),
        terms_in_progress: HashSet::new(),
        producer_gives: HashMap::new(),
        node_terms_cache: HashMap::new(),
        output_matters: cost_terms::outputs_that_matter(inputs),
        instances,
    };
    structural_rows(&mut milp);
    acyclic_rows(&mut milp);
    reach_rows(&mut milp);
    reader_constraint_rows(&mut milp);
    latency_rows(&mut milp)?;
    if options.enforce_accuracy {
        accuracy_rows(&mut milp);
    }
    cost_terms::encode_objective(&mut milp)?;
    Ok(milp)
}

/// Rows 1–4: outputs, one producer per used input, used only if read, and
/// instances. (Row 5, "copies suffice", was deleted on 2026-10-30: Stage 2
/// emits no copies.)
fn structural_rows(milp: &mut SelectionMilp<'_>) {
    let inputs = milp.inputs;
    let usable = |class: PhysicalCandidateId| inputs.candidate_data[class.0].usage.is_ok();
    let query_count = milp.query_count();
    milp.active = (0..milp.instances.len())
        .map(|_| milp.builder.binary("x (instances)"))
        .collect();
    // Owner variables.
    let owner_count = query_count + milp.instances.len();
    for owner in 0..owner_count {
        let region = milp.region(owner);
        let mut variables = OwnerVariables::default();
        for &class in &region.classes {
            if usable(class) {
                let used = milp.builder.binary("s (stateless uses)");
                variables.used.insert(class, used);
            }
        }
        let own = milp
            .instance_of_owner(owner)
            .map(|instance| milp.instances[instance].class);
        let reader_exists = |reader: PhysicalCandidateId| {
            variables.used.contains_key(&reader) || Some(reader) == own
        };
        for &(reader, input, producer) in &region.links {
            // An owner runs each class once, so a class reading itself
            // (a widened scan's `Project` over its own memo group) is a
            // cycle in every plan (row 5).
            if reader == producer {
                continue;
            }
            if reader_exists(reader) && variables.used.contains_key(&producer) {
                let link = milp.builder.binary("e (stateless links)");
                variables.links.push(((reader, input, producer), link));
            }
        }
        for &(reader, input, read) in &region.boundary_reads {
            if !reader_exists(reader) {
                continue;
            }
            for &instance in milp.instances_of_candidate.get(&read).into_iter().flatten() {
                // An instance does not read itself.
                if milp.instance_of_owner(owner) == Some(instance) {
                    continue;
                }
                let link = milp.builder.binary("b (instance reads)");
                variables.reads.push(((reader, input, instance), link));
                milp.reads_of_instance[instance].push((owner, link));
            }
        }
        milp.owners.push(variables);
    }
    // 1. Outputs.
    for reference in 0..milp.query_of_reference.len() {
        let query = milp.query_of_reference[reference];
        let mut sum = Expression::from(0.0);
        for &class in &inputs.output_candidates[reference] {
            if milp.shareable(class) {
                for &instance in milp
                    .instances_of_candidate
                    .get(&class)
                    .into_iter()
                    .flatten()
                {
                    let read = milp.builder.binary("ob (output instances)");
                    milp.output_reads[reference].push((instance, read));
                    milp.reads_of_instance[instance].push((query, read));
                    sum += read;
                }
            } else if let Some(&used) = milp.owners[query].used.get(&class) {
                let output = milp.builder.binary("o (output classes)");
                milp.builder.row("1 outputs", constraint!(output <= used));
                milp.outputs[reference].push((class, output));
                sum += output;
            }
        }
        milp.builder.row("1 outputs", constraint!(sum == 1.0));
    }
    // 2 and 3, per owner.
    for owner in 0..owner_count {
        let mut per_input: HashMap<(PhysicalCandidateId, usize), Expression> = HashMap::new();
        let mut reads_of: HashMap<PhysicalCandidateId, Expression> = HashMap::new();
        for &((reader, input, producer), link) in &milp.owners[owner].links {
            *per_input.entry((reader, input)).or_default() += link;
            *reads_of.entry(producer).or_default() += link;
        }
        for &((reader, input, _), read) in &milp.owners[owner].reads {
            *per_input.entry((reader, input)).or_default() += read;
        }
        if owner < query_count {
            for (reference, outputs) in milp.outputs.iter().enumerate() {
                if milp.query_of_reference[reference] == owner {
                    for &(class, output) in outputs {
                        *reads_of.entry(class).or_default() += output;
                    }
                }
            }
        }
        // 2. One producer per used input.
        for node in milp.nodes_of_owner(owner) {
            let used = milp
                .node_variable(owner, node)
                .expect("a node of the owner");
            for input in 0..inputs.producer_candidates_per_input[node.0].len() {
                let producers = per_input.remove(&(node, input)).unwrap_or_default();
                milp.builder
                    .row("2 one producer per input", constraint!(producers == used));
            }
        }
        // 3. Used only if read.
        let links = milp.owners[owner].links.clone();
        for ((_, _, producer), link) in links {
            let used = milp.owners[owner].used[&producer];
            milp.builder
                .row("3 used only if read", constraint!(link <= used));
        }
        let used: Vec<(PhysicalCandidateId, Variable)> = milp.owners[owner]
            .used
            .iter()
            .map(|(&class, &used)| (class, used))
            .collect();
        for (class, used) in used {
            let reads = reads_of.remove(&class).unwrap_or_default();
            milp.builder
                .row("3 used only if read", constraint!(used <= reads));
        }
    }
    // 4. Instances.
    for instance in 0..milp.instances.len() {
        let active = milp.active[instance];
        let reads: Vec<Variable> = milp.reads_of_instance[instance]
            .iter()
            .map(|&(_, read)| read)
            .collect();
        for &read in &reads {
            milp.builder.row("4 instances", constraint!(read <= active));
        }
        let sum: Expression = reads.iter().copied().sum();
        milp.builder.row("4 instances", constraint!(active <= sum));
    }
    let symmetric: Vec<Vec<usize>> = milp.instances_of_candidate.values().cloned().collect();
    for of_class in symmetric {
        for pair in of_class.windows(2) {
            let (earlier, later) = (milp.active[pair[0]], milp.active[pair[1]]);
            milp.builder
                .row("4 symmetry breaking", constraint!(earlier >= later));
        }
    }
}

/// Row 5: the plan is acyclic. Candidate links can close cycles (a widened
/// scan's `Project` lists its own memo group's candidates, itself included,
/// as producers), and rows 2–3 alone admit a cycle that supports itself
/// with no source below it. A plan's nodes are an owner's stateless uses
/// and the instances, and a cycle of nodes either stays inside one owner
/// (its stateless uses; the instance's own node is only read there) or
/// passes through instances, so two families of order rows suffice:
/// - per owner, on the links inside a strongly connected component of its
///   links (`k` reading `k'`): `rank[o, k'] + 1 ≤ rank[o, k] + n (1 − e)`,
///   with `rank ∈ [0, n − 1]` and `n` the component's size (a class reading
///   itself has no link at all, see [`structural_rows`]);
/// - on instances, per component of "the owner of `i0` may read `i`":
///   `rank[i] + 1 ≤ rank[i0] + n (1 − b)` for each such read `b`.
///
/// Only links inside a cyclic component get a row, so a catalog without
/// cycles adds nothing.
fn acyclic_rows(milp: &mut SelectionMilp<'_>) {
    for owner in 0..milp.owners.len() {
        let links = milp.owners[owner].links.clone();
        // Its stateless uses and, for an instance owner, its own class (a
        // reader only).
        let classes = milp.nodes_of_owner(owner);
        let index: HashMap<PhysicalCandidateId, usize> = classes
            .iter()
            .enumerate()
            .map(|(index, &class)| (class, index))
            .collect();
        let edges: Vec<(usize, usize)> = links
            .iter()
            .map(|&((reader, _, producer), _)| (index[&producer], index[&reader]))
            .collect();
        let components = strongly_connected_components(classes.len(), &edges);
        let mut rank: HashMap<usize, Variable> = HashMap::new();
        for (&(producer, reader), &(_, link)) in edges.iter().zip(&links) {
            let size = components.size_of(reader);
            if components.of[producer] != components.of[reader] || size < 2 {
                continue;
            }
            let n = size as f64;
            let [producer_rank, reader_rank] = [producer, reader].map(|node| {
                *rank.entry(node).or_insert_with(|| {
                    milp.builder
                        .continuous("5 acyclic (stateless ranks)", Some(n - 1.0))
                })
            });
            milp.builder.row(
                "5 acyclic (stateless ranks)",
                constraint!(producer_rank + 1.0 <= reader_rank + n * (1.0 - link)),
            );
        }
    }
    let mut reads: Vec<(usize, usize, Variable)> = Vec::new();
    for reader in 0..milp.instances.len() {
        let owner = milp.instance_owner(reader);
        for &((_, _, read), variable) in &milp.owners[owner].reads {
            reads.push((read, reader, variable));
        }
    }
    let edges: Vec<(usize, usize)> = reads
        .iter()
        .map(|&(read, reader, _)| (read, reader))
        .collect();
    let components = strongly_connected_components(milp.instances.len(), &edges);
    let mut rank: HashMap<usize, Variable> = HashMap::new();
    for (read, reader, variable) in reads {
        let size = components.size_of(reader);
        if components.of[read] != components.of[reader] || size < 2 {
            continue;
        }
        let n = size as f64;
        let [read_rank, reader_rank] = [read, reader].map(|instance| {
            *rank.entry(instance).or_insert_with(|| {
                milp.builder
                    .continuous("5 acyclic (instance ranks)", Some(n - 1.0))
            })
        });
        milp.builder.row(
            "5 acyclic (instance ranks)",
            constraint!(read_rank + 1.0 <= reader_rank + n * (1.0 - variable)),
        );
    }
}

/// The strongly connected components of a directed graph.
struct Components {
    /// Per node, its component.
    of: Vec<usize>,
    /// Per component, its number of nodes.
    sizes: Vec<usize>,
}

impl Components {
    fn size_of(&self, node: usize) -> usize {
        self.sizes[self.of[node]]
    }
}

/// The strongly connected components of the graph on nodes `0..count` with
/// `edges` `(from, to)` (Tarjan's algorithm, without recursion).
fn strongly_connected_components(count: usize, edges: &[(usize, usize)]) -> Components {
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
        // (node, index of its next successor to visit)
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

/// For each instance, the instances whose owners may read it (`i0`).
fn readers_of_instances(milp: &SelectionMilp<'_>) -> Vec<BTreeSet<usize>> {
    let mut readers = vec![BTreeSet::new(); milp.instances.len()];
    for instance in 0..milp.instances.len() {
        let owner = milp.instance_owner(instance);
        for &((_, _, read), _) in &milp.owners[owner].reads {
            readers[read].insert(instance);
        }
    }
    readers
}

/// Row 6: reach, from below for every instance, from above where a reader
/// constraint needs it exact.
fn reach_rows(milp: &mut SelectionMilp<'_>) {
    let inputs = milp.inputs;
    for instance in 0..milp.instances.len() {
        let class = milp.instances[instance].class;
        for &query in &inputs.queries_reaching[class.0] {
            let reach = milp.builder.continuous("a (reach)", Some(1.0));
            milp.reach.insert((query, instance), reach);
        }
    }
    // From below: the query's own reads.
    for instance in 0..milp.instances.len() {
        let direct: Vec<(usize, Variable)> = milp.reads_of_instance[instance]
            .iter()
            .filter(|&&(owner, _)| owner < milp.query_count())
            .copied()
            .collect();
        for (query, read) in direct {
            if let Some(&reach) = milp.reach.get(&(query, instance)) {
                milp.builder
                    .row("6 reach from below", constraint!(reach >= read));
            }
        }
    }
    // From below: through an instance that reads it.
    for reader in 0..milp.instances.len() {
        let owner = milp.instance_owner(reader);
        let reads: Vec<(usize, Variable)> = milp.owners[owner]
            .reads
            .iter()
            .map(|&((_, _, read), variable)| (read, variable))
            .collect();
        let reader_class = milp.instances[reader].class;
        for (read, variable) in reads {
            for &query in &inputs.queries_reaching[reader_class.0] {
                let (Some(&above), Some(&below)) = (
                    milp.reach.get(&(query, reader)),
                    milp.reach.get(&(query, read)),
                ) else {
                    continue;
                };
                milp.builder.row(
                    "6 reach from below",
                    constraint!(below >= above + variable - 1.0),
                );
            }
        }
    }
    // From above, where needed.
    let exact = instances_needing_exact_reach(milp);
    let readers = readers_of_instances(milp);
    for instance in 0..milp.instances.len() {
        if !exact[instance] {
            continue;
        }
        let class = milp.instances[instance].class;
        for &query in &inputs.queries_reaching[class.0] {
            let reach = milp.reach[&(query, instance)];
            let mut sources = Expression::from(0.0);
            for &(owner, read) in &milp.reads_of_instance[instance] {
                if owner == query {
                    sources += read;
                }
            }
            for &reader in &readers[instance] {
                let Some(&above) = milp.reach.get(&(query, reader)) else {
                    continue;
                };
                let owner = milp.instance_owner(reader);
                let reads: Expression = milp.owners[owner]
                    .reads
                    .iter()
                    .filter(|((_, _, read), _)| *read == instance)
                    .map(|(_, variable)| *variable)
                    .sum();
                let through = milp.builder.continuous("t (reach through)", Some(1.0));
                milp.builder
                    .row("6 reach from above", constraint!(through <= above));
                milp.builder
                    .row("6 reach from above", constraint!(through <= reads));
                sources += through;
            }
            milp.builder
                .row("6 reach from above", constraint!(reach <= sources));
        }
    }
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

/// Which instances need `a` exact from above: those of classes with a
/// minimum-readers constraint, those reading (or whose regions read) an
/// instance with a highest-shared-materialization constraint, and every
/// instance reading one of them, transitively.
fn instances_needing_exact_reach(milp: &SelectionMilp<'_>) -> Vec<bool> {
    let constraints = reader_constraints_of_candidates(milp);
    let readers = readers_of_instances(milp);
    let mut exact = vec![false; milp.instances.len()];
    let mut stack = Vec::new();
    for (class, constraints) in &constraints {
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
        let of_class = milp
            .instances_of_candidate
            .get(class)
            .cloned()
            .unwrap_or_default();
        for &instance in &of_class {
            // Its own readers (minimum readers) and the instances reading it
            // (highest shared materialization).
            stack.push(instance);
            stack.extend(readers[instance].iter().copied());
        }
        // A stateless constrained class: the instances whose regions hold it.
        if !milp.shareable(*class) {
            for (instance, data) in milp.instances.iter().enumerate() {
                if milp.inputs.instance_owners[data.owners]
                    .region
                    .classes
                    .contains(class)
                {
                    stack.push(instance);
                }
            }
        }
    }
    while let Some(instance) = stack.pop() {
        if std::mem::replace(&mut exact[instance], true) {
            continue;
        }
        stack.extend(readers[instance].iter().copied());
    }
    exact
}

/// The queries of `query_outputs` buckets (one per query, each its output
/// references).
fn bucket_queries(milp: &SelectionMilp<'_>, query_outputs: &[Vec<usize>]) -> Vec<usize> {
    query_outputs
        .iter()
        .filter_map(|bucket| {
            bucket
                .first()
                .map(|&reference| milp.query_of_reference[reference])
        })
        .collect()
}

/// Row 7: reader constraints, on every node of the constrained class.
fn reader_constraint_rows(milp: &mut SelectionMilp<'_>) {
    let constraints = reader_constraints_of_candidates(milp);
    let query_count = milp.query_count();
    let readers = readers_of_instances(milp);
    for (class, constraints) in constraints {
        // The class's nodes: instances, or uses in owners.
        let instances = milp
            .instances_of_candidate
            .get(&class)
            .cloned()
            .unwrap_or_default();
        let uses: Vec<(usize, Variable)> = (0..milp.owners.len())
            .filter_map(|owner| {
                milp.owners[owner]
                    .used
                    .get(&class)
                    .map(|&used| (owner, used))
            })
            .collect();
        for constraint in constraints {
            match constraint {
                ReaderConstraint::MinimumQueryReaders {
                    query_outputs,
                    minimum,
                    ..
                } => {
                    let queries = bucket_queries(milp, query_outputs);
                    let minimum = *minimum as f64;
                    for &instance in &instances {
                        let count: Expression = queries
                            .iter()
                            .filter_map(|&query| milp.reach.get(&(query, instance)).copied())
                            .sum();
                        let active = milp.active[instance];
                        milp.builder.row(
                            "7 reader constraints",
                            constraint!(count >= minimum * active),
                        );
                    }
                    for &(owner, used) in &uses {
                        let count: Expression = match milp.instance_of_owner(owner) {
                            None => Expression::from(1.0),
                            Some(instance) => queries
                                .iter()
                                .filter_map(|&query| milp.reach.get(&(query, instance)).copied())
                                .sum(),
                        };
                        milp.builder
                            .row("7 reader constraints", constraint!(count >= minimum * used));
                    }
                }
                ReaderConstraint::HighestSharedQueryMaterialization { query_outputs, .. } => {
                    let queries = bucket_queries(milp, query_outputs);
                    let reach_count =
                        |milp: &SelectionMilp<'_>, instance: usize| -> (Expression, f64) {
                            let mut count = Expression::from(0.0);
                            let mut most = 0.0;
                            for &query in &queries {
                                if let Some(&reach) = milp.reach.get(&(query, instance)) {
                                    count += reach;
                                    most += 1.0;
                                }
                            }
                            (count, most)
                        };
                    for &instance in &instances {
                        // A root, a read from a query owner (reached by one
                        // query), or a read from an instance reached by at
                        // most one query.
                        let mut allowed = Expression::from(0.0);
                        for reference in 0..milp.output_reads.len() {
                            for &(read, variable) in &milp.output_reads[reference] {
                                if read == instance {
                                    allowed += variable;
                                }
                            }
                        }
                        for &(owner, read) in &milp.reads_of_instance[instance] {
                            let is_output = milp
                                .output_reads
                                .iter()
                                .flatten()
                                .any(|&(_, variable)| variable == read);
                            if owner < query_count && !is_output {
                                allowed += read;
                            }
                        }
                        for &reader in &readers[instance] {
                            let owner = milp.instance_owner(reader);
                            let reads: Expression = milp.owners[owner]
                                .reads
                                .iter()
                                .filter(|((_, _, read), _)| *read == instance)
                                .map(|(_, variable)| *variable)
                                .sum();
                            let (count, most) = reach_count(milp, reader);
                            let single = milp.builder.binary("g (single-query consumers)");
                            milp.single_query_consumers.push((reader, instance, single));
                            milp.builder
                                .row("7 reader constraints", constraint!(single <= reads));
                            let slack = (most - 1.0).max(0.0);
                            milp.builder.row(
                                "7 reader constraints",
                                constraint!(count <= 1.0 + slack * (1.0 - single)),
                            );
                            allowed += single;
                        }
                        let active = milp.active[instance];
                        milp.builder
                            .row("7 reader constraints", constraint!(active <= allowed));
                    }
                    for &(owner, used) in &uses {
                        if let Some(instance) = milp.instance_of_owner(owner) {
                            let (count, most) = reach_count(milp, instance);
                            let slack = (most - 1.0).max(0.0);
                            milp.builder.row(
                                "7 reader constraints",
                                constraint!(count <= 1.0 + slack * (1.0 - used)),
                            );
                        }
                    }
                }
                ReaderConstraint::AllowedOutputs { outputs, .. } => {
                    for query in 0..query_count {
                        let references: Vec<usize> = (0..milp.query_of_reference.len())
                            .filter(|&reference| milp.query_of_reference[reference] == query)
                            .collect();
                        let disallowed = references
                            .iter()
                            .filter(|reference| !outputs.contains(reference))
                            .count();
                        if disallowed == 0 {
                            continue;
                        }
                        if disallowed < references.len() {
                            // Some of the query's outputs may reach it, others
                            // not: per-output reach is not modeled, so the
                            // query may not reach it at all.
                            milp.note_inexact("AllowedOutputs over part of a query's outputs");
                        }
                        forbid_reach(milp, query, &instances, &uses);
                    }
                }
                ReaderConstraint::IncompatibleOutputs { left, right, .. } => {
                    let (left, right) = (
                        milp.query_of_reference[*left],
                        milp.query_of_reference[*right],
                    );
                    if left == right {
                        milp.note_inexact("IncompatibleOutputs within one query (not encoded)");
                        continue;
                    }
                    for &instance in &instances {
                        if let (Some(&l), Some(&r)) = (
                            milp.reach.get(&(left, instance)),
                            milp.reach.get(&(right, instance)),
                        ) {
                            milp.builder
                                .row("7 reader constraints", constraint!(l + r <= 1.0));
                        }
                    }
                    for &(owner, used) in &uses {
                        if let Some(instance) = milp.instance_of_owner(owner) {
                            if let (Some(&l), Some(&r)) = (
                                milp.reach.get(&(left, instance)),
                                milp.reach.get(&(right, instance)),
                            ) {
                                milp.builder
                                    .row("7 reader constraints", constraint!(l + r + used <= 2.0));
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Query `query` reaches none of the nodes `instances` and `uses`.
fn forbid_reach(
    milp: &mut SelectionMilp<'_>,
    query: usize,
    instances: &[usize],
    uses: &[(usize, Variable)],
) {
    for &instance in instances {
        if let Some(&reach) = milp.reach.get(&(query, instance)) {
            milp.builder
                .row("7 reader constraints", constraint!(reach <= 0.0));
        }
    }
    for &(owner, used) in uses {
        match milp.instance_of_owner(owner) {
            None if owner == query => {
                milp.builder
                    .row("7 reader constraints", constraint!(used <= 0.0));
            }
            None => {}
            Some(instance) => {
                if let Some(&reach) = milp.reach.get(&(query, instance)) {
                    milp.builder
                        .row("7 reader constraints", constraint!(reach + used <= 1.0));
                }
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

/// Row 8: each query's latency, `asap_cost::plan_cost`'s definition: its
/// request time, plus the work of every query-time node it reaches, each
/// once, plus the longest compaction among the ingestion-time states it
/// reaches.
// Query indexes also key `reach`, `latency` and the owners.
#[allow(clippy::needless_range_loop)]
fn latency_rows(milp: &mut SelectionMilp<'_>) -> Result<(), SelectionMilpError> {
    let query_count = milp.query_count();
    let request_ms = milp.cost_inputs.measured_operation_costs.ns_per_evaluation * 1e-6;
    let compaction = cost_terms::compaction_ms(milp);
    let mut instances_reached: Vec<Vec<usize>> = vec![Vec::new(); query_count];
    for &(query, instance) in milp.reach.keys() {
        instances_reached[query].push(instance);
    }
    for reached in &mut instances_reached {
        reached.sort_unstable();
    }
    let latency_of_instance: Vec<(f64, Option<(Variable, f64)>)> = (0..milp.instances.len())
        .map(|instance| region_latency(milp, instance))
        .collect();
    for query in 0..query_count {
        let mut work = Expression::from(request_ms);
        let mut most = request_ms;
        // Its own region.
        for class in milp.nodes_of_owner(query) {
            for term in milp.node_terms(query, class) {
                if term.numbers.latency_ms > 0.0 {
                    work += term.numbers.latency_ms * term.indicator.clone();
                    most += term.numbers.latency_ms;
                }
            }
        }
        // The instances it reaches and their regions.
        let mut longest_compaction = 0.0f64;
        let mut compactions = Vec::new();
        for &instance in &instances_reached[query] {
            let reach = milp.reach[&(query, instance)];
            if let Some(&ms) = compaction.get(&instance) {
                if ms > 0.0 {
                    compactions.push((ms, reach));
                    longest_compaction = longest_compaction.max(ms);
                }
            }
            // The instance's own work, on the reach itself, and its other
            // terms' work as one sum `D_i`, charged when reached:
            // `u ≥ D_i − D̄_i (1 − a[q, i])`.
            let (own_ms, region) = latency_of_instance[instance];
            if own_ms > 0.0 {
                work += own_ms * reach;
                most += own_ms;
            }
            if let Some((region_latency, region_most)) = region {
                let reached = milp
                    .builder
                    .continuous("u (latency of an instance's region)", None);
                milp.builder.row(
                    "8 latency",
                    constraint!(reached >= region_latency - region_most * (1.0 - reach)),
                );
                work += reached;
                most += region_most;
            }
        }
        if !compactions.is_empty() {
            let longest = milp.builder.continuous("C (compaction)", None);
            for (ms, reach) in compactions {
                milp.builder
                    .row("8 latency", constraint!(longest >= ms * reach));
            }
            work += longest;
            most += longest_compaction;
        }
        let latency = milp.builder.continuous("L (latency)", None);
        let bound = latency_bound_ms(&milp.entries[query]).filter(|&bound| bound < most);
        // A binding bound's rows are divided by it, so HiGHS's absolute
        // feasibility tolerance is relative to the bound (latencies of
        // microseconds would otherwise pass a bound by its tolerance).
        let per_ms = bound.map_or(1.0, |bound| 1.0 / bound.max(1e-9));
        milp.builder
            .row("8 latency", constraint!(per_ms * latency >= per_ms * work));
        if let Some(bound) = bound {
            milp.builder
                .row("8 latency", constraint!(per_ms * latency <= per_ms * bound));
        }
        let bound = latency_bound_ms(&milp.entries[query]);
        milp.latency.push(latency);
        milp.latency_max_ms
            .push(bound.map_or(most, |bound| bound.min(most)));
    }
    Ok(())
}

/// Instance `instance`'s latency terms: its own work on `x[i]` (ms), and
/// the sum of its other terms' work with the most it can be, when any.
fn region_latency(milp: &mut SelectionMilp<'_>, instance: usize) -> (f64, Option<(Variable, f64)>) {
    let owner = milp.instance_owner(instance);
    let own = milp.instances[instance].class;
    let mut own_ms = 0.0;
    let mut sum = Expression::from(0.0);
    let mut most = 0.0;
    for class in milp.nodes_of_owner(owner) {
        for term in milp.node_terms(owner, class) {
            let ms = term.numbers.latency_ms;
            if ms <= 0.0 {
                continue;
            }
            if term.plain && class == own {
                own_ms += ms;
            } else {
                sum += ms * term.indicator;
                most += ms;
            }
        }
    }
    if most <= 0.0 {
        return (own_ms, None);
    }
    // One variable `≥` the region's work, shared by every reaching query.
    let region = milp
        .builder
        .continuous("u (latency of an instance's region)", None);
    milp.builder.row("8 latency", constraint!(region >= sum));
    (own_ms, Some((region, most)))
}

/// Row 9: accuracy (A1, revised 2026-10-30), per instance read: an
/// instance unusable for the query is never read, two whose pair is
/// excluded are not read together, and the shares read are at most each
/// read instance's `parts`.
fn accuracy_rows(milp: &mut SelectionMilp<'_>) {
    let inputs = milp.inputs;
    for query in 0..milp.query_count() {
        let exact_target = milp.entries[query].requirements.accuracy.is_exact();
        let mut counted: Vec<(usize, usize)> = Vec::new(); // (instance, parts)
        for (instance, data) in milp.instances.iter().enumerate() {
            let candidate_data = &inputs.candidate_data[data.class.0];
            if !candidate_data.approximate {
                continue;
            }
            let Some(&reach) = milp.reach.get(&(query, instance)) else {
                continue;
            };
            let parts = candidate_data.accuracy_budget_parts;
            let usable = match inputs.accuracy_weights[query].get(&data.class) {
                // Only at positions whose values do not reach the output.
                None => continue,
                Some(_) if exact_target => false,
                Some(AccuracyWeight::One) => true,
                Some(AccuracyWeight::Unusable(_)) => false,
            };
            if usable {
                counted.push((instance, parts));
            } else {
                milp.builder.row("9 accuracy", constraint!(reach <= 0.0));
            }
        }
        // Pairs usable alone that miss the target together: at most one of
        // them per plan of the query.
        let reaches = |class| -> Vec<Variable> {
            counted
                .iter()
                .filter(|&&(instance, _)| milp.instances[instance].class == class)
                .map(|&(instance, _)| milp.reach[&(query, instance)])
                .collect()
        };
        let mut pairs: Vec<(Variable, Variable)> = Vec::new();
        for &(first, second) in &inputs.accuracy_excluded_pairs[query] {
            for one in reaches(first) {
                pairs.extend(reaches(second).into_iter().map(|other| (one, other)));
            }
        }
        for (one, other) in pairs {
            milp.builder
                .row("9 accuracy (pair)", constraint!(one + other <= 1.0));
        }
        // `N_q` as one variable; per distinct `parts`, an indicator that the
        // query reads some instance sized for that many shares.
        let most = counted.len() as f64;
        if counted.iter().all(|&(_, parts)| parts as f64 >= most) {
            continue;
        }
        let shares = milp.builder.continuous("N (approximate values read)", None);
        let sum: Expression = counted
            .iter()
            .map(|&(instance, _)| milp.reach[&(query, instance)])
            .sum();
        milp.builder.row("9 accuracy", constraint!(shares == sum));
        let mut by_parts: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for &(instance, parts) in &counted {
            if (parts as f64) < most {
                by_parts.entry(parts).or_default().push(instance);
            }
        }
        for (parts, instances) in by_parts {
            let reads = milp
                .builder
                .continuous("A (reads at a parts value)", Some(1.0));
            for instance in instances {
                let reach = milp.reach[&(query, instance)];
                milp.builder.row("9 accuracy", constraint!(reads >= reach));
            }
            let slack = most - parts as f64;
            milp.builder.row(
                "9 accuracy",
                constraint!(shares <= parts as f64 + slack * (1.0 - reads)),
            );
        }
    }
}

/// Solves `milp` once with HiGHS, with the time left before `deadline` as
/// its time limit.
pub(crate) fn solve_selection_milp(
    milp: SelectionMilp<'_>,
    deadline: Option<Instant>,
    build_ms: u64,
) -> Result<SelectionMilpSolution, SelectionMilpError> {
    let size = milp.size();
    let warm_started = Instant::now();
    // A query with no plan alone may still have one with sharing: then the
    // solve starts without an incumbent, and an infeasible model names it.
    // The warm start's local search takes at most a fifth of the time left.
    let search_budget = Instant::now()
        + deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()) / 5)
            .unwrap_or(WARM_START_MOST_TIME)
            .min(WARM_START_MOST_TIME);
    let (start, no_plan_alone) = match warm_start::warm_start(&milp, Some(search_budget)) {
        Ok(start) => (Some(start), None),
        Err(no_plan) => (None, Some(no_plan)),
    };
    let warm_start_ms = warm_started.elapsed().as_millis() as u64;
    let warm_start_cost_per_second = start.as_ref().and_then(|start| start.cost_per_second);
    let scale = objective_scale(
        &milp.objective_terms,
        start.as_ref().map(|start| start.values.as_slice()),
    );
    let objective: Expression = milp
        .objective_terms
        .iter()
        .map(|&(variable, coefficient)| coefficient * scale * variable)
        .sum();
    let SelectionMilp {
        builder,
        instances,
        owners,
        active,
        outputs,
        output_reads,
        request_cost_per_second,
        ..
    } = milp;
    let ModelBuilder {
        variables, rows, ..
    } = builder;
    // HiGHS's columns are the variables in the order they were added.
    let column_of: HashMap<Variable, usize> = variables
        .iter_variables_with_def()
        .enumerate()
        .map(|(column, (variable, _))| (variable, column))
        .collect();
    let problem = variables
        .minimise(objective.clone())
        .using(highs)
        .with_all(rows);
    let mut model = problem
        .try_into_inner()
        .map_err(|error| SelectionMilpError::Solver(error.to_string()))?;
    model.set_option("mip_rel_gap", 0.0);
    model.set_option("mip_abs_gap", 0.0);
    model.set_option("output_flag", false);
    // HiGHS's MIP search is mostly serial; a thread per core (each with
    // its own malloc arenas) costs gigabytes of address space on a large
    // host and breaks an 8 GB `ulimit -v` (#1036 workload-01 on 56 cores).
    model.set_option("threads", HIGHS_THREADS);
    // The warm start: every variable when the completion costed it, else
    // its binaries, and HiGHS completes the continuous variables by an LP
    // with the binaries fixed.
    let started_with_plan = start.is_some();
    if let Some(start) = start {
        let mut columns = vec![0.0; column_of.len()];
        for (variable, value) in start.values {
            columns[column_of[&variable]] = value;
        }
        model
            .try_set_solution(Some(&columns), None, None, None)
            .map_err(|status| SelectionMilpError::Solver(format!("warm start: {status:?}")))?;
    }
    // With a warm start, a deadline the warm start used up still ends with
    // it: HiGHS gets a moment to read it back, and the solve reports the
    // deadline (whatever HiGHS says of that moment).
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
            return Err(no_plan_alone.unwrap_or(SelectionMilpError::Infeasible))
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
    let relative_gap = match status {
        SelectionMilpStatus::Optimal => 0.0,
        SelectionMilpStatus::TimeLimit => solved.mip_gap(),
    };
    let dual_bound = solved
        .double_info_value(c"mip_dual_bound")
        .unwrap_or(f64::NAN);
    let objective_value = solved.objective_value();
    let solution = solved.get_solution();
    let columns = solution.columns();
    let value = |variable: Variable| columns[column_of[&variable]];
    let on = |variable: Variable| value(variable) > 0.5;
    let query_count = owners.len() - instances.len();
    let chosen_owner = |owner: &OwnerVariables| ChosenOwner {
        classes: owner
            .used
            .iter()
            .filter(|(_, &used)| on(used))
            .map(|(&class, _)| class)
            .collect(),
        links: owner
            .links
            .iter()
            .filter(|(_, link)| on(*link))
            .map(|&((reader, input, producer), _)| {
                (reader, input, ChosenProducer::Stateless(producer))
            })
            .chain(owner.reads.iter().filter(|(_, read)| on(*read)).map(
                |&((reader, input, instance), _)| {
                    (
                        reader,
                        input,
                        ChosenProducer::Instance {
                            class: instances[instance].class,
                            index: instances[instance].index,
                        },
                    )
                },
            ))
            .collect(),
    };
    let outputs = outputs
        .iter()
        .zip(&output_reads)
        .map(|(stateless, instance_reads)| {
            stateless
                .iter()
                .find(|(_, output)| on(*output))
                .map(|&(class, _)| ChosenProducer::Stateless(class))
                .or_else(|| {
                    instance_reads
                        .iter()
                        .find(|(_, read)| on(*read))
                        .map(|&(instance, _)| ChosenProducer::Instance {
                            class: instances[instance].class,
                            index: instances[instance].index,
                        })
                })
                .ok_or_else(|| SelectionMilpError::Solver("an output has no producer".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SelectionMilpSolution {
        status,
        cost_per_second: objective_value / scale,
        request_cost_per_second,
        cost_lower_bound_per_second: dual_bound / scale,
        relative_gap,
        outputs,
        query_owners: owners[..query_count].iter().map(chosen_owner).collect(),
        instances: instances
            .iter()
            .enumerate()
            .filter(|&(instance, _)| on(active[instance]))
            .map(|(instance, data)| {
                (
                    data.class,
                    data.index,
                    chosen_owner(&owners[query_count + instance]),
                )
            })
            .collect(),
        size,
        build_ms,
        warm_start_ms,
        warm_start_cost_per_second,
        solve_ms,
    })
}

/// HiGHS's threads (its `threads` option).
const HIGHS_THREADS: i32 = 4;

/// HiGHS's time limit when the warm start used up the time to the deadline.
const MOMENT_TO_READ_THE_START_S: f64 = 1e-3;

/// The most time the warm start's local search takes.
const WARM_START_MOST_TIME: std::time::Duration = std::time::Duration::from_secs(60);

/// The factor the objective is multiplied by for HiGHS: the warm start's
/// cost becomes about 1,000 (so the costs HiGHS compares stay well above its
/// absolute tolerances), its largest coefficient at most 10⁷; without a
/// warm start cost, the largest coefficient becomes 1,000.
fn objective_scale(terms: &[(Variable, f64)], start: Option<&[(Variable, f64)]>) -> f64 {
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

/// Every class reachable from `class` through `producers`, `class` included.
pub(crate) fn candidates_below(
    class: PhysicalCandidateId,
    producers: &[Vec<Vec<PhysicalCandidateId>>],
) -> HashSet<PhysicalCandidateId> {
    let mut seen = HashSet::new();
    let mut stack = vec![class];
    while let Some(class) = stack.pop() {
        if seen.insert(class) {
            stack.extend(producers[class.0].iter().flatten().copied());
        }
    }
    seen
}

#[cfg(test)]
pub(crate) mod tests;
