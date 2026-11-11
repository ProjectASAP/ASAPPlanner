//! Each summary algorithm's predefined parameter list $\boldsymbol\theta$,
//! beside [`SketchParams`].

use super::{SketchAlgorithm, SketchParams};

/// The algorithm's predefined $\boldsymbol\theta$ list: structures only; the
/// error of each, for a statistic at a failure probability, is its guarantee
/// (`asap_accuracy::theoretical_guarantee`).
///
/// The widths, `k`s, precisions, `alpha`s and heap sizes span the errors
/// ε = 0.1, 0.05, 0.02, 0.01, 0.005, 0.002 and 0.001, smallest first,
/// without repeats; an `asap_accuracy` test checks every entry has a
/// guarantee at δ = 0.01. Distinct counts (KMV,
/// and UnivMon's distinct-count shapes) stop at ε = 0.01. Where δ is set by
/// a structural parameter, the list covers the δ levels with it: Count-Min
/// lists each width with each depth of [`CMS_DEPTHS`], Count Sketch with
/// each of [`COUNT_SKETCH_DEPTHS`]. A
/// UnivMon's list is its L2-norm shapes, its distinct-count shapes, each L2
/// shape combined with each distinct-count shape, then its shape for every
/// other statistic. A heap variant combines each width × depth with each
/// heap size of [`HEAP_SIZES`]: a top-k needs heap_size ≥ k, and the next
/// listed step up is at most 2.5× too large. Empty for an algorithm without
/// an error model (the Hydra algorithms included), so no build of it is a
/// valid candidate.
pub fn parameter_list(algorithm: SketchAlgorithm) -> &'static [SketchParams] {
    match algorithm {
        SketchAlgorithm::Kll => &KLL_PARAMETERS,
        SketchAlgorithm::DDSketch => &DDSKETCH_PARAMETERS,
        SketchAlgorithm::Hll => &HLL_PARAMETERS,
        SketchAlgorithm::Kmv => &KMV_PARAMETERS,
        SketchAlgorithm::Cms => &CMS_PARAMETERS,
        SketchAlgorithm::CountSketch => &COUNT_SKETCH_PARAMETERS,
        SketchAlgorithm::CmsWithHeap => &CMS_WITH_HEAP_PARAMETERS,
        SketchAlgorithm::CountSketchWithHeap => &COUNT_SKETCH_WITH_HEAP_PARAMETERS,
        SketchAlgorithm::UnivMon => &UNIVMON_PARAMETERS,
        // Listed, not `_`: an algorithm `asap_sketchlib` adds fails to
        // compile here until it gets a list or is listed without one.
        SketchAlgorithm::CountSketchWithL2Norm
        | SketchAlgorithm::CmsWithSpaceSaving
        | SketchAlgorithm::CountSketchWithSpaceSaving
        | SketchAlgorithm::CmsFold
        | SketchAlgorithm::CountSketchFold
        | SketchAlgorithm::HllHip
        | SketchAlgorithm::KllDynamic
        | SketchAlgorithm::SpaceSaving
        | SketchAlgorithm::Elastic
        | SketchAlgorithm::Coco
        | SketchAlgorithm::Bloom
        | SketchAlgorithm::SetAggregator
        | SketchAlgorithm::UnivMonTwoTier
        | SketchAlgorithm::UnivMonQ
        | SketchAlgorithm::HydraCms
        | SketchAlgorithm::HydraCountSketch
        | SketchAlgorithm::HydraKll
        | SketchAlgorithm::HydraHll
        | SketchAlgorithm::HydraUnivMon => &[],
    }
}

const KLL_PARAMETERS: [SketchParams; 7] = [
    SketchParams::Kll { k: 26 },
    SketchParams::Kll { k: 52 },
    SketchParams::Kll { k: 132 },
    SketchParams::Kll { k: 269 },
    SketchParams::Kll { k: 547 },
    SketchParams::Kll { k: 1_404 },
    SketchParams::Kll { k: 2_863 },
];

const DDSKETCH_PARAMETERS: [SketchParams; 7] = [
    SketchParams::DDSketch { alpha: 0.1 },
    SketchParams::DDSketch { alpha: 0.05 },
    SketchParams::DDSketch { alpha: 0.02 },
    SketchParams::DDSketch { alpha: 0.01 },
    SketchParams::DDSketch { alpha: 0.005 },
    SketchParams::DDSketch { alpha: 0.002 },
    SketchParams::DDSketch { alpha: 0.001 },
];

/// HLL's precision saturates at 18 from ε = 0.01 on.
const HLL_PARAMETERS: [SketchParams; 4] = [
    SketchParams::Hll { precision: 13 },
    SketchParams::Hll { precision: 15 },
    SketchParams::Hll { precision: 17 },
    SketchParams::Hll { precision: 18 },
];

const KMV_PARAMETERS: [SketchParams; 4] = [
    SketchParams::Kmv { k: 1_118 },
    SketchParams::Kmv { k: 4_323 },
    SketchParams::Kmv { k: 26_656 },
    SketchParams::Kmv { k: 106_263 },
];

/// The depths each Count-Min width is listed with: δ = e^−d ≤ 1e-2, 1e-3,
/// 1e-4 and 1e-5.
const CMS_DEPTHS: [u32; 4] = [5, 7, 10, 12];
/// The depths each Count Sketch width is listed with, for the same δ
/// levels: Count Sketch reads the median of its rows, each wrong with
/// probability about 1/3, so δ = P(Binomial(d, 1/3) ≥ (d+1)/2), the
/// smallest odd d at or below each level.
const COUNT_SKETCH_DEPTHS: [u32; 4] = [47, 81, 117, 155];

const CMS_WIDTHS: [u32; 7] = [28, 55, 136, 272, 544, 1_360, 2_719];
const COUNT_SKETCH_WIDTHS: [u32; 7] = [300, 1_200, 7_500, 30_000, 120_000, 750_000, 3_000_000];

const CMS_PARAMETERS: [SketchParams; 28] = with_depths(
    &CMS_WIDTHS,
    &CMS_DEPTHS,
    SketchParams::Cms { width: 0, depth: 0 },
);
const COUNT_SKETCH_PARAMETERS: [SketchParams; 28] = with_depths(
    &COUNT_SKETCH_WIDTHS,
    &COUNT_SKETCH_DEPTHS,
    SketchParams::CountSketch { width: 0, depth: 0 },
);

/// Each of `widths` with each of `depths`, by width, then depth, as
/// `shape`'s variant (`Cms` or `CountSketch`).
const fn with_depths(
    widths: &[u32; 7],
    depths: &[u32; 4],
    shape: SketchParams,
) -> [SketchParams; 28] {
    const UNSET: SketchParams = SketchParams::Cms { width: 0, depth: 0 };
    let mut list = [UNSET; 28];
    let mut width = 0;
    while width < widths.len() {
        let mut depth = 0;
        while depth < depths.len() {
            let (width_value, depth_value) = (widths[width], depths[depth]);
            list[width * depths.len() + depth] = match shape {
                SketchParams::Cms { .. } => SketchParams::Cms {
                    width: width_value,
                    depth: depth_value,
                },
                SketchParams::CountSketch { .. } => SketchParams::CountSketch {
                    width: width_value,
                    depth: depth_value,
                },
                _ => panic!("a Count-Min or Count Sketch shape"),
            };
            depth += 1;
        }
        width += 1;
    }
    list
}

/// The heap sizes of a heap variant's list: the 1-2-5 series from 10 to
/// 10 000, so a top-k with k ≤ 10 000 has a listed heap.
const HEAP_SIZES: [u32; 10] = [10, 20, 50, 100, 200, 500, 1_000, 2_000, 5_000, 10_000];

const CMS_WITH_HEAP_PARAMETERS: [SketchParams; 280] = with_heap_sizes(&CMS_PARAMETERS);
const COUNT_SKETCH_WITH_HEAP_PARAMETERS: [SketchParams; 280] =
    with_heap_sizes(&COUNT_SKETCH_PARAMETERS);

/// Each `Cms` (or `CountSketch`) shape of `shapes` with each heap size of
/// [`HEAP_SIZES`], as `CmsWithHeap` (or `CountSketchWithHeap`).
const fn with_heap_sizes(shapes: &[SketchParams; 28]) -> [SketchParams; 280] {
    const UNSET: SketchParams = SketchParams::Cms { width: 0, depth: 0 };
    let mut list = [UNSET; 280];
    let mut shape = 0;
    while shape < shapes.len() {
        let mut heap = 0;
        while heap < HEAP_SIZES.len() {
            let heap_size = HEAP_SIZES[heap];
            list[shape * HEAP_SIZES.len() + heap] = match shapes[shape] {
                SketchParams::Cms { width, depth } => SketchParams::CmsWithHeap {
                    width,
                    depth,
                    heap_size,
                },
                SketchParams::CountSketch { width, depth } => SketchParams::CountSketchWithHeap {
                    width,
                    depth,
                    heap_size,
                },
                _ => panic!("a heap variant extends a Cms or CountSketch shape"),
            };
            heap += 1;
        }
        shape += 1;
    }
    list
}

/// UnivMon's L2-norm column counts and distinct-count heap sizes.
const UNIVMON_L2_COLUMNS: [u32; 7] = [624, 2_367, 14_349, 56_817, 226_130, 1_409_067, 5_630_630];
const UNIVMON_DISTINCT_HEAP_SIZES: [u32; 4] = [13_500, 54_000, 337_500, 1_350_000];

const UNIVMON_PARAMETERS: [SketchParams; 40] = univmon_parameters();

/// The L2-norm shapes, the distinct-count shapes, then each L2 column
/// count × distinct heap size (by column count, then heap size), last the
/// shape for every other statistic.
const fn univmon_parameters() -> [SketchParams; 40] {
    const fn univmon(
        heap_size: u32,
        sketch_rows: u32,
        sketch_cols: u32,
        layers: u8,
    ) -> SketchParams {
        SketchParams::UnivMon {
            heap_size,
            sketch_rows,
            sketch_cols,
            layers,
        }
    }
    const BASELINE: SketchParams = univmon(256, 5, 1_024, 16);
    let mut list = [BASELINE; 40];
    let mut next = 0;
    let mut columns = 0;
    while columns < UNIVMON_L2_COLUMNS.len() {
        list[next] = univmon(1, 5, UNIVMON_L2_COLUMNS[columns], 1);
        next += 1;
        columns += 1;
    }
    let mut heap = 0;
    while heap < UNIVMON_DISTINCT_HEAP_SIZES.len() {
        list[next] = univmon(UNIVMON_DISTINCT_HEAP_SIZES[heap], 1, 1, 19);
        next += 1;
        heap += 1;
    }
    columns = 0;
    while columns < UNIVMON_L2_COLUMNS.len() {
        heap = 0;
        while heap < UNIVMON_DISTINCT_HEAP_SIZES.len() {
            list[next] = univmon(
                UNIVMON_DISTINCT_HEAP_SIZES[heap],
                5,
                UNIVMON_L2_COLUMNS[columns],
                19,
            );
            next += 1;
            heap += 1;
        }
        columns += 1;
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each predefined list holds one parameter type, without repeats, and
    /// is empty exactly for the algorithms without an error model. Count-Min
    /// lists each width with depths 5, 7, 10 and 12, Count Sketch with 47,
    /// 81, 117 and 155, and a
    /// heap variant each of those with every heap size; UnivMon's list has 40
    /// entries.
    #[test]
    fn parameter_list_holds_its_algorithm_without_repeats() {
        let listed = [
            SketchAlgorithm::Kll,
            SketchAlgorithm::DDSketch,
            SketchAlgorithm::Hll,
            SketchAlgorithm::Kmv,
            SketchAlgorithm::Cms,
            SketchAlgorithm::CountSketch,
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap,
            SketchAlgorithm::UnivMon,
        ];
        for algorithm in listed.iter().copied().chain([
            SketchAlgorithm::HydraCms,
            SketchAlgorithm::HydraCountSketch,
            SketchAlgorithm::HydraKll,
            SketchAlgorithm::HllHip,
        ]) {
            let list = parameter_list(algorithm);
            assert_eq!(
                list.is_empty(),
                !listed.contains(&algorithm),
                "{algorithm:?}"
            );
            for (i, parameters) in list.iter().enumerate() {
                assert_eq!(
                    std::mem::discriminant(parameters),
                    std::mem::discriminant(&list[0])
                );
                assert!(!list[..i].contains(parameters), "{parameters:?} repeats");
            }
        }
        let depths = |algorithm| -> Vec<u32> {
            parameter_list(algorithm)
                .iter()
                .take(4)
                .map(|parameters| match parameters {
                    SketchParams::Cms { depth, .. }
                    | SketchParams::CountSketch { depth, .. }
                    | SketchParams::CmsWithHeap { depth, .. }
                    | SketchParams::CountSketchWithHeap { depth, .. } => *depth,
                    other => panic!("{other:?}"),
                })
                .collect()
        };
        assert_eq!(depths(SketchAlgorithm::Cms), [5, 7, 10, 12]);
        assert_eq!(depths(SketchAlgorithm::CountSketch), [47, 81, 117, 155]);
        assert_eq!(depths(SketchAlgorithm::CmsWithHeap), [5, 5, 5, 5]);
        assert_eq!(parameter_list(SketchAlgorithm::Cms).len(), 28);
        assert_eq!(parameter_list(SketchAlgorithm::CmsWithHeap).len(), 280);
        assert_eq!(parameter_list(SketchAlgorithm::UnivMon).len(), 40);
    }
}
