//! Native physical operators. Each module owns its constructors and execution.
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
mod filter;
mod joins;
mod limit;
mod projection;
mod sort;
mod source;
mod summary;
pub use aggregate::Reduction;
pub use sort::SortKey;
#[derive(Clone)]
enum Kind {
    Source(Vec<Batch>),
    Union,
    VectorToScalar {
        column: usize,
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
        statistic: crate::Statistic,
        parameters: std::collections::HashMap<String, String>,
    },
}
/// A bound operation has a fully checked input/output contract before execution.
#[derive(Clone)]
pub struct Operator {
    kind: Kind,
    inputs: Vec<Schema>,
    output: Schema,
}
impl Operator {
    pub(crate) fn is_counter_readout(&self) -> bool {
        matches!(
            self.kind,
            Kind::Readout {
                statistic: crate::Statistic::Rate | crate::Statistic::Increase,
                ..
            }
        )
    }
    pub(crate) fn with_counter_lookback(mut self, lookback: i64) -> Result<Self, Error> {
        if lookback <= 0 {
            return Err(invalid("counter lookback must be positive"));
        }
        if let Kind::Readout { parameters, .. } = &mut self.kind {
            parameters.insert("logical_lookback_ms".into(), lookback.to_string());
        }
        Ok(self)
    }
    pub(super) fn readout_parameters(
        &self,
        context: &RunContext,
    ) -> Result<std::collections::HashMap<String, String>, Error> {
        let Kind::Readout { parameters, .. } = &self.kind else {
            return Ok(Default::default());
        };
        let mut parameters = parameters.clone();
        if let Some(lookback) = parameters.remove("logical_lookback_ms") {
            let lookback: i64 = lookback
                .parse()
                .map_err(|_| invalid("invalid counter lookback"))?;
            let end = match context.scope {
                crate::runtime::Scope::Query {
                    evaluation_time_ms, ..
                } => evaluation_time_ms,
                crate::runtime::Scope::Ingestion { window_end_ms, .. } => window_end_ms,
            };
            let start = end
                .checked_sub(lookback)
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
            parameters.insert("range_start_ms".into(), start.to_string());
            parameters.insert("range_end_ms".into(), end.to_string());
        }
        Ok(parameters)
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
            Kind::Source(_) => Boundedness::Bounded,
            Kind::Limit { groups, .. } if groups.is_empty() => Boundedness::Bounded,
            _ => Boundedness::from_inputs(inputs),
        };
        PlanProperties {
            boundedness,
            emission: if self.requires_bounded_input() {
                Emission::AfterInput
            } else {
                Emission::Incremental
            },
        }
    }

    fn name(&self) -> &str {
        match self.kind {
            Kind::Source(_) => "Source",
            Kind::Union => "Union",
            Kind::VectorToScalar { .. } => "VectorToScalar",
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
        self.readout_parameters(context).map(|_| ())
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
            Kind::Source(_) | Kind::Union | Kind::VectorToScalar { .. } => {
                source::execute(self, inputs, context)
            }
            Kind::Project(_) => projection::execute(self, inputs, context),
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
