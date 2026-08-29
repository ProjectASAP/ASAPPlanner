//! Physical summary-maintenance lifecycle vocabulary.
//!
//! A **summary-maintenance lifecycle** describes when one materialized summary
//! state is created, retained or shared, updated, and retired. It does not
//! describe the broader data lifecycle (collection, transport, and storage),
//! and it is not implied by a logical `SummaryAgg`. Physical planning compares
//! alternatives using the expected number and timing of reads, the source-data
//! arrival/update rate, state-operation costs, and runtime capabilities.

use crate::workload::{DurationMs, TimestampMs};

/// When an operator is evaluated. This is independent of whether it owns
/// state and how long that state is retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EvaluationSchedule {
    OneShot,
    PerUpdate,
    OnRead,
}

/// The physical value crossing an execution boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputRepresentation {
    PlainRows,
    SummaryState,
    FinalizedValue,
}

/// Physical policy for when one materialized summary state is created,
/// retained or shared, updated as data arrives, and retired.
///
/// This is not the lifecycle of the source data or query. Query recurrence
/// provides the expected number and timing of reads; data arrival provides the
/// expected state-update demand. The planner combines those quantities with
/// costs and runtime capabilities to compare these policies.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SummaryMaintenanceLifecycle {
    Ephemeral,
    Prepared {
        activate_at: TimestampMs,
        retire_at: TimestampMs,
    },
    Shared {
        retention: DurationMs,
    },
    ContinuouslyMaintained,
}

/// The lifecycle commitment emitted for one materialized summary deployment.
///
/// This names the summary-maintenance promise explicitly so consumers do not
/// confuse it with guarantees about the broader data lifecycle. Accuracy is a
/// separate [`super::ResultGuarantee`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SummaryMaintenanceLifecycleGuarantee {
    pub summary_maintenance_lifecycle: SummaryMaintenanceLifecycle,
    pub evaluation_schedule: EvaluationSchedule,
    pub output_representation: OutputRepresentation,
}
