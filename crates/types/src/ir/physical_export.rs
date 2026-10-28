//! Physical ASAP DAG (planner-layering stage 2 output).
//!
//! The flattened operators of [`super::flat`], plus the materialization and
//! execution timing (data state) of every node and edge. The input must
//! already be timed ([`crate::ir::properties::timing::apply_materialization_timings`]); export
//! reads each node's timing and materialization and does not re-run
//! data-state validation. The deployment executes the plan as given
//! (#509 Assumption 8): it does not change materialization or timing.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::flat::{flatten_before, NodeId};
use super::scalar::ScalarExpr;
use crate::ir::operator::asap::ASAPOp;
use crate::ir::operator::node::{Operator, OperatorNode};
use crate::ir::operator::non_asap::NonASAPOp;
use crate::ir::operator::operator_properties::Reduction;
use crate::ir::operator::query::QueryRoot;
use crate::ir::properties::execution_data_state::{
    ExecutionDataState, ExecutionDataStateError, ExecutionTiming,
};
use crate::ir::properties::guarantee::AccuracyGuarantee;
use crate::ir::properties::timing::{
    apply_materialization_timings_to_roots, data_state, is_materialization_point,
    MaterializationAssignment,
};
use crate::ir::schema::{FieldDataType, Schema};
use crate::physical::materialization::Materialization;

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
    /// Physical lowering must prove equal pane/query phase or install an
    /// exact boundary residual. The logical DAG alone cannot make that claim.
    #[serde(rename = "RequiresAlignedPanePhaseOrExactBoundaryResidual")]
    RequiresAlignedPanePhaseOrExactWindowEdgeResidual,
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
    /// Whether and where the deployment stores this node's output: the
    /// option the materialization assignment chose for the sub-DAG this node
    /// produces. Any node may be stored (#509 §Materialization); a node with
    /// no assignment is `NotMaterialized`.
    pub materialization: Materialization,
    pub output_schema: Schema,
    pub guarantee: Option<AccuracyGuarantee>,
    /// For a materialized view (a node whose materialization keeps its
    /// output across evaluations), how far back from the newest evaluation
    /// time its output must stay available once refreshed; for a query-time
    /// `Scan`, how far back its source's rows must stay available (`None`
    /// when it reads them unbounded). `None` on every other node, and on
    /// every node of a DAG exported before installation sets it
    /// ([`PhysicalASAPDAG::set_retention`]). A
    /// node field rather than part of [`Materialization`], because Stage 2
    /// enumerates materializations before Stage 3 knows retention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalASAPDAGEdge {
    pub producer: PhysicalASAPNodeId,
    pub consumer: PhysicalASAPNodeId,
    pub role: EdgeRole,
    pub intermediate_schema: Schema,
    pub data_state: ExecutionDataState,
    pub grouping: GroupingEdgeCompatibility,
    pub window: WindowEdgeCompatibility,
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
    #[error(
        "node {node:?} runs at {timing:?}, which its materialization {materialization:?} does not"
    )]
    MaterializationTimingMismatch {
        node: PhysicalASAPNodeId,
        timing: ExecutionTiming,
        materialization: Materialization,
    },
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
    pub fn union(self, other: Self) -> Self {
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

/// The window of the `TimeRange` `time_range`
/// ([`PhysicalASAPDAG::time_range_window`]), with `payload` giving each
/// node's operator.
fn time_range_window_of<'a>(
    time_range: PhysicalASAPNodeId,
    payload: impl Fn(PhysicalASAPNodeId) -> Option<&'a PhysicalASAPOperatorPayload>,
) -> ScanWindow {
    let Some(Operator::NonASAP(NonASAPOp::TimeRange { range, child, .. })) = payload(time_range)
    else {
        return ScanWindow::Unbounded;
    };
    let mut end_offset_ms = 0i64;
    let mut below = payload(*child);
    while let Some(Operator::NonASAP(NonASAPOp::TimeShift { shift, child })) = below {
        if shift.at.is_some() {
            return ScanWindow::Unbounded;
        }
        end_offset_ms = end_offset_ms.saturating_add(shift.offset_ms);
        below = payload(*child);
    }
    ScanWindow::Within {
        end_offset_ms,
        width_ms: i64::try_from(range.as_millis()).unwrap_or(i64::MAX),
    }
}

/// Why [`PhysicalASAPDAG::with_every_node_at_query_time`] failed.
#[derive(Debug, Error)]
pub enum ReferencePlanError {
    /// Bad input: the DAG is not a valid physical ASAP DAG.
    #[error("invalid physical ASAP DAG: {0}")]
    InvalidDag(#[from] PhysicalASAPDAGValidationError),
    /// Bad input: the plan cannot run at query time (a data-state rule of
    /// the timing pass fails).
    #[error("the plan does not run at query time: {0}")]
    Timing(#[from] ExecutionDataStateError),
}

impl PhysicalASAPDAG {
    /// This DAG's plan with nothing materialized: the same operators,
    /// summaries and summary parameters, with every sub-DAG
    /// `NotMaterialized` and so every node at query time (the default
    /// [`MaterializationAssignment`]). Node ids are renumbered, and `roots`
    /// and `scalar_roots` keep their order.
    ///
    /// The correctness reference of a selected plan: a deployment runtime
    /// compares the answers of the plan it installed with the executor
    /// running this one over raw data, which needs the raw data the
    /// installed plan may not keep.
    pub fn with_every_node_at_query_time(&self) -> Result<PhysicalASAPDAG, ReferencePlanError> {
        let roots = self.untimed_query_roots()?;
        let timed =
            apply_materialization_timings_to_roots(&roots, &MaterializationAssignment::default())?;
        Ok(compile_physical_asap_workload(&timed)?)
    }

    /// Every node of this DAG as IR, by node id, untimed and
    /// `NotMaterialized` with its exported guarantee: what a caller derives
    /// IR properties of a plan file's node from, such as its
    /// [`SummaryCoverage`](crate::ir::properties::summary_coverage::SummaryCoverage).
    /// Shared inputs stay shared. Validates the DAG first.
    pub fn operator_nodes(
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
    /// - a `PaneChain` of `n` panes of width `w`: its whole window `(T - s -
    ///   n·w, T - s]`, or only its newest pane `(T - s - w, T - s]` for a
    ///   chain in `kept_chains` (kept across evaluations, so each
    ///   evaluation builds one pane).
    ///
    /// `Unbounded` when a root, any other reader, or a shift with an `@`
    /// anchor reads the `Scan` or one of those shifts, or nothing reads it.
    pub fn scan_window(
        &self,
        scan: PhysicalASAPNodeId,
        kept_chains: &HashSet<PhysicalASAPNodeId>,
    ) -> ScanWindow {
        // Indexed once per call: O(nodes + edges), not a search per edge.
        let payloads: HashMap<PhysicalASAPNodeId, &PhysicalASAPOperatorPayload> = self
            .nodes
            .iter()
            .map(|node| (node.id, &node.payload))
            .collect();
        let mut consumers: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = HashMap::new();
        for edge in &self.edges {
            consumers
                .entry(edge.producer)
                .or_default()
                .push(edge.consumer);
        }
        let payload = |id: PhysicalASAPNodeId| payloads.get(&id).copied();
        let mut window: Option<ScanWindow> = None;
        let mut pending = vec![(scan, 0i64)];
        while let Some((id, end_offset_ms)) = pending.pop() {
            if self.roots.contains(&id) {
                return ScanWindow::Unbounded;
            }
            for &consumer in consumers.get(&id).into_iter().flatten() {
                let width_ms = match payload(consumer) {
                    Some(Operator::NonASAP(NonASAPOp::TimeShift { shift, .. }))
                        if shift.at.is_none() =>
                    {
                        pending.push((consumer, end_offset_ms.saturating_add(shift.offset_ms)));
                        continue;
                    }
                    Some(Operator::NonASAP(NonASAPOp::TimeRange { range, .. })) => {
                        range.as_millis()
                    }
                    Some(Operator::ASAP(ASAPOp::PaneChain { pane_width_ms, .. }))
                        if kept_chains.contains(&consumer) =>
                    {
                        u128::from(*pane_width_ms)
                    }
                    Some(Operator::ASAP(ASAPOp::PaneChain {
                        pane_width_ms,
                        pane_count,
                        ..
                    })) => u128::from(*pane_width_ms) * u128::from(*pane_count),
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
    }

    /// The rows the `TimeRange` `time_range` keeps, back from the
    /// evaluation time `T`: `(T - s - w, T - s]` for a `TimeRange(w)` over
    /// `TimeShift`s whose offsets sum to `s`, the window
    /// [`Self::scan_window`] counts for it. `Unbounded` when one of those
    /// shifts has an `@` anchor, or `time_range` is not a `TimeRange`.
    pub fn time_range_window(&self, time_range: PhysicalASAPNodeId) -> ScanWindow {
        time_range_window_of(time_range, |id| {
            self.nodes
                .iter()
                .find(|node| node.id == id)
                .map(|node| &node.payload)
        })
    }

    /// The interval of data the output of `node` summarizes, back from the
    /// evaluation time `T`: the union of the windows
    /// ([`Self::time_range_window`]) of every `TimeRange` below it. For an
    /// expanded pane, its pane; for a view built from panes, all of them.
    /// `Unbounded` when no `TimeRange` is below it or one of them is
    /// unbounded (a whole SQL table, an `@` anchor). O(nodes + edges).
    pub fn view_window(&self, node: PhysicalASAPNodeId) -> ScanWindow {
        let payloads: HashMap<PhysicalASAPNodeId, &PhysicalASAPOperatorPayload> = self
            .nodes
            .iter()
            .map(|node| (node.id, &node.payload))
            .collect();
        let mut inputs: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = HashMap::new();
        for edge in &self.edges {
            inputs.entry(edge.consumer).or_default().push(edge.producer);
        }
        let mut seen = HashSet::new();
        let mut pending = vec![node];
        let mut window: Option<ScanWindow> = None;
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            if let Some(Operator::NonASAP(NonASAPOp::TimeRange { .. })) = payloads.get(&id) {
                let read = time_range_window_of(id, |id| payloads.get(&id).copied());
                window = Some(window.map_or(read, |window| window.union(read)));
                continue;
            }
            pending.extend(inputs.get(&id).into_iter().flatten());
        }
        window.unwrap_or(ScanWindow::Unbounded)
    }

    /// The IR this DAG exports, one root per query in batch order, every
    /// node untimed and `NotMaterialized`: the inverse of
    /// [`compile_physical_asap_workload`] up to `Rc` identity and timing.
    fn untimed_query_roots(&self) -> Result<Vec<QueryRoot>, PhysicalASAPDAGValidationError> {
        let rebuilt = self.operator_nodes()?;
        let mut operator_roots = self.roots.iter();
        let mut scalar_roots = self.scalar_roots.iter().peekable();
        let mut roots = Vec::with_capacity(self.roots.len() + self.scalar_roots.len());
        for position in 0..self.roots.len() + self.scalar_roots.len() {
            match scalar_roots.peek() {
                Some((at, expr)) if *at == position => {
                    roots.push(QueryRoot::Scalar(
                        expr.map_operator_refs(&mut |id| Rc::clone(&rebuilt[id])),
                    ));
                    scalar_roots.next();
                }
                _ => {
                    let id = operator_roots.next().expect("validated root positions");
                    roots.push(QueryRoot::Operator(Rc::clone(&rebuilt[id])));
                }
            }
        }
        Ok(roots)
    }

    pub fn validate(&self) -> Result<(), PhysicalASAPDAGValidationError> {
        let mut nodes = HashMap::new();
        for node in &self.nodes {
            if nodes.insert(node.id, node).is_some() {
                return Err(PhysicalASAPDAGValidationError::DuplicateNodeId(node.id));
            }
            // A sub-DAG output runs when its materialization says; any other
            // node is not stored and runs when its consumer runs.
            if (is_materialization_point(&node.payload)
                || node.materialization != Materialization::NotMaterialized)
                && node.materialization.timing() != node.output_state.timing
            {
                return Err(
                    PhysicalASAPDAGValidationError::MaterializationTimingMismatch {
                        node: node.id,
                        timing: node.output_state.timing,
                        materialization: node.materialization,
                    },
                );
            }
            if node.retention_ms.is_some()
                && !node.materialization.is_materialized_view()
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
        // Pane merges can have tens of thousands of distinct inputs; linear
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

// ── Compilation from the IR ──────────────────────────────────────────────

/// Compiler-local identity assignment. It deliberately retains `Rc` handles
/// and is not serialized; deployed artifacts persist the physical ASAP node ID
/// together with their physical materialization/query IDs.
#[derive(Debug, Clone)]
pub struct PhysicalASAPNodeIdentityMap {
    nodes_by_id: Vec<Rc<OperatorNode>>,
}

impl PhysicalASAPNodeIdentityMap {
    pub fn node_id(&self, node: &Rc<OperatorNode>) -> Option<PhysicalASAPNodeId> {
        self.nodes_by_id
            .iter()
            .position(|candidate| Rc::ptr_eq(candidate, node))
    }

    pub fn operator_node(&self, id: PhysicalASAPNodeId) -> Option<&Rc<OperatorNode>> {
        self.nodes_by_id.get(id)
    }
}

#[derive(Debug, Clone)]
pub struct PhysicalASAPDAGCompilation {
    pub dag: PhysicalASAPDAG,
    pub node_ids: PhysicalASAPNodeIdentityMap,
}

pub fn compile_physical_asap_dag(
    root: &Rc<OperatorNode>,
) -> Result<PhysicalASAPDAG, ExecutionDataStateError> {
    Ok(compile_physical_asap_dag_with_node_ids(root)?.dag)
}

/// Export the timed DAG below `root`. Every reachable node must carry a
/// timing (see [`crate::ir::properties::timing::apply_materialization_timings`]); the data-state
/// rules were checked by that pass and are not re-run here.
pub fn compile_physical_asap_dag_with_node_ids(
    root: &Rc<OperatorNode>,
) -> Result<PhysicalASAPDAGCompilation, ExecutionDataStateError> {
    compile_physical_asap_workload_with_node_ids(&[QueryRoot::Operator(Rc::clone(root))])
}

/// Export a timed batch as one DAG: operator queries become `roots` and
/// standalone scalar queries `scalar_roots`, keyed by their position in the
/// batch.
pub fn compile_physical_asap_workload(
    roots: &[QueryRoot],
) -> Result<PhysicalASAPDAG, ExecutionDataStateError> {
    Ok(compile_physical_asap_workload_with_node_ids(roots)?.dag)
}

#[derive(Debug, Error)]
pub enum PhysicalASAPCompilationError {
    #[error(transparent)]
    Invalid(#[from] ExecutionDataStateError),
    #[error("planning deadline exceeded exporting the physical DAG")]
    DeadlineExceeded,
}

pub fn compile_physical_asap_workload_with_node_ids(
    roots: &[QueryRoot],
) -> Result<PhysicalASAPDAGCompilation, ExecutionDataStateError> {
    compile_physical_asap_workload_with_node_ids_before(roots, None).map_err(|error| match error {
        PhysicalASAPCompilationError::Invalid(error) => error,
        PhysicalASAPCompilationError::DeadlineExceeded => {
            unreachable!("no export deadline supplied")
        }
    })
}

pub fn compile_physical_asap_workload_with_node_ids_before(
    roots: &[QueryRoot],
    deadline: Option<std::time::Instant>,
) -> Result<PhysicalASAPDAGCompilation, PhysicalASAPCompilationError> {
    let check_deadline = || {
        if deadline.is_some_and(|limit| std::time::Instant::now() >= limit) {
            Err(PhysicalASAPCompilationError::DeadlineExceeded)
        } else {
            Ok(())
        }
    };
    let (flat, nodes_by_id) = flatten_before(roots, deadline)
        .map_err(|_| PhysicalASAPCompilationError::DeadlineExceeded)?;
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
        check_deadline()?;
        let node = &nodes_by_id[id];
        let output_state = data_state(node).ok_or(ExecutionDataStateError::UntimedNode {
            operator: node.operator.kind_name(),
        })?;
        // Children come before their parents, so every producer is already in `nodes`.
        for (producer, role) in edge_roles(&flat_node.operator) {
            check_deadline()?;
            let producer_state = nodes[producer].output_state;
            let ingestion_dependency = producer_state.timing == ExecutionTiming::IngestionTime
                && output_state.timing == ExecutionTiming::IngestionTime;
            edges.push(PhysicalASAPDAGEdge {
                producer,
                consumer: id,
                role,
                intermediate_schema: nodes[producer].output_schema.clone(),
                data_state: producer_state,
                grouping: grouping_compatibility(&nodes[producer].payload, &flat_node.operator),
                window: if ingestion_dependency {
                    WindowEdgeCompatibility::RequiresAlignedPanePhaseOrExactWindowEdgeResidual
                } else {
                    WindowEdgeCompatibility::NotApplicable
                },
            });
        }
        nodes.push(PhysicalASAPDAGNode {
            id,
            payload: flat_node.operator,
            output_state,
            materialization: flat_node.materialization,
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
    check_deadline()?;
    dag.validate().map_err(ExecutionDataStateError::from)?;
    check_deadline()?;
    Ok(PhysicalASAPDAGCompilation {
        dag,
        node_ids: PhysicalASAPNodeIdentityMap { nodes_by_id },
    })
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

fn grouping_compatibility<C>(
    producer: &Operator<C>,
    consumer: &Operator<C>,
) -> GroupingEdgeCompatibility {
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
