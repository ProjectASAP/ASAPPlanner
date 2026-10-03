//! The unified operator node: one node type before and after ASAP
//! optimization. A node is an operator plus the common planning properties
//! every traversal needs (output category and schema, accuracy guarantee,
//! execution timing).

use std::collections::HashSet;
use std::rc::Rc;

use serde::{Deserialize, Serialize};

use super::asap::ASAPOp;
use super::non_asap::NonASAPOp;
use super::summary_coverage::{CoverageError, SummaryCoverage};
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
    #[serde(default)]
    pub coverage: Option<SummaryCoverage>,
}

impl OperatorNode {
    /// Build a node, deriving its schema and output category. Fails when the
    /// schema cannot be derived (a column reference out of range, a reserved
    /// ASAP operator, ...).
    pub fn new(operator: Operator) -> Result<Self, SchemaDerivationError> {
        let schema = operator.output_schema()?;
        let mut node = Self::with_schema(operator, schema);
        if let Some(op @ ASAPOp::SummaryMerge { .. }) = node.asap() {
            node.observation_extent = Some(op.merged_extent()?);
        }
        Ok(node)
    }

    /// Build a node with caller-supplied output names and qualifiers. For
    /// either operator category, `validate_structure` requires all other
    /// schema metadata to agree with derivation; output kind is derived here.
    pub fn with_schema(operator: Operator, schema: Schema) -> Self {
        let result_kind = operator.output_kind();
        Self {
            operator,
            result_kind,
            schema,
            guarantee: None,
            timing: None,
            coverage: None,
        }
    }

    /// Build a shared node for either operator category, deriving its schema
    /// and output category. Use `with_schema` before wrapping in `Rc` when
    /// planning supplies a more specific output schema.
    pub fn new_shared(operator: Operator) -> Result<Rc<Self>, SchemaDerivationError> {
        Self::new(operator).map(Rc::new)
    }

    pub fn with_guarantee(mut self, guarantee: Option<ResultGuarantee>) -> Self {
        self.guarantee = guarantee;
        self
    }

    pub fn with_timing(mut self, timing: Option<ExecutionTiming>) -> Self {
        self.timing = timing;
        self
    }

    /// Attach caller-established coverage. Required on summary nodes; see
    /// [`Self::requires_coverage`].
    pub fn with_coverage(
        mut self,
        coverage: SummaryCoverage,
    ) -> Result<Self, SchemaDerivationError> {
        coverage.validate()?;
        if self.result_kind != OperatorResultKind::State {
            return Err(CoverageError::NotState.into());
        }
        self.coverage = Some(coverage);
        Ok(self)
    }

    /// Summary nodes whose state can be composed must declare coverage.
    pub fn requires_coverage(&self) -> bool {
        matches!(self.asap(), Some(ASAPOp::SummaryAgg { .. }))
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

    /// Rebuild with new inputs, re-deriving all structural schema metadata.
    /// Only names and qualifiers that override the old derived schema are
    /// retained, for either operator category. A change in output arity with
    /// such overrides needs an explicit new naming assignment.
    /// `guarantee` and `timing` depend on the inputs and are cleared.
    pub fn map_children(
        &self,
        f: impl FnMut(&Rc<OperatorNode>) -> Rc<OperatorNode>,
    ) -> Result<Self, SchemaDerivationError> {
        let previous = self.operator.output_schema()?;
        let mut rebuilt = Self::new(self.operator.map_children(f))?;
        for (i, (derived, retained)) in previous.fields.iter().zip(&self.schema.fields).enumerate()
        {
            let renamed = retained.name != derived.name;
            let requalified = retained.table != derived.table;
            if !renamed && !requalified {
                continue;
            }
            if rebuilt.schema.fields.len() != previous.fields.len() {
                return Err(SchemaDerivationError::InvalidScalarSignature(
                    "output arity changed; reassign explicit output names and qualifiers".into(),
                ));
            }
            if renamed {
                rebuilt.schema.fields[i].name = retained.name.clone();
            }
            if requalified {
                rebuilt.schema.fields[i].table = retained.table.clone();
            }
        }
        Ok(rebuilt)
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
    /// derived from the operator. Both operator categories may override field
    /// names and qualifiers; all structural metadata must match derivation.
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
            match &node.coverage {
                Some(coverage) => {
                    (*node.as_ref()).clone().with_coverage(coverage.clone())?;
                }
                None if node.requires_coverage() => return Err(CoverageError::Missing.into()),
                None => {}
            }
            if let Some(op @ ASAPOp::SummaryMerge { .. }) = node.asap() {
                if node.observation_extent.as_ref() != Some(&op.merged_extent()?) {
                    return Err(SchemaDerivationError::InvalidScalarSignature(
                        "retained merge coverage disagrees with input union".into(),
                    ));
                }
            }
            node.operator.validate_inputs()?;
            if node.result_kind != node.operator.output_kind() {
                return Err(SchemaDerivationError::InvalidScalarSignature(
                    "retained result kind disagrees with operation".into(),
                ));
            }
            let mut derived = node.operator.output_schema()?;
            // Normalize only naming overrides, then compare the whole schema
            // so new structural metadata cannot accidentally escape validation.
            for (field, retained) in derived.fields.iter_mut().zip(&node.schema.fields) {
                field.name = retained.name.clone();
                field.table = retained.table.clone();
            }
            let agree = derived == node.schema;
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
