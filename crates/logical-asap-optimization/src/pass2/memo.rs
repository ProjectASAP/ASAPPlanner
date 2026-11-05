//! The logical memo's candidates, rules and enumeration, over the memo
//! groups Stage 0 builds (`crate::stage0`).

/// The candidate DAGs a memo stands for: choices and their composition.
pub(crate) mod enumeration;
pub(crate) mod rules;
/// Tumbling-window widths from the queries' evaluation grids.
pub(crate) mod tumbling_window_widths;
/// Pass 2's error and its check of the memo before its rules.
pub(crate) mod well_formed;
/// Local tumbling-window candidates (rule 1).
pub(crate) mod window_candidates;
