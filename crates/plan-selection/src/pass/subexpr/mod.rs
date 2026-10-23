//! Memo over logical sub-expressions (planner-layering design, "The
//! stages over a memo of sub-expressions"): the stages run on the
//! workload's distinct sub-expressions instead of whole-workload
//! candidates.
//!
//! [`search::memo`] builds the [`Memo`] in this order:
//!
//! 1. [`groups::memo_of`]: the workload's logical DAG hash-consed into
//!    groups (equal scans one group, keyless nodes such as range windows one
//!    per reader), children first, with each group's Pass 1 candidates.
//! 2. [`capability::add_shared_builds`]: summary capability. A reader of a
//!    build shared with other groups (one family over equal inputs), with a
//!    [`CouplingKind::SharedBuildSize`] site; identical expression is group
//!    identity and adds nothing.
//! 3. [`windows::add_pass2_candidates`]: window composition (panes, and
//!    newest-pane reads of a shared chain with a [`CouplingKind::PaneChain`]
//!    site), rollup and exact top-k reuse (candidates reading another
//!    group).
//! 4. [`scans::add_scan_widening`]: readers of a metric's scans widened to
//!    a shared label set, with a [`CouplingKind::ScanLabels`] site.
//! 5. [`scans::add_raw_horizons`]: a [`CouplingKind::RawHorizon`] site on
//!    every candidate that reads a raw window of a source read by two or
//!    more groups.
//! 6. [`scans::scan_materializations`]: a scan read through one window is
//!    only built once; through different windows, also per reader.
//!
//! Each site kind has its thresholds and reader check, and a candidate's
//! node at a threshold: [`capability::size_thresholds`] and
//! [`capability::resized_at_threshold`]; [`windows::chain_thresholds`] and
//! [`windows::read_at_threshold`]; [`scans::label_thresholds`] and
//! [`scans::widened_at_threshold`]; [`scans::horizon_thresholds`] (no
//! rewrite: Stage 3 prices the horizon).
//!
//! [`search::brute_force`] is the reference search, run for
//! [`crate::SearchAlgorithm::MemoBruteForce`]: every
//! consistent choice of one candidate per needed group and one threshold
//! per site its candidates read, composed into one plan (identical nodes
//! shared, equal resized builds made one), implemented by Stage 2 in every
//! materialization and priced by Stage 3. [`coefficients`] turns Stage 3's per-node
//! prices into the coefficients of [`milp`], the memo solved as a
//! mixed-integer linear program ([`crate::SearchAlgorithm::MemoMilp`]).
//! [`clusters`] splits the memo into independent clusters (queries no
//! plan's cost links), each solved with its own model and lazy loop, their
//! plans combined into the workload's.

#![allow(dead_code)]

pub(crate) mod capability;
pub(crate) mod clusters;
pub(crate) mod coefficients;
pub(crate) mod groups;
pub(crate) mod milp;
pub(crate) mod pricing;
pub(crate) mod scans;
pub(crate) mod search;
pub(crate) mod windows;

use std::rc::Rc;

use asap_types::ir::OperatorNode;
use asap_types::physical::Materialization;

/// A group, by index in [`Memo::groups`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct GroupId(pub u32);

/// One distinct logical sub-expression of the workload.
#[derive(Debug, Clone)]
pub(crate) struct MemoGroup {
    /// Its logical node as the frontends (and Pass 1 targets) see it; its
    /// children are other groups' nodes.
    pub node: Rc<OperatorNode>,
    /// Its child groups, in the node's child order.
    pub children: Vec<GroupId>,
    /// The groups that read it.
    pub parents: Vec<GroupId>,
    /// The queries whose roots reach it, by index in the workload.
    pub queries: Vec<usize>,
    /// The queries whose root it is.
    pub roots_of: Vec<usize>,
}

/// The hash-consed workload: its groups, children before parents.
#[derive(Debug, Clone, Default)]
pub(crate) struct Memo {
    pub groups: Vec<MemoGroup>,
    /// Each group's implementation candidates, by group index.
    pub candidates: Vec<Vec<ImplementationCandidate>>,
}

/// One way to implement a group: its node as Stage 2 implements it, over
/// child groups.
#[derive(Debug, Clone)]
pub(crate) struct ImplementationCandidate {
    /// The groups it implements: one, or several for a fused computation
    /// (the topmost first).
    pub implements: Vec<GroupId>,
    /// The implementation's own node(s): its root node, whose descendants
    /// down to `children` are its private nodes.
    pub node: Rc<OperatorNode>,
    /// The groups it reads, whose implementations are chosen separately.
    pub children: Vec<GroupId>,
    /// How it was made: by Pass 1, or by which Pass 2 transformation.
    pub origin: CandidateOrigin,
    /// The coupling sites its cost depends on.
    pub coupling_sites: Vec<CouplingSite>,
    /// The number of equal shares of its measures' accuracy budget its
    /// summaries are sized for: 1 for the whole budget, `n` when it is one
    /// of `n` approximate inputs of a query (a ratio's operand). Valid in a
    /// plan only when `n` is the largest number of approximate inputs among
    /// the queries that read it.
    pub budget_share: usize,
    /// Groups whose chosen implementation must be exact for this one to
    /// be valid (a rollup's finer group, a top-k reuse's input group); a
    /// child or a descendant.
    pub requires_exact_children: Vec<GroupId>,
    /// For a rollup, what it rolls up and from where; `None` otherwise.
    pub rollup_of: Option<RollupRead>,
    /// The materializations Stage 2 may give it; empty for every one
    /// Stage 2 allows. A scan group whose readers all read one window is
    /// only built once (`MatAtQuery`); one read through different windows
    /// may also be read per reader (`NotMaterialized`)
    /// ([`scans::scan_materializations`]).
    pub materializations: Vec<Materialization>,
    /// Sites other groups' chosen implementations must carry for this one
    /// to be valid: a sketch rollup's finer group exposing its state, a
    /// coarser grouping's host group building the pane chain it reads.
    pub requires_sites: Vec<(GroupId, CouplingSite)>,
}

/// What a [`CandidateOrigin::Rollup`] candidate reads, and what it
/// rewrites: the search checks that the finer group's chosen
/// implementation is the one the joint rule would roll up from.
#[derive(Debug, Clone)]
pub(crate) struct RollupRead {
    /// The finer group it reads.
    pub finer_group: GroupId,
    /// For a sketch rollup, the finer build whose state it merges, as its
    /// node reads it: a boundary node, priced with `finer_group`, which
    /// must choose an implementation building that state and exposing it
    /// at the same [`CouplingKind::SharedBuildSize`] site. `None` for an
    /// exact rollup, which reads `finer_group`'s output.
    pub finer_state: Option<Rc<OperatorNode>>,
    /// The coarse group's candidate it rewrites.
    pub coarse_candidate: Rc<OperatorNode>,
    /// That candidate's coarse build, which it no longer builds.
    pub coarse_build: Rc<OperatorNode>,
}

/// Where an implementation candidate came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CandidateOrigin {
    /// Pass 1: exact, exact accumulator, or a summary family and size.
    Pass1,
    /// One build serving several reader groups (summary capability).
    SharedBuild,
    /// Tumbling panes, or the newest panes of another group's chain.
    Panes,
    /// A coarse group regrouping a finer group.
    Rollup,
    /// A top-k read from another group's ranking.
    TopKReuse,
    /// Its metric's scans widened to a shared label set, read through a
    /// `Project` (scan widening).
    WidenedScans,
}

/// A choice several groups' costs depend on; equal keys are one site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct CouplingSite {
    pub key: u64,
    pub kind: CouplingKind,
}

/// What a coupling site's threshold is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum CouplingKind {
    /// A shared summary build's size, set by its strictest reader.
    SharedBuildSize,
    /// A shared pane chain: grid width, longest window, size.
    PaneChain,
    /// A metric's scan label set.
    ScanLabels,
    /// A source's raw-data horizon, the longest raw scan.
    RawHorizon,
}

/// The thresholds chosen for a cluster's coupling sites, by site key: the
/// context a candidate is priced in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub(crate) struct Context {
    /// Per site key, the index of the chosen threshold among the site's
    /// enumerated thresholds.
    pub thresholds: Vec<(u64, u32)>,
    /// The materialization chosen for each shared group in the context.
    pub materialization: Vec<(GroupId, Materialization)>,
}

/// One candidate's usage priced in a context: Stage 3's per-node pricing of
/// its private nodes ([`pricing::private_usage`]). The charges Stage 3 makes
/// only once the whole plan is known are kept apart and added by
/// [`pricing::plan_cost`]: memory held for an evaluation (which needs the
/// readers' whole latency) and the raw data query-time scans keep on disk
/// (each source's longest scan, once).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LocalUsage {
    /// Under cost model 1, its private nodes' price per second, without
    /// held memory and raw data: a lower bound of what it adds.
    pub cost_per_second: Option<f64>,
    pub cpu_ops_per_second: f64,
    pub scanned_bytes_per_second: f64,
    pub average_memory_bytes: f64,
    pub peak_memory_bytes: u64,
    pub disk_bytes: u64,
    /// Per reading query, the query-time work in ms its private nodes add
    /// to the query's latency per evaluation (a query's latency is the sum
    /// over its groups plus its longest compaction).
    pub latency_ms: Vec<(usize, f64)>,
    /// Per reading query, the longest compaction in ms of an
    /// ingestion-time state among its private nodes that the query reads.
    pub compaction_ms: Vec<(usize, f64)>,
    /// Memory its private nodes hold while an evaluation runs, readers by
    /// query index in the workload.
    pub held_memory: Vec<crate::HeldMemory>,
    /// Each query-time raw scan among its private nodes: source and bytes.
    pub raw_scans: Vec<(String, u64)>,
}
