//! Physical ASAP DAG (planner-layering stage 2 output).
//!
//! The flattened operators of [`crate::ir::flat`], plus the lifetime and
//! execution timing (data state) of every node and edge: the only plan
//! structure, in memory and, serialized, in the plan file. Stage 3 builds it
//! directly as its last step, one node per selected execution, each with the
//! timing and lifetime of the physical candidate it was built from.
//! The deployment executes the plan as given
//! (#509 Assumption 8): it does not change lifetime or timing.

use std::collections::{HashMap, HashSet};
#[cfg(any(test, feature = "test-support"))]
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ir::flat::NodeId;
use crate::ir::operator::asap::ASAPOp;
use crate::ir::operator::node::Operator;
#[cfg(any(test, feature = "test-support"))]
use crate::ir::operator::node::OperatorNode;
use crate::ir::operator::non_asap::NonASAPOp;
use crate::ir::scalar::ScalarExpr;

use crate::accuracy::AccuracyGuarantee;
use crate::ir::properties::execution_data_state::{ExecutionDataState, ExecutionTiming};
use crate::ir::properties::selection::SelectionBox;
use crate::ir::schema::Schema;
use crate::stages::PhysicalCandidateId;
use crate::stages::{Lifetime, PhysicalPropertiesError};
use crate::summary_type::SketchParams;

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
    /// is the node's `PhysicalProperties`.
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
    /// The physical candidate the node was built from: in-memory provenance
    /// for validation and tools, not part of the plan file; `None` for a plan
    /// built by a custom pass.
    #[serde(skip)]
    pub selected_physical_candidate: Option<PhysicalCandidateId>,
    /// For a materialized view (lifetime `AcrossEvaluations`), what it
    /// stores: its memo group's aggregate coverage, carried into the plan
    /// as a [`ViewCoverage`] (the selection, and the plan-scope content id
    /// of the definition, [`ViewCoverage::definition_id`]). Set when the
    /// plan is built from the memo, where the node's group is known; `None`
    /// on every other node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_coverage: Option<ViewCoverage>,
}

/// A view's aggregate coverage
/// ([`AggregateCoverage`](crate::ir::properties::AggregateCoverage), `docs/design_docs/proposals/asap-primitive-schema.md`
/// §4.2): what it computes and which rows went in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewCoverage {
    /// The plan-scope content id of what the view computes: its memo
    /// coverage's `AggregateDefinition` with every input group expanded
    /// recursively to its expression, down to the `Scan`s, holding no
    /// memo group or plan node ids, canonically serialized and hashed
    /// (`stages::view_coverage`).
    /// Equal across plans and processes that compute the same thing,
    /// whatever the selection, and different when they do not (a rollup to
    /// a coarser grouping computes something else and has its own). Inside
    /// one memo, coverages compare by `AggregateDefinition` `==` instead
    /// and need no id.
    pub(crate) definition_id: String,
    /// The rows that went in: a union of boxes of value sets, value ranges
    /// and time relative to the evaluation time. A merge's is the union of
    /// its inputs'.
    pub(crate) selection: Vec<SelectionBox>,
}

#[cfg(any(test, feature = "test-support"))]
impl ViewCoverage {
    /// The plan-scope content id of what the view computes.
    pub fn definition_id(&self) -> &str {
        &self.definition_id
    }

    /// The rows that went in.
    pub fn selection(&self) -> &[SelectionBox] {
        &self.selection
    }
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
    PhysicalProperties {
        node: PhysicalASAPNodeId,
        #[source]
        reason: PhysicalPropertiesError,
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
    #[error(
        "summary aggregate node {node:?} output schema does not contain its declared summary type"
    )]
    SummaryTypeSchemaMismatch { node: PhysicalASAPNodeId },
}

impl PhysicalASAPDAG {
    /// The plan's query roots as IR, in entry order: each operator root's
    /// node, or the standalone scalar query keyed by its entry. Shared nodes
    /// stay shared. For tests that inspect a plan as operator trees.
    #[cfg(any(test, feature = "test-support"))]
    pub fn query_roots(&self) -> Result<Vec<crate::ir::QueryRoot>, PhysicalASAPDAGValidationError> {
        use crate::ir::QueryRoot;
        let nodes = self.operator_nodes()?;
        let mut operator_roots = self.roots.iter();
        let count = self.roots.len() + self.scalar_roots.len();
        Ok((0..count)
            .map(
                |entry| match self.scalar_roots.iter().find(|(index, _)| *index == entry) {
                    Some((_, expr)) => {
                        QueryRoot::Scalar(expr.map_operator_refs(&mut |id| Rc::clone(&nodes[id])))
                    }
                    None => QueryRoot::Operator(Rc::clone(
                        &nodes[operator_roots.next().expect("one operator root per entry")],
                    )),
                },
            )
            .collect())
    }

    /// Every node of this DAG as IR, by node id, with its exported
    /// guarantee (an operator node has no physical properties): what
    /// [`Self::query_roots`] reads. Shared inputs stay shared. Validates the
    /// DAG first.
    #[cfg(any(test, feature = "test-support"))]
    fn operator_nodes(
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
                return Err(PhysicalASAPDAGValidationError::PhysicalProperties {
                    node: node.id,
                    reason: PhysicalPropertiesError::OneEvaluationAtIngestion,
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
                summary_type: family,
                ..
            }) = &node.payload
            {
                let mut found_family = false;
                for field in &node.output_schema.fields {
                    if &field.dtype == family {
                        found_family = true;
                    }
                }
                if !found_family {
                    return Err(PhysicalASAPDAGValidationError::SummaryTypeSchemaMismatch {
                        node: node.id,
                    });
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
/// this way; planning builds the plan in Stage 3.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn physical_asap_dag_at_query_time_with_node_ids(
    roots: &[crate::ir::QueryRoot],
) -> Result<(PhysicalASAPDAG, Vec<Rc<OperatorNode>>), PhysicalASAPDAGValidationError> {
    use crate::ir::properties::execution_data_state::DataPrimitive;
    use crate::ir::QueryRoot;
    let (flat, nodes_by_id) = crate::ir::flat::flatten(roots);
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
            selected_physical_candidate: None,
            view_coverage: None,
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
    use crate::ir::operator::arguments::Reduction;
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
