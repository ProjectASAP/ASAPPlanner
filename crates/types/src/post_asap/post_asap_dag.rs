//! Runtime-neutral post-ASAP DAG contract shared by precompute and query engines.

use std::collections::HashMap;
use std::rc::Rc;

use super::{
    validate_execution_data_states, ExecutionDataState, ExecutionDataStateError, PostASAPNode,
    ResultGuarantee, SummaryExpr, SummarySchema,
};
use super::{
    BinaryOperator, CandidateCompleteness, ExecutionTiming, GroupingStrategy, SketchQuery,
    SummaryFamilyType, SummaryUpdate, ValueOperation,
};
use crate::pre_asap::{ColumnRef, JoinKind, PreASAPNode, Predicate, Reduction};
use thiserror::Error;

pub const POST_ASAP_DAG_WIRE_VERSION: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EdgeRole {
    Input,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GroupingEdgeCompatibility {
    Identical,
    ConsumerCoarsensProducer,
    Incompatible,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WindowEdgeCompatibility {
    /// Physical lowering must prove equal pane/query phase or install an
    /// exact boundary residual. The logical DAG alone cannot make that claim.
    #[serde(rename = "RequiresAlignedPanePhaseOrExactBoundaryResidual")]
    RequiresAlignedPanePhaseOrExactWindowEdgeResidual,
    NotApplicable,
}

/// Stable identity of a node within one exported post-ASAP semantic DAG.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct PostASAPNodeId(pub u32);

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PostASAPOperatorPayload {
    Fallback {
        expression: PreASAPNode,
    },
    Binary {
        operator: BinaryOperator,
    },
    Value {
        operation: ValueOperation,
    },
    RelationalJoin {
        join_kind: JoinKind,
        pred: Predicate,
        pruning: Option<CandidateCompleteness>,
    },
    SummaryAgg {
        family: SummaryFamilyType,
        input: SummaryUpdate,
        reduction: Reduction,
        grouping: GroupingStrategy,
    },
    SummaryJoin {
        key: ColumnRef,
        family: SummaryFamilyType,
    },
    SummarySubtract,
    SummaryDelete {
        key: ColumnRef,
    },
    SummaryEstimate {
        query: SketchQuery,
    },
    SummaryMerge,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostASAPDAGNode {
    pub id: PostASAPNodeId,
    /// The payload variant is the sole operator identity (`payload.kind` in JSON).
    pub payload: PostASAPOperatorPayload,
    /// Phase is a placement choice for every operator, independent of payload kind.
    pub output_state: ExecutionDataState,
    pub output_schema: SummarySchema,
    pub guarantee: Option<ResultGuarantee>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostASAPDAGEdge {
    pub producer: PostASAPNodeId,
    pub consumer: PostASAPNodeId,
    pub role: EdgeRole,
    pub intermediate_schema: SummarySchema,
    pub data_state: ExecutionDataState,
    pub grouping: GroupingEdgeCompatibility,
    pub window: WindowEdgeCompatibility,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostASAPDAGTransport {
    pub nodes: Vec<PostASAPDAGNode>,
    pub edges: Vec<PostASAPDAGEdge>,
    /// Semantic workload root. Physical query/precompute sinks are selected
    /// downstream by the control plane.
    pub root: PostASAPNodeId,
}

/// Versioned transport envelope for a post-ASAP semantic DAG.
///
/// Process boundaries exchange this envelope and call [`Self::validate`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostASAPDAGDocument {
    pub schema_version: u32,
    pub dag: PostASAPDAGTransport,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PostASAPDAGValidationError {
    #[error("phase assignment must name every DAG node exactly once")]
    IncompletePhaseAssignment,
    #[error("ingestion node {consumer:?} depends on query node {producer:?}")]
    QueryDependencyInIngestion {
        producer: PostASAPNodeId,
        consumer: PostASAPNodeId,
    },
    #[error("unsupported post-ASAP DAG schema version {0}")]
    UnsupportedVersion(u32),
    #[error("duplicate post-ASAP node id {0:?}")]
    DuplicateNodeId(PostASAPNodeId),
    #[error("post-ASAP DAG root {0:?} does not name a node")]
    MissingRoot(PostASAPNodeId),
    #[error("edge endpoint {0:?} does not name a node")]
    MissingEdgeEndpoint(PostASAPNodeId),
    #[error("edge {producer:?}->{consumer:?} schema differs from producer output")]
    EdgeSchemaMismatch {
        producer: PostASAPNodeId,
        consumer: PostASAPNodeId,
    },
    #[error("edge {producer:?}->{consumer:?} data state differs from producer output")]
    EdgeDataStateMismatch {
        producer: PostASAPNodeId,
        consumer: PostASAPNodeId,
    },
    #[error("post-ASAP DAG contains a cycle")]
    Cycle,
    #[error("post-ASAP node {0:?} is not reachable from the root")]
    UnreachableNode(PostASAPNodeId),
    #[error("summary aggregate node {node:?} output schema does not contain its declared family")]
    SummaryFamilySchemaMismatch { node: PostASAPNodeId },
    #[error(
        "summary aggregate node {node:?} declares grouping inconsistent with its sketch state"
    )]
    SummaryGroupingMismatch { node: PostASAPNodeId },
}

impl PostASAPDAGDocument {
    pub fn new(dag: PostASAPDAGTransport) -> Self {
        Self {
            schema_version: POST_ASAP_DAG_WIRE_VERSION,
            dag,
        }
    }

    pub fn validate(&self) -> Result<(), PostASAPDAGValidationError> {
        if self.schema_version != POST_ASAP_DAG_WIRE_VERSION {
            return Err(PostASAPDAGValidationError::UnsupportedVersion(
                self.schema_version,
            ));
        }
        self.dag.validate()
    }
}

impl PostASAPDAGTransport {
    /// Assign execution phases without changing operator semantics. Phase choices
    /// do not prove deployment support: callers must bind concrete implementations
    /// and storage boundaries before installing this plan.
    pub fn with_execution_phases(
        &self,
        phases: &std::collections::BTreeMap<PostASAPNodeId, ExecutionTiming>,
    ) -> Result<Self, PostASAPDAGValidationError> {
        self.validate()?;
        let mut dag = self.clone();
        assign_phases(&mut dag.nodes, &mut dag.edges, phases)?;
        dag.validate()?;
        Ok(dag)
    }

    pub fn as_view(&self) -> PostASAPDAGView<'_> {
        PostASAPDAGView {
            nodes: &self.nodes,
            edges: &self.edges,
            root: self.root,
            phases: None,
        }
    }
    pub fn validate(&self) -> Result<(), PostASAPDAGValidationError> {
        self.as_view().validate()
    }
}

/// Borrowed compilation/validation projection. Owns no logical computation.
/// A lifecycle assignment overlays its timing on the shared index's records,
/// so node and edge states must be read through [`Self::timing`],
/// [`Self::output_state`] and [`Self::edge_state`], not from the records.
#[derive(Clone, Copy)]
pub struct PostASAPDAGView<'a> {
    nodes: &'a [PostASAPDAGNode],
    edges: &'a [PostASAPDAGEdge],
    root: PostASAPNodeId,
    phases: Option<&'a std::collections::BTreeMap<PostASAPNodeId, ExecutionTiming>>,
}
impl<'a> PostASAPDAGView<'a> {
    pub fn nodes(&self) -> &'a [PostASAPDAGNode] {
        self.nodes
    }
    pub fn edges(&self) -> &'a [PostASAPDAGEdge] {
        self.edges
    }
    pub fn root(&self) -> PostASAPNodeId {
        self.root
    }
    /// Execution timing of `node` under this view's assignment.
    pub fn timing(&self, node: &PostASAPDAGNode) -> ExecutionTiming {
        self.phases
            .and_then(|phases| phases.get(&node.id).copied())
            .unwrap_or(node.output_state.timing)
    }
    pub fn output_state(&self, node: &PostASAPDAGNode) -> ExecutionDataState {
        ExecutionDataState {
            timing: self.timing(node),
            ..node.output_state
        }
    }
    /// An assigned edge carries its producer's assigned state.
    pub fn edge_state(&self, edge: &PostASAPDAGEdge) -> ExecutionDataState {
        match self.phases {
            None => edge.data_state,
            Some(phases) => ExecutionDataState {
                timing: phases
                    .get(&edge.producer)
                    .copied()
                    .unwrap_or(edge.data_state.timing),
                ..edge.data_state
            },
        }
    }
}
impl PostASAPDAGView<'_> {
    pub fn validate(&self) -> Result<(), PostASAPDAGValidationError> {
        use std::collections::{HashMap, HashSet};
        let mut nodes = HashMap::new();
        for node in self.nodes {
            if nodes.insert(node.id, node).is_some() {
                return Err(PostASAPDAGValidationError::DuplicateNodeId(node.id));
            }
            if let PostASAPOperatorPayload::SummaryAgg {
                family, grouping, ..
            } = &node.payload
            {
                let mut found_family = false;
                for field in &node.output_schema.fields {
                    if &field.dtype == family {
                        found_family = true;
                    }
                    if let SummaryFamilyType::Sketch(_, schema_grouping) = &field.dtype {
                        if schema_grouping != grouping {
                            return Err(PostASAPDAGValidationError::SummaryGroupingMismatch {
                                node: node.id,
                            });
                        }
                    }
                }
                if !found_family {
                    return Err(PostASAPDAGValidationError::SummaryFamilySchemaMismatch {
                        node: node.id,
                    });
                }
            }
        }
        if !nodes.contains_key(&self.root) {
            return Err(PostASAPDAGValidationError::MissingRoot(self.root));
        }
        let mut children: HashMap<PostASAPNodeId, Vec<PostASAPNodeId>> = HashMap::new();
        for edge in self.edges {
            let producer = nodes.get(&edge.producer).ok_or(
                PostASAPDAGValidationError::MissingEdgeEndpoint(edge.producer),
            )?;
            if !nodes.contains_key(&edge.consumer) {
                return Err(PostASAPDAGValidationError::MissingEdgeEndpoint(
                    edge.consumer,
                ));
            }
            if self.timing(producer) == ExecutionTiming::QueryTime
                && self.timing(nodes[&edge.consumer]) == ExecutionTiming::IngestionTime
            {
                return Err(PostASAPDAGValidationError::QueryDependencyInIngestion {
                    producer: edge.producer,
                    consumer: edge.consumer,
                });
            }
            if edge.intermediate_schema != producer.output_schema {
                return Err(PostASAPDAGValidationError::EdgeSchemaMismatch {
                    producer: edge.producer,
                    consumer: edge.consumer,
                });
            }
            if self.edge_state(edge) != self.output_state(producer) {
                return Err(PostASAPDAGValidationError::EdgeDataStateMismatch {
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
            id: PostASAPNodeId,
            children: &HashMap<PostASAPNodeId, Vec<PostASAPNodeId>>,
            visiting: &mut HashSet<PostASAPNodeId>,
            visited: &mut HashSet<PostASAPNodeId>,
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
        if !visit(
            self.root,
            &children,
            &mut HashSet::new(),
            &mut HashSet::new(),
        ) {
            return Err(PostASAPDAGValidationError::Cycle);
        }
        let mut reachable = HashSet::new();
        fn mark(
            id: PostASAPNodeId,
            children: &HashMap<PostASAPNodeId, Vec<PostASAPNodeId>>,
            reachable: &mut HashSet<PostASAPNodeId>,
        ) {
            if !reachable.insert(id) {
                return;
            }
            for child in children.get(&id).into_iter().flatten() {
                mark(*child, children, reachable);
            }
        }
        mark(self.root, &children, &mut reachable);
        if let Some(id) = nodes.keys().find(|id| !reachable.contains(id)) {
            return Err(PostASAPDAGValidationError::UnreachableNode(*id));
        }
        Ok(())
    }
}

/// Lifecycle-assigned timing over one indexed shared logical graph.
/// Cloning an assignment shares the graph and its index; it copies no operators.
#[derive(Debug, Clone)]
pub struct PostASAPDAGAssignment {
    index: Rc<PostASAPDAGIndex>,
    phases: std::collections::BTreeMap<PostASAPNodeId, ExecutionTiming>,
}
impl PostASAPDAGAssignment {
    pub fn index(&self) -> &Rc<PostASAPDAGIndex> {
        &self.index
    }
    pub fn new(
        index: Rc<PostASAPDAGIndex>,
        phases: std::collections::BTreeMap<PostASAPNodeId, ExecutionTiming>,
    ) -> Result<Self, PostASAPDAGValidationError> {
        if phases.len() != index.nodes.len()
            || index.nodes.iter().any(|n| !phases.contains_key(&n.id))
        {
            return Err(PostASAPDAGValidationError::IncompletePhaseAssignment);
        }
        let result = Self { index, phases };
        result.view().validate()?;
        Ok(result)
    }
    pub fn phases(&self) -> &std::collections::BTreeMap<PostASAPNodeId, ExecutionTiming> {
        &self.phases
    }
    /// The shared index's records with this assignment's timing overlaid.
    pub fn view(&self) -> PostASAPDAGView<'_> {
        PostASAPDAGView {
            phases: Some(&self.phases),
            ..self.index.view()
        }
    }
    pub fn to_transport(&self) -> PostASAPDAGTransport {
        let view = self.view();
        PostASAPDAGTransport {
            nodes: view
                .nodes
                .iter()
                .map(|node| PostASAPDAGNode {
                    output_state: view.output_state(node),
                    ..node.clone()
                })
                .collect(),
            edges: view
                .edges
                .iter()
                .map(|edge| PostASAPDAGEdge {
                    data_state: view.edge_state(edge),
                    ..edge.clone()
                })
                .collect(),
            root: view.root,
        }
    }
}

fn assign_phases(
    nodes: &mut [PostASAPDAGNode],
    edges: &mut [PostASAPDAGEdge],
    phases: &std::collections::BTreeMap<PostASAPNodeId, ExecutionTiming>,
) -> Result<(), PostASAPDAGValidationError> {
    if phases.len() != nodes.len() || nodes.iter().any(|n| !phases.contains_key(&n.id)) {
        return Err(PostASAPDAGValidationError::IncompletePhaseAssignment);
    }
    for node in nodes.iter_mut() {
        node.output_state.timing = phases[&node.id];
    }
    let states: HashMap<_, _> = nodes.iter().map(|n| (n.id, n.output_state)).collect();
    for edge in edges {
        edge.data_state =
            *states
                .get(&edge.producer)
                .ok_or(PostASAPDAGValidationError::MissingEdgeEndpoint(
                    edge.producer,
                ))?;
    }
    Ok(())
}

/// Compiler-local identity assignment. It deliberately retains `Rc` handles
/// and is not serialized; deployed artifacts persist the post-ASAP node ID
/// together with their physical materialization/query IDs.
#[derive(Debug, Clone)]
pub struct PostASAPNodeIdentityMap {
    nodes_by_id: Vec<Rc<PostASAPNode>>,
}

impl PostASAPNodeIdentityMap {
    pub fn node_id(&self, node: &Rc<PostASAPNode>) -> Option<PostASAPNodeId> {
        self.nodes_by_id
            .iter()
            .position(|candidate| Rc::ptr_eq(candidate, node))
            .map(|id| PostASAPNodeId(id as u32))
    }

    pub fn summary_node(&self, id: PostASAPNodeId) -> Option<&Rc<PostASAPNode>> {
        self.nodes_by_id.get(id.0 as usize)
    }
}

pub fn export_post_asap_dag(
    root: &Rc<PostASAPNode>,
) -> Result<PostASAPDAGTransport, ExecutionDataStateError> {
    let dag = index_post_asap_dag(root)?.to_transport();
    dag.validate()
        .expect("compiler emits a valid post-ASAP DAG");
    Ok(dag)
}

/// Indexed projection of the authoritative shared graph: node identities,
/// plus node and edge records projected once for compilation and transport.
/// Lifecycle assignments overlay timing on these records without copying them.
#[derive(Debug, Clone)]
pub struct PostASAPDAGIndex {
    pub root_id: PostASAPNodeId,
    pub node_ids: PostASAPNodeIdentityMap,
    nodes: Vec<PostASAPDAGNode>,
    edges: Vec<PostASAPDAGEdge>,
}

impl PostASAPDAGIndex {
    /// Node records in ID order, with the logical graph's own timing.
    pub fn node_views(&self) -> &[PostASAPDAGNode] {
        &self.nodes
    }
    pub fn edges(&self) -> &[PostASAPDAGEdge] {
        &self.edges
    }
    pub fn view(&self) -> PostASAPDAGView<'_> {
        PostASAPDAGView {
            nodes: &self.nodes,
            edges: &self.edges,
            root: self.root_id,
            phases: None,
        }
    }
    pub fn to_transport(&self) -> PostASAPDAGTransport {
        PostASAPDAGTransport {
            nodes: self.nodes.clone(),
            edges: self.edges.clone(),
            root: self.root_id,
        }
    }
}

fn project_node(
    id: PostASAPNodeId,
    node: &PostASAPNode,
    state: ExecutionDataState,
) -> PostASAPDAGNode {
    let payload = match &node.expr {
        SummaryExpr::KeepPreAsap(expression) => PostASAPOperatorPayload::Fallback {
            expression: (**expression).clone(),
        },
        SummaryExpr::BinaryOp { operator, .. } => PostASAPOperatorPayload::Binary {
            operator: operator.clone(),
        },

        SummaryExpr::ValueOperation { operation, .. } => PostASAPOperatorPayload::Value {
            operation: operation.clone(),
        },
        SummaryExpr::RelationalJoin {
            kind,
            pred,
            pruning,
            ..
        } => PostASAPOperatorPayload::RelationalJoin {
            join_kind: kind.clone(),
            pred: pred.clone(),
            pruning: pruning.clone(),
        },
        SummaryExpr::SummaryAgg {
            family,
            input,
            reduction,
            grouping,
            ..
        } => PostASAPOperatorPayload::SummaryAgg {
            family: family.clone(),
            input: input.clone(),
            reduction: reduction.clone(),
            grouping: grouping.clone(),
        },
        SummaryExpr::SummaryJoin { key, family, .. } => PostASAPOperatorPayload::SummaryJoin {
            key: key.clone(),
            family: family.clone(),
        },
        SummaryExpr::SummarySubtract { .. } => PostASAPOperatorPayload::SummarySubtract,
        SummaryExpr::SummaryDelete { key, .. } => {
            PostASAPOperatorPayload::SummaryDelete { key: key.clone() }
        }
        SummaryExpr::SummaryEstimate { query, .. } => PostASAPOperatorPayload::SummaryEstimate {
            query: query.clone(),
        },
        SummaryExpr::SummaryMerge { .. } => PostASAPOperatorPayload::SummaryMerge,
    };
    PostASAPDAGNode {
        id,
        payload,
        output_state: state,
        output_schema: node.schema.clone(),
        guarantee: node.guarantee.clone(),
    }
}

/// Assign stable postorder IDs once without copying the logical operators.
pub fn index_post_asap_dag(
    root: &super::PostASAPDAG,
) -> Result<PostASAPDAGIndex, ExecutionDataStateError> {
    let assignment = validate_execution_data_states(root)?;
    let mut edges = Vec::new();
    let mut ids = HashMap::new();
    let mut nodes = Vec::new();

    fn visit(
        node: &Rc<PostASAPNode>,
        assignment: &super::ExecutionDataStateAssignment,
        ids: &mut HashMap<*const PostASAPNode, PostASAPNodeId>,
        nodes: &mut Vec<Rc<PostASAPNode>>,
        edges: &mut Vec<PostASAPDAGEdge>,
    ) -> PostASAPNodeId {
        if let Some(id) = ids.get(&Rc::as_ptr(node)) {
            return *id;
        }
        let children: Vec<(&Rc<PostASAPNode>, EdgeRole)> = match &node.expr {
            SummaryExpr::KeepPreAsap(_) => vec![],
            SummaryExpr::BinaryOp { lhs, rhs, .. } => {
                vec![(lhs, EdgeRole::Left), (rhs, EdgeRole::Right)]
            }

            SummaryExpr::ValueOperation { child, .. } | SummaryExpr::SummaryAgg { child, .. } => {
                vec![(child, EdgeRole::Input)]
            }
            SummaryExpr::RelationalJoin { left, right, .. } => {
                vec![(left, EdgeRole::Left), (right, EdgeRole::Right)]
            }
            SummaryExpr::SummaryJoin { outer, inner, .. } => {
                vec![(outer, EdgeRole::Left), (inner, EdgeRole::Right)]
            }
            SummaryExpr::SummarySubtract { left, right } => {
                vec![(left, EdgeRole::Left), (right, EdgeRole::Right)]
            }
            SummaryExpr::SummaryDelete { summary_input, .. }
            | SummaryExpr::SummaryEstimate { summary_input, .. } => {
                vec![(summary_input, EdgeRole::Input)]
            }
            SummaryExpr::SummaryMerge { children, .. } => {
                children.iter().map(|c| (c, EdgeRole::Input)).collect()
            }
        };
        let child_ids: Vec<_> = children
            .iter()
            .map(|(c, r)| (visit(c, assignment, ids, nodes, edges), *c, *r))
            .collect();
        let id = PostASAPNodeId(nodes.len() as u32);
        nodes.push(Rc::clone(node));
        ids.insert(Rc::as_ptr(node), id);
        for (producer, child, role) in child_ids {
            let maintenance_dependency = assignment
                .data_state_of(&nodes[producer.0 as usize])
                .expect("validated producer")
                .timing
                == ExecutionTiming::IngestionTime
                && assignment
                    .data_state_of(node)
                    .expect("validated consumer")
                    .timing
                    == ExecutionTiming::IngestionTime;
            let grouping = match (&child.expr, &node.expr) {
                (
                    SummaryExpr::SummaryAgg {
                        reduction: producer,
                        ..
                    },
                    SummaryExpr::SummaryAgg {
                        reduction: consumer,
                        ..
                    },
                ) if producer == consumer => GroupingEdgeCompatibility::Identical,
                (
                    SummaryExpr::SummaryAgg {
                        reduction: crate::pre_asap::Reduction::PerEntity,
                        ..
                    },
                    SummaryExpr::SummaryAgg {
                        reduction: crate::pre_asap::Reduction::Reduce(_),
                        ..
                    },
                ) => GroupingEdgeCompatibility::ConsumerCoarsensProducer,
                (
                    SummaryExpr::SummaryAgg {
                        reduction: crate::pre_asap::Reduction::Reduce(producer),
                        ..
                    },
                    SummaryExpr::SummaryAgg {
                        reduction: crate::pre_asap::Reduction::Reduce(consumer),
                        ..
                    },
                ) if !producer.is_without()
                    && !consumer.is_without()
                    && consumer.iter().all(|key| producer.contains(key)) =>
                {
                    GroupingEdgeCompatibility::ConsumerCoarsensProducer
                }
                (SummaryExpr::SummaryAgg { .. }, SummaryExpr::SummaryAgg { .. }) => {
                    GroupingEdgeCompatibility::Incompatible
                }
                _ => GroupingEdgeCompatibility::NotApplicable,
            };
            edges.push(PostASAPDAGEdge {
                producer,
                consumer: id,
                role,
                intermediate_schema: child.schema.clone(),
                // The whole-graph validator owns contextual state assignment,
                // especially for shared KeepPreAsap leaves. Export that
                // authoritative result instead of independently deriving the
                // edge state a second time.
                data_state: assignment
                    .data_state_of(child)
                    .expect("validated child has data state"),
                grouping,
                window: if maintenance_dependency {
                    WindowEdgeCompatibility::RequiresAlignedPanePhaseOrExactWindowEdgeResidual
                } else {
                    WindowEdgeCompatibility::NotApplicable
                },
            });
        }
        id
    }

    let root = visit(root, &assignment, &mut ids, &mut nodes, &mut edges);
    let projected = nodes
        .iter()
        .enumerate()
        .map(|(id, node)| {
            project_node(
                PostASAPNodeId(id as u32),
                node,
                assignment
                    .data_state_of(node)
                    .expect("indexed node has a state"),
            )
        })
        .collect();
    Ok(PostASAPDAGIndex {
        root_id: root,
        node_ids: PostASAPNodeIdentityMap { nodes_by_id: nodes },
        nodes: projected,
        edges,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::post_asap::{
        ExactKind, ExactParams, ExecutionTiming, GroupingStrategy, SummaryFamilyType, SummaryField,
        SummaryUpdate, ValueOperation,
    };
    use crate::pre_asap::schema::{Column, Schema};
    use crate::pre_asap::{ColumnRef, DataType, PreASAPNode, Reduction, Source};
    use std::collections::BTreeMap;

    #[test]
    fn every_physical_payload_can_be_assigned_either_phase() {
        use crate::post_asap::DataPrimitive;
        use crate::pre_asap::{ArithmeticOpKind, BinaryOpKind, JoinKind, Predicate, ScalarValue};
        let family = SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
        let predicate = Predicate(Rc::new(PreASAPNode::Literal(ScalarValue::Boolean(true))));
        let payloads = vec![
            PostASAPOperatorPayload::Fallback {
                expression: PreASAPNode::Literal(ScalarValue::Int64(1)),
            },
            PostASAPOperatorPayload::Binary {
                operator: BinaryOperator {
                    checked_relative_division: false,
                    checked_finite_division: false,
                    kind: BinaryOpKind::Arithmetic(ArithmeticOpKind::Add),
                    vector_match: None,
                },
            },
            PostASAPOperatorPayload::Value {
                operation: ValueOperation::Limit {
                    n: 1,
                    offset: 0,
                    partition_by: Default::default(),
                },
            },
            PostASAPOperatorPayload::RelationalJoin {
                join_kind: JoinKind::Semi,
                pred: predicate,
                pruning: None,
            },
            PostASAPOperatorPayload::SummaryAgg {
                family: family.clone(),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::by(vec![]),
                grouping: GroupingStrategy::default(),
            },
            PostASAPOperatorPayload::SummaryJoin {
                key: ColumnRef::SampleValue,
                family: family.clone(),
            },
            PostASAPOperatorPayload::SummarySubtract,
            PostASAPOperatorPayload::SummaryDelete {
                key: ColumnRef::SampleValue,
            },
            PostASAPOperatorPayload::SummaryEstimate {
                query: SketchQuery::Cardinality,
            },
            PostASAPOperatorPayload::SummaryMerge,
        ];
        for payload in payloads {
            // This checks physical identity and placement, not kernel availability.
            let primitive = match &payload {
                PostASAPOperatorPayload::Fallback { .. }
                | PostASAPOperatorPayload::Binary { .. }
                | PostASAPOperatorPayload::Value { .. }
                | PostASAPOperatorPayload::RelationalJoin { .. }
                | PostASAPOperatorPayload::SummaryEstimate { .. } => DataPrimitive::Raw,
                PostASAPOperatorPayload::SummaryAgg { .. }
                | PostASAPOperatorPayload::SummaryJoin { .. }
                | PostASAPOperatorPayload::SummarySubtract
                | PostASAPOperatorPayload::SummaryDelete { .. }
                | PostASAPOperatorPayload::SummaryMerge => DataPrimitive::SummaryState,
            };
            let dag = PostASAPDAGTransport {
                root: PostASAPNodeId(0),
                edges: vec![],
                nodes: vec![PostASAPDAGNode {
                    id: PostASAPNodeId(0),
                    payload: payload.clone(),
                    output_state: ExecutionDataState {
                        timing: ExecutionTiming::QueryTime,
                        primitive,
                    },
                    output_schema: SummarySchema {
                        fields: vec![SummaryField {
                            name: "value".into(),
                            dtype: family.clone(),
                            nullable: false,
                        }],
                        time_index: None,
                    },
                    guarantee: None,
                }],
            };
            for phase in [ExecutionTiming::IngestionTime, ExecutionTiming::QueryTime] {
                let placed = dag
                    .with_execution_phases(&BTreeMap::from([(dag.root, phase)]))
                    .unwrap();
                assert_eq!(placed.nodes[0].payload, payload);
                assert_eq!(placed.nodes[0].output_state.timing, phase);
                let wire = serde_json::to_value(&placed).unwrap();
                assert!(wire["nodes"][0]["payload"].get("timing").is_none());
                assert_eq!(
                    serde_json::from_value::<PostASAPDAGTransport>(wire).unwrap(),
                    placed
                );
            }
            assert!(dag.with_execution_phases(&BTreeMap::new()).is_err());
        }
    }

    #[test]
    fn phase_assignment_updates_edges_and_rejects_query_dependencies_in_ingestion() {
        use crate::pre_asap::ScalarValue;
        let schema = SummarySchema {
            fields: vec![],
            time_index: None,
        };
        let nodes = [0, 1]
            .into_iter()
            .map(|id| PostASAPDAGNode {
                id: PostASAPNodeId(id),
                payload: PostASAPOperatorPayload::Fallback {
                    expression: PreASAPNode::Literal(ScalarValue::Int64(1)),
                },
                output_state: ExecutionDataState::QUERY_ROWS,
                output_schema: schema.clone(),
                guarantee: None,
            })
            .collect();
        let dag = PostASAPDAGTransport {
            nodes,
            root: PostASAPNodeId(1),
            edges: vec![PostASAPDAGEdge {
                producer: PostASAPNodeId(0),
                consumer: PostASAPNodeId(1),
                role: EdgeRole::Input,
                intermediate_schema: schema,
                data_state: ExecutionDataState::QUERY_ROWS,
                grouping: GroupingEdgeCompatibility::NotApplicable,
                window: WindowEdgeCompatibility::NotApplicable,
            }],
        };
        let placed = dag
            .with_execution_phases(&BTreeMap::from([
                (PostASAPNodeId(0), ExecutionTiming::IngestionTime),
                (PostASAPNodeId(1), ExecutionTiming::QueryTime),
            ]))
            .unwrap();
        assert_eq!(
            placed.edges[0].data_state.timing,
            ExecutionTiming::IngestionTime
        );
        assert_eq!(dag.edges[0].data_state.timing, ExecutionTiming::QueryTime);
        assert!(matches!(
            dag.with_execution_phases(&BTreeMap::from([
                (PostASAPNodeId(0), ExecutionTiming::QueryTime),
                (PostASAPNodeId(1), ExecutionTiming::IngestionTime),
            ])),
            Err(PostASAPDAGValidationError::QueryDependencyInIngestion { .. })
        ));
    }

    #[test]
    fn exports_summary_over_summary_as_typed_precompute_edges() {
        let scan = Rc::new(PreASAPNode::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::new(vec![Column::new("value", DataType::Float64, false)]),
        });
        let raw = Rc::new(PostASAPNode {
            expr: SummaryExpr::KeepPreAsap(scan),
            schema: SummarySchema {
                fields: vec![SummaryField {
                    name: "value".into(),
                    dtype: SummaryFamilyType::Plain(DataType::Float64),
                    nullable: false,
                }],
                time_index: None,
            },
            guarantee: None,
        });
        let make_agg = |child: Rc<PostASAPNode>, kind, params| {
            let family = SummaryFamilyType::ExactAggregate(kind, params);
            Rc::new(PostASAPNode {
                expr: SummaryExpr::SummaryAgg {
                    child,
                    family: family.clone(),
                    input: SummaryUpdate::column(ColumnRef::SampleValue),
                    reduction: Reduction::by(vec![]),
                    grouping: GroupingStrategy::default(),
                },
                schema: SummarySchema {
                    fields: vec![SummaryField {
                        name: "value".into(),
                        dtype: family,
                        nullable: false,
                    }],
                    time_index: None,
                },
                guarantee: None,
            })
        };
        let inner = make_agg(raw, ExactKind::Sum, ExactParams::Sum);
        let outer = make_agg(Rc::clone(&inner), ExactKind::Sum, ExactParams::Sum);
        let root = Rc::new(PostASAPNode {
            expr: SummaryExpr::ValueOperation {
                child: outer,
                operation: ValueOperation::FinalizeExactAccumulator,
                timing: ExecutionTiming::QueryTime,
            },
            schema: SummarySchema {
                fields: vec![SummaryField {
                    name: "value".into(),
                    dtype: SummaryFamilyType::Plain(DataType::Float64),
                    nullable: false,
                }],
                time_index: None,
            },
            guarantee: None,
        });

        let compiled = index_post_asap_dag(&root).unwrap();
        assert_eq!(compiled.node_ids.node_id(&root), Some(PostASAPNodeId(3)));
        assert!(Rc::ptr_eq(
            compiled.node_ids.summary_node(PostASAPNodeId(1)).unwrap(),
            &inner
        ));
        let dag = compiled.to_transport();
        assert_eq!(dag.root, PostASAPNodeId(3));
        assert_eq!(
            dag.nodes[1].output_state,
            ExecutionDataState::INGESTION_SUMMARY
        );
        assert_eq!(
            dag.nodes[2].output_state,
            ExecutionDataState::INGESTION_SUMMARY
        );
        let dependency = dag
            .edges
            .iter()
            .find(|e| e.producer == PostASAPNodeId(1) && e.consumer == PostASAPNodeId(2))
            .unwrap();
        assert_eq!(dependency.data_state, ExecutionDataState::INGESTION_SUMMARY);
        assert_eq!(dependency.grouping, GroupingEdgeCompatibility::Identical);
        assert_eq!(
            dependency.window,
            WindowEdgeCompatibility::RequiresAlignedPanePhaseOrExactWindowEdgeResidual
        );
        assert!(matches!(
            dependency.intermediate_schema.fields[0].dtype,
            SummaryFamilyType::ExactAggregate(ExactKind::Sum, _)
        ));
        let encoded = serde_json::to_string(&dag).expect("serialize post-ASAP DAG");
        let decoded: PostASAPDAGTransport =
            serde_json::from_str(&encoded).expect("deserialize post-ASAP DAG");
        assert_eq!(decoded, dag);
        let document = PostASAPDAGDocument::new(decoded);
        document.validate().unwrap();
        let mut invalid = serde_json::to_value(&document).unwrap();
        invalid["dag"]["nodes"][0]["operator"] = serde_json::json!("Binary");
        assert!(serde_json::from_value::<PostASAPDAGDocument>(invalid).is_err());
        assert!(document.dag.nodes.iter().all(|node| {
            let wire = serde_json::to_value(node).unwrap();
            wire.get("operator").is_none() && wire["payload"]["kind"].is_string()
        }));
        let mut old_version = document.clone();
        old_version.schema_version = 1;
        assert_eq!(
            old_version.validate(),
            Err(PostASAPDAGValidationError::UnsupportedVersion(1))
        );
        let mut unknown = serde_json::to_value(&document).unwrap();
        unknown["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<PostASAPDAGDocument>(unknown).is_err());
        assert!(matches!(
            dag.nodes[2].payload,
            PostASAPOperatorPayload::SummaryAgg {
                family: SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
                reduction: Reduction::Reduce(_),
                ..
            }
        ));

        // An assignment's view overlays its timing on the shared records; its
        // transport equals assigning the same phases to the exported document.
        let index = Rc::new(compiled);
        let phases: BTreeMap<_, _> = index
            .node_views()
            .iter()
            .map(|n| (n.id, ExecutionTiming::QueryTime))
            .collect();
        let assignment = PostASAPDAGAssignment::new(index.clone(), phases.clone()).unwrap();
        let summary = &index.node_views()[1];
        assert_eq!(index.view().timing(summary), ExecutionTiming::IngestionTime);
        assert_eq!(
            assignment.view().timing(summary),
            ExecutionTiming::QueryTime
        );
        assert_eq!(
            assignment.to_transport(),
            index.to_transport().with_execution_phases(&phases).unwrap()
        );
    }

    #[test]
    fn post_asap_node_ids_serialize_in_deterministic_binding_order() {
        let mut bindings = BTreeMap::new();
        bindings.insert(PostASAPNodeId(10), "materialization-10");
        bindings.insert(PostASAPNodeId(2), "query-2");
        assert_eq!(
            serde_json::to_string(&bindings).unwrap(),
            r#"{"2":"query-2","10":"materialization-10"}"#
        );
    }
}
