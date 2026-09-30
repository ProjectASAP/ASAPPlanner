//! Edge sampling probability shared by sketches built from sampled edge frames.
//!
//! An edge that admits each update (DDSketch, Count-Min, Count Sketch) or each
//! distinct key (HLL) with probability `p` stores ~`p`x the true counts, so
//! count, frequency and cardinality readouts scale by `1/p`. `p = 1` means
//! unsampled.
use crate::KernelError;

pub(crate) fn checked(sample_p: f64) -> Result<f64, KernelError> {
    if sample_p > 0.0 && sample_p <= 1.0 {
        Ok(sample_p)
    } else {
        Err(format!("edge sample probability {sample_p} is outside (0, 1]").into())
    }
}

/// `p` is a per-series constant, but a window-reset or Planner-created base is
/// unsampled (`1`); merging it with a sampled state keeps the sampled `p`.
/// Two different sampled probabilities have no common scale.
pub(crate) fn merged(left: f64, right: f64) -> Result<f64, KernelError> {
    if left == right || right == 1.0 {
        Ok(left)
    } else if left == 1.0 {
        Ok(right)
    } else {
        Err(format!("cannot merge states sampled at p={left} and p={right}").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Only probabilities in (0, 1] are accepted.
    #[test]
    fn checked_accepts_only_unit_interval() {
        for p in [0.5, 1.0] {
            assert_eq!(checked(p).unwrap(), p);
        }
        for p in [0.0, -0.1, 1.5, f64::NAN, f64::INFINITY] {
            assert!(checked(p).is_err(), "{p}");
        }
    }

    // Merge keeps a shared p, prefers a sampled p over unsampled, and rejects two sampled p's.
    #[test]
    fn merge_rule() {
        assert_eq!(merged(0.25, 0.25).unwrap(), 0.25);
        assert_eq!(merged(0.25, 1.0).unwrap(), 0.25);
        assert_eq!(merged(1.0, 0.25).unwrap(), 0.25);
        assert_eq!(merged(1.0, 1.0).unwrap(), 1.0);
        assert!(merged(0.25, 0.5).is_err());
    }
}
