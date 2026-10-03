//! Logical ASAP DAG transport (planner-layering stage 1), with no execution timing assigned.
//!
//! This representation preserves operator semantics and summary state types.
//! Timing is derived from materialization during physical planning;
//! physical implementation, materialization and retention remain downstream.
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use super::physical_export::*;
pub use super::wire::NonASAPOpKind;
use super::wire::{grouping_compatibility, input_edges, payload_of};
pub use super::wire::{
    EdgeRole, GroupingEdgeCompatibility, LogicalASAPNodeId, LogicalASAPOperatorPayload,
    WirePredicate, WireProjectItem, WireScalarExpr, WireSortKey,
};
use super::{ASAPOp, Operator, OperatorNode, OperatorResultKind, QueryRoot, SchemaDerivationError};
use crate::post_asap::guarantee::ResultGuarantee;
use crate::pre_asap::{FieldDataType, Schema};

/// Independent envelope version: this replaces the older phase-assigned format.
pub const LOGICAL_ASAP_DAG_WIRE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalASAPDAGNode {
    pub id: LogicalASAPNodeId,
    pub payload: LogicalASAPOperatorPayload,
    pub result_kind: OperatorResultKind,
    pub output_schema: Schema,
    pub guarantee: Option<ResultGuarantee>,
    #[serde(default)]
    pub coverage: Option<super::summary_coverage::SummaryCoverage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalASAPDAGEdge {
    pub producer: LogicalASAPNodeId,
    pub consumer: LogicalASAPNodeId,
    pub role: EdgeRole,
    pub intermediate_schema: Schema,
    pub grouping: GroupingEdgeCompatibility,
}

/// Standalone scalars remain scalar roots rather than fabricated operator nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum LogicalASAPQueryRoot {
    Operator(LogicalASAPNodeId),
    Scalar(WireScalarExpr),
}
impl LogicalASAPQueryRoot {
    pub fn operator_refs(&self) -> Vec<LogicalASAPNodeId> {
        match self {
            Self::Operator(id) => vec![*id],
            Self::Scalar(expr) => expr.operator_refs(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalASAPDAG {
    pub nodes: Vec<LogicalASAPDAGNode>,
    pub edges: Vec<LogicalASAPDAGEdge>,
    /// One root per query of the batch, in workload order. Queries that share
    /// a sub-DAG reference the same exported nodes.
    pub roots: Vec<LogicalASAPQueryRoot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalASAPDAGDocument {
    pub schema_version: u32,
    pub dag: LogicalASAPDAG,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LogicalASAPDAGValidationError {
    #[error("unsupported logical ASAP DAG version {0}")]
    UnsupportedVersion(u32),
    #[error("duplicate logical node {0:?}")]
    DuplicateNode(LogicalASAPNodeId),
    #[error("missing logical node {0:?}")]
    MissingNode(LogicalASAPNodeId),
    #[error("edge schema differs from producer {0:?}")]
    EdgeSchemaMismatch(LogicalASAPNodeId),
    #[error("summary node {0:?} schema does not contain its declared family/grouping")]
    SummarySchemaMismatch(LogicalASAPNodeId),
    #[error("invalid summary coverage at {0:?}")]
    InvalidCoverage(LogicalASAPNodeId),
    #[error("logical ASAP DAG has no query roots")]
    NoRoots,
    #[error("logical ASAP DAG contains a cycle")]
    Cycle,
    #[error("unreachable logical node {0:?}")]
    UnreachableNode(LogicalASAPNodeId),
}

impl LogicalASAPDAGDocument {
    pub fn new(dag: LogicalASAPDAG) -> Self {
        Self {
            schema_version: LOGICAL_ASAP_DAG_WIRE_VERSION,
            dag,
        }
    }

    pub fn validate(&self) -> Result<(), LogicalASAPDAGValidationError> {
        if self.schema_version != LOGICAL_ASAP_DAG_WIRE_VERSION {
            return Err(LogicalASAPDAGValidationError::UnsupportedVersion(
                self.schema_version,
            ));
        }
        self.dag.validate()
    }
}

impl LogicalASAPDAG {
    /// Transport integrity checks; full operator/scalar typing is checked on
    /// the in-memory IR before compilation.
    pub fn validate(&self) -> Result<(), LogicalASAPDAGValidationError> {
        let mut nodes = HashMap::new();
        for node in &self.nodes {
            if nodes.insert(node.id, node).is_some() {
                return Err(LogicalASAPDAGValidationError::DuplicateNode(node.id));
            }
            if let Some(coverage) = &node.coverage {
                if node.result_kind != OperatorResultKind::State || coverage.validate().is_err() {
                    return Err(LogicalASAPDAGValidationError::InvalidCoverage(node.id));
                }
            }
            if let LogicalASAPOperatorPayload::SummaryAgg {
                family, grouping, ..
            } = &node.payload
            {
                if node.coverage.is_none() {
                    return Err(LogicalASAPDAGValidationError::InvalidCoverage(node.id));
                }
                if !node.output_schema.fields.iter().any(|field| &field.dtype == family)
                    || node.output_schema.fields.iter().any(|field| matches!(&field.dtype, FieldDataType::Sketch(_, actual) if actual != grouping)) {
                    return Err(LogicalASAPDAGValidationError::SummarySchemaMismatch(node.id));
                }
            }
        }
        if self.roots.is_empty() {
            return Err(LogicalASAPDAGValidationError::NoRoots);
        }
        let roots: Vec<_> = self
            .roots
            .iter()
            .flat_map(LogicalASAPQueryRoot::operator_refs)
            .collect();
        for root in &roots {
            if !nodes.contains_key(root) {
                return Err(LogicalASAPDAGValidationError::MissingNode(*root));
            }
        }
        let mut inputs: HashMap<_, Vec<_>> = HashMap::new();
        for edge in &self.edges {
            let producer = nodes
                .get(&edge.producer)
                .ok_or(LogicalASAPDAGValidationError::MissingNode(edge.producer))?;
            if !nodes.contains_key(&edge.consumer) {
                return Err(LogicalASAPDAGValidationError::MissingNode(edge.consumer));
            }
            if edge.intermediate_schema != producer.output_schema {
                return Err(LogicalASAPDAGValidationError::EdgeSchemaMismatch(
                    edge.producer,
                ));
            }
            inputs.entry(edge.consumer).or_default().push(edge.producer);
        }
        for node in &self.nodes {
            if matches!(node.payload, LogicalASAPOperatorPayload::SummaryMerge) {
                let coverage = inputs
                    .get(&node.id)
                    .into_iter()
                    .flatten()
                    .map(|id| {
                        nodes[id]
                            .coverage
                            .clone()
                            .ok_or(LogicalASAPDAGValidationError::InvalidCoverage(node.id))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let merged = super::summary_coverage::SummaryCoverage::merge_disjoint(&coverage)
                    .map_err(|_| LogicalASAPDAGValidationError::InvalidCoverage(node.id))?;
                if node.coverage.as_ref() != Some(&merged) {
                    return Err(LogicalASAPDAGValidationError::InvalidCoverage(node.id));
                }
            }
        }
        fn visit(
            id: LogicalASAPNodeId,
            inputs: &HashMap<LogicalASAPNodeId, Vec<LogicalASAPNodeId>>,
            active: &mut HashSet<LogicalASAPNodeId>,
            done: &mut HashSet<LogicalASAPNodeId>,
        ) -> Result<(), LogicalASAPDAGValidationError> {
            if done.contains(&id) {
                return Ok(());
            }
            if !active.insert(id) {
                return Err(LogicalASAPDAGValidationError::Cycle);
            }
            for child in inputs.get(&id).into_iter().flatten() {
                visit(*child, inputs, active, done)?;
            }
            active.remove(&id);
            done.insert(id);
            Ok(())
        }
        let mut done = HashSet::new();
        for root in roots {
            visit(root, &inputs, &mut HashSet::new(), &mut done)?;
        }
        if let Some(id) = nodes.keys().find(|id| !done.contains(id)) {
            return Err(LogicalASAPDAGValidationError::UnreachableNode(*id));
        }
        Ok(())
    }
}

/// Compiler-local identity mapping; IDs are local to this logical export.
#[derive(Debug, Clone)]
pub struct LogicalASAPNodeIdentityMap {
    nodes_by_id: Vec<Rc<OperatorNode>>,
}

impl LogicalASAPNodeIdentityMap {
    pub fn node_id(&self, node: &Rc<OperatorNode>) -> Option<LogicalASAPNodeId> {
        self.nodes_by_id
            .iter()
            .position(|candidate| Rc::ptr_eq(candidate, node))
            .map(|id| LogicalASAPNodeId(id as u32))
    }
    pub fn operator_node(&self, id: LogicalASAPNodeId) -> Option<&Rc<OperatorNode>> {
        self.nodes_by_id.get(id.0 as usize)
    }
}

#[derive(Debug, Clone)]
pub struct LogicalASAPDAGCompilation {
    pub dag: LogicalASAPDAG,
    pub node_ids: LogicalASAPNodeIdentityMap,
}

pub fn compile_logical_asap_dag(
    root: &Rc<OperatorNode>,
) -> Result<LogicalASAPDAG, SchemaDerivationError> {
    Ok(compile_logical_asap_dag_with_node_ids(root)?.dag)
}

pub fn compile_logical_asap_dag_with_node_ids(
    root: &Rc<OperatorNode>,
) -> Result<LogicalASAPDAGCompilation, SchemaDerivationError> {
    compile_logical_asap_query_with_node_ids(&QueryRoot::Operator(Rc::clone(root)))
}

pub fn compile_logical_asap_query(
    root: &QueryRoot,
) -> Result<LogicalASAPDAG, SchemaDerivationError> {
    Ok(compile_logical_asap_query_with_node_ids(root)?.dag)
}

pub fn compile_logical_asap_query_with_node_ids(
    root: &QueryRoot,
) -> Result<LogicalASAPDAGCompilation, SchemaDerivationError> {
    compile_logical_asap_workload_with_node_ids(std::slice::from_ref(root))
}

/// Export a batch of queries as one DAG with one root per query.
pub fn compile_logical_asap_workload(
    roots: &[QueryRoot],
) -> Result<LogicalASAPDAG, SchemaDerivationError> {
    Ok(compile_logical_asap_workload_with_node_ids(roots)?.dag)
}

pub fn compile_logical_asap_workload_with_node_ids(
    roots: &[QueryRoot],
) -> Result<LogicalASAPDAGCompilation, SchemaDerivationError> {
    let mut exporter = Exporter::default();
    let mut exported = Vec::with_capacity(roots.len());
    for root in roots {
        root.validate_structure()?;
        exported.push(match root {
            QueryRoot::Operator(node) => LogicalASAPQueryRoot::Operator(exporter.visit(node)),
            QueryRoot::Scalar(expr) => {
                for node in expr.operator_refs() {
                    exporter.visit(node);
                }
                LogicalASAPQueryRoot::Scalar(WireScalarExpr::from_expr(expr, &mut |n| {
                    exporter.ids[&Rc::as_ptr(n)]
                }))
            }
        });
    }
    let dag = LogicalASAPDAG {
        nodes: exporter.nodes,
        edges: exporter.edges,
        roots: exported,
    };
    Ok(LogicalASAPDAGCompilation {
        dag,
        node_ids: LogicalASAPNodeIdentityMap {
            nodes_by_id: exporter.nodes_by_id,
        },
    })
}

#[derive(Default)]
struct Exporter {
    ids: HashMap<*const OperatorNode, LogicalASAPNodeId>,
    nodes: Vec<LogicalASAPDAGNode>,
    edges: Vec<LogicalASAPDAGEdge>,
    nodes_by_id: Vec<Rc<OperatorNode>>,
}

impl Exporter {
    fn visit(&mut self, node: &Rc<OperatorNode>) -> LogicalASAPNodeId {
        if let Some(id) = self.ids.get(&Rc::as_ptr(node)) {
            return *id;
        }
        let mut producers = Vec::new();
        for (child, role) in input_edges(&node.operator) {
            producers.push((self.visit(child), child, role));
        }
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
                producers.push((self.visit(referenced), referenced, EdgeRole::ScalarRef));
            }
        }
        let id = LogicalASAPNodeId(self.nodes.len() as u32);
        let payload = payload_of(&node.operator, &mut |n| self.ids[&Rc::as_ptr(n)]);
        self.nodes.push(LogicalASAPDAGNode {
            id,
            payload,
            result_kind: node.result_kind,
            output_schema: node.schema.clone(),
            guarantee: node.guarantee.clone(),
            coverage: node.coverage.clone(),
        });
        self.nodes_by_id.push(Rc::clone(node));
        self.ids.insert(Rc::as_ptr(node), id);
        for (producer, child, role) in producers {
            self.edges.push(LogicalASAPDAGEdge {
                producer,
                consumer: id,
                role,
                intermediate_schema: child.schema.clone(),
                grouping: grouping_compatibility(&child.operator, &node.operator),
            });
        }
        id
    }
}
