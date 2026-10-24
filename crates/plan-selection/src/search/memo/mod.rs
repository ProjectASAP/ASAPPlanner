//! Memo over logical sub-expressions (planner-layering design, "The
//! stages over a memo of sub-expressions"): the stages run on the
//! workload's distinct sub-expressions instead of whole-workload
//! candidates.
//!
//! The stage driver establishes logical identity before Pass 1, using
//! [`asap_logical_asap_optimization::memo::LogicalMemo`].
//! [`brute_force::memo_from_initial`] then builds the [`Memo`] in this order:
//!
//! 1. [`groups::populate_pass1`]: fill the initial groups with Pass 1
//!    candidates without changing group IDs. Callers that already have a
//!    Pass 1 inventory can use [`groups::memo_of`] to build identity first.
//! 2. [`rules::capability::add_shared_builds`]: summary capability. A reader of a
//!    build shared with other groups (one family over equal inputs), with a
//!    [`CouplingKind::SharedBuildSize`] site; identical expression is group
//!    identity and adds nothing.
//! 3. [`rules::windows::add_pass2_candidates`]: window composition (panes, and
//!    newest-pane reads of a shared chain with a [`CouplingKind::PaneChain`]
//!    site), rollup and exact top-k reuse (candidates reading another
//!    group).
//! 4. [`rules::scans::add_scan_widening`]: readers of a metric's scans widened to
//!    a shared label set, with a [`CouplingKind::ScanLabels`] site.
//! 5. [`rules::scans::add_raw_horizons`]: a [`CouplingKind::RawHorizon`] site on
//!    every candidate that reads a raw window of a source read by two or
//!    more groups.
//! 6. [`rules::scans::scan_materializations`]: a scan read through one window is
//!    only built once; through different windows, also per reader.
//!
//! Each site kind has its thresholds and reader check, and a candidate's
//! node at a threshold: [`rules::capability::size_thresholds`] and
//! [`rules::capability::resized_at_threshold`]; [`rules::windows::chain_thresholds`] and
//! [`rules::windows::read_at_threshold`]; [`rules::scans::label_thresholds`] and
//! [`rules::scans::widened_at_threshold`]; [`rules::scans::horizon_thresholds`] (no
//! rewrite: Stage 3 prices the horizon).
//!
//! [`brute_force::brute_force`] is the reference search, run for
//! [`crate::SearchAlgorithm::MemoBruteForce`]: every
//! consistent choice of one candidate per needed group and one threshold
//! per site its candidates read, composed into one plan (identical nodes
//! shared, equal resized builds made one), implemented by Stage 2 in every
//! materialization and priced by Stage 3. [`coefficients`](crate::search::milp::coefficients) turns Stage 3's per-node
//! prices into the coefficients of [`milp`](crate::search::milp::model), the memo solved as a
//! mixed-integer linear program ([`crate::SearchAlgorithm::MemoMilp`]).
//! [`clusters`](crate::search::milp::clusters) splits the memo into independent clusters (queries no
//! plan's cost links), each solved with its own model and lazy loop, their
//! plans combined into the workload's.

#![allow(dead_code)]

pub(crate) mod brute_force;
pub(crate) mod groups;
pub(crate) mod incumbent;
pub(crate) mod rules;

use asap_types::physical::Materialization;

pub(crate) use asap_logical_asap_optimization::memo::{MemoGroup, MemoGroupId as GroupId};

pub(crate) use asap_logical_asap_optimization::memo::candidates::{
    CandidateMemo as Memo, CandidateOrigin, CouplingKind, CouplingSite, ImplementationCandidate,
    RollupRead,
};

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
/// its private nodes ([`crate::cost::per_node::private_usage`]). The charges Stage 3 makes
/// only once the whole plan is known are kept apart and added by
/// [`crate::cost::per_node::plan_cost`]: memory held for an evaluation (which needs the
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
