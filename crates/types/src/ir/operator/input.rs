//! How the derivation rules read an operator's inputs: one trait, so the
//! same rules (output schema and kind, input validation, sharing, summary
//! coverage) derive a front-end node's schema and a memo group's
//! `GroupProperties` (concepts doc `memo-groups-properties-requirements.md`,
//! section 3).

use std::rc::Rc;

use super::node::{Operator, OperatorNode, OperatorResultKind};
use super::non_asap::NonASAPOp;
use crate::ir::properties::summary_coverage::SummaryCoverage;
use crate::ir::schema::Schema;
use crate::ir::SchemaDerivationError;

/// An input of an operator, as the rules read it.
pub trait OperatorInput: Sized + Clone {
    fn schema(&self) -> &Schema;
    fn result_kind(&self) -> OperatorResultKind;
    /// The input's own operator, its children as inputs again.
    fn operator(&self) -> Operator<Self>;
    /// What the input covers when it is summary state in the memo; `None`
    /// for any other input.
    fn summary_coverage(&self) -> Option<&SummaryCoverage>;
}

/// A front-end node tree. Node trees carry no summary coverage.
impl OperatorInput for Rc<OperatorNode> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn result_kind(&self) -> OperatorResultKind {
        self.result_kind
    }

    fn operator(&self) -> Operator<Self> {
        self.operator.clone()
    }

    fn summary_coverage(&self) -> Option<&SummaryCoverage> {
        None
    }
}

/// An input a rule builds while it derives (one build over a chain's
/// window, the build a rollup stands for, the build a selection of states
/// stands for): an operator over `I`s that is neither a node of a tree nor
/// a memo group. A rule never builds one over another rule's built input,
/// so the type does not nest.
#[derive(Clone)]
pub(crate) enum BuiltOrInput<I> {
    Input(I),
    Built(Rc<BuiltInput<I>>),
}

pub(crate) struct BuiltInput<I> {
    operator: Operator<BuiltOrInput<I>>,
    schema: Schema,
    result_kind: OperatorResultKind,
}

impl<I: OperatorInput> BuiltOrInput<I> {
    /// A non-ASAP `operator` with its derived schema and kind. Only non-ASAP
    /// rules derive it, which build no input themselves.
    pub(crate) fn build(
        operator: NonASAPOp<BuiltOrInput<I>>,
    ) -> Result<Self, SchemaDerivationError> {
        let schema = operator.output_schema()?;
        Ok(Self::with_schema(Operator::NonASAP(operator), schema))
    }

    /// `operator` with `schema` and its derived kind.
    pub(crate) fn with_schema(operator: Operator<BuiltOrInput<I>>, schema: Schema) -> Self {
        let result_kind = operator.output_kind();
        Self::Built(Rc::new(BuiltInput {
            operator,
            schema,
            result_kind,
        }))
    }
}

impl<I: OperatorInput> OperatorInput for BuiltOrInput<I> {
    fn schema(&self) -> &Schema {
        match self {
            Self::Input(input) => input.schema(),
            Self::Built(built) => &built.schema,
        }
    }

    fn result_kind(&self) -> OperatorResultKind {
        match self {
            Self::Input(input) => input.result_kind(),
            Self::Built(built) => built.result_kind,
        }
    }

    fn operator(&self) -> Operator<Self> {
        match self {
            Self::Input(input) => input
                .operator()
                .map_children(|child| Self::Input(child.clone())),
            Self::Built(built) => built.operator.clone(),
        }
    }

    fn summary_coverage(&self) -> Option<&SummaryCoverage> {
        match self {
            Self::Input(input) => input.summary_coverage(),
            Self::Built(_) => None,
        }
    }
}

impl BuiltOrInput<Rc<OperatorNode>> {
    /// The node tree of this input: a built input as a new node over its
    /// inputs' trees.
    pub(crate) fn to_node(&self) -> Rc<OperatorNode> {
        match self {
            Self::Input(node) => Rc::clone(node),
            Self::Built(built) => Rc::new(OperatorNode::with_schema(
                built.operator.map_children(Self::to_node),
                built.schema.clone(),
            )),
        }
    }
}

/// A build over node trees (`tumbling_window_chain`'s builds) as a node.
pub(crate) fn node_of_build(
    (build, schema): (super::asap::ASAPOp<BuiltOrInput<Rc<OperatorNode>>>, Schema),
) -> OperatorNode {
    OperatorNode::with_schema(
        Operator::ASAP(build.map_children(BuiltOrInput::to_node)),
        schema,
    )
}
