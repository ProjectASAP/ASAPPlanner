//! Runtime-neutral executable DAG contract shared by precompute and query engines.

use std::collections::HashMap;
use std::rc::Rc;

use super::{
    validate_execution_data_states, ExecutionDataState, ExecutionDataStateError, ResultGuarantee,
    SummaryExpr, SummaryNode, SummarySchema,
};
use super::{
    BinaryOperator, CandidateCompleteness, ExecutionTiming, GroupingStrategy, SketchQuery,
    SummaryFamilyType, SummaryUpdate, ValueOperation,
};
use crate::pre_asap::{ColumnRef, GroupKeys, JoinKind, Predicate, QueryExpr, Reduction};
use thiserror::Error;

pub const POST_ASAP_DAG_WIRE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ExecutableOperator {
    Fallback,
    Binary,
    CandidateTopK,
    Value,
    RelationalJoin,
    SummaryAgg,
    SummaryJoin,
    SummarySubtract,
    SummaryDelete,
    SummaryEstimate,
    SummaryMerge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EdgeRole {
    Input,
    Left,
    Right,
    CandidateMembership,
    AuthoritativeValues,
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
    RequiresAlignedPanePhaseOrExactBoundaryResidual,
    NotApplicable,
}

/// Stable identity of a node within one exported post-ASAP semantic DAG.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct PostAsapNodeId(pub u32);

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutableOperatorPayload {
    Fallback {
        expression: QueryExpr,
    },
    Binary {
        operator: BinaryOperator,
    },
    CandidateTopK {
        /// Fixed-width transport value; runtimes validate conversion to their
        /// local collection index type at installation.
        k: u64,
        grouping: GroupKeys,
        completeness: CandidateCompleteness,
    },
    Value {
        operation: ValueOperation,
        timing: ExecutionTiming,
    },
    RelationalJoin {
        join_kind: JoinKind,
        pred: Predicate,
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

impl ExecutableOperatorPayload {
    pub fn operator(&self) -> ExecutableOperator {
        match self {
            Self::Fallback { .. } => ExecutableOperator::Fallback,
            Self::Binary { .. } => ExecutableOperator::Binary,
            Self::CandidateTopK { .. } => ExecutableOperator::CandidateTopK,
            Self::Value { .. } => ExecutableOperator::Value,
            Self::RelationalJoin { .. } => ExecutableOperator::RelationalJoin,
            Self::SummaryAgg { .. } => ExecutableOperator::SummaryAgg,
            Self::SummaryJoin { .. } => ExecutableOperator::SummaryJoin,
            Self::SummarySubtract => ExecutableOperator::SummarySubtract,
            Self::SummaryDelete { .. } => ExecutableOperator::SummaryDelete,
            Self::SummaryEstimate { .. } => ExecutableOperator::SummaryEstimate,
            Self::SummaryMerge => ExecutableOperator::SummaryMerge,
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableDagNode {
    pub id: PostAsapNodeId,
    pub operator: ExecutableOperator,
    pub payload: ExecutableOperatorPayload,
    pub output_state: ExecutionDataState,
    pub output_schema: SummarySchema,
    pub guarantee: Option<ResultGuarantee>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableDagEdge {
    pub producer: PostAsapNodeId,
    pub consumer: PostAsapNodeId,
    pub role: EdgeRole,
    pub intermediate_schema: SummarySchema,
    pub data_state: ExecutionDataState,
    pub grouping: GroupingEdgeCompatibility,
    pub window: WindowEdgeCompatibility,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableDag {
    pub nodes: Vec<ExecutableDagNode>,
    pub edges: Vec<ExecutableDagEdge>,
    /// Semantic workload root. Physical query/precompute sinks are selected
    /// downstream by the control plane.
    pub root: PostAsapNodeId,
}

/// Versioned transport envelope for a post-ASAP semantic DAG.
///
/// `ExecutableDag` remains serializable as a legacy in-process adapter. New
/// process boundaries should exchange this envelope and call [`Self::validate`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostAsapDagDocument {
    pub schema_version: u32,
    pub dag: ExecutableDag,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExecutableDagValidationError {
    #[error("unsupported post-ASAP DAG schema version {0}")]
    UnsupportedVersion(u32),
    #[error("duplicate post-ASAP node id {0:?}")]
    DuplicateNodeId(PostAsapNodeId),
    #[error("post-ASAP DAG root {0:?} does not name a node")]
    MissingRoot(PostAsapNodeId),
    #[error("edge endpoint {0:?} does not name a node")]
    MissingEdgeEndpoint(PostAsapNodeId),
    #[error("node {node:?} declares {declared:?} but its payload is {actual:?}")]
    OperatorPayloadMismatch {
        node: PostAsapNodeId,
        declared: ExecutableOperator,
        actual: ExecutableOperator,
    },
    #[error("edge {producer:?}->{consumer:?} schema differs from producer output")]
    EdgeSchemaMismatch {
        producer: PostAsapNodeId,
        consumer: PostAsapNodeId,
    },
    #[error("edge {producer:?}->{consumer:?} data state differs from producer output")]
    EdgeDataStateMismatch {
        producer: PostAsapNodeId,
        consumer: PostAsapNodeId,
    },
    #[error("post-ASAP DAG contains a cycle")]
    Cycle,
    #[error("post-ASAP node {0:?} is not reachable from the root")]
    UnreachableNode(PostAsapNodeId),
    #[error("summary aggregate node {node:?} output schema does not contain its declared family")]
    SummaryFamilySchemaMismatch { node: PostAsapNodeId },
    #[error(
        "summary aggregate node {node:?} declares grouping inconsistent with its sketch state"
    )]
    SummaryGroupingMismatch { node: PostAsapNodeId },
}

impl PostAsapDagDocument {
    pub fn new(dag: ExecutableDag) -> Self {
        Self {
            schema_version: POST_ASAP_DAG_WIRE_VERSION,
            dag,
        }
    }

    pub fn validate(&self) -> Result<(), ExecutableDagValidationError> {
        if self.schema_version != POST_ASAP_DAG_WIRE_VERSION {
            return Err(ExecutableDagValidationError::UnsupportedVersion(
                self.schema_version,
            ));
        }
        self.dag.validate()
    }
}

impl ExecutableDag {
    pub fn validate(&self) -> Result<(), ExecutableDagValidationError> {
        use std::collections::{HashMap, HashSet};
        let mut nodes = HashMap::new();
        for node in &self.nodes {
            if nodes.insert(node.id, node).is_some() {
                return Err(ExecutableDagValidationError::DuplicateNodeId(node.id));
            }
            let actual = node.payload.operator();
            if node.operator != actual {
                return Err(ExecutableDagValidationError::OperatorPayloadMismatch {
                    node: node.id,
                    declared: node.operator,
                    actual,
                });
            }
            if let ExecutableOperatorPayload::SummaryAgg {
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
                            return Err(ExecutableDagValidationError::SummaryGroupingMismatch {
                                node: node.id,
                            });
                        }
                    }
                }
                if !found_family {
                    return Err(ExecutableDagValidationError::SummaryFamilySchemaMismatch {
                        node: node.id,
                    });
                }
            }
        }
        if !nodes.contains_key(&self.root) {
            return Err(ExecutableDagValidationError::MissingRoot(self.root));
        }
        let mut children: HashMap<PostAsapNodeId, Vec<PostAsapNodeId>> = HashMap::new();
        for edge in &self.edges {
            let producer = nodes.get(&edge.producer).ok_or(
                ExecutableDagValidationError::MissingEdgeEndpoint(edge.producer),
            )?;
            if !nodes.contains_key(&edge.consumer) {
                return Err(ExecutableDagValidationError::MissingEdgeEndpoint(
                    edge.consumer,
                ));
            }
            if edge.intermediate_schema != producer.output_schema {
                return Err(ExecutableDagValidationError::EdgeSchemaMismatch {
                    producer: edge.producer,
                    consumer: edge.consumer,
                });
            }
            if edge.data_state != producer.output_state {
                return Err(ExecutableDagValidationError::EdgeDataStateMismatch {
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
            id: PostAsapNodeId,
            children: &HashMap<PostAsapNodeId, Vec<PostAsapNodeId>>,
            visiting: &mut HashSet<PostAsapNodeId>,
            visited: &mut HashSet<PostAsapNodeId>,
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
            return Err(ExecutableDagValidationError::Cycle);
        }
        let mut reachable = HashSet::new();
        fn mark(
            id: PostAsapNodeId,
            children: &HashMap<PostAsapNodeId, Vec<PostAsapNodeId>>,
            reachable: &mut HashSet<PostAsapNodeId>,
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
            return Err(ExecutableDagValidationError::UnreachableNode(*id));
        }
        Ok(())
    }
}

/// Compiler-local identity assignment. It deliberately retains `Rc` handles
/// and is not serialized; deployed artifacts persist the executable node ID
/// together with their physical materialization/query IDs.
#[derive(Debug, Clone)]
pub struct ExecutableNodeIdentityMap {
    nodes_by_id: Vec<Rc<SummaryNode>>,
}

impl ExecutableNodeIdentityMap {
    pub fn node_id(&self, node: &Rc<SummaryNode>) -> Option<PostAsapNodeId> {
        self.nodes_by_id
            .iter()
            .position(|candidate| Rc::ptr_eq(candidate, node))
            .map(|id| PostAsapNodeId(id as u32))
    }

    pub fn summary_node(&self, id: PostAsapNodeId) -> Option<&Rc<SummaryNode>> {
        self.nodes_by_id.get(id.0 as usize)
    }
}

#[derive(Debug, Clone)]
pub struct ExecutableDagCompilation {
    pub dag: ExecutableDag,
    pub node_ids: ExecutableNodeIdentityMap,
}

pub fn compile_executable_dag(
    root: &Rc<SummaryNode>,
) -> Result<ExecutableDag, ExecutionDataStateError> {
    Ok(compile_executable_dag_with_node_ids(root)?.dag)
}

pub fn compile_executable_dag_with_node_ids(
    root: &Rc<SummaryNode>,
) -> Result<ExecutableDagCompilation, ExecutionDataStateError> {
    let assignment = validate_execution_data_states(root)?;
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut ids = HashMap::new();
    let mut nodes_by_id = Vec::new();

    fn visit(
        node: &Rc<SummaryNode>,
        assignment: &super::ExecutionDataStateAssignment,
        ids: &mut HashMap<*const SummaryNode, PostAsapNodeId>,
        nodes: &mut Vec<ExecutableDagNode>,
        edges: &mut Vec<ExecutableDagEdge>,
        nodes_by_id: &mut Vec<Rc<SummaryNode>>,
    ) -> PostAsapNodeId {
        if let Some(id) = ids.get(&Rc::as_ptr(node)) {
            return *id;
        }
        let children: Vec<(&Rc<SummaryNode>, EdgeRole)> = match &node.expr {
            SummaryExpr::KeepPreAsap(_) => vec![],
            SummaryExpr::BinaryOp { lhs, rhs, .. } => {
                vec![(lhs, EdgeRole::Left), (rhs, EdgeRole::Right)]
            }
            SummaryExpr::CandidateTopK {
                candidates, values, ..
            } => vec![
                (candidates, EdgeRole::CandidateMembership),
                (values, EdgeRole::AuthoritativeValues),
            ],
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
            SummaryExpr::SummaryMerge { children } => {
                children.iter().map(|c| (c, EdgeRole::Input)).collect()
            }
        };
        let child_ids: Vec<_> = children
            .iter()
            .map(|(c, r)| (visit(c, assignment, ids, nodes, edges, nodes_by_id), *c, *r))
            .collect();
        let id = PostAsapNodeId(nodes.len() as u32);
        let state = assignment
            .data_state_of(node)
            .expect("validated node has state");
        let payload = match &node.expr {
            SummaryExpr::KeepPreAsap(expression) => ExecutableOperatorPayload::Fallback {
                expression: (**expression).clone(),
            },
            SummaryExpr::BinaryOp { operator, .. } => ExecutableOperatorPayload::Binary {
                operator: operator.clone(),
            },
            SummaryExpr::CandidateTopK {
                k,
                grouping,
                completeness,
                ..
            } => ExecutableOperatorPayload::CandidateTopK {
                k: u64::try_from(*k).expect("usize always fits into the u64 wire count"),
                grouping: grouping.clone(),
                completeness: completeness.clone(),
            },
            SummaryExpr::ValueOperation {
                operation, timing, ..
            } => ExecutableOperatorPayload::Value {
                operation: operation.clone(),
                timing: *timing,
            },
            SummaryExpr::RelationalJoin { kind, pred, .. } => {
                ExecutableOperatorPayload::RelationalJoin {
                    join_kind: kind.clone(),
                    pred: pred.clone(),
                }
            }
            SummaryExpr::SummaryAgg {
                family,
                input,
                reduction,
                grouping,
                ..
            } => ExecutableOperatorPayload::SummaryAgg {
                family: family.clone(),
                input: input.clone(),
                reduction: reduction.clone(),
                grouping: grouping.clone(),
            },
            SummaryExpr::SummaryJoin { key, family, .. } => {
                ExecutableOperatorPayload::SummaryJoin {
                    key: key.clone(),
                    family: family.clone(),
                }
            }
            SummaryExpr::SummarySubtract { .. } => ExecutableOperatorPayload::SummarySubtract,
            SummaryExpr::SummaryDelete { key, .. } => {
                ExecutableOperatorPayload::SummaryDelete { key: key.clone() }
            }
            SummaryExpr::SummaryEstimate { query, .. } => {
                ExecutableOperatorPayload::SummaryEstimate {
                    query: query.clone(),
                }
            }
            SummaryExpr::SummaryMerge { .. } => ExecutableOperatorPayload::SummaryMerge,
        };
        let operator = payload.operator();
        nodes.push(ExecutableDagNode {
            id,
            operator,
            payload,
            output_state: state,
            output_schema: node.schema.clone(),
            guarantee: node.guarantee.clone(),
        });
        nodes_by_id.push(Rc::clone(node));
        ids.insert(Rc::as_ptr(node), id);
        for (producer, child, role) in child_ids {
            let maintenance_dependency = nodes[producer.0 as usize].output_state.timing
                == ExecutionTiming::MaintenanceTime
                && nodes[id.0 as usize].output_state.timing == ExecutionTiming::MaintenanceTime;
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
            edges.push(ExecutableDagEdge {
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
                    WindowEdgeCompatibility::RequiresAlignedPanePhaseOrExactBoundaryResidual
                } else {
                    WindowEdgeCompatibility::NotApplicable
                },
            });
        }
        id
    }

    let root = visit(
        root,
        &assignment,
        &mut ids,
        &mut nodes,
        &mut edges,
        &mut nodes_by_id,
    );
    let dag = ExecutableDag { nodes, edges, root };
    dag.validate()
        .expect("compiler emits a valid post-ASAP DAG");
    Ok(ExecutableDagCompilation {
        dag,
        node_ids: ExecutableNodeIdentityMap { nodes_by_id },
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
    use crate::pre_asap::{ColumnRef, DataType, QueryExpr, Reduction, Source};
    use std::collections::BTreeMap;

    #[test]
    fn exports_summary_over_summary_as_typed_precompute_edges() {
        let scan = Rc::new(QueryExpr::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::new(vec![Column::new("value", DataType::Float64, false)]),
        });
        let raw = Rc::new(SummaryNode {
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
        let make_agg = |child: Rc<SummaryNode>, kind, params| {
            let family = SummaryFamilyType::ExactAggregate(kind, params);
            Rc::new(SummaryNode {
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
        let root = Rc::new(SummaryNode {
            expr: SummaryExpr::ValueOperation {
                child: outer,
                operation: ValueOperation::FinalizeExactAccumulator,
                timing: ExecutionTiming::ReadTime,
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

        let compiled = compile_executable_dag_with_node_ids(&root).unwrap();
        assert_eq!(compiled.node_ids.node_id(&root), Some(PostAsapNodeId(3)));
        assert!(Rc::ptr_eq(
            compiled.node_ids.summary_node(PostAsapNodeId(1)).unwrap(),
            &inner
        ));
        let dag = compiled.dag;
        assert_eq!(dag.root, PostAsapNodeId(3));
        assert_eq!(
            dag.nodes[1].output_state,
            ExecutionDataState::MAINTENANCE_SUMMARY
        );
        assert_eq!(
            dag.nodes[2].output_state,
            ExecutionDataState::MAINTENANCE_SUMMARY
        );
        let dependency = dag
            .edges
            .iter()
            .find(|e| e.producer == PostAsapNodeId(1) && e.consumer == PostAsapNodeId(2))
            .unwrap();
        assert_eq!(
            dependency.data_state,
            ExecutionDataState::MAINTENANCE_SUMMARY
        );
        assert_eq!(dependency.grouping, GroupingEdgeCompatibility::Identical);
        assert_eq!(
            dependency.window,
            WindowEdgeCompatibility::RequiresAlignedPanePhaseOrExactBoundaryResidual
        );
        assert!(matches!(
            dependency.intermediate_schema.fields[0].dtype,
            SummaryFamilyType::ExactAggregate(ExactKind::Sum, _)
        ));
        let encoded = serde_json::to_string(&dag).expect("serialize executable contract");
        let decoded: ExecutableDag =
            serde_json::from_str(&encoded).expect("deserialize executable contract");
        assert_eq!(decoded, dag);
        let document = PostAsapDagDocument::new(decoded);
        document.validate().unwrap();
        let mut invalid = serde_json::to_value(&document).unwrap();
        invalid["dag"]["nodes"][0]["operator"] = serde_json::json!("Binary");
        let invalid: PostAsapDagDocument = serde_json::from_value(invalid).unwrap();
        assert!(matches!(
            invalid.validate(),
            Err(ExecutableDagValidationError::OperatorPayloadMismatch { .. })
        ));
        let mut unknown = serde_json::to_value(&document).unwrap();
        unknown["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<PostAsapDagDocument>(unknown).is_err());
        assert!(matches!(
            dag.nodes[2].payload,
            ExecutableOperatorPayload::SummaryAgg {
                family: SummaryFamilyType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
                reduction: Reduction::Reduce(_),
                ..
            }
        ));
    }

    #[test]
    fn post_asap_node_ids_serialize_in_deterministic_binding_order() {
        let mut bindings = BTreeMap::new();
        bindings.insert(PostAsapNodeId(10), "materialization-10");
        bindings.insert(PostAsapNodeId(2), "query-2");
        assert_eq!(
            serde_json::to_string(&bindings).unwrap(),
            r#"{"2":"query-2","10":"materialization-10"}"#
        );
    }
}
