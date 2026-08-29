//! Physical lifecycle vocabulary for summary state.
//!
//! These choices are attached by physical planning; a `SummaryAgg` does not
//! imply continuous maintenance by itself.

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
pub enum StateLifecycle {
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
