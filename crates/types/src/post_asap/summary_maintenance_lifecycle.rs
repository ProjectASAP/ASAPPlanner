//! Physical summary-maintenance lifecycle vocabulary.
//!
//! These choices are attached by physical planning; a `SummaryAgg` does not
//! imply continuous maintenance by itself. "Summary maintenance lifecycle"
//! is deliberately narrower than the end-to-end data lifecycle (collection,
//! transmission, storage, and analytics).

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

/// How long one planned summary state deployment exists.
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
