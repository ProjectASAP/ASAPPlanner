//! #509 Stage 2 decision data: exact-operator schema helpers for
//! materialization and the pane primitives of window summaries.

pub mod execution_data_state;
pub mod summary_window;

pub use execution_data_state::{lift_plain, ExactOperationSchemaError};
pub use summary_window::{
    plan_pane_phase, validate_pane_coverage, PaneCoverageError, PaneLayout, WindowEdgeCoverage,
};
