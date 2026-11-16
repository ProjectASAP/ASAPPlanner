//! The workload the planner plans for, by scope: the query workload
//! (`query`), the data workload (`data`), the two planned together
//! (`planning`), the bounds read from them ([`bounds`]), and the data facts
//! a plan assumed because the data workload does not declare them
//! (`assumed_data_fact`).

mod assumed_data_fact;
pub mod bounds;
mod data;
pub mod parsed_workload;
mod planning;
mod query;

pub use assumed_data_fact::{AssumedDataFact, DataFactAssumption};
pub(crate) use data::series_facts;
pub use data::{
    DataArrival, DataWorkload, LabelSetDistinctValues, PrometheusMetricType, Rate, SourceFacts,
    StreamNorms, ValueMagnitudeRange, ONE_SECOND_MS,
};
pub use planning::{PlanningWorkload, WorkloadError};
pub use query::{
    greatest_common_divisor, BatchEntry, DemandEstimate, DurationMs, Predictability, Query,
    QueryLanguage, QueryRecurrence, QueryRequirements, QueryWorkload, QueryWorkloadEntry,
    RangeEvaluation, RepeatedDemand, RepeatingEntry, RepetitionInterval, SqlDialect, TimestampMs,
};
