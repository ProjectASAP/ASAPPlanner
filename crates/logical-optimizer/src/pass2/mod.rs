//! Pass 2: ASAP-aware sharing across targets. A shared summary must meet the
//! strictest accuracy requirement of its readers. The stage pipeline applies
//! the identical-expression rule ([`identical_expressions`]), the
//! summary-capability rule ([`summary_capability`]) and the
//! window-composition rule's tumbling windows and shared segments
//! ([`window_composition`]).

pub mod identical_expressions;
pub mod summary_capability;
pub mod window_composition;
