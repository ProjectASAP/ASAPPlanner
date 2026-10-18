//! #509 Stage 2 decision data: the materialization of each sub-DAG and the
//! window-summary panes it may build.
//!
//! - [`materialization`] — whether a sub-DAG's output is stored and when it
//!   is computed ([`Materialization`], [`Medium`]).
//! - [`summary_window`] — summary-window pane primitives.
pub mod materialization;
pub mod summary_window;

pub use materialization::{Materialization, Medium};
pub use summary_window::{
    plan_pane_phase, validate_pane_coverage, PaneCoverageError, PaneLayout, WindowEdgeCoverage,
};
