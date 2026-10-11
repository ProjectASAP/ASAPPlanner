//! Pass 2: ASAP-aware sharing across targets. A shared summary must meet the
//! strictest accuracy requirement of its readers.

pub mod reconciliation;
pub mod topk_reuse;
