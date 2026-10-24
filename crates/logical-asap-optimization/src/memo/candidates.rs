//! Logical implementation candidates attached to stable memo groups.
//!
//! Pass 1 population owns no pricing or solver state. Existing Pass 2 rule
//! metadata remains here during migration; state groups will replace the
//! implicit shared-state decisions in a subsequent change.

use std::rc::Rc;

use asap_types::ir::OperatorNode;
use asap_types::physical::Materialization;

pub use super::groups::{try_populate_pass1, NodeKeys};
use super::{MemoGroup, MemoGroupId};

/// The hash-consed workload: its groups, children before parents.
#[derive(Debug, Clone, Default)]
pub struct CandidateMemo {
    pub groups: Vec<MemoGroup>,
    /// Each group's implementation candidates, by group index.
    pub candidates: Vec<Vec<ImplementationCandidate>>,
}

/// One way to implement a group: its node as Stage 2 implements it, over
/// child groups.
#[derive(Debug, Clone)]
pub struct ImplementationCandidate {
    /// The groups it implements: one, or several for a fused computation
    /// (the topmost first).
    pub implements: Vec<MemoGroupId>,
    /// The implementation's own node(s): its root node, whose descendants
    /// down to `children` are its private nodes.
    pub node: Rc<OperatorNode>,
    /// The groups it reads, whose implementations are chosen separately.
    pub children: Vec<MemoGroupId>,
    /// How it was made: by Pass 1, or by which Pass 2 transformation.
    pub origin: CandidateOrigin,
    /// The coupling sites its cost depends on.
    pub coupling_sites: Vec<CouplingSite>,
    /// The number of equal shares of its measures' accuracy budget its
    /// summaries are sized for: 1 for the whole budget, `n` when it is one
    /// of `n` approximate inputs of a query (a ratio's operand). This positive
    /// generation parameter is already encoded in concrete sketch parameters;
    /// it is not an equality constraint on the selected number of readers or
    /// approximate inputs. Recompose and validate accuracy at query roots.
    /// Larger allocations may be valid but costlier than necessary.
    pub budget_share: usize,
    /// Groups whose chosen implementation must be exact for this one to
    /// be valid (a rollup's finer group, a top-k reuse's input group); a
    /// child or a descendant.
    pub requires_exact_children: Vec<MemoGroupId>,
    /// For a rollup, what it rolls up and from where; `None` otherwise.
    pub rollup_of: Option<RollupRead>,
    /// The materializations Stage 2 may give it; empty for every one
    /// Stage 2 allows. A scan group whose readers all read one window is
    /// only built once (`MatAtQuery`); one read through different windows
    /// may also be read per reader (`NotMaterialized`)
    /// This is an eligibility restriction, not a selected assignment.
    pub materializations: Vec<Materialization>,
    /// Sites other groups' chosen implementations must carry for this one
    /// to be valid: a sketch rollup's finer group exposing its state, a
    /// coarser grouping's host group building the pane chain it reads.
    pub requires_sites: Vec<(MemoGroupId, CouplingSite)>,
}

/// What a [`CandidateOrigin::Rollup`] candidate reads, and what it
/// rewrites: the search checks that the finer group's chosen
/// implementation is the one the joint rule would roll up from.
#[derive(Debug, Clone)]
pub struct RollupRead {
    /// The finer group it reads.
    pub finer_group: MemoGroupId,
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
pub enum CandidateOrigin {
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
pub struct CouplingSite {
    pub key: u64,
    pub kind: CouplingKind,
}

/// What a coupling site's threshold is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CouplingKind {
    /// A shared summary build's size, set by its strictest reader.
    SharedBuildSize,
    /// A shared pane chain: grid width, longest window, size.
    PaneChain,
    /// A metric's scan label set.
    ScanLabels,
    /// A source's raw-data horizon, the longest raw scan.
    RawHorizon,
}

/// A Pass 1 inventory does not belong to the initial logical memo.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CandidatePopulationError {
    #[error("Pass 1 root dependencies differ from the initial logical memo")]
    RootDependenciesChanged,
    #[error("Pass 1 target {target} is not a valid measure of an initial aggregate group")]
    InvalidTarget { target: usize },
    #[error("Pass 1 target {target} references missing absorbed target {absorbed}")]
    InvalidAbsorption { target: usize, absorbed: usize },
}
