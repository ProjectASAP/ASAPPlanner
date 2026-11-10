//! The workload the planner plans for, by scope: the query workload
//! (`query`), the data workload (`data`), the two planned together
//! (`planning`), and the bounds read from them ([`bounds`]).

pub mod bounds;
mod data;
pub mod parsed_workload;
mod planning;
mod query;

pub use data::{
    DataArrival, DataWorkload, LabelSetDistinctValues, PrometheusMetricType, Rate, SourceFacts,
    StreamNorms,
};
pub use planning::{PlanningWorkload, WorkloadError};
pub use query::{
    greatest_common_divisor, BatchEntry, DemandEstimate, DurationMs, Predictability, Query,
    QueryLanguage, QueryRecurrence, QueryRequirements, QueryWorkload, QueryWorkloadEntry,
    RangeEvaluation, RepeatedDemand, RepeatingEntry, RepetitionInterval, SqlDialect, TimestampMs,
};
