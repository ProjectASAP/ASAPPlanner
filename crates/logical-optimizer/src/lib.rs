//! `asap-logical-optimizer` — #509 Stage 1: logical candidate generation.
//!
//! It takes the pre-ASAP [`OperatorNode`](asap_types::ir::OperatorNode) DAGs a
//! front end produces and lists the logical alternatives for each target
//! sub-DAG: which summary (if any) realizes each aggregate intent. It never
//! prices a plan: only Stage 3 uses the cost model (#572, decision Q36(a)).
//! Cargo enforces the stage order: this crate depends only on `asap-types`,
//! never on a front end, a later stage or the executor.
//!
//! - [`pass1`] — local alternatives per target sub-DAG
//!   ([`pass1::logical_candidates`], the stage pipeline's Stage 1 entry point).
//! - [`pass2`] — ASAP-aware sharing across targets.
//! - [`accuracy`] — the analytical accuracy of each summary family: error
//!   bounds and sizing ([`accuracy::estimators`]), which Stage 3's accuracy
//!   model delegates to.
//!
//! **Common sub-expression elimination (CSE) of identical sub-DAGs is not
//! implemented here.** It runs over the pre-ASAP IR itself
//! (`asap_types::ir::cse`, issues #222/#223). Pass 2's identical-expression
//! rule ([`pass2::identical_expressions`]) decides when to use it: the stage
//! pipeline keeps a variant with and without it. Pass 2 also recognizes
//! sharing that is invisible at that level, such as `Quantile(x, 0.99)` and
//! `Quantile(x, 0.95)` reading one built sketch.

pub mod accuracy;
pub mod pass1;
pub mod pass2;
#[cfg(test)]
mod test_support;

pub use pass1::realization::{has_subpopulations, summary_candidates, Realization};
