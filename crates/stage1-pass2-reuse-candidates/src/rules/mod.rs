//! Pass 2's rules over groups (concepts doc, section 3, "Stage 1, Pass 2"):
//! each reads the memo and adds groups (`Memo::find_or_add_group`) or
//! candidates (`Memo::add_candidate`, through `candidates`); none copies a
//! consumer, so every consumer of a group sees what a rule adds to it.
pub(crate) mod candidates;
pub(crate) mod chains;
pub(crate) mod rollup;
pub(crate) mod topk;
