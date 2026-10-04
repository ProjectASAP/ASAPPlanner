//! Native physical operators. Each module owns its constructors and execution.
mod aligned_binary;
use crate::plan::{Boundedness, Emission, PhysicalOperator, PlanProperties};
use crate::{
    runtime::{Cooperative, Input, OutputStream, Reservation, RunContext},
    values::{field, group_key, plain, Batch, SchemaRef, Value},
    Error,
};
use futures::StreamExt;
use planner_types::ir::scalar::ColumnRef;
use planner_types::ir::schema::{
    DataType, Field as SummaryField, FieldDataType as SummaryFamilyType, Schema, SummaryUpdate,
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
mod projection;
mod scope_timestamp;
mod series_labels;
mod series_window;
mod sort;
mod source;
mod summary;
mod unchecked;
pub(crate) mod vector_binary;
pub(crate) mod vector_window;
pub use aggregate::Reduction;
pub use series_window::SubquerySteps;
pub use sort::SortKey;
pub use summary::SummaryEvaluation;
#[derive(Clone, serde::Serialize, serde::Deserialize)]
enum Kind {
    #[serde(skip)]
    Source(Vec<Batch>),
    Constant {
        value: Value,
        dtype: DataType,
    },
    EvaluationTime,
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
        operator: crate::expressions::binary::BinaryOperator,
        return_bool: bool,
    },
    AlignedBinary {
        keys: Vec<(usize, usize)>,
        values: (usize, usize),
        operator: crate::expressions::binary::BinaryOperator,
    },
    RangeWindow {
        intent: Box<planner_types::ir::operator::AggIntent<ColumnRef>>,
    },
    HistogramQuantile,
    SeriesWindow {
        function: Option<Box<planner_types::ir::operator::AggIntent<ColumnRef>>>,
        coordinate: usize,
        value: usize,
        range_ms: i64,
        offset_ms: i64,
        at_ms: Option<i64>,
        steps: Option<SubquerySteps>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        range_at: Option<planner_types::ir::operator::AtModifier>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        steps_range_at: Option<planner_types::ir::operator::AtModifier>,
    },
    SeriesLabels {
        kind: planner_types::ir::operator::VectorMatchKind,
        labels: Vec<String>,
        unique: bool,
    },
    SeriesBinary {
        operator: crate::expressions::binary::BinaryOperator,
        scalars: [bool; 2],
    },
    SeriesRelabel {
        destination: String,
        replacement: String,
        source_regex: Option<(String, String)>,
    },
    SeriesHistogramQuantile {
        /// `f64` bits: JSON cannot encode the NaN and infinite quantiles.
        quantile: u64,
        le: usize,
    },
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
        intent: Box<planner_types::ir::operator::AggIntent<ColumnRef>>,
        coordinate: usize,
        value: usize,
        groups: Vec<usize>,
        window: Option<(i64, i64)>,
    },
    SQLWindowSum {
        column: usize,
    },
    Aggregate {
        groups: Vec<usize>,
        measures: Vec<Reduction>,
        /// Per-measure row filters (SQL `FILTER (WHERE …)`); empty when none.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        filters: Vec<Option<Expression>>,
    },
    SemiJoin {
        keys: Vec<(usize, usize)>,
        require_complete_right: bool,
    },
    Join {
        kind: planner_types::ir::operator::JoinKind,
        predicate: Box<crate::expressions::CompiledExpression>,
    },
    /// `value: None`: every row adds a unit weight (SQL `COUNT(*)`).
    SummaryBuild {
        family: SummaryFamilyType,
        value: Option<usize>,
        time: Option<usize>,
        groups: Vec<usize>,
        /// Rows for which this is not true update no state; their group is kept.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<Box<Expression>>,
    },
    KeyedSummaryBuild {
        family: SummaryFamilyType,
        value: usize,
        items: Vec<usize>,
        groups: Vec<usize>,
        /// Rows for which this is not true update no state; their group is kept.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<Box<Expression>>,
    },
    /// One shared state for all groups (HydraCms); `weight: None` is a unit count.
    SharedSummaryBuild {
        family: SummaryFamilyType,
        item: usize,
        weight: Option<usize>,
        groups: Vec<usize>,
        /// Rows for which this is not true update no state; their group is kept.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<Box<Expression>>,
    },
    KeyedEvaluation {
        state: usize,
        k: usize,
    },
    SummaryMerge {
        state: usize,
        groups: Vec<usize>,
    },
    Evaluation {
        state: usize,
        query: SummaryEvaluation,
    },
}
/// A bound operation has a fully checked input/output contract before execution.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "unchecked::UncheckedOperator")]
pub struct Operator {
    kind: Kind,
    inputs: Vec<SchemaRef>,
    output: SchemaRef,
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

    pub(crate) fn is_counter_evaluation(&self) -> bool {
        matches!(
            self.kind,
            Kind::Evaluation {
                query: SummaryEvaluation::Exact(crate::summary_kernels::exact::ExactEvaluation {
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
        if let Kind::Evaluation {
            query: SummaryEvaluation::Exact(evaluation),
            ..
        } = &mut self.kind
        {
            evaluation.lookback_ms = Some(lookback);
        }
        Ok(self)
    }

    /// Resolve a counter evaluation's logical lookback to this run's evaluation range.
    pub(super) fn evaluation_range(
        &self,
        context: &RunContext,
    ) -> Result<Option<(i64, i64)>, Error> {
        let Kind::Evaluation {
            query:
                SummaryEvaluation::Exact(crate::summary_kernels::exact::ExactEvaluation {
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

    pub(crate) fn with_output_schema(mut self, output: SchemaRef) -> Result<Self, Error> {
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
    pub fn schema(&self) -> SchemaRef {
        self.output.clone()
    }
}
impl PhysicalOperator<Batch, SchemaRef> for Operator {
    fn requires_bounded_input(&self) -> bool {
        matches!(
            self.kind,
            Kind::Sort { .. }
                | Kind::AlignedBinary { .. }
                | Kind::VectorBinary { .. }
                | Kind::RangeWindow { .. }
                | Kind::HistogramQuantile
                | Kind::CurrentSeries { .. }
                | Kind::SeriesWindow { .. }
                | Kind::SeriesLabels { .. }
                | Kind::SeriesBinary { .. }
                | Kind::SeriesHistogramQuantile { .. }
                | Kind::SeriesRelabel { .. }
                | Kind::SQLWindowSum { .. }
                | Kind::Aggregate { .. }
                | Kind::Window { .. }
                | Kind::Join { .. }
                | Kind::SemiJoin { .. }
                | Kind::SummaryBuild { .. }
                | Kind::KeyedSummaryBuild { .. }
                | Kind::SharedSummaryBuild { .. }
                | Kind::SummaryMerge { .. }
                | Kind::VectorToScalar { .. }
        )
    }
    fn properties(&self, inputs: &[PlanProperties]) -> PlanProperties {
        let boundedness = match &self.kind {
            Kind::Source(_) | Kind::Constant { .. } | Kind::EvaluationTime => Boundedness::Bounded,
            Kind::Limit { groups, .. } if groups.is_empty() => Boundedness::Bounded,
            _ => Boundedness::from_inputs(inputs),
        };
        PlanProperties {
            boundedness,
            emission: if matches!(self.kind, Kind::ScopeTimestamp { .. }) {
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
            Kind::EvaluationTime => "EvaluationTime",
            Kind::Constant { .. } => "Constant",
            Kind::ScopeTimestamp { .. } => "ScopeTimestamp",
            Kind::Union => "Union",
            Kind::CurrentSeries { .. } => "CurrentSeries",
            Kind::VectorToScalar { .. } => "VectorToScalar",
            Kind::VectorBinary { .. } => "VectorBinary",
            Kind::AlignedBinary { .. } => "AlignedBinary",
            Kind::RangeWindow { .. } => "RangeWindow",
            Kind::HistogramQuantile => "HistogramQuantile",
            Kind::SeriesWindow { .. } => "SeriesWindow",
            Kind::SeriesLabels { .. } => "SeriesLabels",
            Kind::SeriesBinary { .. } => "SeriesBinary",
            Kind::SeriesRelabel { .. } => "SeriesRelabel",
            Kind::SeriesHistogramQuantile { .. } => "SeriesHistogramQuantile",
            Kind::Project(_) => "Project",
            Kind::Filter(_) => "Filter",
            Kind::Limit { .. } => "Limit",
            Kind::Sort { .. } => "Sort",
            Kind::SQLWindowSum { .. } => "SQLWindowSum",
            Kind::Aggregate { .. } => "Aggregate",
            Kind::Window { .. } => "WindowAggregate",
            Kind::SemiJoin { .. } => "SemiJoin",
            Kind::Join { .. } => "RelationalJoin",
            Kind::SummaryBuild { .. }
            | Kind::KeyedSummaryBuild { .. }
            | Kind::SharedSummaryBuild { .. } => "SummaryAgg",
            Kind::KeyedEvaluation { .. } => "SummaryEstimate",
            Kind::SummaryMerge { .. } => "SummaryMerge",
            Kind::Evaluation { .. } => "SummaryEvaluation",
        }
    }
    fn validate_context(&self, context: &RunContext) -> Result<(), Error> {
        current_series::validate_context(self, context)?;
        series_window::validate_context(self, context)?;
        self.evaluation_range(context).map(|_| ())
    }
    fn input_schemas(&self) -> Vec<SchemaRef> {
        self.inputs.clone()
    }
    fn output_schema(&self) -> SchemaRef {
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
            Kind::Source(_)
            | Kind::Constant { .. }
            | Kind::EvaluationTime
            | Kind::Union
            | Kind::VectorToScalar { .. } => source::execute(self, inputs, context),
            Kind::VectorBinary { .. } => vector_binary::execute(self, inputs, context),
            Kind::AlignedBinary { .. } => aligned_binary::execute(self, inputs, context),
            Kind::RangeWindow { .. } | Kind::HistogramQuantile => {
                vector_window::execute(self, inputs, context)
            }
            Kind::Project(_) => projection::execute(self, inputs, context),
            Kind::CurrentSeries { .. } => current_series::execute(self, inputs, context),
            Kind::ScopeTimestamp { .. } => scope_timestamp::execute(self, inputs, context),
            Kind::SeriesWindow { .. } => series_window::execute(self, inputs, context),
            Kind::SeriesLabels { .. }
            | Kind::SeriesBinary { .. }
            | Kind::SeriesHistogramQuantile { .. }
            | Kind::SeriesRelabel { .. } => series_labels::execute(self, inputs, context),
            Kind::Filter(_) => filter::execute(self, inputs, context),
            Kind::Limit { .. } => limit::execute(self, inputs, context),
            Kind::Sort { .. } => sort::execute(self, inputs, context),
            Kind::SQLWindowSum { .. } | Kind::Window { .. } | Kind::Aggregate { .. } => {
                aggregate::execute(self, inputs, context)
            }
            Kind::Join { .. } | Kind::SemiJoin { .. } => joins::execute(self, inputs, context),
            Kind::SummaryMerge { .. } => summary::execute_merge(self, inputs, context),
            Kind::SummaryBuild { .. }
            | Kind::Evaluation { .. }
            | Kind::KeyedSummaryBuild { .. }
            | Kind::SharedSummaryBuild { .. }
            | Kind::KeyedEvaluation { .. } => summary::execute(self, inputs, context),
        }
    }
}
