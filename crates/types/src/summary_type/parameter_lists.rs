//! Each summary algorithm's predefined parameter list $\boldsymbol\theta$,
//! beside [`SketchParams`].

use super::{SketchAlgorithm, SketchParams};

/// The algorithm's predefined $\boldsymbol\theta$ list: structures only; the
/// error of each, for a statistic at a failure probability, is its guarantee
/// (`asap_accuracy::theoretical_guarantee`).
pub fn parameter_list(algorithm: SketchAlgorithm) -> &'static [SketchParams] {
    let _ = algorithm;
    todo!("#1207 PR 1: the predefined θ list per algorithm")
}
