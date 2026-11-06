//! The logical memo's rules over the memo groups Stage 0 builds
//! (`crate::stage0`).

pub(crate) mod rules;
/// Rule 1: tumbling-window layouts of a summary state group.
pub(crate) mod window_candidates;
