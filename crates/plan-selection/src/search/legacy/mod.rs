//! The current search: whole-workload candidates, split into independent
//! components and with interchangeable queries collapsed.

pub(crate) mod components;
pub(crate) mod interchangeable;
/// Costed reuse of compatible physical pane producers.
pub mod pane_sharing;
pub(crate) mod query_candidates;
pub(crate) mod stage1_to_stage3;
