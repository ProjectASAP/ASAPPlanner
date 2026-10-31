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
use asap_types::ir::physical_export::{
    PhysicalASAPDAG, PhysicalASAPDAGNode, PhysicalASAPNodeId, ScanWindow,
};
use asap_types::ir::properties::execution_data_state::{DataPrimitive, ExecutionDataState};
use asap_types::ir::properties::{AccuracyGuarantee, ExecutionTiming};
use asap_types::ir::schema::{FieldDataType, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode};
use asap_types::physical::Materialization;
use asap_types::stages::{output_references, MemoGroupId, PhysicalASAPMemoCandidates};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use crate::SelectionModels;

use asap_types::ir::OperatorResultKind;
use asap_types::stages::{PhysicalCandidateId, ReaderConstraint};

/// The most contexts (combinations of producer outputs, scan windows and
/// pane retentions) one candidate is costed under; beyond it the candidate's usage
/// is marked [`UsageContext::TooManyContexts`].
pub(crate) const MAX_CONTEXTS: usize = 64;

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
    /// Indexed by [`PhysicalCandidateId`]: the queries that can reach the
    /// candidate after pruning, ascending.
    pub(crate) queries_reaching: Vec<Vec<usize>>,
    /// One per query, in workload order.
    pub(crate) query_owners: Vec<OwnerRegion>,
    /// One per shareable candidate left that some query can reach, ascending by
    /// candidate: its `m_s` instance owners share one region.
    pub(crate) instance_owners: Vec<InstanceOwners>,
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

/// The choices one owner holds: its region and how it reads outside it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OwnerRegion {
    /// `Reg(o)`: the stateless candidates reachable from its top before a
    /// shareable candidate, ascending.
    pub(crate) classes: Vec<PhysicalCandidateId>,
    /// Producer links inside it: `(reader, input, producer)` with both in
    /// `candidates`, or the reader the instance's own candidate.
    pub(crate) links: Vec<(PhysicalCandidateId, usize, PhysicalCandidateId)>,
    /// Its boundary reads: `(reader, input, shareable candidate)`, the reader
    /// in `candidates` or the instance's own candidate.
    pub(crate) boundary_reads: Vec<(PhysicalCandidateId, usize, PhysicalCandidateId)>,
    /// For a query owner, its output references' shareable candidates:
    /// `(output reference, shareable candidate)` read directly as the output.
    pub(crate) output_reads: Vec<(usize, PhysicalCandidateId)>,
}

/// The instance owners `(s, j)`, `j < m_s`, of shareable candidate `s`: their
/// regions are equal, so they are stored once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstanceOwners {
    pub(crate) class: PhysicalCandidateId,
    /// `m_s`: the number of queries that can reach `s`.
    pub(crate) instance_count: usize,
    /// The region below `s`; its links and boundary reads include `s`'s own
    /// inputs.
    pub(crate) region: OwnerRegion,
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

/// The state a shareable candidate holds (#1095 Step 2), which makes it worth
/// reading from several readers. Checked in this order; the first that
/// applies is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum HeldState {
    /// `MatAtQueryKeptAcrossEvaluations`: a result kept across evaluations.
    KeptAcrossEvaluations,
    /// `MatAtQuery`: a result built once per evaluation for its readers.
    MaterializedAtQuery,
    /// `MatAtIngestion`: a result built as data arrives and kept for readers.
    MaterializedAtIngestion,
    /// A tumbling-pane chain (`PaneChain`), whatever its materialization.
    PaneChain,
    /// Unfinalized summary or accumulator state
    /// ([`OperatorResultKind::State`]), e.g. a `SummaryAgg` or `SummaryMerge`.
    SummaryState,
    /// A query-time `Scan`: it reads raw data the deployment retains, so it is
    /// where raw-data retention is paid. A shared scan of the shared-scan
    /// rule (#951) is a `Scan` too and is covered here.
    RetainedRawData,
}

/// The state a candidate of `node` at `timing` and `materialization` holds,
/// or `None` for a stateless one, which each reader computes for itself.
pub(crate) fn held_state(
    node: &OperatorNode,
    timing: ExecutionTiming,
    materialization: Materialization,
) -> Option<HeldState> {
    match materialization {
        Materialization::MatAtQueryKeptAcrossEvaluations(_) => {
            return Some(HeldState::KeptAcrossEvaluations)
        }
        Materialization::MatAtQuery(_) => return Some(HeldState::MaterializedAtQuery),
        Materialization::MatAtIngestion(_) => return Some(HeldState::MaterializedAtIngestion),
        Materialization::NotMaterialized => {}
    }
    if matches!(node.asap(), Some(asap_types::ir::ASAPOp::PaneChain { .. })) {
        Some(HeldState::PaneChain)
    } else if node.result_kind == OperatorResultKind::State {
        Some(HeldState::SummaryState)
    } else if timing == ExecutionTiming::QueryTime
        && matches!(node.operator, Operator::NonASAP(NonASAPOp::Scan { .. }))
    {
        Some(HeldState::RetainedRawData)
    } else {
        None
    }
}

/// Per candidate of a [`PhysicalASAPMemoCandidates`]: its producer
/// candidates per input, ascending and without repeats, and the state it
/// holds.
struct CandidateLinks {
    producers_per_input: Vec<Vec<Vec<PhysicalCandidateId>>>,
    held_state: Vec<Option<HeldState>>,
}

impl CandidateLinks {
    fn new(physical: &PhysicalASAPMemoCandidates) -> Self {
        let producers_per_input = physical
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
        let held_state = physical
            .candidates
            .iter()
            .enumerate()
            .map(|(id, candidate)| {
                held_state(
                    physical.physical_node(PhysicalCandidateId(id)),
                    candidate.timing,
                    candidate.materialization,
                )
            })
            .collect();
        Self {
            producers_per_input,
            held_state,
        }
    }

    /// Whether `candidate` holds state, so readers may share one instance;
    /// a stateless candidate is costed per reader.
    fn is_shareable(&self, candidate: PhysicalCandidateId) -> bool {
        self.held_state[candidate.0].is_some()
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
        plan_selection: cost_inputs,
    };
    prepare_milp_inputs_with(physical, entries, data, &models)
}

/// [`MilpInputs`] for `physical`'s candidates, its accuracy positions
/// checked with `models` (the deployment's accuracy model and evidence) and
/// its usage costed with `models.plan_selection`.
pub(crate) fn prepare_milp_inputs_with(
    physical: &PhysicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: &SelectionModels<'_>,
) -> MilpInputs {
    let cost_inputs = models.plan_selection;
    let links = &CandidateLinks::new(physical);
    let class_count = physical.candidates.len();
    let all_alive = vec![true; class_count];
    let unpruned_links = producer_candidates_left(links, &all_alive);
    let unpruned_outputs = output_candidates(physical, &all_alive);
    let unpruned_reach = queries_reaching(physical, &unpruned_links, &unpruned_outputs);
    let candidate_data =
        candidate_data(physical, links, entries, &unpruned_reach, data, cost_inputs);
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
    let queries_reaching =
        queries_reaching(physical, &producer_candidates_per_input, &output_candidates);
    let shareable = |class: PhysicalCandidateId| links.is_shareable(class);
    let output_refs = output_references(&physical.query_roots);
    let query_owners = (0..physical.query_roots.len())
        .map(|query| {
            let outputs: Vec<(usize, &[PhysicalCandidateId])> = output_refs
                .iter()
                .enumerate()
                .filter(|(_, (q, _))| *q == query)
                .map(|(reference, _)| (reference, output_candidates[reference].as_slice()))
                .collect();
            let mut region = region_below(
                outputs
                    .iter()
                    .flat_map(|(_, out)| out.iter().copied())
                    .filter(|&class| !shareable(class)),
                &producer_candidates_per_input,
                shareable,
            );
            region.output_reads = outputs
                .iter()
                .flat_map(|&(reference, out)| {
                    out.iter()
                        .copied()
                        .filter(|&class| shareable(class))
                        .map(move |class| (reference, class))
                })
                .collect();
            region
        })
        .collect();
    let instance_owners = (0..class_count)
        .map(PhysicalCandidateId)
        .filter(|&class| alive[class.0] && shareable(class))
        .filter_map(|class| {
            let instance_count = queries_reaching[class.0].len();
            (instance_count > 0).then(|| InstanceOwners {
                class,
                instance_count,
                region: region_below([class], &producer_candidates_per_input, shareable),
            })
        })
        .collect();
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
        queries_reaching,
        query_owners,
        instance_owners,
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

/// The region below `tops`: the stateless candidates reached from them before
/// a shareable candidate (the stateless tops included, a shareable top not),
/// the links among them and the boundary reads.
fn region_below(
    tops: impl IntoIterator<Item = PhysicalCandidateId>,
    producer_candidates_per_input: &[Vec<Vec<PhysicalCandidateId>>],
    shareable: impl Fn(PhysicalCandidateId) -> bool,
) -> OwnerRegion {
    let mut region = OwnerRegion::default();
    let mut seen = HashSet::new();
    let mut stack: Vec<PhysicalCandidateId> = tops.into_iter().collect();
    while let Some(reader) = stack.pop() {
        if !seen.insert(reader) {
            continue;
        }
        if !shareable(reader) {
            region.classes.push(reader);
        }
        for (input, producers) in producer_candidates_per_input[reader.0].iter().enumerate() {
            for &producer in producers {
                if shareable(producer) {
                    region.boundary_reads.push((reader, input, producer));
                } else {
                    region.links.push((reader, input, producer));
                    stack.push(producer);
                }
            }
        }
    }
    region.classes.sort_unstable();
    region.links.sort_unstable();
    region.boundary_reads.sort_unstable();
    region
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

/// The windows a query-time scan `scan` may read, one per way a reader
/// reads it (through time shifts), distinct; `Unbounded` alone when nothing
/// reads it.
fn scan_window_options(
    scan: PhysicalCandidateId,
    nodes: &[PhysicalASAPDAGNode],
    readers: &[Vec<(PhysicalCandidateId, usize)>],
) -> Vec<ScanWindow> {
    let mut windows = Vec::new();
    for (_, read) in scan_windows_per_reader(scan, nodes, readers) {
        for window in read {
            if !windows.contains(&window) {
                windows.push(window);
            }
        }
    }
    if windows.is_empty() {
        windows.push(ScanWindow::Unbounded);
    }
    windows
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
/// it (its range-evaluation span), distinct; its pane count for any other
/// node.
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
        if !options.contains(&panes) {
            options.push(panes);
        }
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
        && (std::ptr::eq(left.payload, right.payload) || left.payload == right.payload)
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
fn candidate_data(
    physical: &PhysicalASAPMemoCandidates,
    links: &CandidateLinks,
    entries: &[QueryWorkloadEntry],
    reaching: &[Vec<usize>],
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
        for producers in inputs {
            let mut options: Vec<ProducerOutput<'_>> = Vec::new();
            for producer in producers {
                match &outputs[producer.0] {
                    Some(Ok(produced)) => {
                        for output in produced {
                            if !options
                                .iter()
                                .any(|known| same_producer_output(known, output))
                            {
                                options.push(output.clone());
                            }
                        }
                    }
                    Some(Err(_)) => {
                        failed.get_or_insert_with(|| {
                            format!("producer class {} has no usage", producer.0)
                        });
                    }
                    None => on_a_cycle = true,
                }
            }
            if options.is_empty() {
                failed.get_or_insert_with(|| "an input has no producer with a usage".into());
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
                scan_window_options(class, &nodes, &readers)
            }
            _ => vec![ScanWindow::Unbounded],
        };
        let retained =
            retained_pane_options(class, &nodes, &first_producer, &reaching[class.0], entries);
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
        if window_changes_usage {
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
