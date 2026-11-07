//! Helpers that only other crates' tests call, exposed under the
//! `test-support` feature so the normal public API stays as narrow as its
//! production users need. Each forwards to the crate-private function or
//! constant of the same name.

use asap_types::ir::properties::guarantee::AccuracyGuarantee;
use asap_types::ir::properties::AccuracyTarget;

/// `crate::satisfies`.
pub fn satisfies(guarantee: &AccuracyGuarantee, target: &AccuracyTarget) -> bool {
    crate::satisfies(guarantee, target)
}

/// `crate::estimators::cms::cms_depth`.
pub fn cms_depth(delta: f64) -> u32 {
    crate::estimators::cms::cms_depth(delta)
}

/// `crate::estimators::kll::kll_rank_error_99`.
pub fn kll_rank_error_99(k: u32) -> f64 {
    crate::estimators::kll::kll_rank_error_99(k)
}

/// `crate::sizing::DEFAULT_DELTA`.
pub const DEFAULT_DELTA: f64 = crate::sizing::DEFAULT_DELTA;
