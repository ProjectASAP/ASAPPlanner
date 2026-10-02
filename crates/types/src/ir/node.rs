//! The unified operator node: one node type before and after ASAP
//! optimization. A node is an operator plus the common planning properties
//! every traversal needs (output category and schema, accuracy guarantee,
//! execution timing).

use std::collections::HashSet;
use std::rc::Rc;

use serde::{Deserialize, Serialize};

use super::asap::ASAPOp;
use super::non_asap::NonASAPOp;
use crate::ir::SchemaDerivationError;
use crate::post_asap::execution_data_state::ExecutionTiming;
use crate::post_asap::guarantee::ResultGuarantee;
use crate::pre_asap::schema::Schema;

/// The output category of an operator, derived from the operation and its
/// inputs. Matching column schemas do not make categories interchangeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorResultKind {
    Relation,
    InstantVector,
    RangeVector,
    /// Unfinalized summary / accumulator state.
    State,
}

/// The operation a node performs: an ordinary query operator or an ASAP
/// summary operator. Either category can consume the other's output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Operator {
    NonASAP(NonASAPOp),
    ASAP(ASAPOp),
}

impl Operator {
    pub fn children(&self) -> Vec<&Rc<OperatorNode>> {
        match self {
            Operator::NonASAP(op) => op.children(),
            Operator::ASAP(op) => op.children(),
        }
    }

    pub fn map_children(&self, f: impl FnMut(&Rc<OperatorNode>) -> Rc<OperatorNode>) -> Self {
        match self {
            Operator::NonASAP(op) => Operator::NonASAP(op.map_children(f)),
            Operator::ASAP(op) => Operator::ASAP(op.map_children(f)),
        }
    }

    pub fn output_schema(&self) -> Result<Schema, SchemaDerivationError> {
        match self {
            Operator::NonASAP(op) => op.output_schema(),
            Operator::ASAP(op) => op.output_schema(),
        }
    }

    pub fn output_kind(&self) -> OperatorResultKind {
        match self {
            Operator::NonASAP(op) => op.output_kind(),
            Operator::ASAP(op) => op.output_kind(),
        }
    }

    pub fn validate_inputs(&self) -> Result<(), SchemaDerivationError> {
        match self {
            Operator::NonASAP(op) => op.validate_inputs(),
            Operator::ASAP(op) => op.validate_inputs(),
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Operator::NonASAP(op) => op.kind_name(),
            Operator::ASAP(op) => op.kind_name(),
        }
    }
}

/// A node of the logical DAG. Nodes are immutable and shared through `Rc`;
/// a structurally identical sub-DAG referenced from several parents is one
/// node.
///
/// `schema` and `result_kind` are derived from `operator` and its children
/// at construction and retained. `guarantee` is `None` until accuracy
/// assessment establishes one (`None` never means exact). `timing` is `None`
/// until a lifecycle assignment is applied; export rejects an executable
/// node without one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperatorNode {
    pub operator: Operator,
    pub result_kind: OperatorResultKind,
    pub schema: Schema,
    pub guarantee: Option<ResultGuarantee>,
    pub timing: Option<ExecutionTiming>,
}

impl OperatorNode {
    /// Build a node, deriving its schema and output category. Fails when the
    /// schema cannot be derived (a column reference out of range, a reserved
    /// ASAP operator, ...).
    pub fn new(operator: Operator) -> Result<Self, SchemaDerivationError> {
        let schema = operator.output_schema()?;
        Ok(Self::with_schema(operator, schema))
    }

    /// Build a node with a caller-supplied output schema. Summary planning
    /// uses this where its evaluation naming is more specific than the derived
    /// shape; the output category is still derived.
    pub fn with_schema(operator: Operator, schema: Schema) -> Self {
        let result_kind = operator.output_kind();
        Self {
            operator,
            result_kind,
            schema,
            guarantee: None,
            timing: None,
        }
    }

    pub fn non_asap_node(op: NonASAPOp) -> Result<Rc<Self>, SchemaDerivationError> {
        Self::new(Operator::NonASAP(op)).map(Rc::new)
    }

    /// An ASAP node with a planner-supplied schema and guarantee.
    pub fn asap_node(op: ASAPOp, schema: Schema, guarantee: Option<ResultGuarantee>) -> Rc<Self> {
        Rc::new(Self::with_schema(Operator::ASAP(op), schema).with_guarantee(guarantee))
    }

    pub fn with_guarantee(mut self, guarantee: Option<ResultGuarantee>) -> Self {
        self.guarantee = guarantee;
        self
    }

    pub fn with_timing(mut self, timing: Option<ExecutionTiming>) -> Self {
        self.timing = timing;
        self
    }

    pub fn non_asap(&self) -> Option<&NonASAPOp> {
        match &self.operator {
            Operator::NonASAP(op) => Some(op),
            Operator::ASAP(_) => None,
        }
    }

    pub fn asap(&self) -> Option<&ASAPOp> {
        match &self.operator {
            Operator::ASAP(op) => Some(op),
            Operator::NonASAP(_) => None,
        }
    }

    pub fn is_asap(&self) -> bool {
        matches!(self.operator, Operator::ASAP(_))
    }

    /// The non-ASAP operator of a node that is known to be one; a front-end
    /// DAG never contains an ASAP node, so one here is a caller bug.
    pub fn expect_non_asap(&self) -> &NonASAPOp {
        self.non_asap().unwrap_or_else(|| {
            panic!(
                "expected a non-ASAP operator, found {}",
                self.operator.kind_name()
            )
        })
    }

    /// Direct inputs, including the operator nodes referenced from this
    /// node's scalar expressions.
    pub fn children(&self) -> Vec<&Rc<OperatorNode>> {
        self.operator.children()
    }

    /// Rebuild this node with `f` applied to every direct input. The schema
    /// is re-derived for a non-ASAP operator (its shape follows its inputs);
    /// an ASAP node keeps its retained schema. `guarantee` and `timing` are
    /// cleared: both depend on the inputs and must be re-established.
    pub fn map_children(
        &self,
        f: impl FnMut(&Rc<OperatorNode>) -> Rc<OperatorNode>,
    ) -> Result<Self, SchemaDerivationError> {
        let operator = self.operator.map_children(f);
        match &operator {
            Operator::NonASAP(_) => Self::new(operator),
            Operator::ASAP(_) => Ok(Self::with_schema(operator, self.schema.clone())),
        }
    }

    /// Whether any node reachable from this one (including itself) is an
    /// ASAP operator. Visits each shared node once.
    pub fn contains_asap(&self) -> bool {
        fn walk(node: &OperatorNode, seen: &mut HashSet<*const OperatorNode>) -> bool {
            if node.is_asap() {
                return true;
            }
            node.children()
                .iter()
                .any(|child| seen.insert(Rc::as_ptr(child)) && walk(child, seen))
        }
        walk(self, &mut HashSet::new())
    }

    /// Every unique node reachable from `root`, parents before children
    /// (pre-order, deduplicated by pointer identity).
    pub fn reachable(root: &Rc<OperatorNode>) -> Vec<Rc<OperatorNode>> {
        fn walk(
            node: &Rc<OperatorNode>,
            seen: &mut HashSet<*const OperatorNode>,
            out: &mut Vec<Rc<OperatorNode>>,
        ) {
            if !seen.insert(Rc::as_ptr(node)) {
                return;
            }
            out.push(Rc::clone(node));
            for child in node.children() {
                walk(child, seen, out);
            }
        }
        let mut out = Vec::new();
        walk(root, &mut HashSet::new(), &mut out);
        out
    }

    /// Validate assigned phases without imposing any particular runtime implementation.
    pub fn validate_execution_timing(self: &Rc<Self>) -> Result<(), SchemaDerivationError> {
        self.validate_structure()?;
        for node in Self::reachable(self) {
            let timing = node.timing.ok_or_else(|| {
                SchemaDerivationError::InvalidScalarSignature(
                    "execution timing is unassigned".into(),
                )
            })?;
            if timing == crate::post_asap::ExecutionTiming::IngestionTime
                && node.children().iter().any(|child| {
                    child.timing != Some(crate::post_asap::ExecutionTiming::IngestionTime)
                })
            {
                return Err(SchemaDerivationError::InvalidScalarSignature(
                    "ingestion-time operation depends on a query-time or unassigned input".into(),
                ));
            }
        }
        Ok(())
    }

    /// Validate the whole DAG reachable from this node: every operator's
    /// input contract, scalar typing against the owning operator's input
    /// schema, and agreement between each retained schema and the one
    /// derived from the operator. For an ASAP node the planner may retain
    /// more specific column names, so only the field types must agree.
    /// `timing` may be `None`.
    pub fn validate_structure(self: &Rc<Self>) -> Result<(), SchemaDerivationError> {
        for node in Self::reachable(self) {
            if node.schema.time_index.is_some_and(|i| {
                node.schema
                    .fields
                    .get(i)
                    .is_none_or(|f| f.plain_dtype() != Some(&crate::pre_asap::DataType::Timestamp))
            }) || node
                .schema
                .unique_keys
                .iter()
                .flatten()
                .any(|i| *i >= node.schema.fields.len())
            {
                return Err(SchemaDerivationError::InvalidScalarSignature(
                    "invalid time or identity column in schema".into(),
                ));
            }
            node.operator.validate_inputs()?;
            if node.result_kind != node.operator.output_kind() {
                return Err(SchemaDerivationError::InvalidScalarSignature(
                    "retained result kind disagrees with operation".into(),
                ));
            }
            let derived = node.operator.output_schema()?;
            let agree = match &node.operator {
                Operator::NonASAP(_) => derived == node.schema,
                Operator::ASAP(_) => {
                    derived.fields.len() == node.schema.fields.len()
                        && derived
                            .fields
                            .iter()
                            .zip(&node.schema.fields)
                            .all(|(d, r)| d.dtype == r.dtype && d.nullable == r.nullable)
                }
            };
            if !agree {
                return Err(SchemaDerivationError::InvalidScalarSignature(format!(
                    "retained schema of {} disagrees with its derived schema",
                    node.operator.kind_name()
                )));
            }
        }
        Ok(())
    }
}
