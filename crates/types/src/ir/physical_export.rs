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

use super::flat::{flatten, NodeId};
use super::scalar::ScalarExpr;
use crate::ir::operator::asap::ASAPOp;
use crate::ir::operator::node::{Operator, OperatorNode};
use crate::ir::operator::non_asap::NonASAPOp;
use crate::ir::operator::operator_properties::Reduction;
use crate::ir::operator::pane_chain::expand_pane_chains_in_workload;
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
use crate::ir::SchemaDerivationError;
use crate::physical::materialization::Materialization;

pub const PHYSICAL_ASAP_DAG_WIRE_VERSION: u32 = 10;

/// A node's operator, with each child replaced by its node id.
pub type PhysicalASAPOperatorPayload = Operator<NodeId>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Versioned transport envelope for a physical ASAP DAG.
///
/// Process boundaries exchange this envelope and call [`Self::validate`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalASAPDAGDocument {
    pub schema_version: u32,
    pub dag: PhysicalASAPDAG,
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
    #[error("unsupported physical ASAP DAG schema version {0}")]
    UnsupportedVersion(u32),
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

impl PhysicalASAPDAGDocument {
    pub fn new(dag: PhysicalASAPDAG) -> Self {
        Self {
            schema_version: PHYSICAL_ASAP_DAG_WIRE_VERSION,
            dag,
        }
    }

    pub fn validate(&self) -> Result<(), PhysicalASAPDAGValidationError> {
        if self.schema_version != PHYSICAL_ASAP_DAG_WIRE_VERSION {
            return Err(PhysicalASAPDAGValidationError::UnsupportedVersion(
                self.schema_version,
            ));
        }
        self.dag.validate()
    }
}

/// Whether [`PhysicalASAPDAG::query_roots`] keeps each node's timing and
/// materialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeepTiming {
    Yes,
    No,
}

/// Why [`PhysicalASAPDAG::with_every_node_at_query_time`] failed.
#[derive(Debug, Error)]
pub enum ReferencePlanError {
    /// Bad input: the document is not a valid physical ASAP DAG.
    #[error("invalid physical ASAP DAG: {0}")]
    InvalidDag(#[from] PhysicalASAPDAGValidationError),
    /// Bad input: the plan cannot run at query time (a data-state rule of
    /// the timing pass fails).
    #[error("the plan does not run at query time: {0}")]
    Timing(#[from] ExecutionDataStateError),
}

/// Why [`PhysicalASAPDAG::with_pane_chains_expanded`] failed.
#[derive(Debug, Error)]
pub enum PaneChainExpansionError {
    /// Bad input: the document is not a valid physical ASAP DAG.
    #[error("invalid physical ASAP DAG: {0}")]
    InvalidDag(#[from] PhysicalASAPDAGValidationError),
    /// Bad input: a chain is read by something other than a `SummaryMerge`,
    /// or its panes' offsets overflow.
    #[error("pane chain expansion: {0}")]
    Expansion(#[from] SchemaDerivationError),
    /// Internal bug: the expanded panes do not export (a pane left untimed).
    #[error("expanded DAG does not export: {0}")]
    Export(#[from] ExecutionDataStateError),
}

impl PhysicalASAPDAG {
    /// This DAG with every `PaneChain` expanded into the panes it stands for
    /// ([`expand_pane_chains`](crate::ir::operator::pane_chain::expand_pane_chains)):
    /// each `SummaryMerge` over a chain reads the
    /// chain's panes, newest first, as plain `SummaryAgg` nodes, each with
    /// the chain's timing and materialization. Every other node keeps its
    /// operator, timing and materialization; node ids are renumbered, and
    /// `roots` and `scalar_roots` keep their order. A DAG without a chain is
    /// returned unchanged up to ids.
    ///
    /// For a deployment runtime that holds only the exported document: the
    /// executor runs a chain as its panes, and the runtime binds each pane's
    /// raw input, so it expands before it compiles.
    pub fn with_pane_chains_expanded(&self) -> Result<PhysicalASAPDAG, PaneChainExpansionError> {
        let roots = self.query_roots(KeepTiming::Yes)?;
        let expanded = expand_pane_chains_in_workload(&roots)?;
        Ok(compile_physical_asap_workload(&expanded)?)
    }

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
        let roots = self.query_roots(KeepTiming::No)?;
        let timed =
            apply_materialization_timings_to_roots(&roots, &MaterializationAssignment::default())?;
        Ok(compile_physical_asap_workload(&timed)?)
    }

    /// The IR this DAG exports, one root per query in batch order: the
    /// inverse of [`compile_physical_asap_workload`], up to `Rc` identity,
    /// with each node's timing and materialization when `timing` keeps them
    /// and untimed and `NotMaterialized` otherwise.
    fn query_roots(
        &self,
        timing: KeepTiming,
    ) -> Result<Vec<QueryRoot>, PhysicalASAPDAGValidationError> {
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
                let restored = match timing {
                    KeepTiming::Yes => restored
                        .with_timing(Some(node.output_state.timing))
                        .with_materialization(node.materialization),
                    KeepTiming::No => restored,
                };
                rebuilt.insert(id, Rc::new(restored));
            }
        }
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
        let mut expected: HashMap<PhysicalASAPNodeId, Vec<(PhysicalASAPNodeId, EdgeRole)>> = self
            .nodes
            .iter()
            .map(|node| (node.id, edge_roles(&node.payload)))
            .collect();
        for edge in &self.edges {
            let inputs = expected.get_mut(&edge.consumer).ok_or(
                PhysicalASAPDAGValidationError::MissingEdgeEndpoint(edge.consumer),
            )?;
            let position = inputs
                .iter()
                .position(|input| *input == (edge.producer, edge.role))
                .ok_or(PhysicalASAPDAGValidationError::EdgeRoleMismatch {
                    consumer: edge.consumer,
                })?;
            inputs.swap_remove(position);
        }
        if let Some((consumer, _)) = expected.iter().find(|(_, inputs)| !inputs.is_empty()) {
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

pub fn compile_physical_asap_workload_with_node_ids(
    roots: &[QueryRoot],
) -> Result<PhysicalASAPDAGCompilation, ExecutionDataStateError> {
    let (flat, nodes_by_id) = flatten(roots);
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
        let output_state = data_state(node).ok_or(ExecutionDataStateError::UntimedNode {
            operator: node.operator.kind_name(),
        })?;
        // Children come before their parents, so every producer is already in `nodes`.
        for (producer, role) in edge_roles(&flat_node.operator) {
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
        });
    }
    let dag = PhysicalASAPDAG {
        nodes,
        edges,
        roots,
        scalar_roots,
    };
    dag.validate()?;
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
            ASAPOp::SummaryMerge { children } => vec![Input; children.len()],
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
