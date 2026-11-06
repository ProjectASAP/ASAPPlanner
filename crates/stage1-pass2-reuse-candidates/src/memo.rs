//! The logical memo's rules over the memo groups Stage 0 builds
//! (`asap_stage0_memo`).

pub(crate) mod rules;
/// Rule 1: tumbling-window layouts of a summary state group.
pub(crate) mod window_candidates;
