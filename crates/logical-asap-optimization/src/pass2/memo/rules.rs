//! Pass 2's rules over groups: each matches groups and their candidates'
//! expressions, then adds groups (`Memo::find_or_add_group`) and candidates
//! (`Memo::add_candidate`). Physical configuration and cost selection live
//! downstream.
pub(crate) mod finer_aggregate;
pub(super) mod reuse;
pub(crate) mod rollup;
pub(crate) mod scan_labels;
pub(crate) mod topk;
