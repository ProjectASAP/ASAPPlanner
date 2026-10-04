//! Pass 2: ASAP-aware sharing across targets. A shared summary must meet the
//! strictest accuracy requirement of its readers. The stage pipeline applies
//! the identical-expression rule ([`identical_expressions`]) and the
//! summary-capability rule ([`summary_capability`]).

pub mod identical_expressions;
pub mod reconciliation;
pub mod summary_capability;
pub mod topk_reuse;
