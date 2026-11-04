//! The logical memo's candidates, rules and enumeration, over the memo
//! groups Stage 0 builds (`crate::stage0`).

/// The candidate DAGs a memo stands for: choices and their composition.
pub(crate) mod enumeration;
pub(crate) mod rules;
/// Shared finite state configuration enumeration.
pub(crate) mod shared_configurations;
/// Concrete summary-state groups exposed by sharing rules.
pub(crate) mod state_groups;
/// Local tumbling-window candidates with selected-reader grid configurations.
pub(crate) mod window_candidates;
