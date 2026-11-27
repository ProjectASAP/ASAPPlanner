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
        SketchAlgorithm::KLL,
        SketchAlgorithm::DDSketch,
        SketchAlgorithm::HLL,
        SketchAlgorithm::KMV,
        SketchAlgorithm::CountMinSketch,
        SketchAlgorithm::CountSketch,
        SketchAlgorithm::CountMinSketchWithHeap,
        SketchAlgorithm::CountSketchWithHeap,
        SketchAlgorithm::UnivMon,
    ];
    for algorithm in listed.iter().copied().chain([
        SketchAlgorithm::HydraCountMinSketch,
        SketchAlgorithm::HydraCountSketch,
        SketchAlgorithm::HydraKLL,
        SketchAlgorithm::HLLHip,
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
    assert_eq!(depths(SketchAlgorithm::CountMinSketch), [5, 7, 10, 12]);
    assert_eq!(depths(SketchAlgorithm::CountSketch), [47, 81, 117, 155]);
    assert_eq!(
        depths(SketchAlgorithm::CountMinSketchWithHeap),
        [5, 5, 5, 5]
    );
    assert_eq!(parameter_list(SketchAlgorithm::CountMinSketch).len(), 28);
    assert_eq!(
        parameter_list(SketchAlgorithm::CountMinSketchWithHeap).len(),
        280
    );
    assert_eq!(parameter_list(SketchAlgorithm::UnivMon).len(), 40);
}
