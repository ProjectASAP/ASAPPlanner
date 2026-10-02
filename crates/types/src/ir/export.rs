//! Post-ASAP DAG export (wire version 7) over the unified operator IR.
//!
//! One exported node per operator — relational operators included — with
//! children as edges and no embedded sub-DAGs. The input must already be
//! timed ([`super::timing::apply_lifecycle_timings`]); export reads each
//! node's timing and does not re-run data-state validation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::asap::ASAPOp;
use super::node::{Operator, OperatorNode};
use super::non_asap::{BinaryOperator, NonASAPOp, TimeRangeKind};
use super::scalar::{ExprSemantics, Predicate, ProjectItem, ScalarExpr, SortKey};
use super::timing::data_state;
use crate::ir::operator_properties::{
    ConcatDiscriminatorKey, GroupKeys, InfoMatcher, JoinKind, Reduction, RelationalSetOpKind,
    SampleKind, Source, TimeShift, WindowFrame, WindowFuncKind,
};
use crate::post_asap::execution_data_state::{
    ExecutionDataState, ExecutionDataStateError, ExecutionTiming,
};
use crate::post_asap::guarantee::ResultGuarantee;
use crate::post_asap::maintained_population::{MaintainedPopulation, PopulationStatistic};
use crate::post_asap::sketch::{GroupingStrategy, SketchStatistic, SummaryUpdate};
use crate::pre_asap::agg_intent::AggIntent;
use crate::pre_asap::expr_ir::{ArithmeticOpKind, CompareOpKind, ScalarValue};
use crate::pre_asap::schema::{ColumnId, DataType, FieldDataType, Schema};

pub const POST_ASAP_DAG_WIRE_VERSION: u32 = 7;

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

/// Stable identity of a node within one exported post-ASAP semantic DAG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PostAsapNodeId(pub u32);

// ── Wire mirrors of the scalar language ──────────────────────────────────

/// [`ScalarExpr`] with every operator reference replaced by the id of the
/// exported node (connected to the owner by an [`EdgeRole::ScalarRef`] edge).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WireScalarExpr {
    Column(ColumnId),
    Literal(ScalarValue),
    Negative {
        expr: Box<WireScalarExpr>,
        semantics: ExprSemantics,
    },
    Compare {
        left: Box<WireScalarExpr>,
        op: CompareOpKind,
        right: Box<WireScalarExpr>,
        semantics: ExprSemantics,
    },
    BoolAnd(Vec<WireScalarExpr>),
    BoolOr(Vec<WireScalarExpr>),
    Not(Box<WireScalarExpr>),
    IsNull(Box<WireScalarExpr>),
    IsNotNull(Box<WireScalarExpr>),
    Cast {
        expr: Box<WireScalarExpr>,
        to: DataType,
        try_cast: bool,
    },
    InList {
        expr: Box<WireScalarExpr>,
        list: Vec<WireScalarExpr>,
        negated: bool,
    },
    FunctionCall {
        name: String,
        args: Vec<WireScalarExpr>,
    },
    Arithmetic {
        op: ArithmeticOpKind,
        left: Box<WireScalarExpr>,
        right: Box<WireScalarExpr>,
        semantics: ExprSemantics,
    },
    Case {
        operand: Option<Box<WireScalarExpr>>,
        branches: Vec<(WireScalarExpr, WireScalarExpr)>,
        else_expr: Option<Box<WireScalarExpr>>,
    },
    CurrentTimestamp,
    EvalTimestamp,
    PromqlScalarFromVector(PostAsapNodeId),
    ScalarSubquery(PostAsapNodeId),
    Exists {
        subquery: PostAsapNodeId,
        negated: bool,
    },
    InSubquery {
        expr: Box<WireScalarExpr>,
        subquery: PostAsapNodeId,
        negated: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WirePredicate(pub WireScalarExpr);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireProjectItem {
    pub alias: Option<String>,
    pub expr: WireScalarExpr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireSortKey {
    pub expr: WireScalarExpr,
    pub ascending: bool,
    pub nulls_first: bool,
}

impl WireScalarExpr {
    /// Mirror `expr`, resolving every operator reference through `id_of`.
    pub fn from_expr(
        expr: &ScalarExpr,
        id_of: &mut impl FnMut(&Rc<OperatorNode>) -> PostAsapNodeId,
    ) -> Self {
        fn boxed(
            e: &ScalarExpr,
            id_of: &mut impl FnMut(&Rc<OperatorNode>) -> PostAsapNodeId,
        ) -> Box<WireScalarExpr> {
            Box::new(WireScalarExpr::from_expr(e, id_of))
        }
        fn list(
            es: &[ScalarExpr],
            id_of: &mut impl FnMut(&Rc<OperatorNode>) -> PostAsapNodeId,
        ) -> Vec<WireScalarExpr> {
            es.iter()
                .map(|e| WireScalarExpr::from_expr(e, id_of))
                .collect()
        }
        match expr {
            ScalarExpr::Column(id) => WireScalarExpr::Column(*id),
            ScalarExpr::Literal(v) => WireScalarExpr::Literal(v.clone()),
            ScalarExpr::Negative { expr, semantics } => WireScalarExpr::Negative {
                expr: boxed(expr, id_of),
                semantics: *semantics,
            },
            ScalarExpr::Compare {
                left,
                op,
                right,
                semantics,
            } => WireScalarExpr::Compare {
                left: boxed(left, id_of),
                op: op.clone(),
                right: boxed(right, id_of),
                semantics: *semantics,
            },
            ScalarExpr::BoolAnd(parts) => WireScalarExpr::BoolAnd(list(parts, id_of)),
            ScalarExpr::BoolOr(parts) => WireScalarExpr::BoolOr(list(parts, id_of)),
            ScalarExpr::Not(e) => WireScalarExpr::Not(boxed(e, id_of)),
            ScalarExpr::IsNull(e) => WireScalarExpr::IsNull(boxed(e, id_of)),
            ScalarExpr::IsNotNull(e) => WireScalarExpr::IsNotNull(boxed(e, id_of)),
            ScalarExpr::Cast { expr, to, try_cast } => WireScalarExpr::Cast {
                expr: boxed(expr, id_of),
                to: to.clone(),
                try_cast: *try_cast,
            },
            ScalarExpr::InList {
                expr,
                list: items,
                negated,
            } => WireScalarExpr::InList {
                expr: boxed(expr, id_of),
                list: list(items, id_of),
                negated: *negated,
            },
            ScalarExpr::FunctionCall { name, args } => WireScalarExpr::FunctionCall {
                name: name.clone(),
                args: list(args, id_of),
            },
            ScalarExpr::Arithmetic {
                op,
                left,
                right,
                semantics,
            } => WireScalarExpr::Arithmetic {
                op: op.clone(),
                left: boxed(left, id_of),
                right: boxed(right, id_of),
                semantics: *semantics,
            },
            ScalarExpr::Case {
                operand,
                branches,
                else_expr,
            } => WireScalarExpr::Case {
                operand: operand.as_ref().map(|e| boxed(e, id_of)),
                branches: branches
                    .iter()
                    .map(|(w, t)| (Self::from_expr(w, id_of), Self::from_expr(t, id_of)))
                    .collect(),
                else_expr: else_expr.as_ref().map(|e| boxed(e, id_of)),
            },
            ScalarExpr::CurrentTimestamp => WireScalarExpr::CurrentTimestamp,
            ScalarExpr::EvalTimestamp => WireScalarExpr::EvalTimestamp,
            ScalarExpr::PromqlScalarFromVector(node) => {
                WireScalarExpr::PromqlScalarFromVector(id_of(node))
            }
            ScalarExpr::ScalarSubquery(node) => WireScalarExpr::ScalarSubquery(id_of(node)),
            ScalarExpr::Exists { subquery, negated } => WireScalarExpr::Exists {
                subquery: id_of(subquery),
                negated: *negated,
            },
            ScalarExpr::InSubquery {
                expr,
                subquery,
                negated,
            } => WireScalarExpr::InSubquery {
                expr: boxed(expr, id_of),
                subquery: id_of(subquery),
                negated: *negated,
            },
        }
    }
}

impl WirePredicate {
    fn from_pred(
        p: &Predicate,
        id_of: &mut impl FnMut(&Rc<OperatorNode>) -> PostAsapNodeId,
    ) -> Self {
        WirePredicate(WireScalarExpr::from_expr(&p.0, id_of))
    }
}

impl WireSortKey {
    fn from_keys(
        keys: &[SortKey],
        id_of: &mut impl FnMut(&Rc<OperatorNode>) -> PostAsapNodeId,
    ) -> Vec<Self> {
        keys.iter()
            .map(|k| WireSortKey {
                expr: WireScalarExpr::from_expr(&k.expr, id_of),
                ascending: k.ascending,
                nulls_first: k.nulls_first,
            })
            .collect()
    }
}

// ── Wire mirror of the non-ASAP operator vocabulary ──────────────────────

/// [`NonASAPOp`] without its child fields (children are edges) and with
/// every scalar expression mirrored as [`WireScalarExpr`]. Fields named
/// `kind` in the IR are renamed so they do not collide with the variant tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NonASAPOpKind {
    Scan {
        source: Source,
        #[serde(default)]
        predicates: Vec<WirePredicate>,
        schema: Schema,
    },
    Values {
        rows: Vec<Vec<WireScalarExpr>>,
        schema: Schema,
    },
    Filter {
        pred: WirePredicate,
    },
    Project {
        cols: Vec<WireProjectItem>,
        #[serde(default)]
        qualifier: Option<String>,
    },
    Aggregate {
        reduction: Reduction,
        measures: Vec<AggIntent>,
        #[serde(default)]
        output_names: Vec<String>,
        #[serde(default)]
        filters: Vec<Option<WirePredicate>>,
        #[serde(default)]
        having: Option<WirePredicate>,
    },
    Join {
        join_kind: JoinKind,
        pred: WirePredicate,
    },
    SetOp {
        set_kind: RelationalSetOpKind,
        all: bool,
    },
    Concat {
        #[serde(default)]
        discriminator_unique_key: Option<ConcatDiscriminatorKey>,
    },
    Dedup {
        cols: Vec<ColumnId>,
    },
    Sort {
        keys: Vec<WireSortKey>,
        #[serde(default)]
        partition_by: GroupKeys,
    },
    Limit {
        n: Option<usize>,
        offset: usize,
        #[serde(default)]
        partition_by: GroupKeys,
    },
    BinaryOp {
        operator: BinaryOperator,
        #[serde(default)]
        return_bool: bool,
    },
    #[serde(rename = "sql_window_func")]
    SQLWindowFunc {
        func: WindowFuncKind,
        args: Vec<WireScalarExpr>,
        partition_by: GroupKeys,
        order_by: Vec<WireSortKey>,
        #[serde(default)]
        frame: Option<WindowFrame>,
        output_name: String,
    },
    TimeRange {
        range: Duration,
        range_kind: TimeRangeKind,
    },
    TimeShift {
        shift: TimeShift,
    },
    PromqlVectorFromScalar {
        expr: WireScalarExpr,
    },
    PromqlRelabel {
        dst: String,
        value: WireScalarExpr,
    },
    PromqlInfoEnrich {
        #[serde(default)]
        selector: Vec<InfoMatcher>,
    },
    PromqlSeriesSample {
        #[serde(default)]
        by: GroupKeys,
        sample_kind: SampleKind,
    },
    PromqlSubquery {
        range: Duration,
        #[serde(default)]
        resolution: Option<Duration>,
    },
}

impl NonASAPOpKind {
    /// Mirror `op`, resolving every operator node its scalar expressions
    /// reference through `id_of`.
    pub fn from_op(
        op: &NonASAPOp,
        id_of: &mut impl FnMut(&Rc<OperatorNode>) -> PostAsapNodeId,
    ) -> Self {
        use NonASAPOp as Op;
        match op {
            Op::Scan {
                source,
                predicates,
                schema,
            } => NonASAPOpKind::Scan {
                source: source.clone(),
                predicates: predicates
                    .iter()
                    .map(|p| WirePredicate::from_pred(p, id_of))
                    .collect(),
                schema: schema.clone(),
            },
            Op::Values { rows, schema } => NonASAPOpKind::Values {
                rows: rows
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(|e| WireScalarExpr::from_expr(e, id_of))
                            .collect()
                    })
                    .collect(),
                schema: schema.clone(),
            },
            Op::Filter { pred, .. } => NonASAPOpKind::Filter {
                pred: WirePredicate::from_pred(pred, id_of),
            },
            Op::Project {
                cols, qualifier, ..
            } => NonASAPOpKind::Project {
                cols: cols
                    .iter()
                    .map(|ProjectItem { alias, expr }| WireProjectItem {
                        alias: alias.clone(),
                        expr: WireScalarExpr::from_expr(expr, id_of),
                    })
                    .collect(),
                qualifier: qualifier.clone(),
            },
            Op::Aggregate {
                reduction,
                measures,
                output_names,
                filters,
                having,
                ..
            } => NonASAPOpKind::Aggregate {
                reduction: reduction.clone(),
                measures: measures.clone(),
                output_names: output_names.clone(),
                filters: filters
                    .iter()
                    .map(|p| p.as_ref().map(|p| WirePredicate::from_pred(p, id_of)))
                    .collect(),
                having: having.as_ref().map(|p| WirePredicate::from_pred(p, id_of)),
            },
            Op::Join { kind, pred, .. } => NonASAPOpKind::Join {
                join_kind: kind.clone(),
                pred: WirePredicate::from_pred(pred, id_of),
            },
            Op::SetOp { kind, all, .. } => NonASAPOpKind::SetOp {
                set_kind: kind.clone(),
                all: *all,
            },
            Op::Concat {
                discriminator_unique_key,
                ..
            } => NonASAPOpKind::Concat {
                discriminator_unique_key: discriminator_unique_key.clone(),
            },
            Op::Dedup { cols, .. } => NonASAPOpKind::Dedup { cols: cols.clone() },
            Op::Sort {
                keys, partition_by, ..
            } => NonASAPOpKind::Sort {
                keys: WireSortKey::from_keys(keys, id_of),
                partition_by: partition_by.clone(),
            },
            Op::Limit {
                n,
                offset,
                partition_by,
                ..
            } => NonASAPOpKind::Limit {
                n: *n,
                offset: *offset,
                partition_by: partition_by.clone(),
            },
            Op::BinaryOp {
                operator,
                return_bool,
                ..
            } => NonASAPOpKind::BinaryOp {
                operator: operator.clone(),
                return_bool: *return_bool,
            },
            Op::SQLWindowFunc {
                func,
                args,
                partition_by,
                order_by,
                frame,
                output_name,
                ..
            } => NonASAPOpKind::SQLWindowFunc {
                func: func.clone(),
                args: args
                    .iter()
                    .map(|e| WireScalarExpr::from_expr(e, id_of))
                    .collect(),
                partition_by: partition_by.clone(),
                order_by: WireSortKey::from_keys(order_by, id_of),
                frame: frame.clone(),
                output_name: output_name.clone(),
            },
            Op::TimeRange { range, kind, .. } => NonASAPOpKind::TimeRange {
                range: *range,
                range_kind: *kind,
            },
            Op::TimeShift { shift, .. } => NonASAPOpKind::TimeShift { shift: *shift },
            Op::PromqlVectorFromScalar(e) => NonASAPOpKind::PromqlVectorFromScalar {
                expr: WireScalarExpr::from_expr(e, id_of),
            },
            Op::PromqlRelabel { dst, value, .. } => NonASAPOpKind::PromqlRelabel {
                dst: dst.clone(),
                value: WireScalarExpr::from_expr(value, id_of),
            },
            Op::PromqlInfoEnrich { selector, .. } => NonASAPOpKind::PromqlInfoEnrich {
                selector: selector.clone(),
            },
            Op::PromqlSeriesSample { by, kind, .. } => NonASAPOpKind::PromqlSeriesSample {
                by: by.clone(),
                sample_kind: *kind,
            },
            Op::PromqlSubquery {
                range, resolution, ..
            } => NonASAPOpKind::PromqlSubquery {
                range: *range,
                resolution: *resolution,
            },
        }
    }
}

// ── The exported DAG ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PostAsapOperatorPayload {
    Relational {
        operator: NonASAPOpKind,
    },
    SummaryAgg {
        family: FieldDataType,
        input: SummaryUpdate,
        reduction: Reduction,
        grouping: GroupingStrategy,
        #[serde(default)]
        filter: Option<WirePredicate>,
    },
    SummaryEstimate {
        query: SketchStatistic,
    },
    FinalizeExactAccumulator,
    MaintainPopulation {
        population: MaintainedPopulation<OperatorNode>,
    },
    EvaluatePopulation {
        evaluation: PopulationStatistic,
    },
    SummaryMerge,
    SummarySubtract,
    SummaryDelete {
        key: ColumnId,
    },
    SummaryJoin {
        key: ColumnId,
        family: FieldDataType,
    },
    Extension {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostAsapDAGNode {
    pub id: PostAsapNodeId,
    /// The payload variant is the sole operator identity (`payload.kind` in JSON).
    pub payload: PostAsapOperatorPayload,
    /// Phase is a placement choice for every operator, independent of payload kind.
    pub output_state: ExecutionDataState,
    pub output_schema: Schema,
    pub guarantee: Option<ResultGuarantee>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostAsapDAGEdge {
    pub producer: PostAsapNodeId,
    pub consumer: PostAsapNodeId,
    pub role: EdgeRole,
    pub intermediate_schema: Schema,
    pub data_state: ExecutionDataState,
    pub grouping: GroupingEdgeCompatibility,
    pub window: WindowEdgeCompatibility,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostAsapDAG {
    pub nodes: Vec<PostAsapDAGNode>,
    pub edges: Vec<PostAsapDAGEdge>,
    /// Semantic workload root. Physical query/precompute sinks are selected
    /// downstream by the control plane.
    pub root: PostAsapNodeId,
}

/// Versioned transport envelope for a post-ASAP semantic DAG.
///
/// Process boundaries exchange this envelope and call [`Self::validate`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostAsapDAGDocument {
    pub schema_version: u32,
    pub dag: PostAsapDAG,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PostAsapDAGValidationError {
    #[error("phase assignment must name every DAG node exactly once")]
    IncompletePhaseAssignment,
    #[error("ingestion node {consumer:?} depends on query node {producer:?}")]
    QueryDependencyInIngestion {
        producer: PostAsapNodeId,
        consumer: PostAsapNodeId,
    },
    #[error("unsupported post-ASAP DAG schema version {0}")]
    UnsupportedVersion(u32),
    #[error("duplicate post-ASAP node id {0:?}")]
    DuplicateNodeId(PostAsapNodeId),
    #[error("post-ASAP DAG root {0:?} does not name a node")]
    MissingRoot(PostAsapNodeId),
    #[error("edge endpoint {0:?} does not name a node")]
    MissingEdgeEndpoint(PostAsapNodeId),
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

impl PostAsapDAGDocument {
    pub fn new(dag: PostAsapDAG) -> Self {
        Self {
            schema_version: POST_ASAP_DAG_WIRE_VERSION,
            dag,
        }
    }

    pub fn validate(&self) -> Result<(), PostAsapDAGValidationError> {
        if self.schema_version != POST_ASAP_DAG_WIRE_VERSION {
            return Err(PostAsapDAGValidationError::UnsupportedVersion(
                self.schema_version,
            ));
        }
        self.dag.validate()
    }
}

impl PostAsapDAG {
    /// Assign execution phases without changing operator semantics. Phase choices
    /// do not prove deployment support: callers must bind concrete implementations
    /// and storage boundaries before installing this plan.
    pub fn with_execution_phases(
        &self,
        phases: &BTreeMap<PostAsapNodeId, ExecutionTiming>,
    ) -> Result<Self, PostAsapDAGValidationError> {
        self.validate()?;
        if phases.len() != self.nodes.len()
            || self.nodes.iter().any(|node| !phases.contains_key(&node.id))
        {
            return Err(PostAsapDAGValidationError::IncompletePhaseAssignment);
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

    pub fn validate(&self) -> Result<(), PostAsapDAGValidationError> {
        let mut nodes = HashMap::new();
        for node in &self.nodes {
            if nodes.insert(node.id, node).is_some() {
                return Err(PostAsapDAGValidationError::DuplicateNodeId(node.id));
            }
            if let PostAsapOperatorPayload::SummaryAgg {
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
                            return Err(PostAsapDAGValidationError::SummaryGroupingMismatch {
                                node: node.id,
                            });
                        }
                    }
                }
                if !found_family {
                    return Err(PostAsapDAGValidationError::SummaryFamilySchemaMismatch {
                        node: node.id,
                    });
                }
            }
        }
        if !nodes.contains_key(&self.root) {
            return Err(PostAsapDAGValidationError::MissingRoot(self.root));
        }
        let mut children: HashMap<PostAsapNodeId, Vec<PostAsapNodeId>> = HashMap::new();
        for edge in &self.edges {
            let producer = nodes.get(&edge.producer).ok_or(
                PostAsapDAGValidationError::MissingEdgeEndpoint(edge.producer),
            )?;
            if !nodes.contains_key(&edge.consumer) {
                return Err(PostAsapDAGValidationError::MissingEdgeEndpoint(
                    edge.consumer,
                ));
            }
            if producer.output_state.timing == ExecutionTiming::QueryTime
                && nodes[&edge.consumer].output_state.timing == ExecutionTiming::IngestionTime
            {
                return Err(PostAsapDAGValidationError::QueryDependencyInIngestion {
                    producer: edge.producer,
                    consumer: edge.consumer,
                });
            }
            if edge.intermediate_schema != producer.output_schema {
                return Err(PostAsapDAGValidationError::EdgeSchemaMismatch {
                    producer: edge.producer,
                    consumer: edge.consumer,
                });
            }
            if edge.data_state != producer.output_state {
                return Err(PostAsapDAGValidationError::EdgeDataStateMismatch {
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
            return Err(PostAsapDAGValidationError::Cycle);
        }
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
        let mut reachable = HashSet::new();
        mark(self.root, &children, &mut reachable);
        if let Some(id) = nodes.keys().find(|id| !reachable.contains(id)) {
            return Err(PostAsapDAGValidationError::UnreachableNode(*id));
        }
        Ok(())
    }
}

// ── Compilation from the IR ──────────────────────────────────────────────

/// Compiler-local identity assignment. It deliberately retains `Rc` handles
/// and is not serialized; deployed artifacts persist the post-ASAP node ID
/// together with their physical materialization/query IDs.
#[derive(Debug, Clone)]
pub struct PostAsapNodeIdentityMap {
    nodes_by_id: Vec<Rc<OperatorNode>>,
}

impl PostAsapNodeIdentityMap {
    pub fn node_id(&self, node: &Rc<OperatorNode>) -> Option<PostAsapNodeId> {
        self.nodes_by_id
            .iter()
            .position(|candidate| Rc::ptr_eq(candidate, node))
            .map(|id| PostAsapNodeId(id as u32))
    }

    pub fn operator_node(&self, id: PostAsapNodeId) -> Option<&Rc<OperatorNode>> {
        self.nodes_by_id.get(id.0 as usize)
    }
}

#[derive(Debug, Clone)]
pub struct PostAsapDAGCompilation {
    pub dag: PostAsapDAG,
    pub node_ids: PostAsapNodeIdentityMap,
}

pub fn compile_post_asap_dag(
    root: &Rc<OperatorNode>,
) -> Result<PostAsapDAG, ExecutionDataStateError> {
    Ok(compile_post_asap_dag_with_node_ids(root)?.dag)
}

/// Export the timed DAG below `root`. Every reachable node must carry a
/// timing (see [`super::timing::apply_lifecycle_timings`]); the data-state
/// rules were checked by that pass and are not re-run here.
pub fn compile_post_asap_dag_with_node_ids(
    root: &Rc<OperatorNode>,
) -> Result<PostAsapDAGCompilation, ExecutionDataStateError> {
    let mut exporter = Exporter::default();
    let root = exporter.visit(root)?;
    let dag = PostAsapDAG {
        nodes: exporter.nodes,
        edges: exporter.edges,
        root,
    };
    dag.validate()
        .expect("compiler emits a valid post-ASAP DAG");
    Ok(PostAsapDAGCompilation {
        dag,
        node_ids: PostAsapNodeIdentityMap {
            nodes_by_id: exporter.nodes_by_id,
        },
    })
}

#[derive(Default)]
struct Exporter {
    ids: HashMap<*const OperatorNode, PostAsapNodeId>,
    nodes: Vec<PostAsapDAGNode>,
    edges: Vec<PostAsapDAGEdge>,
    nodes_by_id: Vec<Rc<OperatorNode>>,
}

impl Exporter {
    fn visit(
        &mut self,
        node: &Rc<OperatorNode>,
    ) -> Result<PostAsapNodeId, ExecutionDataStateError> {
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
        let id = PostAsapNodeId(self.nodes.len() as u32);
        let payload = {
            let ids = &self.ids;
            let mut id_of = |n: &Rc<OperatorNode>| ids[&Rc::as_ptr(n)];
            payload_of(&node.operator, &mut id_of)
        };
        self.nodes.push(PostAsapDAGNode {
            id,
            payload,
            output_state,
            output_schema: node.schema.clone(),
            guarantee: node.guarantee.clone(),
        });
        self.nodes_by_id.push(Rc::clone(node));
        self.ids.insert(Rc::as_ptr(node), id);
        for (producer, child, role) in producers {
            let producer_state = self.nodes[producer.0 as usize].output_state;
            let maintenance_dependency = producer_state.timing == ExecutionTiming::IngestionTime
                && output_state.timing == ExecutionTiming::IngestionTime;
            self.edges.push(PostAsapDAGEdge {
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

/// The operator's own inputs with their edge roles, in field order.
fn input_edges(operator: &Operator) -> Vec<(&Rc<OperatorNode>, EdgeRole)> {
    match operator {
        Operator::NonASAP(op) => match op {
            NonASAPOp::Join { left, right, .. }
            | NonASAPOp::SetOp { left, right, .. }
            | NonASAPOp::BinaryOp {
                lhs: left,
                rhs: right,
                ..
            } => vec![(left, EdgeRole::Left), (right, EdgeRole::Right)],
            NonASAPOp::Concat { children, .. } => {
                children.iter().map(|c| (c, EdgeRole::Input)).collect()
            }
            NonASAPOp::Filter { child, .. }
            | NonASAPOp::Project { child, .. }
            | NonASAPOp::Aggregate { child, .. }
            | NonASAPOp::Dedup { child, .. }
            | NonASAPOp::Sort { child, .. }
            | NonASAPOp::Limit { child, .. }
            | NonASAPOp::SQLWindowFunc { child, .. }
            | NonASAPOp::TimeRange { child, .. }
            | NonASAPOp::TimeShift { child, .. }
            | NonASAPOp::PromqlRelabel { child, .. }
            | NonASAPOp::PromqlInfoEnrich { child, .. }
            | NonASAPOp::PromqlSeriesSample { child, .. }
            | NonASAPOp::PromqlSubquery { child, .. } => vec![(child, EdgeRole::Input)],
            NonASAPOp::Scan { .. }
            | NonASAPOp::Values { .. }
            | NonASAPOp::PromqlVectorFromScalar(_) => vec![],
        },
        Operator::ASAP(op) => match op {
            ASAPOp::SummarySubtract { left, right }
            | ASAPOp::SummaryJoin {
                outer: left,
                inner: right,
                ..
            } => vec![(left, EdgeRole::Left), (right, EdgeRole::Right)],
            ASAPOp::SummaryMerge { children } => {
                children.iter().map(|c| (c, EdgeRole::Input)).collect()
            }
            ASAPOp::SummaryAgg { child, .. }
            | ASAPOp::FinalizeExactAccumulator { child }
            | ASAPOp::MaintainPopulation { child, .. }
            | ASAPOp::EvaluatePopulation { child, .. }
            | ASAPOp::Extension { child, .. } => vec![(child, EdgeRole::Input)],
            ASAPOp::SummaryEstimate { summary_input, .. }
            | ASAPOp::SummaryDelete { summary_input, .. } => {
                vec![(summary_input, EdgeRole::Input)]
            }
        },
    }
}

fn payload_of(
    operator: &Operator,
    id_of: &mut impl FnMut(&Rc<OperatorNode>) -> PostAsapNodeId,
) -> PostAsapOperatorPayload {
    match operator {
        Operator::NonASAP(op) => PostAsapOperatorPayload::Relational {
            operator: NonASAPOpKind::from_op(op, id_of),
        },
        Operator::ASAP(op) => match op {
            ASAPOp::SummaryAgg {
                family,
                input,
                reduction,
                grouping,
                filter,
                ..
            } => PostAsapOperatorPayload::SummaryAgg {
                family: family.clone(),
                input: input.clone(),
                reduction: reduction.clone(),
                grouping: grouping.clone(),
                filter: filter.as_ref().map(|p| WirePredicate::from_pred(p, id_of)),
            },
            ASAPOp::SummaryEstimate { query, .. } => PostAsapOperatorPayload::SummaryEstimate {
                query: query.clone(),
            },
            ASAPOp::FinalizeExactAccumulator { .. } => {
                PostAsapOperatorPayload::FinalizeExactAccumulator
            }
            ASAPOp::MaintainPopulation { population, .. } => {
                PostAsapOperatorPayload::MaintainPopulation {
                    population: population.clone(),
                }
            }
            ASAPOp::EvaluatePopulation { evaluation, .. } => {
                PostAsapOperatorPayload::EvaluatePopulation {
                    evaluation: evaluation.clone(),
                }
            }
            ASAPOp::SummaryMerge { .. } => PostAsapOperatorPayload::SummaryMerge,
            ASAPOp::SummarySubtract { .. } => PostAsapOperatorPayload::SummarySubtract,
            ASAPOp::SummaryDelete { key, .. } => {
                PostAsapOperatorPayload::SummaryDelete { key: *key }
            }
            ASAPOp::SummaryJoin { key, family, .. } => PostAsapOperatorPayload::SummaryJoin {
                key: *key,
                family: family.clone(),
            },
            ASAPOp::Extension { name, .. } => {
                PostAsapOperatorPayload::Extension { name: name.clone() }
            }
        },
    }
}

/// Grouping compatibility between two `SummaryAgg`s by their reductions.
fn grouping_compatibility(producer: &Operator, consumer: &Operator) -> GroupingEdgeCompatibility {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::timing::{apply_lifecycle_timings, LifecycleAssignment, TimingMemo};
    use crate::post_asap::execution_data_state::DataPrimitive;
    use crate::post_asap::maintained_population::{CurrentSeriesInput, PopulationInput};
    use crate::post_asap::sketch::{ExactKind, ExactParams};
    use crate::pre_asap::schema::Field;
    use crate::pre_asap::ColumnRef;

    fn timed(root: &Rc<OperatorNode>) -> Rc<OperatorNode> {
        apply_lifecycle_timings(
            root,
            &LifecycleAssignment::default_maintained(),
            &mut TimingMemo::new(),
        )
        .expect("timed")
    }

    fn scan(fields: Vec<Field>) -> Rc<OperatorNode> {
        OperatorNode::new_shared(crate::ir::Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::new(fields),
        }))
        .unwrap()
    }

    fn value_scan() -> Rc<OperatorNode> {
        scan(vec![Field::plain("value", DataType::Float64, false)])
    }

    fn true_pred() -> Predicate {
        Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true)))
    }

    fn sum_agg(child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
        Rc::new(OperatorNode::with_schema(
            Operator::ASAP(ASAPOp::SummaryAgg {
                child,
                family: family.clone(),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::by(vec![]),
                grouping: GroupingStrategy::default(),
                filter: None,
            }),
            Schema::lifted(vec![Field::new("value", family, false)], None),
        ))
    }

    fn finalize(child: Rc<OperatorNode>) -> Rc<OperatorNode> {
        Rc::new(OperatorNode::with_schema(
            Operator::ASAP(ASAPOp::FinalizeExactAccumulator { child }),
            Schema::lifted(vec![Field::plain("value", DataType::Float64, false)], None),
        ))
    }

    fn relational_count(dag: &PostAsapDAG) -> usize {
        dag.nodes
            .iter()
            .filter(|n| matches!(n.payload, PostAsapOperatorPayload::Relational { .. }))
            .count()
    }

    #[test]
    fn every_physical_payload_can_be_assigned_either_phase() {
        let family = FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum);
        let payloads = vec![
            PostAsapOperatorPayload::Relational {
                operator: NonASAPOpKind::Limit {
                    n: Some(1),
                    offset: 0,
                    partition_by: GroupKeys::none(),
                },
            },
            PostAsapOperatorPayload::SummaryAgg {
                family: family.clone(),
                input: SummaryUpdate::column(ColumnRef::SampleValue),
                reduction: Reduction::by(vec![]),
                grouping: GroupingStrategy::default(),
                filter: None,
            },
            PostAsapOperatorPayload::SummaryEstimate {
                query: SketchStatistic::Cardinality,
            },
            PostAsapOperatorPayload::FinalizeExactAccumulator,
            PostAsapOperatorPayload::MaintainPopulation {
                population: MaintainedPopulation {
                    input: PopulationInput::CurrentSeries(CurrentSeriesInput {
                        metric: "m".into(),
                        matchers: vec![],
                        grouping: vec![],
                        without: false,
                        lookback_ms: 300_000,
                    }),
                    max_k: 10,
                    quantiles: false,
                },
            },
            PostAsapOperatorPayload::EvaluatePopulation {
                evaluation: PopulationStatistic::Count,
            },
            PostAsapOperatorPayload::SummaryMerge,
            PostAsapOperatorPayload::SummarySubtract,
            PostAsapOperatorPayload::SummaryDelete { key: 0 },
            PostAsapOperatorPayload::SummaryJoin {
                key: 0,
                family: family.clone(),
            },
            PostAsapOperatorPayload::Extension { name: "ext".into() },
        ];
        for payload in payloads {
            // This checks physical identity and placement, not kernel availability.
            let primitive = match &payload {
                PostAsapOperatorPayload::Relational { .. }
                | PostAsapOperatorPayload::SummaryEstimate { .. }
                | PostAsapOperatorPayload::FinalizeExactAccumulator
                | PostAsapOperatorPayload::EvaluatePopulation { .. }
                | PostAsapOperatorPayload::Extension { .. } => DataPrimitive::Raw,
                PostAsapOperatorPayload::SummaryAgg { .. }
                | PostAsapOperatorPayload::MaintainPopulation { .. }
                | PostAsapOperatorPayload::SummaryJoin { .. }
                | PostAsapOperatorPayload::SummarySubtract
                | PostAsapOperatorPayload::SummaryDelete { .. }
                | PostAsapOperatorPayload::SummaryMerge => DataPrimitive::SummaryState,
            };
            let dag = PostAsapDAG {
                root: PostAsapNodeId(0),
                edges: vec![],
                nodes: vec![PostAsapDAGNode {
                    id: PostAsapNodeId(0),
                    payload: payload.clone(),
                    output_state: ExecutionDataState {
                        timing: ExecutionTiming::QueryTime,
                        primitive,
                    },
                    output_schema: Schema::lifted(
                        vec![Field::new("value", family.clone(), false)],
                        None,
                    ),
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
                assert_eq!(serde_json::from_value::<PostAsapDAG>(wire).unwrap(), placed);
            }
            assert!(dag.with_execution_phases(&BTreeMap::new()).is_err());
        }
    }

    #[test]
    fn phase_assignment_updates_edges_and_rejects_query_dependencies_in_ingestion() {
        let schema = Schema::lifted(vec![], None);
        let nodes = [0, 1]
            .into_iter()
            .map(|id| PostAsapDAGNode {
                id: PostAsapNodeId(id),
                payload: PostAsapOperatorPayload::Relational {
                    operator: NonASAPOpKind::Scan {
                        source: Source::TimeSeries { metric: "m".into() },
                        predicates: vec![],
                        schema: schema.clone(),
                    },
                },
                output_state: ExecutionDataState::QUERY_ROWS,
                output_schema: schema.clone(),
                guarantee: None,
            })
            .collect();
        let dag = PostAsapDAG {
            nodes,
            root: PostAsapNodeId(1),
            edges: vec![PostAsapDAGEdge {
                producer: PostAsapNodeId(0),
                consumer: PostAsapNodeId(1),
                role: EdgeRole::Input,
                intermediate_schema: schema,
                data_state: ExecutionDataState::QUERY_ROWS,
                grouping: GroupingEdgeCompatibility::NotApplicable,
                window: WindowEdgeCompatibility::NotApplicable,
            }],
        };
        let placed = dag
            .with_execution_phases(&BTreeMap::from([
                (PostAsapNodeId(0), ExecutionTiming::IngestionTime),
                (PostAsapNodeId(1), ExecutionTiming::QueryTime),
            ]))
            .unwrap();
        assert_eq!(
            placed.edges[0].data_state.timing,
            ExecutionTiming::IngestionTime
        );
        assert_eq!(dag.edges[0].data_state.timing, ExecutionTiming::QueryTime);
        assert!(matches!(
            dag.with_execution_phases(&BTreeMap::from([
                (PostAsapNodeId(0), ExecutionTiming::QueryTime),
                (PostAsapNodeId(1), ExecutionTiming::IngestionTime),
            ])),
            Err(PostAsapDAGValidationError::QueryDependencyInIngestion { .. })
        ));
    }

    #[test]
    fn exports_summary_over_summary_as_typed_precompute_edges() {
        let inner = sum_agg(value_scan());
        let outer = sum_agg(Rc::clone(&inner));
        let root = timed(&finalize(outer));
        let inner = Rc::clone(root.children()[0].children()[0]);

        let compiled = compile_post_asap_dag_with_node_ids(&root).unwrap();
        assert_eq!(compiled.node_ids.node_id(&root), Some(PostAsapNodeId(3)));
        assert!(Rc::ptr_eq(
            compiled.node_ids.operator_node(PostAsapNodeId(1)).unwrap(),
            &inner
        ));
        let dag = compiled.dag;
        assert_eq!(dag.root, PostAsapNodeId(3));
        assert_eq!(
            dag.nodes[0].output_state,
            ExecutionDataState::INGESTION_ROWS
        );
        assert_eq!(
            dag.nodes[1].output_state,
            ExecutionDataState::INGESTION_SUMMARY
        );
        assert_eq!(
            dag.nodes[2].output_state,
            ExecutionDataState::INGESTION_SUMMARY
        );
        assert_eq!(dag.nodes[3].output_state, ExecutionDataState::QUERY_ROWS);
        let dependency = dag
            .edges
            .iter()
            .find(|e| e.producer == PostAsapNodeId(1) && e.consumer == PostAsapNodeId(2))
            .unwrap();
        assert_eq!(dependency.role, EdgeRole::Input);
        assert_eq!(dependency.data_state, ExecutionDataState::INGESTION_SUMMARY);
        assert_eq!(dependency.grouping, GroupingEdgeCompatibility::Identical);
        assert_eq!(
            dependency.window,
            WindowEdgeCompatibility::RequiresAlignedPanePhaseOrExactWindowEdgeResidual
        );
        assert!(matches!(
            dependency.intermediate_schema.fields[0].dtype,
            FieldDataType::ExactAggregate(ExactKind::Sum, _)
        ));
        let encoded = serde_json::to_string(&dag).expect("serialize post-ASAP DAG");
        let decoded: PostAsapDAG =
            serde_json::from_str(&encoded).expect("deserialize post-ASAP DAG");
        assert_eq!(decoded, dag);
        let document = PostAsapDAGDocument::new(decoded);
        document.validate().unwrap();
        let mut invalid = serde_json::to_value(&document).unwrap();
        invalid["dag"]["nodes"][0]["operator"] = serde_json::json!("Binary");
        assert!(serde_json::from_value::<PostAsapDAGDocument>(invalid).is_err());
        assert!(document.dag.nodes.iter().all(|node| {
            let wire = serde_json::to_value(node).unwrap();
            wire.get("operator").is_none() && wire["payload"]["kind"].is_string()
        }));
        let mut old_version = document.clone();
        old_version.schema_version = 1;
        assert_eq!(
            old_version.validate(),
            Err(PostAsapDAGValidationError::UnsupportedVersion(1))
        );
        let mut unknown = serde_json::to_value(&document).unwrap();
        unknown["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<PostAsapDAGDocument>(unknown).is_err());
        assert!(matches!(
            dag.nodes[2].payload,
            PostAsapOperatorPayload::SummaryAgg {
                family: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
                reduction: Reduction::Reduce(_),
                ..
            }
        ));
    }

    #[test]
    fn relational_operators_above_and_below_a_summary_export_one_node_each() {
        // Scan -> Filter -> SummaryAgg -> FinalizeExactAccumulator -> Limit
        let filter = OperatorNode::new_shared(crate::ir::Operator::NonASAP(NonASAPOp::Filter {
            pred: true_pred(),
            child: value_scan(),
        }))
        .unwrap();
        let limit = OperatorNode::new_shared(crate::ir::Operator::NonASAP(NonASAPOp::Limit {
            n: Some(1),
            offset: 0,
            partition_by: GroupKeys::none(),
            child: finalize(sum_agg(filter)),
        }))
        .unwrap();
        let root = timed(&limit);
        let dag = compile_post_asap_dag(&root).unwrap();
        assert_eq!(dag.nodes.len(), 5);
        assert_eq!(dag.edges.len(), 4);
        assert_eq!(relational_count(&dag), 3);
        assert_eq!(dag.root, PostAsapNodeId(4));
        assert_eq!(
            dag.nodes[0].output_state,
            ExecutionDataState::INGESTION_ROWS
        );
        assert_eq!(
            dag.nodes[1].output_state,
            ExecutionDataState::INGESTION_ROWS
        );
        assert_eq!(dag.nodes[4].output_state, ExecutionDataState::QUERY_ROWS);
        assert!(matches!(
            &dag.nodes[1].payload,
            PostAsapOperatorPayload::Relational {
                operator: NonASAPOpKind::Filter {
                    pred: WirePredicate(WireScalarExpr::Literal(ScalarValue::Boolean(true)))
                }
            }
        ));
        assert!(matches!(
            &dag.nodes[4].payload,
            PostAsapOperatorPayload::Relational {
                operator: NonASAPOpKind::Limit { n: Some(1), .. }
            }
        ));
        // Filter -> SummaryAgg: both at ingestion time, so pane alignment is
        // a lowering obligation; the relational producer has no grouping.
        let edge = &dag.edges[1];
        assert_eq!(
            (edge.producer, edge.consumer),
            (PostAsapNodeId(1), PostAsapNodeId(2))
        );
        assert_eq!(edge.grouping, GroupingEdgeCompatibility::NotApplicable);
        assert_eq!(
            edge.window,
            WindowEdgeCompatibility::RequiresAlignedPanePhaseOrExactWindowEdgeResidual
        );
        assert!(dag.edges.iter().all(|e| e.role == EdgeRole::Input));

        let encoded = serde_json::to_string(&dag).unwrap();
        let decoded: PostAsapDAG = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, dag);
    }

    #[test]
    fn shared_scan_under_two_consumers_is_exported_once() {
        let shared = value_scan();
        let branch = |child| {
            OperatorNode::new_shared(crate::ir::Operator::NonASAP(NonASAPOp::Filter {
                pred: true_pred(),
                child,
            }))
            .unwrap()
        };
        let join = OperatorNode::new_shared(crate::ir::Operator::NonASAP(NonASAPOp::Join {
            kind: JoinKind::Inner,
            pred: true_pred(),
            left: branch(Rc::clone(&shared)),
            right: branch(shared),
        }))
        .unwrap();
        let root = timed(&join);
        let compiled = compile_post_asap_dag_with_node_ids(&root).unwrap();
        let dag = compiled.dag;
        assert_eq!(dag.nodes.len(), 4);
        assert_eq!(dag.edges.len(), 4);
        let scan_id = PostAsapNodeId(0);
        assert!(matches!(
            dag.nodes[0].payload,
            PostAsapOperatorPayload::Relational {
                operator: NonASAPOpKind::Scan { .. }
            }
        ));
        let from_scan: Vec<_> = dag.edges.iter().filter(|e| e.producer == scan_id).collect();
        assert_eq!(from_scan.len(), 2);
        assert_ne!(from_scan[0].consumer, from_scan[1].consumer);
        let into_join: Vec<_> = dag
            .edges
            .iter()
            .filter(|e| e.consumer == dag.root)
            .map(|e| e.role)
            .collect();
        assert_eq!(into_join, vec![EdgeRole::Left, EdgeRole::Right]);
        let timed_scan = Rc::clone(root.children()[0].children()[0]);
        assert_eq!(compiled.node_ids.node_id(&timed_scan), Some(scan_id));
    }

    #[test]
    fn scalar_bridge_reference_exports_a_scalar_ref_edge() {
        let vector = scan(vec![
            Field::plain("ts", DataType::Timestamp, false),
            Field::plain("value", DataType::Float64, false),
        ]);
        let bridge = OperatorNode::new_shared(crate::ir::Operator::NonASAP(
            NonASAPOp::PromqlVectorFromScalar(ScalarExpr::PromqlScalarFromVector(vector)),
        ))
        .unwrap();
        let root = timed(&bridge);
        let dag = compile_post_asap_dag(&root).unwrap();
        assert_eq!(dag.nodes.len(), 2);
        assert_eq!(dag.root, PostAsapNodeId(1));
        assert_eq!(
            dag.nodes[1].payload,
            PostAsapOperatorPayload::Relational {
                operator: NonASAPOpKind::PromqlVectorFromScalar {
                    expr: WireScalarExpr::PromqlScalarFromVector(PostAsapNodeId(0)),
                },
            }
        );
        assert_eq!(dag.edges.len(), 1);
        assert_eq!(dag.edges[0].role, EdgeRole::ScalarRef);
        assert_eq!(dag.edges[0].producer, PostAsapNodeId(0));
        assert_eq!(dag.edges[0].consumer, PostAsapNodeId(1));
        let wire = serde_json::to_value(&dag).unwrap();
        assert_eq!(wire["nodes"][1]["payload"]["kind"], "relational");
        assert_eq!(
            wire["nodes"][1]["payload"]["operator"]["kind"],
            "promql_vector_from_scalar"
        );
        assert_eq!(
            wire["nodes"][1]["payload"]["operator"]["expr"]["PromqlScalarFromVector"],
            0
        );
        assert_eq!(serde_json::from_value::<PostAsapDAG>(wire).unwrap(), dag);
    }

    #[test]
    fn untimed_input_is_rejected() {
        let root = value_scan();
        assert!(matches!(
            compile_post_asap_dag(&root),
            Err(ExecutionDataStateError::UntimedNode { operator: "Scan" })
        ));
    }

    #[test]
    fn wire_5_document_is_rejected() {
        let root = timed(&finalize(sum_agg(value_scan())));
        let document = PostAsapDAGDocument::new(compile_post_asap_dag(&root).unwrap());
        assert_eq!(document.schema_version, 7);
        let mut wire = serde_json::to_value(&document).unwrap();
        wire["schema_version"] = serde_json::json!(5);
        let old: PostAsapDAGDocument = serde_json::from_value(wire).unwrap();
        assert_eq!(
            old.validate(),
            Err(PostAsapDAGValidationError::UnsupportedVersion(5))
        );
        let encoded = serde_json::to_string(&document).unwrap();
        let decoded: PostAsapDAGDocument = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, document);
        decoded.validate().unwrap();
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
