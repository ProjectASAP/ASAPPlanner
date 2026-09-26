//! UnivMon currently certifies only its exact unit-update total readout.
use super::*;

pub(super) fn guarantee(query: &SketchQuery) -> Option<ResultGuarantee> {
    matches!(query, SketchQuery::PointCount { value: None, .. })
        .then(|| ResultGuarantee::exact("univmon_unit_update_total"))
}

pub(super) fn size_params() -> SketchParams {
    // Baseline dimensions are candidates, not an inverted accuracy bound.
    SketchParams::UnivMon {
        heap_size: 256,
        sketch_rows: 5,
        sketch_cols: 1024,
        layers: 16,
    }
}
