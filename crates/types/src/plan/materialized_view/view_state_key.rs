//! What a materialized view's stored state is, in any plan
//! ([`ViewStateKey`]): logical-layer information carried on the view's
//! physical node, like its coverage.

use serde::{Deserialize, Serialize};

use crate::stages::LogicalProperties;

/// What a view's stored state is, in any plan and process, from the view
/// node alone: what it computes (`definition_id`), the windows it is kept
/// in (`window`) and its candidate's logical properties (θ). Two views with
/// equal keys and one concrete selection hold interchangeable states, so a
/// store keys states by `(ViewStateKey, concrete selection)`. The nodes
/// below the view say how the state is computed, not what it is, and do not
/// enter: equivalent implementations in one memo group with equal θ share a
/// key. Physical properties (timing, lifetime, retention, storage medium)
/// do not enter either.
///
/// Example: a KLL chain view of 1-minute windows with `k = 200` is
/// `{ definition_id: "…", window: Some(Tumbling { width_ms: 60000 }),
/// logical_properties: { parameters: Kll { k: 200 } } }`; the same chain
/// with `k = 400` or 5-minute windows has another key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewStateKey {
    /// What is computed: the memo group's coverage definition
    /// ([`ViewCoverage::definition_id`](crate::plan::physical_asap_dag::ViewCoverage)).
    pub definition_id: String,
    /// The view node's own window structure, from its operator; `None` for
    /// a view that is one state, not a chain of windows.
    pub window: Option<ViewWindow>,
    /// The view node's candidate's logical properties (θ).
    pub logical_properties: LogicalProperties,
}

/// The windows a view keeps its states in, from its operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ViewWindow {
    /// Tumbling windows `width_ms` wide, on a grid anchored at the epoch: a
    /// `SummaryTumblingWindowChain`'s or a `SummaryMergeWindows`'.
    Tumbling { width_ms: u64 },
}
