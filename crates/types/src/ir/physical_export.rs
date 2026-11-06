//! Physical ASAP DAG (planner-layering stage 2 output).
//!
//! The flattened operators of [`super::flat`], plus the lifetime and
//! execution timing (data state) of every node and edge. The realizer
//! (`AssignmentRealizer::realize`) builds it from a `DagOfSelectedCandidates`,
//! each node's timing and lifetime from the `PhysicalCandidate` it realizes.
//! The deployment executes the plan as given
//! (#509 Assumption 8): it does not change lifetime or timing.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::flat::NodeId;
use super::scalar::ScalarExpr;
use crate::ir::operator::asap::ASAPOp;
use crate::ir::operator::node::{Operator, OperatorNode};
use crate::ir::operator::non_asap::NonASAPOp;

use crate::ir::properties::execution_data_state::{ExecutionDataState, ExecutionTiming};
use crate::ir::properties::guarantee::AccuracyGuarantee;
use crate::ir::schema::{FieldDataType, Schema, SketchParams};
use crate::physical::{InvalidPhysicalProperties, Lifetime};

/// A node's operator, with each child replaced by its node id.
pub type PhysicalASAPOperatorPayload = Operator<NodeId>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EdgeRole {
    Input,
    Left,
    Right,
    /// The consumer reads the producer from inside one of its scalar
    /// expressions (`scalar(v)`, a scalar subquery, `EXISTS`, `IN`).
    ScalarRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupingEdgeCompatibility {
    Identical,
    ConsumerCoarsensProducer,
    Incompatible,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowEdgeCompatibility {
    /// Physical lowering must prove equal tumbling-window and query phases or install an
    /// exact boundary residual. The logical DAG alone cannot make that claim.
    #[serde(rename = "RequiresAlignedPanePhaseOrExactBoundaryResidual")]
    RequiresAlignedTumblingWindowPhaseOrExactWindowEdgeResidual,
    NotApplicable,
}

pub type PhysicalASAPNodeId = NodeId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalASAPDAGNode {
    pub id: PhysicalASAPNodeId,
    pub payload: PhysicalASAPOperatorPayload,
    /// Phase is a placement choice for every operator, independent of payload kind.
    pub output_state: ExecutionDataState,
    /// How long and where the deployment materializes this node's output: the
    /// lifetime the physical properties assignment chose for the sub-DAG this
    /// node produces. Any node may be materialized (#509 §Materialization); a node
    /// with no assignment has lifetime `None`. With `output_state.timing` it
    /// is the node's [`PhysicalProperties`].
    pub lifetime: Lifetime,
    /// θ of a summary build (`SummaryAgg`, `SummaryTumblingWindowChain`): its
    /// candidate's `LogicalProperties::parameters`, which the executor builds
    /// the sketch from. `None` on every other node and for an exact build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<SketchParams>,
    pub output_schema: Schema,
    pub guarantee: Option<AccuracyGuarantee>,
    /// For a materialized view (a node whose lifetime materializes its output
    /// across evaluations), how far back from the newest evaluation
    /// time its output must stay available once refreshed; for a query-time
    /// `Scan`, how far back its source's rows must stay available (`None`
    /// when it reads them unbounded). `None` on every other node, and on
    /// every node of a DAG exported before installation sets it
    /// ([`PhysicalASAPDAG::set_retention`]). A
    /// node field rather than part of [`Lifetime`], because Stage 2
    /// enumerates lifetimes before Stage 3 knows retention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalASAPDAGEdge {
    pub producer: PhysicalASAPNodeId,
    pub consumer: PhysicalASAPNodeId,
    pub role: EdgeRole,
    pub(crate) intermediate_schema: Schema,
    pub(crate) data_state: ExecutionDataState,
    pub(crate) grouping: GroupingEdgeCompatibility,
    pub(crate) window: WindowEdgeCompatibility,
}

#[cfg(any(test, feature = "test-support"))]
impl PhysicalASAPDAGEdge {
    pub fn new(
        producer: PhysicalASAPNodeId,
        consumer: PhysicalASAPNodeId,
        role: EdgeRole,
        intermediate_schema: Schema,
        data_state: ExecutionDataState,
        grouping: GroupingEdgeCompatibility,
        window: WindowEdgeCompatibility,
    ) -> Self {
        Self {
            producer,
            consumer,
            role,
            intermediate_schema,
            data_state,
            grouping,
            window,
        }
    }

    pub fn intermediate_schema(&self) -> &Schema {
        &self.intermediate_schema
    }

    /// For tests that change a compiled DAG's edge schema.
    pub fn intermediate_schema_mut(&mut self) -> &mut Schema {
        &mut self.intermediate_schema
    }

    pub fn data_state(&self) -> ExecutionDataState {
        self.data_state
    }

    /// For tests that retime a compiled DAG's edges.
    pub fn data_state_mut(&mut self) -> &mut ExecutionDataState {
        &mut self.data_state
    }

    pub fn grouping(&self) -> GroupingEdgeCompatibility {
        self.grouping
    }

    pub fn window(&self) -> WindowEdgeCompatibility {
        self.window
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalASAPDAG {
    pub nodes: Vec<PhysicalASAPDAGNode>,
    pub edges: Vec<PhysicalASAPDAGEdge>,
    /// Semantic workload roots. Physical query/precompute sinks are selected
    /// downstream by the control plane.
    /// One root per operator query of the batch, in workload order.
    pub roots: Vec<PhysicalASAPNodeId>,
    /// One expression per standalone scalar query, keyed by the query's
    /// position in the batch, in increasing order. Its operator references
    /// name nodes of this DAG. A standalone scalar query is not a node (#511
    /// §2.3), so query `i` of the batch is the scalar root keyed `i` when
    /// there is one, and otherwise the next entry of `roots`.
    pub scalar_roots: Vec<(usize, ScalarExpr<PhysicalASAPNodeId>)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PhysicalASAPDAGValidationError {
    /// A node whose timing and lifetime cannot go together: a planner bug.
    #[error("node {node:?}: {reason}")]
    InvalidPhysicalProperties {
        node: PhysicalASAPNodeId,
        #[source]
        reason: InvalidPhysicalProperties,
    },
    /// A summary build (or maintained population) that is not materialized but does
    /// not run at query time: a planner bug.
    #[error(
        "node {node:?} is a summary build that is not materialized but does not run at query time"
    )]
    UnmaterializedSummaryBuildNotAtQueryTime { node: PhysicalASAPNodeId },
    #[error("ingestion node {consumer:?} depends on query node {producer:?}")]
    QueryDependencyInIngestion {
        producer: PhysicalASAPNodeId,
        consumer: PhysicalASAPNodeId,
    },
    #[error(
        "node {0:?} has a retention_ms but is neither a materialized view nor a query-time scan"
    )]
    UnexpectedRetention(PhysicalASAPNodeId),
    #[error("duplicate physical ASAP node id {0:?}")]
    DuplicateNodeId(PhysicalASAPNodeId),
    #[error("physical ASAP DAG root {0:?} does not name a node")]
    MissingRoot(PhysicalASAPNodeId),
    #[error("edge endpoint {0:?} does not name a node")]
    MissingEdgeEndpoint(PhysicalASAPNodeId),
    #[error("edge {producer:?}->{consumer:?} schema differs from producer output")]
    EdgeSchemaMismatch {
        producer: PhysicalASAPNodeId,
        consumer: PhysicalASAPNodeId,
    },
    #[error("edge {producer:?}->{consumer:?} data state differs from producer output")]
    EdgeDataStateMismatch {
        producer: PhysicalASAPNodeId,
        consumer: PhysicalASAPNodeId,
    },
    #[error("incoming edges of node {consumer:?} do not match its operator's inputs")]
    EdgeRoleMismatch { consumer: PhysicalASAPNodeId },
    #[error("physical ASAP DAG has no query roots")]
    NoRoots,
    #[error("scalar root keyed {0} is out of order or outside the batch")]
    ScalarRootPosition(usize),
    #[error("physical ASAP DAG contains a cycle")]
    Cycle,
    #[error("physical ASAP node {0:?} is not reachable from the root")]
    UnreachableNode(PhysicalASAPNodeId),
    #[error("summary aggregate node {node:?} output schema does not contain its declared family")]
    SummaryFamilySchemaMismatch { node: PhysicalASAPNodeId },
    #[error(
        "summary aggregate node {node:?} declares grouping inconsistent with its sketch state"
    )]
    SummaryGroupingMismatch { node: PhysicalASAPNodeId },
}

/// Which rows of a `Scan` its readers read, back from an evaluation time `T`
/// ([`PhysicalASAPDAG::scan_window`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScanWindow {
    /// The rows with time in `(T - end_offset_ms - width_ms, T -
    /// end_offset_ms]`.
    Within { end_offset_ms: i64, width_ms: i64 },
    /// Rows at any time.
    Unbounded,
}

impl ScanWindow {
    /// The smallest window holding both.
    pub(crate) fn union(self, other: Self) -> Self {
        let (
            Self::Within {
                end_offset_ms: a_end,
                width_ms: a_width,
            },
            Self::Within {
                end_offset_ms: b_end,
                width_ms: b_width,
            },
        ) = (self, other)
        else {
            return Self::Unbounded;
        };
        let end_offset_ms = a_end.min(b_end);
        let start_back_ms = a_end
            .saturating_add(a_width)
            .max(b_end.saturating_add(b_width));
        Self::Within {
            end_offset_ms,
            width_ms: start_back_ms.saturating_sub(end_offset_ms),
        }
    }

    /// How far back from `T` the oldest row read lies, `end_offset_ms +
    /// width_ms` (0 for a window ending after `T` that reaches no further
    /// back); `None` when unbounded.
    pub fn lookback_ms(self) -> Option<u64> {
        match self {
            Self::Within {
                end_offset_ms,
                width_ms,
            } => Some(u64::try_from(end_offset_ms.saturating_add(width_ms)).unwrap_or(0)),
            Self::Unbounded => None,
        }
    }

    /// `(start, end]` at evaluation time `evaluation_time_ms`; `None` when
    /// unbounded.
    pub fn bounds_ms(self, evaluation_time_ms: i64) -> Option<(i64, i64)> {
        match self {
            Self::Within {
                end_offset_ms,
                width_ms,
            } => {
                let end = evaluation_time_ms.saturating_sub(end_offset_ms);
                Some((end.saturating_sub(width_ms), end))
            }
            Self::Unbounded => None,
        }
    }
}

/// The rows the `TimeRange` `time_range` keeps, back from the evaluation
/// time `T`: `(T - s - w, T - s]` for a `TimeRange(w)` over `TimeShift`s
/// whose offsets sum to `s`, the window [`PhysicalASAPDAG::scan_window`]
/// counts for it. `Unbounded` when one of those shifts has an `@` anchor,
/// or `time_range` is not a `TimeRange`. `payload_of` gives each node's
/// operator by id; an indexed lookup keeps this O(shifts), so a caller
/// asking for every `TimeRange` of a tumbling-window-expanded DAG stays linear.
pub fn time_range_window<'a>(
    time_range: PhysicalASAPNodeId,
    payload_of: impl Fn(PhysicalASAPNodeId) -> Option<&'a PhysicalASAPOperatorPayload>,
) -> ScanWindow {
    let Some(Operator::NonASAP(NonASAPOp::TimeRange { range, child, .. })) = payload_of(time_range)
    else {
        return ScanWindow::Unbounded;
    };
    let mut end_offset_ms = 0i64;
    let mut below = payload_of(*child);
    while let Some(Operator::NonASAP(NonASAPOp::TimeShift { shift, child })) = below {
        if shift.at.is_some() {
            return ScanWindow::Unbounded;
        }
        end_offset_ms = end_offset_ms.saturating_add(shift.offset_ms);
        below = payload_of(*child);
    }
    ScanWindow::Within {
        end_offset_ms,
        width_ms: i64::try_from(range.as_millis()).unwrap_or(i64::MAX),
    }
}

/// How long before the evaluation time `node` is evaluated: the sum of the
/// offsets of the `TimeShift`s from `node` down; `None` when one of them has
/// an `@` anchor or the sum overflows.
pub(crate) fn offset_below<'a>(
    node: PhysicalASAPNodeId,
    payload_of: impl Fn(PhysicalASAPNodeId) -> Option<&'a PhysicalASAPOperatorPayload>,
) -> Option<i64> {
    let mut end_offset_ms = 0i64;
    let mut below = payload_of(node);
    while let Some(Operator::NonASAP(NonASAPOp::TimeShift { shift, child })) = below {
        if shift.at.is_some() {
            return None;
        }
        end_offset_ms = end_offset_ms.checked_add(shift.offset_ms)?;
        below = payload_of(*child);
    }
    Some(end_offset_ms)
}

impl PhysicalASAPDAG {
    /// Every node of this DAG as IR, by node id, with its exported
    /// guarantee (an operator node has no physical properties): what a caller derives
    /// IR properties of a plan file's node from, such as its
    /// [`SummaryCoverage`](crate::ir::properties::summary_coverage::SummaryCoverage).
    /// Shared inputs stay shared. Validates the DAG first.
    pub(crate) fn operator_nodes(
        &self,
    ) -> Result<HashMap<PhysicalASAPNodeId, Rc<OperatorNode>>, PhysicalASAPDAGValidationError> {
        self.validate()?;
        let by_id: HashMap<PhysicalASAPNodeId, &PhysicalASAPDAGNode> =
            self.nodes.iter().map(|node| (node.id, node)).collect();
        let mut rebuilt: HashMap<PhysicalASAPNodeId, Rc<OperatorNode>> = HashMap::new();
        // Children first, without recursion: `validate` rejected cycles.
        for node in &self.nodes {
            let mut pending = vec![(node.id, false)];
            while let Some((id, children_done)) = pending.pop() {
                if rebuilt.contains_key(&id) {
                    continue;
                }
                let node = by_id[&id];
                if !children_done {
                    pending.push((id, true));
                    pending.extend(node.payload.children().into_iter().map(|c| (*c, false)));
                    continue;
                }
                let operator = node
                    .payload
                    .map_children(|child| Rc::clone(&rebuilt[child]));
                let restored = OperatorNode::with_schema(operator, node.output_schema.clone())
                    .with_guarantee(node.guarantee.clone());
                rebuilt.insert(id, Rc::new(restored));
            }
        }
        Ok(rebuilt)
    }

    /// The rows of the `Scan` `scan` its readers read, back from the
    /// evaluation time `T`: the union of each reader's window. A reader
    /// reaches the `Scan` through `TimeShift`s without an `@` anchor
    /// (offsets summing to `s`) and is
    /// - a `TimeRange(w)`, range or instant: `(T - s - w, T - s]`;
    /// - a `SummaryTumblingWindowChain` of `n` tumbling windows of width `w`: its whole window `(T - s -
    ///   n·w, T - s]`, or only its newest tumbling window `(T - s - w, T - s]` for a
    ///   chain in `materialized_chains` (materialized across evaluations, so each
    ///   evaluation builds one tumbling window).
    ///
    /// `Unbounded` when a root, any other reader, or a shift with an `@`
    /// anchor reads the `Scan` or one of those shifts, or nothing reads it.
    pub fn scan_window(
        &self,
        scan: PhysicalASAPNodeId,
        materialized_chains: &HashSet<PhysicalASAPNodeId>,
    ) -> ScanWindow {
        self.scan_windows([scan], materialized_chains)[&scan]
    }

    /// [`Self::scan_window`] of each of `scans`, by id, with the DAG
    /// indexed once for all of them: O(nodes + edges + the readers walked).
    pub fn scan_windows(
        &self,
        scans: impl IntoIterator<Item = PhysicalASAPNodeId>,
        materialized_chains: &HashSet<PhysicalASAPNodeId>,
    ) -> HashMap<PhysicalASAPNodeId, ScanWindow> {
        let payloads = self.payloads_by_id();
        let mut consumers: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = HashMap::new();
        for edge in &self.edges {
            consumers
                .entry(edge.producer)
                .or_default()
                .push(edge.consumer);
        }
        let roots: HashSet<PhysicalASAPNodeId> = self.roots.iter().copied().collect();
        let window_of = |scan: PhysicalASAPNodeId| {
            let mut window: Option<ScanWindow> = None;
            let mut pending = vec![(scan, 0i64)];
            while let Some((id, end_offset_ms)) = pending.pop() {
                if roots.contains(&id) {
                    return ScanWindow::Unbounded;
                }
                for &consumer in consumers.get(&id).into_iter().flatten() {
                    let width_ms = match payloads.get(&consumer) {
                        Some(Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }))
                            if shift.at.is_none() =>
                        {
                            pending.push((consumer, end_offset_ms.saturating_add(shift.offset_ms)));
                            continue;
                        }
                        Some(Operator::NonASAP(NonASAPOp::TimeRange { range, .. })) => {
                            range.as_millis()
                        }
                        Some(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                            tumbling_window_width_ms,
                            ..
                        })) if materialized_chains.contains(&consumer) => {
                            u128::from(*tumbling_window_width_ms)
                        }
                        Some(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                            tumbling_window_width_ms,
                            tumbling_window_count,
                            ..
                        })) => {
                            u128::from(*tumbling_window_width_ms)
                                * u128::from(*tumbling_window_count)
                        }
                        _ => return ScanWindow::Unbounded,
                    };
                    let read = ScanWindow::Within {
                        end_offset_ms,
                        width_ms: i64::try_from(width_ms).unwrap_or(i64::MAX),
                    };
                    window = Some(window.map_or(read, |window| window.union(read)));
                }
            }
            window.unwrap_or(ScanWindow::Unbounded)
        };
        scans
            .into_iter()
            .map(|scan| (scan, window_of(scan)))
            .collect()
    }

    /// The interval of data the output of `node` summarizes, back from the
    /// evaluation time `T`: the union of the windows
    /// ([`time_range_window`]) of every `TimeRange` below it, and of every
    /// unexpanded `SummaryTumblingWindowChain` (all its tumbling windows). For an expanded tumbling window, its
    /// tumbling window; for a view built from tumbling windows, all of them. `Unbounded` when no
    /// `TimeRange` or chain is below it or one of them is unbounded (a whole
    /// SQL table, an `@` anchor). O(nodes + edges).
    #[cfg(any(test, feature = "test-support"))]
    pub fn view_window(&self, node: PhysicalASAPNodeId) -> ScanWindow {
        self.view_windows([node])[&node]
    }

    /// `Self::view_window` of each of `nodes`, by id, with the DAG indexed
    /// once for all of them: O(nodes + edges + the nodes walked below each).
    /// It walks each operator's inputs (a valid DAG's edges), so it also
    /// answers for a DAG of nodes alone.
    pub fn view_windows(
        &self,
        nodes: impl IntoIterator<Item = PhysicalASAPNodeId>,
    ) -> HashMap<PhysicalASAPNodeId, ScanWindow> {
        let payloads = self.payloads_by_id();
        let window_of = |node: PhysicalASAPNodeId| {
            let mut seen = HashSet::new();
            let mut pending = vec![node];
            let mut window: Option<ScanWindow> = None;
            while let Some(id) = pending.pop() {
                if !seen.insert(id) {
                    continue;
                }
                let read = match payloads.get(&id) {
                    Some(Operator::NonASAP(NonASAPOp::TimeRange { .. })) => {
                        time_range_window(id, |id| payloads.get(&id).copied())
                    }
                    Some(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                        child,
                        tumbling_window_width_ms,
                        tumbling_window_count,
                        ..
                    })) => {
                        let width_ms =
                            tumbling_window_width_ms.checked_mul(u64::from(*tumbling_window_count));
                        match (
                            offset_below(*child, |id| payloads.get(&id).copied()),
                            width_ms,
                        ) {
                            (Some(end_offset_ms), Some(width_ms)) => ScanWindow::Within {
                                end_offset_ms,
                                width_ms: i64::try_from(width_ms).unwrap_or(i64::MAX),
                            },
                            _ => ScanWindow::Unbounded,
                        }
                    }
                    Some(payload) => {
                        pending.extend(payload.children().into_iter().copied());
                        continue;
                    }
                    None => continue,
                };
                window = Some(window.map_or(read, |window| window.union(read)));
            }
            window.unwrap_or(ScanWindow::Unbounded)
        };
        nodes
            .into_iter()
            .map(|node| (node, window_of(node)))
            .collect()
    }

    /// Each node's operator, by id.
    pub(crate) fn payloads_by_id(
        &self,
    ) -> HashMap<PhysicalASAPNodeId, &PhysicalASAPOperatorPayload> {
        self.nodes
            .iter()
            .map(|node| (node.id, &node.payload))
            .collect()
    }

    pub fn validate(&self) -> Result<(), PhysicalASAPDAGValidationError> {
        let mut nodes = HashMap::new();
        for node in &self.nodes {
            if nodes.insert(node.id, node).is_some() {
                return Err(PhysicalASAPDAGValidationError::DuplicateNodeId(node.id));
            }
            // A node's timing and lifetime must go together; a summary
            // build that is not materialized runs at query time. Any other node that
            // is not materialized runs when its consumer runs.
            if node.output_state.timing == ExecutionTiming::IngestionTime
                && matches!(node.lifetime, Lifetime::OneEvaluation(_))
            {
                return Err(PhysicalASAPDAGValidationError::InvalidPhysicalProperties {
                    node: node.id,
                    reason: InvalidPhysicalProperties::OneEvaluationAtIngestion,
                });
            }
            if is_materializable_state_output(&node.payload)
                && node.output_state.timing == ExecutionTiming::IngestionTime
                && node.lifetime == Lifetime::None
            {
                return Err(
                    PhysicalASAPDAGValidationError::UnmaterializedSummaryBuildNotAtQueryTime {
                        node: node.id,
                    },
                );
            }
            if node.retention_ms.is_some()
                && !matches!(node.lifetime, Lifetime::AcrossEvaluations(_))
                && !super::materialized_view::is_query_time_scan(node)
            {
                return Err(PhysicalASAPDAGValidationError::UnexpectedRetention(node.id));
            }
            if let Operator::ASAP(ASAPOp::SummaryAgg {
                family, grouping, ..
            }) = &node.payload
            {
                let mut found_family = false;
                for field in &node.output_schema.fields {
                    if &field.dtype == family {
                        found_family = true;
                    }
                    if let FieldDataType::Sketch(_, schema_grouping) = &field.dtype {
                        if schema_grouping != grouping {
                            return Err(PhysicalASAPDAGValidationError::SummaryGroupingMismatch {
                                node: node.id,
                            });
                        }
                    }
                }
                if !found_family {
                    return Err(
                        PhysicalASAPDAGValidationError::SummaryFamilySchemaMismatch {
                            node: node.id,
                        },
                    );
                }
            }
        }
        if self.roots.is_empty() && self.scalar_roots.is_empty() {
            return Err(PhysicalASAPDAGValidationError::NoRoots);
        }
        let queries = self.roots.len() + self.scalar_roots.len();
        let mut next = 0;
        for (position, _) in &self.scalar_roots {
            if *position < next || *position >= queries {
                return Err(PhysicalASAPDAGValidationError::ScalarRootPosition(
                    *position,
                ));
            }
            next = position + 1;
        }
        // Operator roots and the nodes scalar roots read.
        let root_ids: Vec<PhysicalASAPNodeId> = self
            .roots
            .iter()
            .copied()
            .chain(
                self.scalar_roots
                    .iter()
                    .flat_map(|(_, expr)| expr.operator_refs().into_iter().copied()),
            )
            .collect();
        for root in &root_ids {
            if !nodes.contains_key(root) {
                return Err(PhysicalASAPDAGValidationError::MissingRoot(*root));
            }
        }
        // Each node's incoming edges are exactly its operator's inputs, with
        // the role of each input's position, so a swapped `Left`/`Right`
        // cannot reorder the inputs of a binary operator.
        // Count input occurrences instead of scanning a vector for each edge.
        // Tumbling-window merges can have tens of thousands of distinct inputs; linear
        // searches made validation quadratic in that fan-in. Multiplicities
        // still matter when the same producer occupies several input slots.
        let mut expected: HashMap<_, HashMap<_, usize>> = self
            .nodes
            .iter()
            .map(|node| {
                let mut counts = HashMap::new();
                for input in edge_roles(&node.payload) {
                    *counts.entry(input).or_insert(0) += 1;
                }
                (node.id, counts)
            })
            .collect();
        for edge in &self.edges {
            let inputs = expected.get_mut(&edge.consumer).ok_or(
                PhysicalASAPDAGValidationError::MissingEdgeEndpoint(edge.consumer),
            )?;
            let count = inputs
                .get_mut(&(edge.producer, edge.role))
                .filter(|count| **count > 0)
                .ok_or(PhysicalASAPDAGValidationError::EdgeRoleMismatch {
                    consumer: edge.consumer,
                })?;
            *count -= 1;
        }
        if let Some((consumer, _)) = expected
            .iter()
            .find(|(_, inputs)| inputs.values().any(|count| *count != 0))
        {
            return Err(PhysicalASAPDAGValidationError::EdgeRoleMismatch {
                consumer: *consumer,
            });
        }
        let mut children: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = HashMap::new();
        for edge in &self.edges {
            let producer = nodes.get(&edge.producer).ok_or(
                PhysicalASAPDAGValidationError::MissingEdgeEndpoint(edge.producer),
            )?;
            if !nodes.contains_key(&edge.consumer) {
                return Err(PhysicalASAPDAGValidationError::MissingEdgeEndpoint(
                    edge.consumer,
                ));
            }
            if producer.output_state.timing == ExecutionTiming::QueryTime
                && nodes[&edge.consumer].output_state.timing == ExecutionTiming::IngestionTime
            {
                return Err(PhysicalASAPDAGValidationError::QueryDependencyInIngestion {
                    producer: edge.producer,
                    consumer: edge.consumer,
                });
            }
            if edge.intermediate_schema != producer.output_schema {
                return Err(PhysicalASAPDAGValidationError::EdgeSchemaMismatch {
                    producer: edge.producer,
                    consumer: edge.consumer,
                });
            }
            if edge.data_state != producer.output_state {
                return Err(PhysicalASAPDAGValidationError::EdgeDataStateMismatch {
                    producer: edge.producer,
                    consumer: edge.consumer,
                });
            }
            children
                .entry(edge.consumer)
                .or_default()
                .push(edge.producer);
        }
        fn visit(
            id: PhysicalASAPNodeId,
            children: &HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
            visiting: &mut HashSet<PhysicalASAPNodeId>,
            visited: &mut HashSet<PhysicalASAPNodeId>,
        ) -> bool {
            if visited.contains(&id) {
                return true;
            }
            if !visiting.insert(id) {
                return false;
            }
            if children
                .get(&id)
                .into_iter()
                .flatten()
                .any(|child| !visit(*child, children, visiting, visited))
            {
                return false;
            }
            visiting.remove(&id);
            visited.insert(id);
            true
        }
        let mut visited = HashSet::new();
        for root in &root_ids {
            if !visit(*root, &children, &mut HashSet::new(), &mut visited) {
                return Err(PhysicalASAPDAGValidationError::Cycle);
            }
        }
        fn mark(
            id: PhysicalASAPNodeId,
            children: &HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
            reachable: &mut HashSet<PhysicalASAPNodeId>,
        ) {
            if !reachable.insert(id) {
                return;
            }
            for child in children.get(&id).into_iter().flatten() {
                mark(*child, children, reachable);
            }
        }
        let mut reachable = HashSet::new();
        for root in &root_ids {
            mark(*root, &children, &mut reachable);
        }
        if let Some(id) = nodes.keys().find(|id| !reachable.contains(id)) {
            return Err(PhysicalASAPDAGValidationError::UnreachableNode(*id));
        }
        Ok(())
    }
}

/// Each input of `operator` with its edge role: the operator inputs, then the
/// nodes read by its scalar expressions (the order of [`Operator::children`]).
/// A node referenced several times from the scalar expressions is one
/// `ScalarRef` input, as cost lowering reads it once; ids are per node
/// pointer, so this dedupes by pointer.
fn edge_roles<C: Copy + PartialEq>(operator: &Operator<C>) -> Vec<(C, EdgeRole)> {
    use EdgeRole::*;
    let inputs: Vec<EdgeRole> = match operator {
        Operator::NonASAP(op) => match op {
            NonASAPOp::Join { .. } | NonASAPOp::SetOp { .. } | NonASAPOp::BinaryOp { .. } => {
                vec![Left, Right]
            }
            NonASAPOp::Concat { children, .. } => vec![Input; children.len()],
            NonASAPOp::Scan { .. }
            | NonASAPOp::Values { .. }
            | NonASAPOp::PromqlVectorFromScalar(_) => vec![],
            _ => vec![Input],
        },
        Operator::ASAP(op) => match op {
            ASAPOp::SummarySubtract { .. } | ASAPOp::SummaryJoin { .. } => vec![Left, Right],
            ASAPOp::SummaryMerge { children, .. } => vec![Input; children.len()],
            _ => vec![Input],
        },
    };
    let children = operator.children();
    let (direct, scalar_refs) = children.split_at(inputs.len());
    let mut edges: Vec<(C, EdgeRole)> = direct.iter().map(|c| **c).zip(inputs).collect();
    for producer in scalar_refs {
        if !edges.contains(&(**producer, ScalarRef)) {
            edges.push((**producer, ScalarRef));
        }
    }
    edges
}

/// Whether `operator`'s output is a sub-DAG output that materialization may
/// store: a `SummaryAgg`, a `SummaryTumblingWindowChain` or a `MaintainPopulation`.
pub(crate) fn is_materializable_state_output<C>(operator: &Operator<C>) -> bool {
    matches!(
        operator,
        Operator::ASAP(
            ASAPOp::SummaryAgg { .. }
                | ASAPOp::SummaryTumblingWindowChain { .. }
                | ASAPOp::MaintainPopulation { .. }
        )
    )
}

/// Test-only: `roots` exported as one DAG with every node at query time and
/// not materialized, and no θ (every `parameters` is `None`), with each
/// exported node's operator node by node id. Executor tests hand-build DAGs
/// this way; planning exports only through the realizer
/// (`AssignmentRealizer::realize`).
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn physical_asap_dag_at_query_time_with_node_ids(
    roots: &[crate::ir::QueryRoot],
) -> Result<(PhysicalASAPDAG, Vec<Rc<OperatorNode>>), PhysicalASAPDAGValidationError> {
    use crate::ir::properties::execution_data_state::DataPrimitive;
    use crate::ir::QueryRoot;
    let (flat, nodes_by_id) = super::flat::flatten(roots);
    let mut scalar_roots = Vec::new();
    let roots = flat
        .roots
        .into_iter()
        .enumerate()
        .filter_map(|(position, root)| match root {
            QueryRoot::Operator(id) => Some(id),
            QueryRoot::Scalar(expr) => {
                scalar_roots.push((position, expr));
                None
            }
        })
        .collect();
    let mut nodes: Vec<PhysicalASAPDAGNode> = Vec::with_capacity(flat.nodes.len());
    let mut edges = Vec::new();
    for (id, flat_node) in flat.nodes.into_iter().enumerate() {
        let node = &nodes_by_id[id];
        let primitive = match &node.operator {
            Operator::ASAP(op) if op.produced_state().is_some() => DataPrimitive::SummaryState,
            Operator::ASAP(ASAPOp::MaintainPopulation { .. }) => DataPrimitive::SummaryState,
            _ if node.result_kind == crate::ir::OperatorResultKind::State => {
                DataPrimitive::SummaryState
            }
            _ => DataPrimitive::Raw,
        };
        let output_state = ExecutionDataState {
            timing: ExecutionTiming::QueryTime,
            primitive,
        };
        // Children come before their parents, so every producer is already in `nodes`.
        for (producer, role) in edge_roles(&flat_node.operator) {
            edges.push(PhysicalASAPDAGEdge {
                producer,
                consumer: id,
                role,
                intermediate_schema: nodes[producer].output_schema.clone(),
                data_state: nodes[producer].output_state,
                grouping: grouping_compatibility(&nodes[producer].payload, &flat_node.operator),
                window: WindowEdgeCompatibility::NotApplicable,
            });
        }
        nodes.push(PhysicalASAPDAGNode {
            id,
            payload: flat_node.operator,
            output_state,
            lifetime: Lifetime::None,
            parameters: None,
            output_schema: flat_node.schema,
            guarantee: flat_node.guarantee,
            retention_ms: None,
        });
    }
    let dag = PhysicalASAPDAG {
        nodes,
        edges,
        roots,
        scalar_roots,
    };
    dag.validate()?;
    Ok((dag, nodes_by_id))
}

#[cfg(any(test, feature = "test-support"))]
fn grouping_compatibility<C>(
    producer: &Operator<C>,
    consumer: &Operator<C>,
) -> GroupingEdgeCompatibility {
    use crate::ir::operator::operator_properties::Reduction;
    let (
        Operator::ASAP(ASAPOp::SummaryAgg {
            reduction: producer,
            ..
        }),
        Operator::ASAP(ASAPOp::SummaryAgg {
            reduction: consumer,
            ..
        }),
    ) = (producer, consumer)
    else {
        return GroupingEdgeCompatibility::NotApplicable;
    };
    match (producer, consumer) {
        (p, c) if p == c => GroupingEdgeCompatibility::Identical,
        (Reduction::PerEntity, Reduction::Reduce(_)) => {
            GroupingEdgeCompatibility::ConsumerCoarsensProducer
        }
        (Reduction::Reduce(p), Reduction::Reduce(c))
            if !p.is_without() && !c.is_without() && c.iter().all(|key| p.contains(key)) =>
        {
            GroupingEdgeCompatibility::ConsumerCoarsensProducer
        }
        _ => GroupingEdgeCompatibility::Incompatible,
    }
}
