//! Physical ASAP DAG transport (planner-layering stage 2 output).
//!
//! Same operator payloads as the logical export, plus the execution timing
//! (data state) of every node and edge. The input must already be timed
//! ([`crate::ir::properties::timing::apply_materialization_timings`]); export reads each node's
//! timing and does not re-run data-state validation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ir::operator::asap::ASAPOp;
use crate::ir::operator::node::{Operator, OperatorNode};
use crate::ir::properties::execution::{
    ExecutionDataState, ExecutionDataStateError, ExecutionTiming,
};
use crate::ir::properties::guarantee::ResultGuarantee;
use crate::ir::properties::timing::data_state;
use crate::ir::schema::{FieldDataType, Schema};
use crate::ir::wire::{grouping_compatibility, input_edges, payload_of};
use crate::ir::wire::{EdgeRole, GroupingEdgeCompatibility, LogicalASAPOperatorPayload};

pub const PHYSICAL_ASAP_DAG_WIRE_VERSION: u32 = 8;

/// Operator payloads are shared with the logical export.
pub type PhysicalASAPOperatorPayload = LogicalASAPOperatorPayload;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowEdgeCompatibility {
    /// Physical lowering must prove equal pane/query phase or install an
    /// exact boundary residual. The logical DAG alone cannot make that claim.
    #[serde(rename = "RequiresAlignedPanePhaseOrExactBoundaryResidual")]
    RequiresAlignedPanePhaseOrExactWindowEdgeResidual,
    NotApplicable,
}

/// Node ids are shared with the logical export, since payloads embed them.
pub type PhysicalASAPNodeId = crate::ir::wire::LogicalASAPNodeId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalASAPDAGNode {
    pub id: PhysicalASAPNodeId,
    /// The payload variant is the sole operator identity (`payload.kind` in JSON).
    pub payload: PhysicalASAPOperatorPayload,
    /// Phase is a placement choice for every operator, independent of payload kind.
    pub output_state: ExecutionDataState,
    pub output_schema: Schema,
    pub guarantee: Option<ResultGuarantee>,
    #[serde(default)]
    pub coverage: Option<crate::ir::properties::summary_coverage::SummaryCoverage>,
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
            if let PhysicalASAPOperatorPayload::SummaryAgg {
                family, grouping, ..
            } = &node.payload
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
            .map(|id| crate::ir::wire::LogicalASAPNodeId(id as u32))
    }

    pub fn operator_node(&self, id: PhysicalASAPNodeId) -> Option<&Rc<OperatorNode>> {
        self.nodes_by_id.get(id.0 as usize)
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
    let mut exporter = Exporter::default();
    let roots = roots
        .iter()
        .map(|root| exporter.visit(root))
        .collect::<Result<Vec<_>, _>>()?;
    let dag = PhysicalASAPDAG {
        nodes: exporter.nodes,
        edges: exporter.edges,
        roots,
    };
    dag.validate()
        .expect("compiler emits a valid physical ASAP DAG");
    Ok(PhysicalASAPDAGCompilation {
        dag,
        node_ids: PhysicalASAPNodeIdentityMap {
            nodes_by_id: exporter.nodes_by_id,
        },
    })
}

#[derive(Default)]
struct Exporter {
    ids: HashMap<*const OperatorNode, PhysicalASAPNodeId>,
    nodes: Vec<PhysicalASAPDAGNode>,
    edges: Vec<PhysicalASAPDAGEdge>,
    nodes_by_id: Vec<Rc<OperatorNode>>,
}

impl Exporter {
    fn visit(
        &mut self,
        node: &Rc<OperatorNode>,
    ) -> Result<PhysicalASAPNodeId, ExecutionDataStateError> {
        if let Some(id) = self.ids.get(&Rc::as_ptr(node)) {
            return Ok(*id);
        }
        let output_state = data_state(node).ok_or(ExecutionDataStateError::UntimedNode {
            operator: node.operator.kind_name(),
        })?;
        // Operator inputs first, then the nodes read from scalar expressions.
        let mut producers = Vec::new();
        for (child, role) in input_edges(&node.operator) {
            producers.push((self.visit(child)?, child, role));
        }
        {
            let scalars = match &node.operator {
                Operator::NonASAP(op) => op.scalar_exprs(),
                Operator::ASAP(ASAPOp::SummaryAgg {
                    filter: Some(filter),
                    ..
                }) => vec![&filter.0],
                _ => vec![],
            };
            for expr in scalars {
                for referenced in expr.operator_refs() {
                    producers.push((self.visit(referenced)?, referenced, EdgeRole::ScalarRef));
                }
            }
        }
        let id = crate::ir::wire::LogicalASAPNodeId(self.nodes.len() as u32);
        let payload = {
            let ids = &self.ids;
            let mut id_of = |n: &Rc<OperatorNode>| ids[&Rc::as_ptr(n)];
            payload_of(&node.operator, &mut id_of)
        };
        self.nodes.push(PhysicalASAPDAGNode {
            id,
            payload,
            output_state,
            output_schema: node.schema.clone(),
            guarantee: node.guarantee.clone(),
            coverage: node.coverage.clone(),
        });
        self.nodes_by_id.push(Rc::clone(node));
        self.ids.insert(Rc::as_ptr(node), id);
        for (producer, child, role) in producers {
            let producer_state = self.nodes[producer.0 as usize].output_state;
            let maintenance_dependency = producer_state.timing == ExecutionTiming::IngestionTime
                && output_state.timing == ExecutionTiming::IngestionTime;
            self.edges.push(PhysicalASAPDAGEdge {
                producer,
                consumer: id,
                role,
                intermediate_schema: child.schema.clone(),
                data_state: producer_state,
                grouping: grouping_compatibility(&child.operator, &node.operator),
                window: if maintenance_dependency {
                    WindowEdgeCompatibility::RequiresAlignedPanePhaseOrExactWindowEdgeResidual
                } else {
                    WindowEdgeCompatibility::NotApplicable
                },
            });
        }
        Ok(id)
    }
}
