//! Count-Min Sketch L1 frequency error.
use super::*;

pub(super) fn guarantee(params: &SketchParams) -> Option<AccuracyGuarantee> {
    let (SketchParams::Cms { width, depth } | SketchParams::CmsWithHeap { width, depth, .. }) =
        params
    else {
        return None;
    };
    Some(super::bounded_guarantee(
        AccuracyMetric::ABSOLUTE_STREAM_L1,
        std::f64::consts::E / f64::from(*width),
        ProbabilityExpr::Constant {
            value: (-f64::from(*depth)).exp(),
        },
    ))
}

#[cfg(test)]
mod tests;
