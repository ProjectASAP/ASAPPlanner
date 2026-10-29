//! Logical implementation candidates attached to stable memo groups.
//!
//! Rule expansion owns no pricing or solver state. Shared computations are
//! explicit state groups connected through candidate child dependencies.

use std::rc::Rc;

use asap_types::ir::{OperatorNode, QueryRoot};

pub use super::groups::try_populate_pass1;
use super::{MemoGroup, MemoGroupId};

/// The hash-consed workload: its groups, children before parents.
#[derive(Debug, Clone, Default)]
pub struct CandidateMemo {
    pub groups: Vec<MemoGroup>,
    /// Each query's root over groups, by index in the workload
    /// ([`super::DagOfLogicalMemoGroups::query_roots`]); groups added later
    /// are never a query's root.
    pub query_roots: Vec<QueryRoot<MemoGroupId>>,
    /// Each group's implementation candidates, by group index.
    pub candidates: Vec<Vec<CandidateForMemoGroup>>,
}

/// One way to implement a group: its node as Stage 2 implements it, over
/// child groups.
#[derive(Debug, Clone)]
pub struct CandidateForMemoGroup {
    /// The groups it implements: one, or several for a fused computation
    /// (the topmost first).
    pub implements: Vec<MemoGroupId>,
    /// The implementation's own node(s): its root node, whose descendants
    /// down to [`Self::input_groups`] are its private nodes.
    pub root_node: Rc<OperatorNode>,
    /// The groups it reads, whose implementations are chosen separately.
    pub input_groups: Vec<MemoGroupId>,
    /// How it was made: by Pass 1, or by which Pass 2 transformation.
    pub origin: OriginOfCandidateForMemoGroup,
    /// The number of equal shares of its measures' accuracy budget its
    /// summaries are sized for: 1 for the whole budget, `n` when it is one
    /// of `n` approximate inputs of a query (a ratio's operand). This positive
    /// generation parameter is already encoded in concrete sketch parameters;
    /// it is not an equality constraint on the selected number of readers or
    /// approximate inputs. Recompose and validate accuracy at query roots.
    /// Larger allocations may be valid but costlier than necessary.
    pub accuracy_budget_parts: usize,
}

/// Where an implementation candidate came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginOfCandidateForMemoGroup {
    /// Pass 1: exact, exact accumulator, or a summary family and size.
    Pass1,
    /// One build serving several reader groups (summary capability).
    SharedBuild,
    /// A summary readout factored from a composite implementation.
    SummaryResult,
    /// Canonical value preprocessing feeding a summary build.
    SummaryInput,
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
