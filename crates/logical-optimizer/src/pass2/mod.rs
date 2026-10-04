//! Pass 2: ASAP-aware sharing across targets. A shared summary must meet the
//! strictest accuracy requirement of its readers. The stage pipeline applies
//! the identical-expression rule ([`identical_expressions`]).

pub mod identical_expressions;
pub mod reconciliation;
pub mod topk_reuse;
