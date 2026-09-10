//! Runtime-neutral executable DAG contract shared by precompute and query engines.

use std::collections::HashMap;
use std::rc::Rc;

use super::{
    assigned_child_data_state, validate_execution_data_states, ExecutionDataState,
    ExecutionDataStateError, ResultGuarantee, SummaryExpr, SummaryNode, SummarySchema,
};
use super::{
    BinaryOperator, CandidateCompleteness, ExecutionTiming, GroupingStrategy, SketchQuery,
    SummaryFamilyType, SummaryUpdate, ValueOperation,
};
use crate::pre_asap::{ColumnRef, GroupKeys, QueryExpr, Reduction};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ExecutableOperator {
    Fallback,
    Binary,
    CandidateTopK,
    Value,
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
        k: usize,
        grouping: GroupKeys,
        completeness: CandidateCompleteness,
    },
    Value {
        operation: ValueOperation,
        timing: ExecutionTiming,
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
pub struct ExecutableDagNode {
    pub id: PostAsapNodeId,
    pub operator: ExecutableOperator,
    pub payload: ExecutableOperatorPayload,
    pub output_state: ExecutionDataState,
    pub output_schema: SummarySchema,
    pub guarantee: Option<ResultGuarantee>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
pub struct ExecutableDag {
    pub nodes: Vec<ExecutableDagNode>,
    pub edges: Vec<ExecutableDagEdge>,
    /// Semantic workload root. Physical query/precompute sinks are selected
    /// downstream by the control plane.
    pub root: PostAsapNodeId,
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
                k: *k,
                grouping: grouping.clone(),
                completeness: completeness.clone(),
            },
            SummaryExpr::ValueOperation {
                operation, timing, ..
            } => ExecutableOperatorPayload::Value {
                operation: operation.clone(),
                timing: *timing,
            },
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
                data_state: assigned_child_data_state(&node.expr, child),
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
    Ok(ExecutableDagCompilation {
        dag: ExecutableDag { nodes, edges, root },
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
