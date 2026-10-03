//! Physical ASAP DAG (planner-layering stage 2 output).
//!
//! The flattened operators of [`super::flat`], plus the execution timing
//! (data state) of every node and edge. The input must already be timed
//! ([`super::timing::apply_materialization_timings`]); export reads each node's
//! timing and does not re-run data-state validation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::asap::ASAPOp;
use super::flat::{flatten, NodeId};
use super::node::{Operator, OperatorNode};
use super::non_asap::NonASAPOp;
use super::operator_properties::Reduction;
use super::query::QueryRoot;
use super::timing::data_state;
use crate::post_asap::execution_data_state::{
    ExecutionDataState, ExecutionDataStateError, ExecutionTiming,
};
use crate::post_asap::guarantee::ResultGuarantee;
use crate::pre_asap::schema::{FieldDataType, Schema};

pub const PHYSICAL_ASAP_DAG_WIRE_VERSION: u32 = 8;

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
    pub output_schema: Schema,
    pub guarantee: Option<ResultGuarantee>,
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
    /// Semantic workload root. Physical query/precompute sinks are selected
    /// downstream by the control plane.
    /// One root per query of the batch, in workload order. Scalar query roots
    /// are not physical nodes yet.
    pub roots: Vec<PhysicalASAPNodeId>,
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
    #[error("phase assignment must name every DAG node exactly once")]
    IncompletePhaseAssignment,
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
    #[error("physical ASAP DAG has no query roots")]
    NoRoots,
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

impl PhysicalASAPDAG {
    /// Assign execution phases without changing operator semantics. Phase choices
    /// do not prove deployment support: callers must bind concrete implementations
    /// and storage boundaries before installing this plan.
    pub fn with_execution_phases(
        &self,
        phases: &BTreeMap<PhysicalASAPNodeId, ExecutionTiming>,
    ) -> Result<Self, PhysicalASAPDAGValidationError> {
        self.validate()?;
        if phases.len() != self.nodes.len()
            || self.nodes.iter().any(|node| !phases.contains_key(&node.id))
        {
            return Err(PhysicalASAPDAGValidationError::IncompletePhaseAssignment);
        }
        let mut dag = self.clone();
        for node in &mut dag.nodes {
            node.output_state.timing = phases[&node.id];
        }
        let states: HashMap<_, _> = dag.nodes.iter().map(|n| (n.id, n.output_state)).collect();
        for edge in &mut dag.edges {
            edge.data_state = states[&edge.producer];
        }
        dag.validate()?;
        Ok(dag)
    }

    pub fn validate(&self) -> Result<(), PhysicalASAPDAGValidationError> {
        let mut nodes = HashMap::new();
        for node in &self.nodes {
            if nodes.insert(node.id, node).is_some() {
                return Err(PhysicalASAPDAGValidationError::DuplicateNodeId(node.id));
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
        if self.roots.is_empty() {
            return Err(PhysicalASAPDAGValidationError::NoRoots);
        }
        for root in &self.roots {
            if !nodes.contains_key(root) {
                return Err(PhysicalASAPDAGValidationError::MissingRoot(*root));
            }
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
        for root in &self.roots {
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
        for root in &self.roots {
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
/// timing (see [`super::timing::apply_materialization_timings`]); the data-state
/// rules were checked by that pass and are not re-run here.
pub fn compile_physical_asap_dag_with_node_ids(
    root: &Rc<OperatorNode>,
) -> Result<PhysicalASAPDAGCompilation, ExecutionDataStateError> {
    compile_physical_asap_workload_with_node_ids(std::slice::from_ref(root))
}

/// Export a timed batch as one DAG with one root per query.
pub fn compile_physical_asap_workload(
    roots: &[Rc<OperatorNode>],
) -> Result<PhysicalASAPDAG, ExecutionDataStateError> {
    Ok(compile_physical_asap_workload_with_node_ids(roots)?.dag)
}

pub fn compile_physical_asap_workload_with_node_ids(
    roots: &[Rc<OperatorNode>],
) -> Result<PhysicalASAPDAGCompilation, ExecutionDataStateError> {
    let roots: Vec<QueryRoot> = roots.iter().cloned().map(QueryRoot::Operator).collect();
    let (flat, nodes_by_id) = flatten(&roots);
    let roots = flat
        .roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(id) => *id,
            QueryRoot::Scalar(_) => unreachable!("only operator roots are flattened"),
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
            let maintenance_dependency = producer_state.timing == ExecutionTiming::IngestionTime
                && output_state.timing == ExecutionTiming::IngestionTime;
            edges.push(PhysicalASAPDAGEdge {
                producer,
                consumer: id,
                role,
                intermediate_schema: nodes[producer].output_schema.clone(),
                data_state: producer_state,
                grouping: grouping_compatibility(&nodes[producer].payload, &flat_node.operator),
                window: if maintenance_dependency {
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
            output_schema: flat_node.schema,
            guarantee: flat_node.guarantee,
        });
    }
    let dag = PhysicalASAPDAG {
        nodes,
        edges,
        roots,
    };
    dag.validate()
        .expect("compiler emits a valid physical ASAP DAG");
    Ok(PhysicalASAPDAGCompilation {
        dag,
        node_ids: PhysicalASAPNodeIdentityMap { nodes_by_id },
    })
}

/// Each child of `operator` with its edge role: the operator inputs, then the
/// nodes read by its scalar expressions (the order of [`Operator::children`]).
fn edge_roles<C: Copy>(operator: &Operator<C>) -> Vec<(C, EdgeRole)> {
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
    let scalar_refs = children.len() - inputs.len();
    children
        .into_iter()
        .copied()
        .zip(
            inputs
                .into_iter()
                .chain(std::iter::repeat_n(ScalarRef, scalar_refs)),
        )
        .collect()
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
