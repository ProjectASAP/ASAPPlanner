//! What a materialized view's stored state is, in any plan
//! ([`ViewStateKey`]): logical-layer information carried on the view's
//! physical node, like its coverage.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::ir::operator::asap::ASAPOp;
use crate::ir::operator::node::Operator;
use crate::ir::operator::non_asap::NonASAPOp;
use crate::plan::physical_asap_dag::{PhysicalASAPDAGNode, PhysicalASAPNodeId, ViewCoverage};
use crate::stages::LogicalProperties;

/// What a view's stored state is, in any plan and process, from the view
/// node alone: what it computes (`definition_id`), the windows its rows
/// are (`window`) and its candidate's logical properties (θ). Two views with
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
    /// The windows the view's rows are, from its own operator
    /// ([`ViewWindow`]); `None` for one state per group, whose window is in
    /// its selection.
    pub window: Option<ViewWindow>,
    /// The view node's candidate's logical properties (θ).
    pub logical_properties: LogicalProperties,
}

impl ViewStateKey {
    /// The key of the view at `node` with `coverage`, from the node alone:
    /// its definition, the windows its rows are ([`ViewWindow::of_node`]
    /// over `nodes`, the DAG's nodes by id) and its θ.
    pub(super) fn of_view(
        node: &PhysicalASAPDAGNode,
        coverage: &ViewCoverage,
        nodes: &HashMap<PhysicalASAPNodeId, &PhysicalASAPDAGNode>,
    ) -> Self {
        ViewStateKey {
            definition_id: coverage.definition_id().to_owned(),
            window: ViewWindow::of_node(node, nodes),
            logical_properties: LogicalProperties::of_parameters(node.parameters.clone()),
        }
    }
}

/// The windows a view's rows are, from its operator: `None` in a key for a
/// view that is one state per group, whose window is in its selection.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ViewWindow {
    /// Tumbling windows `width_ms` wide, on a grid anchored at the epoch: a
    /// `SummaryTumblingWindowChain`'s or a `SummaryMergeWindows`' rows, or
    /// those a `TimeRange` or `TimeShift` over one selects.
    Tumbling { width_ms: u64 },
    /// The windows a `TimeRangeCover` selects from its levels, one width
    /// per level in its level order (widest first).
    TumblingLevels { widths_ms: Vec<u64> },
}

impl ViewWindow {
    /// The windows `node`'s rows are: a chain's or `SummaryMergeWindows`'
    /// width; through a `TimeRange` or `TimeShift`, those of the node it
    /// selects from; a `TimeRangeCover`'s levels' widths; `None` for any
    /// other node (one state per group).
    ///
    /// Example: `TimeRange(5m, Chain[1m])` is `Tumbling { width_ms: 60000 }`;
    /// `SummaryMerge(TimeRange(5m, Chain[1m]))` is `None`.
    fn of_node(
        node: &PhysicalASAPDAGNode,
        nodes: &HashMap<PhysicalASAPNodeId, &PhysicalASAPDAGNode>,
    ) -> Option<Self> {
        match &node.payload {
            Operator::ASAP(
                ASAPOp::SummaryTumblingWindowChain {
                    tumbling_window_width_ms,
                    ..
                }
                | ASAPOp::SummaryMergeWindows {
                    tumbling_window_width_ms,
                    ..
                },
            ) => Some(ViewWindow::Tumbling {
                width_ms: *tumbling_window_width_ms,
            }),
            Operator::NonASAP(
                NonASAPOp::TimeRange { child, .. } | NonASAPOp::TimeShift { child, .. },
            ) => Self::of_node(nodes[child], nodes),
            Operator::NonASAP(NonASAPOp::TimeRangeCover { levels, .. }) => {
                let widths_ms = levels
                    .iter()
                    .map(|level| match Self::of_node(nodes[level], nodes) {
                        Some(ViewWindow::Tumbling { width_ms }) => Some(width_ms),
                        _ => None,
                    })
                    .collect::<Option<Vec<u64>>>();
                // Every level of a cover is a chain of one build: a planner bug
                // otherwise, which a key without the window would hide.
                debug_assert!(widths_ms.is_some(), "a TimeRangeCover level is no chain");
                Some(ViewWindow::TumblingLevels {
                    widths_ms: widths_ms?,
                })
            }
            _ => None,
        }
    }
}
