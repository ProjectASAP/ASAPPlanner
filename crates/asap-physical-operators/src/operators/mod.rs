//! Native physical operators. Each module owns its constructors and execution.
mod aligned_binary;
use crate::plan::{Boundedness, Emission, PhysicalOperator, PlanProperties};
use crate::{
    runtime::{Cooperative, Input, OutputStream, Reservation, RunContext},
    values::{field, group_key, plain, Batch, Schema, Value},
    Error,
};
use futures::StreamExt;
use planner_types::{
    post_asap::{SummaryFamilyType, SummaryField, SummarySchema, SummaryUpdate},
    pre_asap::{ColumnRef, DataType},
};
use std::{collections::BTreeMap, sync::Arc};
pub(crate) mod common;
use crate::expressions::ordered;
pub use crate::expressions::Expression;
use common::*;
mod aggregate;
mod current_series;
mod filter;
mod joins;
mod limit;
mod panes;
mod projection;
mod sort;
mod source;
mod summary;
mod unchecked;
pub(crate) mod vector_binary;
pub(crate) mod vector_window;
pub use aggregate::Reduction;
pub use sort::SortKey;
pub use summary::ReadoutQuery;
#[derive(Clone, serde::Serialize, serde::Deserialize)]
enum Kind {
    #[serde(skip)]
    Source(Vec<Batch>),
    Constant {
        value: Value,
        dtype: DataType,
    },
    PaneInput {
        coordinate: usize,
        layout: planner_types::post_asap::PaneLayout,
        offset_ms: Option<i64>,
    },
    ScopeTimestamp {
        columns: Vec<Option<usize>>,
    },
    CurrentSeries {
        identity: usize,
        coordinate: usize,
        value: usize,
        lookback_ms: i64,
    },
    Union,
    VectorToScalar {
        column: usize,
    },
    VectorBinary {
        operator: planner_types::post_asap::BinaryOperator,
        return_bool: bool,
    },
    AlignedBinary {
        keys: Vec<(usize, usize)>,
        values: (usize, usize),
        operator: planner_types::post_asap::BinaryOperator,
    },
    RangeWindow {
        intent: Box<planner_types::pre_asap::AggIntent<ColumnRef>>,
    },
    HistogramQuantile,
    Project(Vec<Expression>),
    Filter(Expression),
    Limit {
        n: u64,
        offset: u64,
        groups: Vec<usize>,
    },
    Sort {
        keys: Vec<SortKey>,
        groups: Vec<usize>,
    },
    Window {
        intent: Box<planner_types::pre_asap::AggIntent<ColumnRef>>,
        coordinate: usize,
        value: usize,
        groups: Vec<usize>,
        window: Option<(i64, i64)>,
    },
    Aggregate {
        groups: Vec<usize>,
        measures: Vec<Reduction>,
    },
    SemiJoin {
        keys: Vec<(usize, usize)>,
        require_complete_right: bool,
    },
    Join {
        kind: planner_types::pre_asap::JoinKind,
        predicate: Box<crate::expressions::CompiledExpression>,
    },
    SummaryBuild {
        family: SummaryFamilyType,
        value: usize,
        time: Option<usize>,
        groups: Vec<usize>,
    },
    KeyedSummaryBuild {
        family: SummaryFamilyType,
        value: usize,
        items: Vec<usize>,
        groups: Vec<usize>,
    },
    KeyedReadout {
        state: usize,
        k: usize,
    },
    SummaryMerge {
        state: usize,
        groups: Vec<usize>,
    },
    Readout {
        state: usize,
        query: ReadoutQuery,
    },
}
/// A bound operation has a fully checked input/output contract before execution.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "unchecked::UncheckedOperator")]
pub struct Operator {
    kind: Kind,
    inputs: Vec<Schema>,
    output: Schema,
}
impl Operator {
    pub(crate) fn row_preserving_input(&self) -> Option<usize> {
        match self.kind {
            Kind::Filter(_) | Kind::Sort { .. } | Kind::Limit { .. } | Kind::SemiJoin { .. } => {
                Some(0)
            }
            _ => None,
        }
    }

    pub(crate) fn is_counter_readout(&self) -> bool {
        matches!(
            self.kind,
            Kind::Readout {
                query: ReadoutQuery::Exact(crate::summary_kernels::exact::ExactReadout {
                    statistic: crate::Statistic::Rate | crate::Statistic::Increase,
                    ..
                }),
                ..
            }
        )
    }

    pub(crate) fn with_counter_lookback(mut self, lookback: i64) -> Result<Self, Error> {
        if lookback <= 0 {
            return Err(invalid("counter lookback must be positive"));
        }
        if let Kind::Readout {
            query: ReadoutQuery::Exact(readout),
            ..
        } = &mut self.kind
        {
            readout.lookback_ms = Some(lookback);
        }
        Ok(self)
    }

    /// Resolve a counter readout's logical lookback to this run's evaluation range.
    pub(super) fn readout_range(&self, context: &RunContext) -> Result<Option<(i64, i64)>, Error> {
        let Kind::Readout {
            query:
                ReadoutQuery::Exact(crate::summary_kernels::exact::ExactReadout {
                    lookback_ms: Some(lookback),
                    ..
                }),
            ..
        } = &self.kind
        else {
            return Ok(None);
        };
        let end = match context.scope {
            crate::runtime::Scope::Query {
                evaluation_time_ms, ..
            } => evaluation_time_ms,
            crate::runtime::Scope::Ingestion { window_end_ms, .. } => window_end_ms,
        };
        let start = end
            .checked_sub(*lookback)
            .ok_or_else(|| invalid("counter window overflows Int64"))?;
        if let crate::runtime::Scope::Ingestion {
            window_start_ms, ..
        } = context.scope
        {
            if window_start_ms != start {
                return Err(invalid(
                    "maintenance window differs from logical counter window",
                ));
            }
        }
        Ok(Some((start, end)))
    }

    pub(crate) fn with_output_schema(mut self, output: Schema) -> Result<Self, Error> {
        if self.output.fields.len() != output.fields.len()
            || self
                .output
                .fields
                .iter()
                .zip(&output.fields)
                .any(|(actual, declared)| {
                    actual.dtype != declared.dtype || (actual.nullable && !declared.nullable)
                })
        {
            return Err(invalid("native output type differs from Planner output"));
        }
        if output.time_index.is_some_and(|i| {
            i >= output.fields.len()
                || output.fields[i].dtype != SummaryFamilyType::Plain(DataType::Timestamp)
        }) {
            return Err(invalid("invalid output time column"));
        }
        self.output = output;
        Ok(self)
    }
    pub fn schema(&self) -> Schema {
        self.output.clone()
    }
}
impl PhysicalOperator<Batch, Schema> for Operator {
    fn requires_bounded_input(&self) -> bool {
        matches!(
            self.kind,
            Kind::Sort { .. }
                | Kind::AlignedBinary { .. }
                | Kind::VectorBinary { .. }
                | Kind::RangeWindow { .. }
                | Kind::HistogramQuantile
                | Kind::CurrentSeries { .. }
                | Kind::Aggregate { .. }
                | Kind::Window { .. }
                | Kind::Join { .. }
                | Kind::SemiJoin { .. }
                | Kind::SummaryBuild { .. }
                | Kind::KeyedSummaryBuild { .. }
                | Kind::SummaryMerge { .. }
                | Kind::VectorToScalar { .. }
        )
    }
    fn properties(&self, inputs: &[PlanProperties]) -> PlanProperties {
        let boundedness = match &self.kind {
            Kind::Source(_) | Kind::Constant { .. } => Boundedness::Bounded,
            Kind::Limit { groups, .. } if groups.is_empty() => Boundedness::Bounded,
            _ => Boundedness::from_inputs(inputs),
        };
        PlanProperties {
            boundedness,
            emission: if matches!(
                self.kind,
                Kind::PaneInput { .. } | Kind::ScopeTimestamp { .. }
            ) {
                inputs
                    .first()
                    .map_or(Emission::Unknown, |input| input.emission)
            } else if self.requires_bounded_input() {
                Emission::AfterInput
            } else {
                Emission::Incremental
            },
        }
    }

    fn name(&self) -> &str {
        match self.kind {
            Kind::Source(_) => "Source",
            Kind::Constant { .. } => "Constant",
            Kind::PaneInput { .. } => "PaneInput",
            Kind::ScopeTimestamp { .. } => "ScopeTimestamp",
            Kind::Union => "Union",
            Kind::CurrentSeries { .. } => "CurrentSeries",
            Kind::VectorToScalar { .. } => "VectorToScalar",
            Kind::VectorBinary { .. } => "VectorBinary",
            Kind::AlignedBinary { .. } => "AlignedBinary",
            Kind::RangeWindow { .. } => "RangeWindow",
            Kind::HistogramQuantile => "HistogramQuantile",
            Kind::Project(_) => "Project",
            Kind::Filter(_) => "Filter",
            Kind::Limit { .. } => "Limit",
            Kind::Sort { .. } => "Sort",
            Kind::Aggregate { .. } => "Aggregate",
            Kind::Window { .. } => "WindowAggregate",
            Kind::SemiJoin { .. } => "SemiJoin",
            Kind::Join { .. } => "RelationalJoin",
            Kind::SummaryBuild { .. } | Kind::KeyedSummaryBuild { .. } => "SummaryAgg",
            Kind::KeyedReadout { .. } => "SummaryEstimate",
            Kind::SummaryMerge { .. } => "SummaryMerge",
            Kind::Readout { .. } => "SummaryReadout",
        }
    }
    fn validate_context(&self, context: &RunContext) -> Result<(), Error> {
        panes::validate_context(self, context)?;
        current_series::validate_context(self, context)?;
        self.readout_range(context).map(|_| ())
    }
    fn input_schemas(&self) -> Vec<Schema> {
        self.inputs.clone()
    }
    fn output_schema(&self) -> Schema {
        self.output.clone()
    }
    fn output_bytes(&self, value: &Batch) -> usize {
        value.bytes()
    }
    fn start<'a>(
        &'a self,
        inputs: Vec<Input<'a, Batch>>,
        context: RunContext,
    ) -> Result<OutputStream<'a, Batch>, Error> {
        match self.kind {
            Kind::Source(_) | Kind::Constant { .. } | Kind::Union | Kind::VectorToScalar { .. } => {
                source::execute(self, inputs, context)
            }
            Kind::VectorBinary { .. } => vector_binary::execute(self, inputs, context),
            Kind::AlignedBinary { .. } => aligned_binary::execute(self, inputs, context),
            Kind::RangeWindow { .. } | Kind::HistogramQuantile => {
                vector_window::execute(self, inputs, context)
            }
            Kind::Project(_) => projection::execute(self, inputs, context),
            Kind::CurrentSeries { .. } => current_series::execute(self, inputs, context),
            Kind::PaneInput { .. } | Kind::ScopeTimestamp { .. } => {
                panes::execute(self, inputs, context)
            }
            Kind::Filter(_) => filter::execute(self, inputs, context),
            Kind::Limit { .. } => limit::execute(self, inputs, context),
            Kind::Sort { .. } => sort::execute(self, inputs, context),
            Kind::Window { .. } | Kind::Aggregate { .. } => {
                aggregate::execute(self, inputs, context)
            }
            Kind::Join { .. } | Kind::SemiJoin { .. } => joins::execute(self, inputs, context),
            Kind::SummaryMerge { .. } => summary::execute_merge(self, inputs, context),
            Kind::SummaryBuild { .. }
            | Kind::Readout { .. }
            | Kind::KeyedSummaryBuild { .. }
            | Kind::KeyedReadout { .. } => summary::execute(self, inputs, context),
        }
    }
}
