//! #1095 C3: what the owner-scoped candidate MILP (§2.0) is built from. Given
//! Stage 2's physical candidates (one per operator node, timing and
//! materialization; Stage 2 makes no copies), it computes:
//!
//! 1. **Candidate dominance** ([`prune_dominated_candidates`]): a candidate no optimal
//!    plan under cost model 1 uses is deleted, with its links.
//! 2. **Per-candidate data** ([`CandidateData`]): whether the candidate outputs
//!    approximate values and the budget parts it is sized for, and its usage
//!    per execution, standing cost, held bytes and own latency, every number
//!    from [`asap_cost::node_usage`].
//! 3. **Owners** ([`OwnerRegion`]): one per query and one per possible
//!    instance of each shareable candidate, each with its region of stateless
//!    candidates, the links inside it and the shareable candidates it may read.
//! 4. **Accuracy weights** ([`AccuracyWeight`]) per query and approximate
//!    candidate, and pairs excluded together, from the composition of the
//!    query's outputs.
//!
//! Nothing calls it yet; the MILP (C4) will.
//!
//! ## Usage per execution and its context
//!
//! A candidate's usage is [`asap_cost::node_usage`] of its operator node at one
//! execution per second (`rate = (1, 1.0)`); its standing cost is the same
//! call's cost at no execution (`rate = (1, 0.0)`), and its execution cost
//! the difference. `node_usage` also needs facts the candidate alone does not
//! fix, its *context*:
//! - the outputs of the producers it reads (one per input; the producer
//!   candidates of an input may output different statistics);
//! - for a query-time scan, the window each reader reads, through time
//!   shifts ([`window_read_through`], which follows
//!   [`PhysicalASAPDAG::scan_window`] for a plan with that one reader);
//! - for a pane chain kept beyond one evaluation, the panes it keeps, which
//!   grow with the range-evaluation span of the queries reaching it
//!   ([`asap_cost::pane_retention_ms`], [`asap_cost::retained_panes`]).
//!
//! Every combination of these options is costed (at most
//! [`MAX_CONTEXTS`]); a combination `asap-cost` cannot cost is skipped
//! (no plan can use it). When the numbers agree, the usage is
//! plan-independent; otherwise [`CandidateData::usage_depends_on`] says which
//! options changed it, and [`CandidateData::usage`] holds the first costed
//! combination's numbers. An ingestion-time node's maintained window is
//! read from the first producer candidate of each input
//! ([`asap_cost::maintained_window_ms`]).
//!
//! Candidate links can close a cycle (on #1036 workload-01, a widened scan's
//! `Project` that reads its own memo group). Candidates are costed producers first; on a
//! cycle, a candidate is costed from its producers already costed and marked
//! [`UsageContext::ProducerOnACycle`].

//! ## Dominance (exact under cost model 1)
//!
//! Candidate `k` is deleted when another candidate `k'` (the *dominating* candidate):
//! - is in the same memo group, at the same timing and materialization,
//!   with the same held state;
//! - has the same producer candidates per input, the same readers (every
//!   (reader candidate, input) that may read `k` may read `k'`, and nothing
//!   else), the same output references it may produce, the same reader
//!   constraints and keep flags;
//! - has the same `(approx, parts)`;
//! - has a guarantee at least as strong ([`guarantee_at_least_as_strong`]);
//! - outputs the same schema, sources, state family and unchanged columns
//!   with an operator of the same kind (pane chains: the same panes), so its
//!   readers are costed with the same formulas;
//! - and has every number at most `k`'s: CPU operations and scanned bytes
//!   per execution, latency (work per execution), output rows and bytes,
//!   kept memory (average and peak), disk bytes, held bytes and raw data
//!   kept.
//!
//! Both candidates' usage must be plan-independent (otherwise the comparison
//! would hold for one context only). Readers' usage is assumed
//! non-decreasing in their inputs' rows and bytes. Among candidates that
//! dominate each other the one with the smallest id is kept. Every link to
//! a deleted candidate is deleted; its readers still read the dominating candidate.
//!
//! ## Accuracy weights
//!
//! `w(q, k)` says whether query `q` may read approximate candidate `k` (a
//! sketch [`ASAPOp::SummaryAgg`] or [`ASAPOp::PaneChain`], what
//! [`asap_accuracy::propagation::approximate_inputs`] counts), and pairs of
//! approximate candidates `q` may not read together. Both come from the
//! composition itself, never from an operator table
//! ([`accuracy_positions`]): `q`'s outputs are built over the candidates
//! with `k` approximate and every other candidate exact, their guarantees
//! composed by `asap-accuracy` ([`asap_accuracy::propagation::composed_guarantee`])
//! and checked by the same check the selected plan passes
//! ([`crate::checks::guarantee_check`], with the deployment's model):
//! - every output exact: `k`'s values do not reach the output (only a row
//!   count does), and `k` gets no weight;
//! - every output meets the target: `w(q, k) = 1`, one share of `q`'s
//!   budget ([`asap_accuracy::budget`]); `k`'s sizes are those of its
//!   `parts`, so a lone denominator sized for the whole target
//!   (`ε/(1 − ε) > ε`) fails here;
//! - otherwise [`AccuracyWeight::Unusable`], with the check's cause
//!   ([`UnusableReason`]): e.g. a rank error through any arithmetic, or an
//!   HLL whose declared distinct bound is outside its contract.
//!
//! For each pair `k1, k2` of candidates usable alone, both sized for at
//! least two shares, whose values meet in one plan of `q` (under two inputs
//! of one candidate, under two references of a scalar root, or one below
//! the other), the outputs are built and checked with both approximate; a
//! pair that fails is excluded (`a[q, k1] + a[q, k2] ≤ 1`). A pair with a
//! member sized for one share is excluded by the shares row already. The
//! rows still bound the shares a plan reads by each candidate's `parts`,
//! which the equal split proves for one product or quotient of two inputs;
//! three or more approximate inputs are not built together.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use asap_cost::{CostInputs, NodeUsage, ProducerOutput};
use asap_types::ir::materialized_view::RefreshWindow;
use asap_types::ir::physical_export::{
    PhysicalASAPDAG, PhysicalASAPDAGNode, PhysicalASAPNodeId, ScanWindow,
};
use asap_types::ir::properties::execution_data_state::{DataPrimitive, ExecutionDataState};
use asap_types::ir::properties::{AccuracyGuarantee, ExecutionTiming};
use asap_types::ir::schema::{FieldDataType, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode};
use asap_types::physical::Materialization;
use asap_types::stages::{
    output_references, MemoGroupId, OperatorNodeOfCandidateForMemoGroupId,
    PhysicalASAPMemoCandidates,
};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::SelectionModels;

use asap_types::stages::{PhysicalCandidateId, ReaderConstraint};

/// The most contexts (combinations of producer outputs, scan windows and
/// pane retentions) one candidate is costed under; beyond it the candidate's usage
/// is marked [`UsageContext::TooManyContexts`].
pub(crate) const MAX_CONTEXTS: usize = 256;

/// What the MILP is built from (module documentation).
#[derive(Debug, Clone)]
pub(crate) struct MilpInputs {
    /// Indexed by [`PhysicalCandidateId`]: each candidate's data, pruned or not.
    pub(crate) candidate_data: Vec<CandidateData>,
    /// The candidates dominance deleted, ascending.
    pub(crate) pruned_candidates: Vec<DominatedCandidate>,
    /// Indexed by [`PhysicalCandidateId`]: per input, the producer candidates left
    /// after pruning (empty for a pruned candidate).
    pub(crate) producer_candidates_per_input: Vec<Vec<Vec<PhysicalCandidateId>>>,
    /// Per output reference (`output_references` of the query roots): the
    /// candidates left that may produce it, `Out(r)`, ascending.
    pub(crate) output_candidates: Vec<Vec<PhysicalCandidateId>>,
    /// Indexed by [`PhysicalCandidateId`]: where it runs.
    pub(crate) kind: Vec<RunKind>,
    /// Indexed by [`PhysicalCandidateId`]: the result it gives.
    pub(crate) slot: Vec<ResultSlot>,
    /// The execution contexts: one per query (`0..Q`, in workload order),
    /// then the ingestion pipelines `I_r` some context reads, ascending by
    /// [`IngestionPipeline`], then one per shareable candidate some context
    /// may read, ascending by candidate.
    pub(crate) contexts: Vec<ExecutionContext>,
    /// Per shareable candidate with a context, its index in `contexts`.
    pub(crate) context_of_shareable: BTreeMap<PhysicalCandidateId, usize>,
    /// Per ingestion-time shareable candidate whose context reads a
    /// candidate of an `I_r`: the index of that `I_r` in `contexts`.
    pub(crate) ingestion_context_of_shareable: BTreeMap<PhysicalCandidateId, usize>,
    /// Per query: `w(q, k)` for each approximate candidate `k` whose values
    /// reach its output.
    pub(crate) accuracy_weights: Vec<BTreeMap<PhysicalCandidateId, AccuracyWeight>>,
    /// Per query: the pairs `(k1, k2)`, `k1 < k2`, of candidates usable alone
    /// that it may not read together.
    pub(crate) accuracy_excluded_pairs: Vec<Vec<(PhysicalCandidateId, PhysicalCandidateId)>>,
}

/// One candidate's data for the MILP.
#[derive(Debug, Clone)]
pub(crate) struct CandidateData {
    /// Whether it outputs approximate values: a sketch `SummaryAgg` or
    /// `PaneChain`, each an approximate input of the queries reading it.
    pub(crate) approximate: bool,
    /// `parts(k)`: its operator node's `accuracy_budget_parts` (Stage 2's
    /// `OperatorNodeOfCandidateForMemoGroup`: for a factored node, the
    /// largest among its candidates).
    pub(crate) accuracy_budget_parts: usize,
    /// Its usage under the first context, or why `asap-cost` could not cost
    /// it.
    pub(crate) usage: Result<CandidateUsage, String>,
    /// The context options that changed its usage, ascending; empty when it
    /// is plan-independent.
    pub(crate) usage_depends_on: Vec<UsageContext>,
    /// The inputs whose producer (or that producer's output) changes its
    /// usage or its own output, ascending.
    pub(crate) inputs_changing_usage: Vec<usize>,
    /// Its usage and its own output (an index below `output_count`) per
    /// choice of a producer and that producer's output for each of
    /// `inputs_changing_usage` (in that order); `None` when `asap-cost` could
    /// not cost that choice, so no plan can make it. Empty when
    /// `inputs_changing_usage` is.
    pub(crate) usage_per_producers: Vec<(Vec<ProducerChoice>, Option<UsageAndOutput>)>,
    /// For a query-time scan whose usage depends on the window its reader
    /// reads ([`UsageContext::ScanWindowOfReader`]): its usage per window it
    /// can be costed at, with its output there. Empty otherwise.
    pub(crate) usage_per_window: Vec<(ScanWindow, CandidateUsage, usize)>,
    /// How many distinct outputs (statistics its readers are costed from)
    /// it can give, by context.
    pub(crate) output_count: usize,
    /// For a query-time scan: per reader, the windows it may read the scan
    /// at (through time shifts, [`window_read_through`]). Empty for any
    /// other candidate.
    pub(crate) windows_per_reader: Vec<(PhysicalCandidateId, Vec<ScanWindow>)>,
    /// For a kept pane chain whose usage depends on the panes it retains
    /// ([`UsageContext::PaneRetentionOfReachingQueries`]): its usage per
    /// retained pane count. Empty otherwise.
    pub(crate) usage_per_retained_panes: Vec<(u64, CandidateUsage)>,
    /// With `usage_per_retained_panes`: per query that can reach it, the
    /// panes it needs retained.
    pub(crate) retained_panes_per_query: Vec<(usize, u64)>,
    /// Whether some entry of `usage_per_producers` stands for several usages
    /// (a producer whose own output depends on its context), of which the
    /// costliest is kept.
    pub(crate) producer_usage_ambiguous: bool,
}

impl Default for CandidateData {
    fn default() -> Self {
        Self {
            approximate: false,
            accuracy_budget_parts: 1,
            usage: Err("not costed".to_string()),
            usage_depends_on: Vec::new(),
            inputs_changing_usage: Vec::new(),
            usage_per_producers: Vec::new(),
            usage_per_window: Vec::new(),
            output_count: 1,
            windows_per_reader: Vec::new(),
            usage_per_retained_panes: Vec::new(),
            retained_panes_per_query: Vec::new(),
            producer_usage_ambiguous: false,
        }
    }
}

/// The most producer choices [`CandidateData::usage_per_producers`] lists; beyond
/// it the candidate's usage is marked [`UsageContext::TooManyContexts`] and
/// its first costed usage stands for every choice.
pub(crate) const MAX_PRODUCER_CHOICES: usize = 256;

/// One candidate's usage, from [`asap_cost::node_usage`] (module
/// documentation).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CandidateUsage {
    /// `u(k)`: CPU operations of one execution.
    pub(crate) cpu_ops_per_execution: f64,
    /// `u(k)`: bytes one execution scans.
    pub(crate) scanned_bytes_per_execution: f64,
    /// Milliseconds of one execution on one core.
    pub(crate) work_ms_per_execution: f64,
    /// `d_k`: what it adds to a reaching query's latency in
    /// [`asap_cost::plan_cost`], its work per execution at query time; 0
    /// at ingestion time, whose latency is the plan-level compaction term.
    pub(crate) own_latency_ms: f64,
    /// Cost model 1: the cost of one execution; `None` under other models.
    pub(crate) cost_per_execution: Option<f64>,
    /// Cost model 1: the cost per second of what it keeps whatever runs it
    /// (kept memory and disk); `None` under other models.
    pub(crate) standing_cost_per_second: Option<f64>,
    /// Memory kept between executions, averaged over time.
    pub(crate) average_memory_bytes: f64,
    pub(crate) peak_memory_bytes: u64,
    pub(crate) disk_bytes: u64,
    /// Query-time memory held while an evaluation reading it runs (a
    /// `MatAtQuery` state, or a not-materialized node's working memory).
    pub(crate) held_bytes: u64,
    /// Raw data a query-time scan keeps on disk.
    pub(crate) raw_data_kept_bytes: u64,
    /// An in-memory ingestion-time state's bytes per window.
    pub(crate) maintained_window_bytes: u64,
    pub(crate) output_rows: u64,
    pub(crate) output_bytes: u64,
}

/// A candidate's usage and its output (an index below
/// [`CandidateData::output_count`]) in one context.
pub(crate) type UsageAndOutput = (CandidateUsage, usize);

/// One producer choice for an input: a producer candidate, and for one
/// with several outputs ([`CandidateData::output_count`]), which output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProducerChoice {
    pub(crate) class: PhysicalCandidateId,
    pub(crate) output: Option<usize>,
}

/// A context option that changed a candidate's usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum UsageContext {
    /// Which producer candidate (of different output) an input reads.
    ProducerOutput,
    /// Which reader reads a query-time scan, so which window it scans.
    ScanWindowOfReader,
    /// Which queries reach a kept pane chain, so how many panes it keeps.
    PaneRetentionOfReachingQueries,
    /// A producer candidate on a cycle of candidate links was not costed yet, so
    /// its outputs were not among the options.
    ProducerOnACycle,
    /// `asap-cost` could not cost some contexts (e.g. a scan read with no
    /// window from a source without `rows_held`); the usage is the first
    /// costed one's.
    UncostableContext,
    /// More than [`MAX_CONTEXTS`] combinations: not all were costed.
    TooManyContexts,
}

/// A candidate dominance deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DominatedCandidate {
    pub(crate) class: PhysicalCandidateId,
    pub(crate) dominated_by: PhysicalCandidateId,
    pub(crate) reason: String,
}

/// `w(q, k)`: one share of `q`'s budget, or unusable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum AccuracyWeight {
    One,
    /// `∞`: `k` is unusable for `q`.
    Unusable(UnusableReason),
}

/// Why a position's weight is `∞`: the check of `q`'s outputs with `k`
/// approximate failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum UnusableReason {
    /// Some output has no guarantee
    /// ([`RejectionCause::NoAccuracyGuarantee`](crate::report::RejectionCause::NoAccuracyGuarantee)).
    NoAccuracyGuarantee,
    /// Some output's guarantee misses the target
    /// ([`RejectionCause::AccuracyTargetMissed`](crate::report::RejectionCause::AccuracyTargetMissed)).
    AccuracyTargetMissed,
    /// Not checked, so not usable: more than
    /// [`accuracy_positions::MAX_COMPOSITIONS`] outputs to build, or a node
    /// that does not rebuild over its producers.
    NotChecked,
}

/// Where a candidate runs (#1095 §2.0, "Memo groups and execution
/// contexts"), by its materialization alone, as `asap_cost::plan_cost`
/// counts its executions:
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RunKind {
    /// `NotMaterialized` at query time: computed once in each execution
    /// context (a query evaluation, or a shared query-time candidate's
    /// inputs) that selects it.
    PerContext,
    /// `NotMaterialized` at ingestion time: runs in the ingestion pipeline
    /// `I_r` of its readers' refresh window `r`, once per second of
    /// arriving data for all of them.
    Ingestion,
    /// Materialized (`MatAtIngestion`, `MatAtQuery`,
    /// `MatAtQueryKeptAcrossEvaluations`): selected once, its own execution
    /// context for its inputs, charged once.
    Shareable,
}

/// The [`RunKind`] of a candidate at `timing` and `materialization`.
pub(crate) fn run_kind(timing: ExecutionTiming, materialization: Materialization) -> RunKind {
    match (materialization, timing) {
        (Materialization::NotMaterialized, ExecutionTiming::QueryTime) => RunKind::PerContext,
        (Materialization::NotMaterialized, ExecutionTiming::IngestionTime) => RunKind::Ingestion,
        _ => RunKind::Shareable,
    }
}

/// What "one candidate per memo group per context" (row 2) ranges over:
/// the candidates that give one result. A memo group's implementation
/// roots give the group's result; an implementation's private nodes
/// (`is_implementation_root == false`) give an intermediate result of
/// their own, so each private operator node is its own slot (its
/// candidates differ only in timing and materialization).
///
/// The memo group graph is not acyclic: a widened scan puts a `Project` in
/// the memo group of the scan it reads, so a group's candidate may read
/// its own group. Such a candidate and its producer cannot share one slot
/// (one context would need both), so a group's roots are split by
/// *self-read depth*: 0 for a root reading no root of its own group (the
/// scan), 1 for one that may (the `Project`). A depth-1 root reading
/// another depth-1 root of its group in one context (a `Project` over a
/// `Project` of the widened scan) is then a read of its own slot, which
/// [`CandidateLinks`] drops for candidates that run in a context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum ResultSlot {
    Group(MemoGroupId, u32),
    PrivateNode(OperatorNodeOfCandidateForMemoGroupId),
}

/// The [`ResultSlot`] of every candidate (see there).
pub(crate) fn result_slots(physical: &PhysicalASAPMemoCandidates) -> Vec<ResultSlot> {
    let count = physical.candidates.len();
    let root = |candidate: usize| {
        let operator_node = physical.candidates[candidate].operator_node;
        let node = &physical.operator_nodes_of_candidates_for_memo_groups[operator_node.0];
        node.is_implementation_root
            .then(|| node.group())
            .ok_or(operator_node)
    };
    // Self-read depth: 1 for a root that may read a root of its own group.
    let depth: Vec<u32> = (0..count)
        .map(|candidate| {
            let Ok(group) = root(candidate) else {
                return 0;
            };
            let reads_own_group = physical.candidates[candidate]
                .producer_candidates_per_input
                .iter()
                .flat_map(|producers| producers.iter())
                .any(|producer| producer.0 != candidate && root(producer.0) == Ok(group));
            u32::from(reads_own_group)
        })
        .collect();
    (0..count)
        .map(|candidate| match root(candidate) {
            Ok(group) => ResultSlot::Group(group, depth[candidate]),
            Err(operator_node) => ResultSlot::PrivateNode(operator_node),
        })
        .collect()
}

/// Per candidate of a [`PhysicalASAPMemoCandidates`]: its producer
/// candidates per input, ascending and without repeats, and its
/// [`RunKind`].
struct CandidateLinks {
    producers_per_input: Vec<Vec<Vec<PhysicalCandidateId>>>,
    kind: Vec<RunKind>,
}

impl CandidateLinks {
    fn new(physical: &PhysicalASAPMemoCandidates) -> Self {
        let producers_per_input: Vec<Vec<Vec<PhysicalCandidateId>>> = physical
            .candidates
            .iter()
            .map(|candidate| {
                candidate
                    .producer_candidates_per_input
                    .iter()
                    .map(|producers| {
                        let set: BTreeSet<_> = producers.iter().copied().collect();
                        set.into_iter().collect()
                    })
                    .collect()
            })
            .collect();
        let kind: Vec<RunKind> = physical
            .candidates
            .iter()
            .map(|candidate| run_kind(candidate.timing, candidate.materialization))
            .collect();
        // With one candidate per result slot per context, a candidate that
        // runs in a context cannot read its own slot there (only a self-loop
        // could): such links are dropped (a widened scan's `Project` lists
        // its own memo group). A shareable candidate reads in its own
        // context, so it keeps them, but never reads itself.
        let slot = result_slots(physical);
        let producers_per_input = producers_per_input
            .into_iter()
            .enumerate()
            .map(|(reader, inputs): (usize, Vec<Vec<PhysicalCandidateId>>)| {
                inputs
                    .into_iter()
                    .map(|producers| {
                        producers
                            .into_iter()
                            .filter(|producer| {
                                producer.0 != reader
                                    && (kind[reader] == RunKind::Shareable
                                        || slot[producer.0] != slot[reader])
                            })
                            .collect()
                    })
                    .collect()
            })
            .collect();
        Self {
            producers_per_input,
            kind,
        }
    }

    /// Whether `candidate` is materialized: selected once, its own context.
    fn is_shareable(&self, candidate: PhysicalCandidateId) -> bool {
        self.kind[candidate.0] == RunKind::Shareable
    }
}

/// Per candidate, its reader constraints, each as an id equal for equal
/// constraints once the constrained candidate is left out, ascending and
/// without repeats.
fn reader_constraint_ids_per_candidate(physical: &PhysicalASAPMemoCandidates) -> Vec<Vec<u32>> {
    let mut ids: HashMap<ReaderConstraint, u32> = HashMap::new();
    let mut per_candidate = vec![Vec::new(); physical.candidates.len()];
    for constraint in &physical.reader_constraints {
        let mut unowned = constraint.clone();
        *unowned.candidate_mut() = PhysicalCandidateId(usize::MAX);
        let next = ids.len() as u32;
        per_candidate[constraint.candidate().0].push(*ids.entry(unowned).or_insert(next));
    }
    for constraint_ids in &mut per_candidate {
        constraint_ids.sort_unstable();
        constraint_ids.dedup();
    }
    per_candidate
}

/// [`MilpInputs`] for `physical`'s candidates; `entries[q]` is query `q`'s
/// workload entry. Accuracy is checked with the built-in model and no
/// evidence ([`prepare_milp_inputs_with`]).
#[cfg(test)]
pub(crate) fn prepare_milp_inputs(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    cost_inputs: &CostInputs,
) -> MilpInputs {
    let models = SelectionModels {
        accuracy: &asap_accuracy::BuiltinAccuracyModel,
        evidence: &asap_accuracy::NoAccuracyEvidence,
        cost_inputs,
    };
    prepare_milp_inputs_with(physical, entries, data, &models)
}

/// [`MilpInputs`] for `physical`'s candidates, its accuracy positions
/// checked with `models` (the deployment's accuracy model and evidence) and
/// its usage costed with `models.cost_inputs`.
pub(crate) fn prepare_milp_inputs_with(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: &SelectionModels<'_>,
) -> MilpInputs {
    let cost_inputs = models.cost_inputs;
    let links = &CandidateLinks::new(physical);
    let class_count = physical.candidates.len();
    let all_alive = vec![true; class_count];
    let unpruned_links = producer_candidates_left(links, &all_alive);
    let unpruned_outputs = output_candidates(physical, &all_alive);
    let unpruned_reach = queries_reaching(physical, &unpruned_links, &unpruned_outputs);
    let refresh_windows = refresh_windows(physical, links);
    // Every context before pruning, for the windows each scan may be read
    // at within one context.
    let all_contexts = execution_contexts(
        physical,
        &links.kind,
        &unpruned_links,
        &unpruned_outputs,
        &refresh_windows,
        |_| true,
    )
    .contexts;
    let candidate_data = candidate_data(
        physical,
        links,
        entries,
        &unpruned_reach,
        &all_contexts,
        &unpruned_outputs,
        data,
        cost_inputs,
    );
    let pruned_candidates = prune_dominated_candidates(
        &dominance_profiles(physical, links, &unpruned_outputs),
        &candidate_data,
    );
    let mut alive = all_alive;
    for pruned in &pruned_candidates {
        alive[pruned.class.0] = false;
    }
    let producer_candidates_per_input = producer_candidates_left(links, &alive);
    let output_candidates = output_candidates(physical, &alive);
    let kind = links.kind.clone();
    let slot = result_slots(physical);
    let usable = |class: PhysicalCandidateId| candidate_data[class.0].usage.is_ok();
    let ExecutionContexts {
        contexts,
        context_of_shareable,
        ingestion_context_of_shareable,
    } = execution_contexts(
        physical,
        &kind,
        &producer_candidates_per_input,
        &output_candidates,
        &refresh_windows,
        usable,
    );
    let AccuracyPositions {
        weights: accuracy_weights,
        excluded_pairs: accuracy_excluded_pairs,
    } = accuracy_positions::accuracy_positions(
        physical,
        &candidate_data,
        &producer_candidates_per_input,
        &output_candidates,
        entries,
        data,
        models,
    );
    MilpInputs {
        candidate_data,
        pruned_candidates,
        producer_candidates_per_input,
        output_candidates,
        kind,
        slot,
        contexts,
        context_of_shareable,
        ingestion_context_of_shareable,
        accuracy_weights,
        accuracy_excluded_pairs,
    }
}

/// Per candidate, per input, its producer candidates that are `alive` (none for a
/// candidate that is not).
fn producer_candidates_left(
    links: &CandidateLinks,
    alive: &[bool],
) -> Vec<Vec<Vec<PhysicalCandidateId>>> {
    links
        .producers_per_input
        .iter()
        .enumerate()
        .map(|(class, producers_per_input)| match alive[class] {
            false => Vec::new(),
            true => producers_per_input
                .iter()
                .map(|producers| {
                    producers
                        .iter()
                        .copied()
                        .filter(|producer| alive[producer.0])
                        .collect()
                })
                .collect(),
        })
        .collect()
}

/// `Out(r)` per output reference: the `alive` candidates among its output
/// candidates, ascending.
fn output_candidates(
    physical: &PhysicalASAPMemoCandidates,
    alive: &[bool],
) -> Vec<Vec<PhysicalCandidateId>> {
    (0..physical.output_reference_count())
        .map(|reference| {
            let set: BTreeSet<_> = physical
                .query_output_candidates(reference)
                .filter(|candidate| alive[candidate.0])
                .collect();
            set.into_iter().collect()
        })
        .collect()
}

/// Per candidate, the queries whose output candidates reach it through the links
/// `producer_candidates_per_input`, ascending.
fn queries_reaching(
    physical: &PhysicalASAPMemoCandidates,
    producer_candidates_per_input: &[Vec<Vec<PhysicalCandidateId>>],
    output_candidates: &[Vec<PhysicalCandidateId>],
) -> Vec<Vec<usize>> {
    let mut reaching = vec![Vec::new(); producer_candidates_per_input.len()];
    let output_refs = output_references(&physical.query_roots);
    for query in 0..physical.query_roots.len() {
        let mut seen = vec![false; producer_candidates_per_input.len()];
        let mut stack: Vec<PhysicalCandidateId> = output_refs
            .iter()
            .enumerate()
            .filter(|(_, (q, _))| *q == query)
            .flat_map(|(reference, _)| output_candidates[reference].iter().copied())
            .collect();
        while let Some(class) = stack.pop() {
            if std::mem::replace(&mut seen[class.0], true) {
                continue;
            }
            reaching[class.0].push(query);
            stack.extend(
                producer_candidates_per_input[class.0]
                    .iter()
                    .flatten()
                    .copied(),
            );
        }
    }
    reaching
}

/// Which execution context (#1095 §2.0): a query evaluation, an ingestion
/// pipeline `I_r`, or a shareable candidate's own inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum ContextKind {
    Query(usize),
    Ingestion(IngestionPipeline),
    Shared(PhysicalCandidateId),
}

/// Which ingestion pipeline `I_r` (#1095 §2.5 O3(b)): the deployment
/// refreshes the ingestion-time views of one refresh window in one run, so
/// `asap_cost::execution_demand::computations` charges a `NotMaterialized`
/// ingestion-time node once per refresh window of the views reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum IngestionPipeline {
    /// The views of this refresh window.
    Window(RefreshWindow),
    /// An ingestion-time shareable candidate whose window is unbounded or
    /// empty: refreshed alone.
    Alone(PhysicalCandidateId),
}

/// The [`IngestionPipeline`] of ingestion-time shareable candidate
/// `shareable`, from its refresh window ([`refresh_windows`]).
pub(crate) fn ingestion_pipeline(
    shareable: PhysicalCandidateId,
    refresh_windows: &HashMap<PhysicalCandidateId, Option<RefreshWindow>>,
) -> IngestionPipeline {
    match refresh_windows.get(&shareable) {
        Some(Some(window)) => IngestionPipeline::Window(*window),
        _ => IngestionPipeline::Alone(shareable),
    }
}

/// Each ingestion-time shareable candidate's refresh window, as
/// `asap_cost::execution_demand::computations` computes it on the realized
/// plan ([`PhysicalASAPDAG::refresh_windows`]): on the DAG of the candidates
/// below it, each input read from its first producer candidate. That is
/// exact when the producer candidates of each input read one window, which
/// is debug-asserted ([`producer_window_disagreements`]).
fn refresh_windows(
    physical: &PhysicalASAPMemoCandidates,
    links: &CandidateLinks,
) -> HashMap<PhysicalCandidateId, Option<RefreshWindow>> {
    let views: Vec<PhysicalCandidateId> = (0..physical.candidates.len())
        .map(PhysicalCandidateId)
        .filter(|&class| {
            links.is_shareable(class)
                && physical.candidates[class.0].timing == ExecutionTiming::IngestionTime
        })
        .collect();
    let dag = dag_below(physical, links, &views);
    debug_assert!(
        producer_window_disagreements(links, &dag).is_empty(),
        "producer candidates of one input read different windows: {:?}",
        producer_window_disagreements(links, &dag)
    );
    dag.refresh_windows(views.iter().map(|view| view.0))
        .into_iter()
        .map(|(id, window)| (PhysicalCandidateId(id), window))
        .collect()
}

/// The DAG of `tops` and every candidate below them, without edges: each
/// candidate a node ([`catalog_node`]) whose inputs are its first producer
/// candidates.
fn dag_below(
    physical: &PhysicalASAPMemoCandidates,
    links: &CandidateLinks,
    tops: &[PhysicalCandidateId],
) -> PhysicalASAPDAG {
    let mut nodes = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = tops.to_vec();
    while let Some(class) = stack.pop() {
        if seen.insert(class) {
            nodes.push(catalog_node(physical, links, class));
            stack.extend(links.producers_per_input[class.0].iter().flatten());
        }
    }
    let nodes: Vec<&PhysicalASAPDAGNode> = nodes.iter().collect();
    nodes_dag(&nodes)
}

/// The `(candidate, input)` pairs of `dag`'s candidates
/// ([`dag_below`]) whose producer candidates have different view windows
/// ([`PhysicalASAPDAG::view_windows`]).
fn producer_window_disagreements(
    links: &CandidateLinks,
    dag: &PhysicalASAPDAG,
) -> Vec<(PhysicalCandidateId, usize)> {
    let windows = dag.view_windows(dag.nodes.iter().map(|node| node.id));
    let mut disagreements = Vec::new();
    for node in &dag.nodes {
        for (input, producers) in links.producers_per_input[node.id].iter().enumerate() {
            let mut read = producers.iter().map(|producer| windows[&producer.0]);
            if let Some(first) = read.next() {
                if read.any(|window| window != first) {
                    disagreements.push((PhysicalCandidateId(node.id), input));
                }
            }
        }
    }
    disagreements
}

/// [`producer_window_disagreements`] over every candidate of `physical`.
#[cfg(test)]
pub(crate) fn producer_window_disagreements_of_all(
    physical: &PhysicalASAPMemoCandidates,
) -> Vec<(PhysicalCandidateId, usize)> {
    let links = CandidateLinks::new(physical);
    let all: Vec<PhysicalCandidateId> = (0..physical.candidates.len())
        .map(PhysicalCandidateId)
        .collect();
    producer_window_disagreements(&links, &dag_below(physical, &links, &all))
}

/// One execution context and the candidates that may give a result in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecutionContext {
    pub(crate) kind: ContextKind,
    /// Whether it runs at query time (a query, or a query-time shareable
    /// candidate's inputs): its `PerContext` candidates are computed in it.
    pub(crate) at_query_time: bool,
    /// The candidates that may give a result slot in it, ascending: each
    /// computed in it ([`ExecutionContext::computes`]) or read across its
    /// boundary (a shareable candidate, or a candidate of an `I_r`).
    pub(crate) options: Vec<PhysicalCandidateId>,
}

impl ExecutionContext {
    /// Whether a candidate of `kind` is computed in this context (otherwise
    /// it is read across the boundary).
    pub(crate) fn computes(&self, kind: RunKind) -> bool {
        match kind {
            RunKind::PerContext => self.at_query_time,
            RunKind::Ingestion => matches!(self.kind, ContextKind::Ingestion(_)),
            RunKind::Shareable => false,
        }
    }
}

impl MilpInputs {
    /// The context where context `context` reads a candidate of an `I_r`:
    /// itself for an `I_r`, `I_{r(m)}` for ingestion-time shareable `m`'s
    /// context; `None` for any other context (it reads none).
    pub(crate) fn ingestion_context_read_by(&self, context: usize) -> Option<usize> {
        match self.contexts[context].kind {
            ContextKind::Ingestion(_) => Some(context),
            ContextKind::Shared(m) => self.ingestion_context_of_shareable.get(&m).copied(),
            ContextKind::Query(_) => None,
        }
    }
}

/// [`execution_contexts`]' result (see [`MilpInputs`] for each field).
pub(crate) struct ExecutionContexts {
    pub(crate) contexts: Vec<ExecutionContext>,
    pub(crate) context_of_shareable: BTreeMap<PhysicalCandidateId, usize>,
    pub(crate) ingestion_context_of_shareable: BTreeMap<PhysicalCandidateId, usize>,
}

/// The execution contexts and their options (module documentation): from
/// each query's output candidates, each shareable candidate's producers
/// and, per ingestion pipeline `I_r`, every candidate of `I_r` an
/// ingestion-time shareable candidate of refresh window `r`
/// (`refresh_windows`) reads across its boundary, expanded through the
/// inputs of the candidates each context computes. Candidates `usable`
/// rejects (no usage) are left out. A query-time `PerContext` candidate
/// that only an ingestion-time context could read is left out too: it
/// cannot run there.
pub(crate) fn execution_contexts(
    physical: &PhysicalASAPMemoCandidates,
    kind: &[RunKind],
    producer_candidates_per_input: &[Vec<Vec<PhysicalCandidateId>>],
    output_candidates: &[Vec<PhysicalCandidateId>],
    refresh_windows: &HashMap<PhysicalCandidateId, Option<RefreshWindow>>,
    usable: impl Fn(PhysicalCandidateId) -> bool,
) -> ExecutionContexts {
    let query_count = physical.query_roots.len();
    let output_refs = output_references(&physical.query_roots);
    // Expands `seeds` in a context: the options and, through the candidates
    // it computes, their producers.
    let expand = |context: &ExecutionContext, seeds: Vec<PhysicalCandidateId>| {
        let mut seen: BTreeSet<PhysicalCandidateId> = BTreeSet::new();
        let mut stack = seeds;
        while let Some(class) = stack.pop() {
            if !usable(class) || seen.contains(&class) {
                continue;
            }
            let class_kind = kind[class.0];
            if class_kind == RunKind::PerContext && !context.at_query_time {
                continue;
            }
            seen.insert(class);
            if context.computes(class_kind) {
                stack.extend(
                    producer_candidates_per_input[class.0]
                        .iter()
                        .flatten()
                        .copied(),
                );
            }
        }
        seen.into_iter().collect::<Vec<_>>()
    };
    let mut contexts: Vec<ExecutionContext> = Vec::new();
    for query in 0..query_count {
        let mut context = ExecutionContext {
            kind: ContextKind::Query(query),
            at_query_time: true,
            options: Vec::new(),
        };
        let seeds = output_refs
            .iter()
            .enumerate()
            .filter(|(_, (q, _))| *q == query)
            .flat_map(|(reference, _)| output_candidates[reference].iter().copied())
            .collect();
        context.options = expand(&context, seeds);
        contexts.push(context);
    }
    // Shareable contexts, discovered from the options, then each `I_r` from
    // every candidate of `I_r` read across a boundary; repeated until
    // nothing new.
    let mut context_of_shareable: BTreeMap<PhysicalCandidateId, usize> = BTreeMap::new();
    let mut shared: Vec<ExecutionContext> = Vec::new();
    // Per `I_r`: its context and its seeds.
    let mut ingestion: BTreeMap<
        IngestionPipeline,
        (ExecutionContext, BTreeSet<PhysicalCandidateId>),
    > = BTreeMap::new();
    let mut scanned = 0;
    loop {
        let mut pending: Vec<PhysicalCandidateId> = Vec::new();
        let mut seeds: Vec<(IngestionPipeline, PhysicalCandidateId)> = Vec::new();
        let unscanned = contexts.iter().chain(shared.iter()).skip(scanned);
        for context in unscanned.chain(ingestion.values().map(|(context, _)| context)) {
            // Only an ingestion-time shareable candidate reads a candidate
            // of an `I_r` (Stage 2's timing rule).
            let pipeline = match context.kind {
                ContextKind::Shared(m) if !context.at_query_time => {
                    Some(ingestion_pipeline(m, refresh_windows))
                }
                _ => None,
            };
            for &class in &context.options {
                match kind[class.0] {
                    RunKind::Shareable => {
                        if !context_of_shareable.contains_key(&class) && !pending.contains(&class) {
                            pending.push(class);
                        }
                    }
                    RunKind::Ingestion => {
                        if let Some(pipeline) = pipeline {
                            seeds.push((pipeline, class));
                        }
                    }
                    RunKind::PerContext => {}
                }
            }
        }
        scanned = contexts.len() + shared.len();
        let mut grown: BTreeSet<IngestionPipeline> = BTreeSet::new();
        for (pipeline, class) in seeds {
            let (_, known) = ingestion.entry(pipeline).or_insert_with(|| {
                let context = ExecutionContext {
                    kind: ContextKind::Ingestion(pipeline),
                    at_query_time: false,
                    options: Vec::new(),
                };
                (context, BTreeSet::new())
            });
            if known.insert(class) {
                grown.insert(pipeline);
            }
        }
        if pending.is_empty() && grown.is_empty() {
            break;
        }
        pending.sort_unstable();
        for class in pending {
            let at_query_time = physical.candidates[class.0].timing == ExecutionTiming::QueryTime;
            let mut context = ExecutionContext {
                kind: ContextKind::Shared(class),
                at_query_time,
                options: Vec::new(),
            };
            context.options = expand(
                &context,
                producer_candidates_per_input[class.0]
                    .iter()
                    .flatten()
                    .copied()
                    .collect(),
            );
            context_of_shareable.insert(class, usize::MAX);
            shared.push(context);
        }
        for pipeline in grown {
            let (context, known) = ingestion.get_mut(&pipeline).expect("seeded above");
            context.options = expand(context, known.iter().copied().collect());
        }
    }
    shared.sort_by_key(|context| context.kind);
    let mut context_of_pipeline: BTreeMap<IngestionPipeline, usize> = BTreeMap::new();
    for (pipeline, (context, _)) in ingestion {
        context_of_pipeline.insert(pipeline, contexts.len());
        contexts.push(context);
    }
    let mut ingestion_context_of_shareable = BTreeMap::new();
    for context in shared {
        if let ContextKind::Shared(class) = context.kind {
            context_of_shareable.insert(class, contexts.len());
            if !context.at_query_time {
                let pipeline = ingestion_pipeline(class, refresh_windows);
                if let Some(&at) = context_of_pipeline.get(&pipeline) {
                    ingestion_context_of_shareable.insert(class, at);
                }
            }
        }
        contexts.push(context);
    }
    ExecutionContexts {
        contexts,
        context_of_shareable,
        ingestion_context_of_shareable,
    }
}

// ---------------------------------------------------------------------------
// Per-candidate data
// ---------------------------------------------------------------------------

/// Whether `node` outputs approximate values ([`CandidateData::approximate`]).
fn outputs_approximate_values(node: &OperatorNode) -> bool {
    matches!(
        node.asap(),
        Some(
            ASAPOp::SummaryAgg {
                family: FieldDataType::Sketch(..),
                ..
            } | ASAPOp::PaneChain {
                family: FieldDataType::Sketch(..),
                ..
            }
        )
    )
}

/// The physical-DAG node of `candidate`, with node id `candidate`
/// and each input's child id its first producer candidate (`usize::MAX` when it
/// has none), as Stage 2's realizer would export it before retention.
fn catalog_node(
    physical: &PhysicalASAPMemoCandidates,
    links: &CandidateLinks,
    class: PhysicalCandidateId,
) -> PhysicalASAPDAGNode {
    let candidate = &physical.candidates[class.0];
    let node = physical.physical_node(class);
    let mut input = 0;
    let payload = node.operator.map_children(|_| {
        let child = links.producers_per_input[class.0]
            .get(input)
            .and_then(|producers| producers.first())
            .map_or(usize::MAX, |producer| producer.0);
        input += 1;
        child
    });
    PhysicalASAPDAGNode {
        id: class.0,
        payload,
        output_state: ExecutionDataState {
            timing: candidate.timing,
            primitive: if node.asap().and_then(ASAPOp::produced_state).is_some() {
                DataPrimitive::SummaryState
            } else {
                DataPrimitive::Raw
            },
        },
        materialization: candidate.materialization,
        output_schema: node.schema.clone(),
        guarantee: node.guarantee.clone(),
        retention_ms: None,
    }
}

/// A DAG of `nodes` alone, without edges: what
/// [`asap_cost::pane_retention_ms`] reads (payload children and nodes).
fn nodes_dag(nodes: &[&PhysicalASAPDAGNode]) -> PhysicalASAPDAG {
    PhysicalASAPDAG {
        nodes: nodes.iter().map(|&node| node.clone()).collect(),
        edges: Vec::new(),
        roots: Vec::new(),
        scalar_roots: Vec::new(),
    }
}

/// The window a scan read through `path` (time shifts, then the reader that
/// ends it) covers, as [`PhysicalASAPDAG::scan_window`] computes it for a
/// plan where `path` is the scan's only reader: the shifts' offsets add up,
/// and a `TimeRange` reads its range, a kept pane chain one pane, any other
/// pane chain its whole window; any other reader reads `Unbounded`.
fn window_read_through(path: &[&PhysicalASAPDAGNode]) -> ScanWindow {
    let mut end_offset_ms: i64 = 0;
    for node in path {
        let width_ms: u128 = match &node.payload {
            Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }) if shift.at.is_none() => {
                end_offset_ms = end_offset_ms.saturating_add(shift.offset_ms);
                continue;
            }
            Operator::NonASAP(NonASAPOp::TimeRange { range, .. }) => range.as_millis(),
            Operator::ASAP(ASAPOp::PaneChain { pane_width_ms, .. })
                if matches!(
                    node.materialization,
                    Materialization::MatAtQueryKeptAcrossEvaluations(_)
                ) =>
            {
                u128::from(*pane_width_ms)
            }
            Operator::ASAP(ASAPOp::PaneChain {
                pane_width_ms,
                pane_count,
                ..
            }) => u128::from(*pane_width_ms) * u128::from(*pane_count),
            _ => return ScanWindow::Unbounded,
        };
        return ScanWindow::Within {
            end_offset_ms,
            width_ms: i64::try_from(width_ms).unwrap_or(i64::MAX),
        };
    }
    ScanWindow::Unbounded
}

/// The windows query-time scan `scan` may be read at: each reader's
/// windows (one per path through time shifts) and, per execution context,
/// the unions of the window kinds its readers there read (one scan run read
/// by several readers reads the union, `PhysicalASAPDAG::scan_window`):
/// every union of up to [`MAX_WINDOW_KINDS`] kinds, else the union of all.
/// A read as an output or as a shareable candidate's own input is
/// `Unbounded`. Distinct; `Unbounded` alone when nothing reads it.
fn scan_window_options(
    scan: PhysicalCandidateId,
    windows_per_reader: &[(PhysicalCandidateId, Vec<ScanWindow>)],
    contexts: &[ExecutionContext],
    kind: &[RunKind],
    producers_per_input: &[Vec<Vec<PhysicalCandidateId>>],
    output_candidates: &[Vec<PhysicalCandidateId>],
) -> Vec<ScanWindow> {
    use crate::milp::cost_terms::window_union;
    let mut windows: Vec<ScanWindow> = Vec::new();
    let add = |window: ScanWindow, windows: &mut Vec<ScanWindow>| {
        if !windows.contains(&window) {
            windows.push(window);
        }
    };
    for (_, read) in windows_per_reader {
        for &window in read {
            add(window, &mut windows);
        }
    }
    let window_of = |reader: PhysicalCandidateId| {
        windows_per_reader
            .iter()
            .find(|(known, _)| *known == reader)
            .map_or(ScanWindow::Unbounded, |(_, read)| {
                window_union(read.iter().copied())
            })
    };
    let shareable = kind[scan.0] == RunKind::Shareable;
    let mut all_kinds: Vec<ScanWindow> = Vec::new();
    for context in contexts {
        if !context.options.contains(&scan) {
            continue;
        }
        let mut kinds: Vec<ScanWindow> = Vec::new();
        for &reader in &context.options {
            if context.computes(kind[reader.0])
                && producers_per_input[reader.0]
                    .iter()
                    .any(|producers| producers.contains(&scan))
            {
                let window = window_of(reader);
                if !kinds.contains(&window) {
                    kinds.push(window);
                }
            }
        }
        // A shareable candidate reading the scan as its own input reads it
        // at its own window; a read as an output reads it whole.
        if let ContextKind::Shared(own) = context.kind {
            if producers_per_input[own.0]
                .iter()
                .any(|producers| producers.contains(&scan))
            {
                let window = window_of(own);
                if !kinds.contains(&window) {
                    kinds.push(window);
                }
            }
        }
        let read_whole = match context.kind {
            ContextKind::Query(_) => output_candidates.iter().any(|out| out.contains(&scan)),
            ContextKind::Shared(_) | ContextKind::Ingestion(_) => false,
        };
        if read_whole && !kinds.contains(&ScanWindow::Unbounded) {
            kinds.push(ScanWindow::Unbounded);
        }
        if shareable {
            for window in kinds {
                if !all_kinds.contains(&window) {
                    all_kinds.push(window);
                }
            }
            continue;
        }
        add_unions(&kinds, &mut windows);
    }
    if shareable {
        add_unions(&all_kinds, &mut windows);
    }
    if windows.is_empty() {
        windows.push(ScanWindow::Unbounded);
    }
    windows
}

/// Whether window `outer` contains window `inner`.
fn window_contains(outer: ScanWindow, inner: ScanWindow) -> bool {
    match (outer, inner) {
        (ScanWindow::Unbounded, _) => true,
        (ScanWindow::Within { .. }, ScanWindow::Unbounded) => false,
        (
            ScanWindow::Within {
                end_offset_ms: outer_end,
                width_ms: outer_width,
            },
            ScanWindow::Within {
                end_offset_ms: inner_end,
                width_ms: inner_width,
            },
        ) => {
            outer_end <= inner_end
                && outer_end.saturating_add(outer_width) >= inner_end.saturating_add(inner_width)
        }
    }
}

/// The most window kinds of one scan run whose every union is costed (and
/// encoded exactly, [`crate::milp::cost_terms`]); beyond it only the union
/// of all.
pub(crate) const MAX_WINDOW_KINDS: usize = 4;

/// Adds to `windows` every union of a nonempty subset of `kinds` (only the
/// union of all beyond [`MAX_WINDOW_KINDS`]).
fn add_unions(kinds: &[ScanWindow], windows: &mut Vec<ScanWindow>) {
    use crate::milp::cost_terms::scan_window_read_at as window_union;
    let mut push = |window: ScanWindow| {
        if !windows.contains(&window) {
            windows.push(window);
        }
    };
    if kinds.len() > MAX_WINDOW_KINDS {
        push(window_union(kinds.iter().copied()));
        return;
    }
    for set in 1usize..(1 << kinds.len()) {
        push(window_union(
            (0..kinds.len())
                .filter(|bit| set & (1 << bit) != 0)
                .map(|bit| kinds[bit]),
        ));
    }
}

/// Per class reading the query-time scan `scan`, the windows it may read it
/// at (through time shifts above it), distinct.
fn scan_windows_per_reader(
    scan: PhysicalCandidateId,
    nodes: &[PhysicalASAPDAGNode],
    readers: &[Vec<(PhysicalCandidateId, usize)>],
) -> Vec<(PhysicalCandidateId, Vec<ScanWindow>)> {
    /// Time shifts nested deeper than this are not followed.
    const MAX_SHIFTS: usize = 4;
    let mut per_reader: Vec<(PhysicalCandidateId, Vec<ScanWindow>)> = Vec::new();
    let mut paths: Vec<Vec<PhysicalCandidateId>> = vec![vec![scan]];
    while let Some(path) = paths.pop() {
        let last = *path.last().expect("never empty");
        let follow = path.len() == 1
            || (matches!(
                nodes[last.0].payload,
                Operator::NonASAP(NonASAPOp::TimeShift { .. })
            ) && path.len() <= MAX_SHIFTS
                && !readers[last.0].is_empty());
        if !follow {
            // The path ends at a reader: what the scan reads under it.
            let through: Vec<_> = path[1..].iter().map(|class| &nodes[class.0]).collect();
            let window = window_read_through(&through);
            let Some(&reader) = path.get(1) else {
                continue;
            };
            match per_reader.iter_mut().find(|(known, _)| *known == reader) {
                Some((_, windows)) if windows.contains(&window) => {}
                Some((_, windows)) => windows.push(window),
                None => per_reader.push((reader, vec![window])),
            }
            continue;
        }
        for &(reader, _) in readers[last.0].iter().rev() {
            if !path.contains(&reader) {
                let mut longer = path.clone();
                longer.push(reader);
                paths.push(longer);
            }
        }
    }
    per_reader.sort_by_key(|(reader, _)| *reader);
    per_reader
}

/// The panes kept pane chain `chain` may keep, one per query that can reach
/// it (its range-evaluation span), in `reaching` order; its pane count for
/// any other node.
fn retained_pane_options(
    chain: PhysicalCandidateId,
    nodes: &[PhysicalASAPDAGNode],
    first_producer: &[Option<PhysicalCandidateId>],
    reaching: &[usize],
    entries: &[QueryWorkloadEntry],
) -> Vec<u64> {
    let node = &nodes[chain.0];
    let kept = matches!(
        (&node.payload, node.materialization),
        (
            Operator::ASAP(ASAPOp::PaneChain { .. }),
            Materialization::MatAtIngestion(_)
                | Materialization::MatAtQueryKeptAcrossEvaluations(_)
        )
    );
    if !kept {
        return vec![asap_cost::panes_of(node)];
    }
    // The chain over the time shifts below it, which set its input offset.
    let mut below = Vec::new();
    let mut current = first_producer[chain.0];
    while let Some(producer) = current {
        if !matches!(
            nodes[producer.0].payload,
            Operator::NonASAP(NonASAPOp::TimeShift { .. })
        ) || below.contains(&producer)
        {
            break;
        }
        below.push(producer);
        current = first_producer[producer.0];
    }
    let mut dag_nodes: Vec<&PhysicalASAPDAGNode> =
        below.iter().rev().map(|class| &nodes[class.0]).collect();
    dag_nodes.push(node);
    let dag = nodes_dag(&dag_nodes);
    let mut options = Vec::new();
    let reach = vec![HashSet::from([chain.0])];
    let spans: Vec<_> = match reaching.is_empty() {
        true => vec![None],
        false => reaching
            .iter()
            .map(|&query| entries.get(query).and_then(|entry| entry.range_evaluation))
            .collect(),
    };
    for range in spans {
        let retention = asap_cost::pane_retention_ms(&dag, &reach, &[range]);
        let panes = asap_cost::retained_panes(node, &retention);
        options.push(panes);
    }
    options
}

/// Whether two producer outputs cost their readers the same.
fn same_producer_output(left: &ProducerOutput<'_>, right: &ProducerOutput<'_>) -> bool {
    left.output.rows == right.output.rows
        && left.output.bytes == right.output.bytes
        && left.sources == right.sources
        && left.family == right.family
        && left.unchanged_columns == right.unchanged_columns
        && (std::ptr::eq(left.payload, right.payload)
            || left.payload.map_children(|_| 0usize) == right.payload.map_children(|_| 0usize))
        && (std::ptr::eq(left.output_schema, right.output_schema)
            || left.output_schema == right.output_schema)
}

/// `usage` as a reader of `node` sees it.
fn producer_output<'a>(node: &'a PhysicalASAPDAGNode, usage: &NodeUsage<'a>) -> ProducerOutput<'a> {
    ProducerOutput {
        payload: &node.payload,
        output_schema: &node.output_schema,
        output: usage.output,
        sources: usage.sources.clone(),
        family: usage.family,
        unchanged_columns: usage.unchanged_columns.clone(),
    }
}

/// Per candidate, every `(reader candidate, input)` that may read it, ascending.
fn readers_of(
    producer_candidates_per_input: &[Vec<Vec<PhysicalCandidateId>>],
) -> Vec<Vec<(PhysicalCandidateId, usize)>> {
    let mut readers = vec![Vec::new(); producer_candidates_per_input.len()];
    for (reader, inputs) in producer_candidates_per_input.iter().enumerate() {
        for (input, producers) in inputs.iter().enumerate() {
            for producer in producers {
                readers[producer.0].push((PhysicalCandidateId(reader), input));
            }
        }
    }
    readers
}

/// Each candidate's [`CandidateData`] (module documentation, "Usage per execution
/// and its context"). `reaching[k]` are the queries that can reach candidate
/// `k` before pruning.
#[allow(clippy::too_many_arguments)]
fn candidate_data(
    physical: &PhysicalASAPMemoCandidates,
    links: &CandidateLinks,
    entries: &[QueryWorkloadEntry],
    reaching: &[Vec<usize>],
    contexts: &[ExecutionContext],
    output_candidates: &[Vec<PhysicalCandidateId>],
    data: &DataWorkload,
    cost_inputs: &CostInputs,
) -> Vec<CandidateData> {
    let count = physical.candidates.len();
    let formula_scale = cost_inputs.frozen_calibration_factor.unwrap_or(1.0);
    let nodes: Vec<PhysicalASAPDAGNode> = (0..count)
        .map(|class| catalog_node(physical, links, PhysicalCandidateId(class)))
        .collect();
    let node_by_id: asap_cost::NodeMap<'_> = nodes.iter().map(|node| (node.id, node)).collect();
    let first_producer: Vec<Option<PhysicalCandidateId>> = links
        .producers_per_input
        .iter()
        .map(|producers_per_input| {
            producers_per_input
                .first()
                .and_then(|producers| producers.first().copied())
        })
        .collect();
    let first_producers_of: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = links
        .producers_per_input
        .iter()
        .enumerate()
        .map(|(class, producers_per_input)| {
            (
                class,
                producers_per_input
                    .iter()
                    .filter_map(|producers| producers.first().map(|producer| producer.0))
                    .collect(),
            )
        })
        .collect();
    let readers = readers_of(&links.producers_per_input);
    // Per candidate, the distinct outputs it may give its readers; `None` until
    // it is costed.
    let mut outputs: Vec<Option<Result<Vec<ProducerOutput<'_>>, String>>> = vec![None; count];
    let mut result: Vec<CandidateData> = (0..count)
        .map(|class| {
            let node = physical.physical_node(PhysicalCandidateId(class));
            let operator_node = physical.candidates[class].operator_node;
            let accuracy_budget_parts = physical.operator_nodes_of_candidates_for_memo_groups
                [operator_node.0]
                .accuracy_budget_parts;
            CandidateData {
                approximate: outputs_approximate_values(node),
                accuracy_budget_parts,
                usage: Err("not costed".to_string()),
                ..CandidateData::default()
            }
        })
        .collect();
    // Producers first (Kahn's order). Candidate links can close a cycle
    // (e.g. a widened scan's `Project` that reads its own memo group): when no candidate
    // is ready, the smallest one each of whose inputs has a costed producer
    // is costed from those producers alone, and marked
    // [`UsageContext::ProducerOnACycle`].
    let producer_sets: Vec<BTreeSet<usize>> = links
        .producers_per_input
        .iter()
        .map(|producers_per_input| producers_per_input.iter().flatten().map(|p| p.0).collect())
        .collect();
    let mut waiting: Vec<usize> = producer_sets.iter().map(BTreeSet::len).collect();
    let mut ready: std::collections::BinaryHeap<std::cmp::Reverse<usize>> = (0..count)
        .filter(|&class| waiting[class] == 0)
        .map(std::cmp::Reverse)
        .collect();
    let mut costed_classes = vec![false; count];
    loop {
        let class = match ready.pop() {
            Some(std::cmp::Reverse(class)) => class,
            None => match (0..count).find(|&class| {
                !costed_classes[class]
                    && links.producers_per_input[class]
                        .iter()
                        .all(|producers| producers.iter().any(|p| costed_classes[p.0]))
            }) {
                Some(class) => class,
                None => break,
            },
        };
        if std::mem::replace(&mut costed_classes[class], true) {
            continue;
        }
        let mut readers_of_class: BTreeSet<usize> = BTreeSet::new();
        for &(reader, _) in &readers[class] {
            readers_of_class.insert(reader.0);
        }
        for reader in readers_of_class {
            waiting[reader] -= 1;
            if waiting[reader] == 0 && !costed_classes[reader] {
                ready.push(std::cmp::Reverse(reader));
            }
        }
        let class = PhysicalCandidateId(class);
        let node = &nodes[class.0];
        let inputs = &links.producers_per_input[class.0];
        // Per input, the distinct outputs of its costed producer candidates.
        let mut producer_options: Vec<Vec<ProducerOutput<'_>>> = Vec::with_capacity(inputs.len());
        let mut failed = None;
        let mut on_a_cycle = false;
        // A scan's outputs at windows that do not contain this reader's own
        // window are never what it reads (the scan reads at least the
        // union of its readers' windows).
        let reader = class;
        let reads_output =
            |result: &[CandidateData], producer: PhysicalCandidateId, at: usize| -> bool {
                let windowed = &result[producer.0].usage_per_window;
                if windowed.is_empty() {
                    return true;
                }
                let Some((_, own)) = result[producer.0]
                    .windows_per_reader
                    .iter()
                    .find(|(known, _)| *known == reader)
                else {
                    return true;
                };
                let own = crate::milp::cost_terms::window_union(own.iter().copied());
                // An unbounded read is priced at the other readers' widest
                // window (`plan_cost`), whatever it is.
                if own == ScanWindow::Unbounded {
                    return true;
                }
                windowed
                    .iter()
                    .any(|&(window, _, output)| output == at && window_contains(window, own))
            };
        for producers in inputs {
            let mut options: Vec<ProducerOutput<'_>> = Vec::new();
            for producer in producers {
                match &outputs[producer.0] {
                    Some(Ok(produced)) => {
                        for (at, output) in produced.iter().enumerate() {
                            if !reads_output(&result, *producer, at) {
                                continue;
                            }
                            if !options
                                .iter()
                                .any(|known| same_producer_output(known, output))
                            {
                                options.push(output.clone());
                            }
                        }
                    }
                    // An uncostable producer is one no plan uses: the
                    // others still serve this input.
                    Some(Err(_)) => {}
                    None => on_a_cycle = true,
                }
            }
            if options.is_empty() {
                failed.get_or_insert_with(|| {
                    format!(
                        "input {} has no producer with a usage (producers {:?})",
                        producer_options.len(),
                        producers.iter().map(|p| p.0).collect::<Vec<_>>()
                    )
                });
            }
            producer_options.push(options);
        }
        if let Some(reason) = failed {
            outputs[class.0] = Some(Err(reason.clone()));
            result[class.0].usage = Err(reason);
            continue;
        }
        let windows = match (&node.payload, node.output_state.timing) {
            (Operator::NonASAP(NonASAPOp::Scan { .. }), ExecutionTiming::QueryTime) => {
                result[class.0].windows_per_reader =
                    scan_windows_per_reader(class, &nodes, &readers);
                scan_window_options(
                    class,
                    &result[class.0].windows_per_reader,
                    contexts,
                    &links.kind,
                    &links.producers_per_input,
                    output_candidates,
                )
            }
            _ => vec![ScanWindow::Unbounded],
        };
        let retained_per_query =
            retained_pane_options(class, &nodes, &first_producer, &reaching[class.0], entries);
        let mut retained: Vec<u64> = Vec::new();
        for &panes in &retained_per_query {
            if !retained.contains(&panes) {
                retained.push(panes);
            }
        }
        if retained.len() > 1 {
            result[class.0].retained_panes_per_query = reaching[class.0]
                .iter()
                .copied()
                .zip(retained_per_query.iter().copied())
                .collect();
        }
        let ingestion_window_ms = (node.output_state.timing == ExecutionTiming::IngestionTime)
            .then(|| asap_cost::maintained_window_ms(class.0, &node_by_id, &first_producers_of))
            .flatten();
        // Every combination of the options.
        let combinations = producer_options
            .iter()
            .map(Vec::len)
            .chain([windows.len(), retained.len()])
            .try_fold(1usize, |total, options| total.checked_mul(options))
            .unwrap_or(usize::MAX);
        // Each costed combination: its usage, its output, and its picks
        // (the option per input, the window and the retained panes).
        let mut costed: Vec<(CandidateUsage, ProducerOutput<'_>, Picks)> = Vec::new();
        let mut error = None;
        for index in 0..combinations.min(MAX_CONTEXTS) {
            let mut rest = index;
            let mut pick = |options: usize| {
                let picked = rest % options;
                rest /= options;
                picked
            };
            let retained_pick = pick(retained.len());
            let retained_panes = retained[retained_pick];
            let window_pick = pick(windows.len());
            let window = windows[window_pick];
            let mut option_picks: Vec<usize> = producer_options
                .iter()
                .rev()
                .map(|options| pick(options.len()))
                .collect();
            option_picks.reverse();
            let producers: Vec<ProducerOutput<'_>> = producer_options
                .iter()
                .zip(&option_picks)
                .map(|(options, &picked)| options[picked].clone())
                .collect();
            let usage_at = |per_second: f64| {
                asap_cost::node_usage(
                    node,
                    &producers,
                    (1, per_second),
                    window,
                    retained_panes,
                    ingestion_window_ms,
                    data,
                    cost_inputs,
                    formula_scale,
                )
            };
            match (usage_at(1.0), usage_at(0.0)) {
                (Ok(executed), Ok(standing)) => {
                    let usage = candidate_usage(&executed, &standing, node.output_state.timing);
                    costed.push((
                        usage,
                        producer_output(node, &executed),
                        Picks {
                            options: option_picks,
                            window: window_pick,
                            retained: retained_pick,
                        },
                    ));
                }
                (Err(e), _) | (_, Err(e)) => {
                    error.get_or_insert(e.to_string());
                }
            }
        }
        // A context `asap-cost` cannot cost is one no plan can use.
        let uncostable_context = error.is_some();
        if costed.is_empty() {
            let error = error.unwrap_or_else(|| "no context".into());
            outputs[class.0] = Some(Err(error.clone()));
            result[class.0].usage = Err(error);
            continue;
        }
        let first = costed[0].0;
        let mut depends_on = Vec::new();
        // Its distinct outputs, and the one each costed combination gives.
        let mut produced: Vec<ProducerOutput<'_>> = Vec::new();
        let mut output_of_combination = Vec::with_capacity(costed.len());
        for (_, output, _) in &costed {
            let at = match produced
                .iter()
                .position(|known| same_producer_output(known, output))
            {
                Some(at) => at,
                None => {
                    produced.push(output.clone());
                    produced.len() - 1
                }
            };
            output_of_combination.push(at);
        }
        // What a combination gives its plan: its usage and its output.
        let given: Vec<(CandidateUsage, usize)> = costed
            .iter()
            .zip(&output_of_combination)
            .map(|((usage, _, _), &output)| (*usage, output))
            .collect();
        // The inputs whose option changes the usage or the output, and
        // whether the window does: two costed combinations differing only
        // there give different usages or outputs.
        let changes = |differs: &dyn Fn(&Picks, &Picks) -> bool| {
            costed.iter().zip(&given).any(|((_, _, picks), gives)| {
                costed
                    .iter()
                    .zip(&given)
                    .any(|((_, _, others), other)| differs(picks, others) && gives != other)
            })
        };
        let inputs_changing_usage: Vec<usize> = (0..producer_options.len())
            .filter(|&input| {
                changes(&|a: &Picks, b: &Picks| {
                    a.window == b.window
                        && a.retained == b.retained
                        && a.options[input] != b.options[input]
                        && (0..a.options.len())
                            .all(|other| other == input || a.options[other] == b.options[other])
                })
            })
            .collect();
        let window_changes_usage = changes(&|a: &Picks, b: &Picks| {
            a.window != b.window && a.retained == b.retained && a.options == b.options
        });
        if !inputs_changing_usage.is_empty() {
            // Per changing input, each producer and the options it gives:
            // one choice per output of a producer with several.
            let options_of_producers: Vec<Vec<(ProducerChoice, Vec<usize>)>> =
                inputs_changing_usage
                    .iter()
                    .map(|&input| {
                        let mut choices = Vec::new();
                        for &producer in &inputs[input] {
                            let Some(Ok(produced)) = &outputs[producer.0] else {
                                continue;
                            };
                            let option_of = |output: &ProducerOutput<'_>| {
                                producer_options[input]
                                    .iter()
                                    .position(|known| same_producer_output(known, output))
                            };
                            if produced.len() == 1 {
                                choices.push((
                                    ProducerChoice {
                                        class: producer,
                                        output: None,
                                    },
                                    produced.iter().filter_map(option_of).collect(),
                                ));
                                continue;
                            }
                            for (at, output) in produced.iter().enumerate() {
                                if !reads_output(&result, producer, at) {
                                    continue;
                                }
                                choices.push((
                                    ProducerChoice {
                                        class: producer,
                                        output: Some(at),
                                    },
                                    option_of(output).into_iter().collect(),
                                ));
                            }
                        }
                        choices
                    })
                    .collect();
            let choices = options_of_producers
                .iter()
                .try_fold(1usize, |total, producers| {
                    total.checked_mul(producers.len())
                })
                .unwrap_or(usize::MAX);
            if choices > MAX_PRODUCER_CHOICES {
                depends_on.push(UsageContext::TooManyContexts);
            } else {
                for index in 0..choices {
                    let mut rest = index;
                    let chosen: Vec<&(ProducerChoice, Vec<usize>)> = options_of_producers
                        .iter()
                        .map(|producers| {
                            let picked = &producers[rest % producers.len()];
                            rest /= producers.len();
                            picked
                        })
                        .collect();
                    let matching: Vec<&(CandidateUsage, usize)> = costed
                        .iter()
                        .zip(&given)
                        .filter(|((_, _, picks), _)| {
                            inputs_changing_usage.iter().zip(&chosen).all(
                                |(&input, (_, options))| options.contains(&picks.options[input]),
                            )
                        })
                        .map(|(_, gives)| gives)
                        .collect();
                    let costliest = matching.iter().copied().max_by(|a, b| {
                        let total = |usage: &CandidateUsage| {
                            usage.cost_per_execution.unwrap_or(0.0)
                                + usage.standing_cost_per_second.unwrap_or(0.0)
                                + usage.work_ms_per_execution
                        };
                        total(&a.0).total_cmp(&total(&b.0))
                    });
                    if matching.iter().any(|gives| Some(*gives) != costliest) {
                        result[class.0].producer_usage_ambiguous = true;
                    }
                    result[class.0].usage_per_producers.push((
                        chosen.iter().map(|(choice, _)| *choice).collect(),
                        costliest.copied(),
                    ));
                }
                result[class.0].inputs_changing_usage = inputs_changing_usage;
            }
        }
        if retained.len() > 1 {
            let retained_changes_usage = changes(&|a: &Picks, b: &Picks| {
                a.retained != b.retained && a.window == b.window && a.options == b.options
            });
            if retained_changes_usage {
                for (pick, &panes) in retained.iter().enumerate() {
                    if let Some(at) = costed
                        .iter()
                        .position(|(_, _, picks)| picks.retained == pick)
                    {
                        result[class.0]
                            .usage_per_retained_panes
                            .push((panes, given[at].0));
                    }
                }
            } else {
                result[class.0].retained_panes_per_query.clear();
            }
        }
        // A query-time scan keeps its usage per window even when every
        // costed window gives the same: a window `asap-cost` cannot cost
        // (an unbounded read with no `rows_held`) must stay unusable.
        let query_time_scan = matches!(
            (&node.payload, node.output_state.timing),
            (
                Operator::NonASAP(NonASAPOp::Scan { .. }),
                ExecutionTiming::QueryTime
            )
        );
        if window_changes_usage || query_time_scan {
            for (pick, window) in windows.iter().enumerate() {
                if let Some(at) = costed.iter().position(|(_, _, picks)| picks.window == pick) {
                    let (usage, output) = given[at];
                    result[class.0]
                        .usage_per_window
                        .push((*window, usage, output));
                }
            }
        }
        if given.iter().any(|gives| gives.0 != first) {
            if producer_options.iter().any(|options| options.len() > 1) {
                depends_on.push(UsageContext::ProducerOutput);
            }
            if windows.len() > 1 {
                depends_on.push(UsageContext::ScanWindowOfReader);
            }
            if retained.len() > 1 {
                depends_on.push(UsageContext::PaneRetentionOfReachingQueries);
            }
        }
        if on_a_cycle {
            depends_on.push(UsageContext::ProducerOnACycle);
        }
        if uncostable_context {
            depends_on.push(UsageContext::UncostableContext);
        }
        if combinations > MAX_CONTEXTS {
            depends_on.push(UsageContext::TooManyContexts);
        }
        result[class.0].output_count = produced.len();
        outputs[class.0] = Some(Ok(produced));
        result[class.0].usage = Ok(first);
        result[class.0].usage_depends_on = depends_on;
    }
    for (class, costed) in costed_classes.iter().enumerate() {
        if !costed {
            result[class].usage = Err("its producer links form a cycle".into());
        }
    }
    result
}

/// One costed combination of a class's context options, by index: the
/// option per input, the window and the retained panes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Picks {
    options: Vec<usize>,
    window: usize,
    retained: usize,
}

/// The numbers of one execution (`executed`, at one execution per second)
/// and of none (`standing`) of a node at `timing`.
fn candidate_usage(
    executed: &NodeUsage<'_>,
    standing: &NodeUsage<'_>,
    timing: ExecutionTiming,
) -> CandidateUsage {
    let standing_cost = standing.cost.cost_per_second;
    CandidateUsage {
        cpu_ops_per_execution: executed.cpu_ops_per_second,
        scanned_bytes_per_execution: executed.scanned_bytes_per_second,
        work_ms_per_execution: executed.work_ms_per_execution,
        own_latency_ms: match timing {
            ExecutionTiming::QueryTime => executed.work_ms_per_execution,
            ExecutionTiming::IngestionTime => 0.0,
        },
        cost_per_execution: executed
            .cost
            .cost_per_second
            .zip(standing_cost)
            .map(|(total, standing)| total - standing),
        standing_cost_per_second: standing_cost,
        average_memory_bytes: executed.average_memory_bytes,
        peak_memory_bytes: executed.peak_memory_bytes,
        disk_bytes: executed.disk_bytes,
        held_bytes: executed.held.map_or(0, |(bytes, _, _)| bytes),
        raw_data_kept_bytes: executed.raw_scan.map_or(0, |(_, bytes)| bytes),
        maintained_window_bytes: executed.maintained.unwrap_or(0),
        output_rows: executed.output.rows,
        output_bytes: executed.output.bytes,
    }
}

// ---------------------------------------------------------------------------
// Dominance
// ---------------------------------------------------------------------------

/// What one candidate is compared on (module documentation, "Dominance").
#[derive(Debug, Clone)]
pub(crate) struct DominanceProfile<'a> {
    /// Must be equal for one candidate to dominate another.
    pub(crate) same: DominanceKey<'a>,
    pub(crate) guarantee: Option<&'a AccuracyGuarantee>,
}

/// The parts of a [`DominanceProfile`] that must be equal.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DominanceKey<'a> {
    pub(crate) group: MemoGroupId,
    pub(crate) timing: ExecutionTiming,
    pub(crate) materialization: Materialization,
    pub(crate) shareable: bool,
    pub(crate) producer_candidates_per_input: &'a [Vec<PhysicalCandidateId>],
    pub(crate) readers: Vec<(PhysicalCandidateId, usize)>,
    pub(crate) output_references: Vec<usize>,
    pub(crate) reader_constraint_ids: Vec<u32>,
    pub(crate) may_keep_across_evaluations_per_sole_reader_query: &'a [bool],
    pub(crate) output_schema: &'a Schema,
    pub(crate) operator_kind: OperatorKindForReaders,
}

/// The parts of a node's operator its readers' costs read: its kind, and a
/// pane chain's panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OperatorKindForReaders {
    PaneChain { pane_width_ms: u64, pane_count: u32 },
    Other(std::mem::Discriminant<Operator>, &'static str),
}

/// Each candidate's [`DominanceProfile`]. `output_candidates` are `Out(r)` before
/// pruning.
fn dominance_profiles<'a>(
    physical: &'a PhysicalASAPMemoCandidates,
    links: &'a CandidateLinks,
    output_candidates: &[Vec<PhysicalCandidateId>],
) -> Vec<DominanceProfile<'a>> {
    let constraint_ids = reader_constraint_ids_per_candidate(physical);
    let mut readers = readers_of(&links.producers_per_input);
    for list in &mut readers {
        list.sort_unstable();
    }
    let mut output_refs_of = vec![Vec::new(); physical.candidates.len()];
    for (reference, out) in output_candidates.iter().enumerate() {
        for class in out {
            output_refs_of[class.0].push(reference);
        }
    }
    links
        .producers_per_input
        .iter()
        .enumerate()
        .map(|(class, producers_per_input)| {
            let copy = PhysicalCandidateId(class);
            let candidate = &physical.candidates[copy.0];
            let node = physical.physical_node(copy);
            DominanceProfile {
                same: DominanceKey {
                    group: physical.memo_group_of(copy),
                    timing: candidate.timing,
                    materialization: candidate.materialization,
                    shareable: links.is_shareable(copy),
                    producer_candidates_per_input: producers_per_input,
                    readers: std::mem::take(&mut readers[class]),
                    output_references: std::mem::take(&mut output_refs_of[class]),
                    reader_constraint_ids: constraint_ids[copy.0].clone(),
                    may_keep_across_evaluations_per_sole_reader_query: &candidate
                        .may_keep_across_evaluations_per_sole_reader_query,
                    output_schema: &node.schema,
                    operator_kind: match &node.operator {
                        Operator::ASAP(ASAPOp::PaneChain {
                            pane_width_ms,
                            pane_count,
                            ..
                        }) => OperatorKindForReaders::PaneChain {
                            pane_width_ms: *pane_width_ms,
                            pane_count: *pane_count,
                        },
                        operator => OperatorKindForReaders::Other(
                            std::mem::discriminant(operator),
                            node.operator.kind_name(),
                        ),
                    },
                },
                guarantee: node.guarantee.as_ref(),
            }
        })
        .collect()
}

/// Whether `stronger` is at least as strong as `weaker`: equal, or `stronger`
/// exact, or both in one metric and scope with constant bounds and failure
/// probabilities no larger. Anything else (an unknown statistic, different
/// metrics) is not comparable.
pub(crate) fn guarantee_at_least_as_strong(
    stronger: Option<&AccuracyGuarantee>,
    weaker: Option<&AccuracyGuarantee>,
) -> bool {
    match (stronger, weaker) {
        (None, None) => true,
        (Some(stronger), Some(weaker)) => {
            if stronger.spec == weaker.spec || stronger.is_exact() {
                return true;
            }
            stronger.spec.accuracy_metric == weaker.spec.accuracy_metric
                && stronger.spec.accuracy_scope == weaker.spec.accuracy_scope
                && matches!(
                    (stronger.spec.error.evaluate(), weaker.spec.error.evaluate()),
                    (Some(s), Some(w)) if s <= w
                )
                && matches!(
                    (
                        stronger.spec.failure_probability.evaluate(),
                        weaker.spec.failure_probability.evaluate()
                    ),
                    (Some(s), Some(w)) if s <= w
                )
        }
        _ => false,
    }
}

/// Every number of `usage` that a candidate it dominates must not beat, in a
/// fixed order.
fn compared_numbers(usage: &CandidateUsage) -> [f64; 12] {
    [
        usage.cpu_ops_per_execution,
        usage.scanned_bytes_per_execution,
        usage.work_ms_per_execution,
        usage.own_latency_ms,
        usage.average_memory_bytes,
        usage.peak_memory_bytes as f64,
        usage.disk_bytes as f64,
        usage.held_bytes as f64,
        usage.raw_data_kept_bytes as f64,
        usage.maintained_window_bytes as f64,
        usage.output_rows as f64,
        usage.output_bytes as f64,
    ]
}

/// Whether candidate `by` dominates candidate `candidate` (module documentation).
pub(crate) fn dominates(
    by: (&DominanceProfile<'_>, &CandidateData),
    class: (&DominanceProfile<'_>, &CandidateData),
) -> bool {
    let ((by_profile, by_data), (profile, data)) = (by, class);
    let (Ok(by_usage), Ok(usage)) = (&by_data.usage, &data.usage) else {
        return false;
    };
    by_profile.same == profile.same
        && by_data.usage_depends_on.is_empty()
        && data.usage_depends_on.is_empty()
        && by_data.approximate == data.approximate
        && by_data.accuracy_budget_parts == data.accuracy_budget_parts
        && guarantee_at_least_as_strong(by_profile.guarantee, profile.guarantee)
        && compared_numbers(by_usage)
            .iter()
            .zip(compared_numbers(usage))
            .all(|(by, class)| *by <= class)
}

/// The candidates another candidate dominates (module documentation), each with
/// the smallest dominating candidate that is kept. Among candidates that dominate
/// each other, the smallest id is kept.
pub(crate) fn prune_dominated_candidates(
    profiles: &[DominanceProfile<'_>],
    candidate_data: &[CandidateData],
) -> Vec<DominatedCandidate> {
    // Only candidates with equal keys can dominate each other.
    let mut buckets: Vec<Vec<usize>> = Vec::new();
    let mut bucket_of_key: HashMap<(MemoGroupId, ExecutionTiming, Materialization), Vec<usize>> =
        HashMap::new();
    for (class, profile) in profiles.iter().enumerate() {
        let candidates = bucket_of_key
            .entry((
                profile.same.group,
                profile.same.timing,
                profile.same.materialization,
            ))
            .or_default();
        match candidates
            .iter()
            .find(|&&bucket| profiles[buckets[bucket][0]].same == profile.same)
        {
            Some(&bucket) => buckets[bucket].push(class),
            None => {
                candidates.push(buckets.len());
                buckets.push(vec![class]);
            }
        }
    }
    let mut pruned = Vec::new();
    for bucket in buckets.iter().filter(|bucket| bucket.len() > 1) {
        for &class in bucket {
            let dominated_by = bucket.iter().copied().find(|&other| {
                other != class
                    && dominates(
                        (&profiles[other], &candidate_data[other]),
                        (&profiles[class], &candidate_data[class]),
                    )
                    && (other < class
                        || !dominates(
                            (&profiles[class], &candidate_data[class]),
                            (&profiles[other], &candidate_data[other]),
                        ))
            });
            if let Some(other) = dominated_by {
                pruned.push(DominatedCandidate {
                    class: PhysicalCandidateId(class),
                    dominated_by: PhysicalCandidateId(other),
                    reason: format!(
                        "candidate {other} has the same memo group, producers, readers and \
                         (approx, parts), a guarantee at least as strong, and no larger \
                         usage, latency, output or held state"
                    ),
                });
            }
        }
    }
    pruned.sort_by_key(|pruned| pruned.class);
    pruned
}

mod accuracy_positions;

use accuracy_positions::AccuracyPositions;

#[cfg(test)]
mod tests;
