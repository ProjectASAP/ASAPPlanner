//! Runtime-neutral executable DAG contract shared by precompute and query engines.

use std::collections::HashMap;
use std::rc::Rc;

use super::{
    assigned_child_data_state, validate_execution_data_states, ExecutionDataState,
    ExecutionDataStateError, SummaryExpr, SummaryNode, SummarySchema,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Precompute,
    Query,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeRole {
    Input,
    Left,
    Right,
    CandidateMembership,
    AuthoritativeValues,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupingEdgeCompatibility {
    Identical,
    ConsumerCoarsensProducer,
    Incompatible,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowEdgeCompatibility {
    /// Both materializations must publish/consume at the same pane boundary.
    SameEvaluationBoundary,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecutableDagNode {
    pub id: u32,
    pub operator: ExecutableOperator,
    pub mode: ExecutionMode,
    pub output_schema: SummarySchema,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecutableDagEdge {
    pub producer: u32,
    pub consumer: u32,
    pub role: EdgeRole,
    pub intermediate_schema: SummarySchema,
    pub data_state: ExecutionDataState,
    pub grouping: GroupingEdgeCompatibility,
    pub window: WindowEdgeCompatibility,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecutableDag {
    pub nodes: Vec<ExecutableDagNode>,
    pub edges: Vec<ExecutableDagEdge>,
    pub query_sink: u32,
    pub precompute_sinks: Vec<u32>,
}

pub fn compile_executable_dag(
    root: &Rc<SummaryNode>,
) -> Result<ExecutableDag, ExecutionDataStateError> {
    let assignment = validate_execution_data_states(root)?;
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut ids = HashMap::new();

    fn visit(
        node: &Rc<SummaryNode>,
        assignment: &super::ExecutionDataStateAssignment,
        ids: &mut HashMap<*const SummaryNode, u32>,
        nodes: &mut Vec<ExecutableDagNode>,
        edges: &mut Vec<ExecutableDagEdge>,
    ) -> u32 {
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
            .map(|(c, r)| (visit(c, assignment, ids, nodes, edges), *c, *r))
            .collect();
        let id = nodes.len() as u32;
        let state = assignment
            .data_state_of(node)
            .expect("validated node has state");
        let mode = if state == ExecutionDataState::READ_ROWS {
            ExecutionMode::Query
        } else {
            ExecutionMode::Precompute
        };
        let operator = match &node.expr {
            SummaryExpr::KeepPreAsap(_) => ExecutableOperator::Fallback,
            SummaryExpr::BinaryOp { .. } => ExecutableOperator::Binary,
            SummaryExpr::CandidateTopK { .. } => ExecutableOperator::CandidateTopK,
            SummaryExpr::ValueOperation { .. } => ExecutableOperator::Value,
            SummaryExpr::SummaryAgg { .. } => ExecutableOperator::SummaryAgg,
            SummaryExpr::SummaryJoin { .. } => ExecutableOperator::SummaryJoin,
            SummaryExpr::SummarySubtract { .. } => ExecutableOperator::SummarySubtract,
            SummaryExpr::SummaryDelete { .. } => ExecutableOperator::SummaryDelete,
            SummaryExpr::SummaryEstimate { .. } => ExecutableOperator::SummaryEstimate,
            SummaryExpr::SummaryMerge { .. } => ExecutableOperator::SummaryMerge,
        };
        nodes.push(ExecutableDagNode {
            id,
            operator,
            mode,
            output_schema: node.schema.clone(),
        });
        ids.insert(Rc::as_ptr(node), id);
        for (producer, child, role) in child_ids {
            let precompute_dependency = nodes[producer as usize].mode == ExecutionMode::Precompute
                && nodes[id as usize].mode == ExecutionMode::Precompute;
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
                window: if precompute_dependency {
                    WindowEdgeCompatibility::SameEvaluationBoundary
                } else {
                    WindowEdgeCompatibility::NotApplicable
                },
            });
        }
        id
    }

    let query_sink = visit(root, &assignment, &mut ids, &mut nodes, &mut edges);
    let mut consumed_precompute = std::collections::HashSet::new();
    for edge in &edges {
        if nodes[edge.producer as usize].mode == ExecutionMode::Precompute
            && nodes[edge.consumer as usize].mode == ExecutionMode::Query
        {
            consumed_precompute.insert(edge.producer);
        }
    }
    let precompute_sinks = consumed_precompute.into_iter().collect();
    Ok(ExecutableDag {
        nodes,
        edges,
        query_sink,
        precompute_sinks,
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

        let dag = compile_executable_dag(&root).unwrap();
        assert_eq!(dag.query_sink, 3);
        assert_eq!(dag.nodes[1].mode, ExecutionMode::Precompute);
        assert_eq!(dag.nodes[2].mode, ExecutionMode::Precompute);
        let dependency = dag
            .edges
            .iter()
            .find(|e| e.producer == 1 && e.consumer == 2)
            .unwrap();
        assert_eq!(
            dependency.data_state,
            ExecutionDataState::MAINTENANCE_SUMMARY
        );
        assert_eq!(dependency.grouping, GroupingEdgeCompatibility::Identical);
        assert_eq!(
            dependency.window,
            WindowEdgeCompatibility::SameEvaluationBoundary
        );
        assert!(matches!(
            dependency.intermediate_schema.fields[0].dtype,
            SummaryFamilyType::ExactAggregate(ExactKind::Sum, _)
        ));
    }
}
