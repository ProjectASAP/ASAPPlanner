//! How the derivation rules read an operator's inputs: one trait, so the
//! same rules (output schema and kind, input validation, sharing, summary
//! coverage) derive a front-end node's schema and a memo group's
//! `GroupProperties` (concepts doc `memo-groups-properties-requirements.md`,
//! section 3).

use std::collections::BTreeSet;
use std::rc::Rc;

use super::arguments::Source;
use super::node::{Operator, OperatorNode, OperatorResultKind};
use super::non_asap::NonASAPOp;
use crate::ir::properties::AggregateCoverage;
use crate::ir::schema::Schema;
use crate::ir::SchemaDerivationError;

/// An input of an operator, as the rules read it.
pub trait OperatorInput: Sized + Clone {
    fn schema(&self) -> &Schema;
    fn result_kind(&self) -> OperatorResultKind;
    /// The input's own operator, its children as inputs again.
    fn operator(&self) -> Operator<Self>;
    /// How an aggregate's definition names this input
    /// ([`AggregateCoverage`]): a memo group's id in the memo.
    type Reference: Clone + PartialEq;
    /// This input's reference; `None` for a build a rule constructs only
    /// while it derives, which has none.
    fn reference(&self) -> Option<Self::Reference>;
    /// What the input covers when its coverage is stored (an aggregate
    /// group in the memo); `None` for any other input.
    fn aggregate_coverage(&self) -> Option<&AggregateCoverage<Self::Reference>>;
    /// The sources the `Scan`s at or below the input read
    /// (`Operator::scanned_sources` of its operator): a memo group's stored
    /// `GroupProperties::scanned_sources`, recomputed for a node tree.
    fn scanned_sources(&self) -> BTreeSet<Source>;
}

/// A front-end node tree. Node trees store no aggregate coverage.
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

    /// A node tree is named by nothing: no definition over a node tree is
    /// stored or compared.
    type Reference = ();

    fn reference(&self) -> Option<()> {
        Some(())
    }

    fn aggregate_coverage(&self) -> Option<&AggregateCoverage<()>> {
        None
    }

    fn scanned_sources(&self) -> BTreeSet<Source> {
        self.operator.scanned_sources()
    }
}

/// An input of a rule: a real input `I`, or a build the rule constructs
/// only while it derives.
#[derive(Clone)]
pub(crate) enum InputOrTemporaryBuild<I> {
    Input(I),
    TemporaryBuild(Rc<TemporaryBuild<I>>),
}

/// A build a rule constructs only while it derives (the whole-window build
/// of a chain, the build a rollup stands for, the build a selection of
/// states stands for): an operator over `I`s with its derived schema and
/// kind. Never stored in the memo. A rule never builds one over another
/// rule's temporary build, so the type does not nest.
pub(crate) struct TemporaryBuild<I> {
    operator: Operator<InputOrTemporaryBuild<I>>,
    schema: Schema,
    result_kind: OperatorResultKind,
}

impl<I: OperatorInput> InputOrTemporaryBuild<I> {
    /// A non-ASAP `operator` with its derived schema and kind. Only non-ASAP
    /// rules derive it, which build no input themselves.
    pub(crate) fn build(
        operator: NonASAPOp<InputOrTemporaryBuild<I>>,
    ) -> Result<Self, SchemaDerivationError> {
        let schema = operator.output_schema()?;
        Ok(Self::with_schema(Operator::NonASAP(operator), schema))
    }

    /// `operator` with `schema` and its derived kind.
    pub(crate) fn with_schema(
        operator: Operator<InputOrTemporaryBuild<I>>,
        schema: Schema,
    ) -> Self {
        let result_kind = operator.output_kind();
        Self::TemporaryBuild(Rc::new(TemporaryBuild {
            operator,
            schema,
            result_kind,
        }))
    }
}

impl<I: OperatorInput> OperatorInput for InputOrTemporaryBuild<I> {
    fn schema(&self) -> &Schema {
        match self {
            Self::Input(input) => input.schema(),
            Self::TemporaryBuild(build) => &build.schema,
        }
    }

    fn result_kind(&self) -> OperatorResultKind {
        match self {
            Self::Input(input) => input.result_kind(),
            Self::TemporaryBuild(build) => build.result_kind,
        }
    }

    fn operator(&self) -> Operator<Self> {
        match self {
            Self::Input(input) => input
                .operator()
                .map_children(|child| Self::Input(child.clone())),
            Self::TemporaryBuild(build) => build.operator.clone(),
        }
    }

    type Reference = I::Reference;

    fn reference(&self) -> Option<I::Reference> {
        match self {
            Self::Input(input) => input.reference(),
            Self::TemporaryBuild(_) => None,
        }
    }

    fn aggregate_coverage(&self) -> Option<&AggregateCoverage<I::Reference>> {
        match self {
            Self::Input(input) => input.aggregate_coverage(),
            Self::TemporaryBuild(_) => None,
        }
    }

    fn scanned_sources(&self) -> BTreeSet<Source> {
        match self {
            Self::Input(input) => input.scanned_sources(),
            Self::TemporaryBuild(_) => self.operator().scanned_sources(),
        }
    }
}
