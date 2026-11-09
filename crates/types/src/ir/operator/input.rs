//! How the derivation rules read an operator's inputs: one trait, so the
//! same rules (output schema and kind, input validation, sharing, summary
//! coverage) derive a front-end node's schema and a memo group's
//! `GroupProperties` (concepts doc `memo-groups-properties-requirements.md`,
//! section 3).

use std::rc::Rc;

use super::arguments::Source;
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
    /// What the input covers when it is an aggregate group in the memo;
    /// `None` for any other input.
    fn aggregate_coverage(&self) -> Option<&SummaryCoverage>;
    /// Whether every `Scan` below it reads a time series (PromQL rows):
    /// [`reads_only_time_series`] of its operator, a memo group's stored
    /// property.
    fn reads_only_time_series(&self) -> bool;
}

/// Whether every `Scan` at or below `operator` reads a time series: a
/// `Scan` of a `Source::TimeSeries`, or any other operator all of whose
/// inputs do.
pub(crate) fn reads_only_time_series<I: OperatorInput>(operator: &Operator<I>) -> bool {
    match operator {
        Operator::NonASAP(NonASAPOp::Scan { source, .. }) => {
            matches!(source, Source::TimeSeries { .. })
        }
        operator => operator
            .children()
            .into_iter()
            .all(OperatorInput::reads_only_time_series),
    }
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

    fn aggregate_coverage(&self) -> Option<&SummaryCoverage> {
        None
    }

    fn reads_only_time_series(&self) -> bool {
        reads_only_time_series(&self.operator)
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

    fn aggregate_coverage(&self) -> Option<&SummaryCoverage> {
        match self {
            Self::Input(input) => input.aggregate_coverage(),
            Self::TemporaryBuild(_) => None,
        }
    }

    fn reads_only_time_series(&self) -> bool {
        match self {
            Self::Input(input) => input.reads_only_time_series(),
            Self::TemporaryBuild(build) => reads_only_time_series(&build.operator),
        }
    }
}
