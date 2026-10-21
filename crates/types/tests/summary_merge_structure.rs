//! Window composition merges compatible summary states without consuming raw rows.
use asap_types::ir::operator::operator_properties::{Reduction, Source};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::scalar::expr_ir::{CompareOpKind, ScalarValue};
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::{
    DataType, ExactKind, ExactParams, FamilyCapability, Field, FieldDataType, GroupingStrategy,
    HydraKind, HydraParams, MergeInputRule, MergedError, SamplingKind, SamplingParams, Schema,
    SketchAlgorithm, SketchKind, SketchParams, SketchStatistic, StatModelKind, StatModelParams,
    StatisticKind, SummaryUpdate, WaveletKind, WaveletParams,
};
use asap_types::ir::{
    apply_materialization_timings, ASAPOp, ExprSemantics, MaterializationAssignment, NonASAPOp,
    Operator, OperatorNode, Predicate, ScalarExpr, TimingMemo,
};
use asap_types::physical::{Materialization, Medium};
use std::rc::Rc;
/// KLL over `value` for one `region`, so states of different regions are
/// disjoint.
fn state(k: u32, region: &str) -> Rc<OperatorNode> {
    family_state(
        FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::Kll,
                SketchParams::Kll {
                    k,
                    failure_probability: None,
                },
            ),
            Default::default(),
        ),
        region,
    )
}
/// A `family` state over `value` for one `region`.
fn family_state(family: FieldDataType, region: &str) -> Rc<OperatorNode> {
    let scan = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "latencies".into(),
        },
        predicates: vec![],
        schema: Schema::new(vec![
            Field::plain("region", DataType::Utf8, false),
            Field::plain("value", DataType::Float64, false),
        ]),
    }))
    .unwrap();
    let only_region = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter {
        pred: Predicate(ScalarExpr::Compare {
            left: Box::new(ScalarExpr::Column(0)),
            op: CompareOpKind::Eq,
            right: Box::new(ScalarExpr::Literal(ScalarValue::Utf8(region.into()))),
            semantics: ExprSemantics::Sql,
        }),
        child: scan,
    }))
    .unwrap();
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: only_region,
        family,
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::by(vec![]),
        grouping: GroupingStrategy::default(),
        filter: None,
    }))
    .unwrap()
}
/// Two KLL panes compose into one typed logical state without timing assignment.
#[test]
fn compatible_panes_merge_structurally() {
    let root = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge {
        children: vec![state(200, "us"), state(200, "eu")],
    }))
    .unwrap();
    root.validate_structure().unwrap();
    assert_eq!(root.schema.fields.len(), 1);
}
/// An empty merge, raw rows and differently sized state cannot masquerade as compatible panes.
#[test]
fn incompatible_merge_inputs_fail() {
    for children in [
        vec![],
        vec![state(200, "us"), state(300, "eu")],
        vec![state(200, "us").children()[0].clone()],
    ] {
        assert!(
            OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge { children })).is_err()
        );
    }
}

fn sketch(algorithm: SketchAlgorithm, params: SketchParams) -> FieldDataType {
    FieldDataType::Sketch(SketchKind::new(algorithm, params), Default::default())
}
fn hydra(
    algorithm: SketchAlgorithm,
    params: SketchParams,
    kind: HydraKind,
    hydra_params: HydraParams,
) -> FieldDataType {
    FieldDataType::Sketch(
        SketchKind::new(algorithm, params),
        GroupingStrategy::SharedMultiSubpopulation {
            kind,
            params: hydra_params,
        },
    )
}
fn exact(kind: ExactKind, params: ExactParams) -> FieldDataType {
    FieldDataType::ExactAggregate(kind, params)
}
fn kll() -> FieldDataType {
    sketch(
        SketchAlgorithm::Kll,
        SketchParams::Kll {
            k: 200,
            failure_probability: None,
        },
    )
}
fn hll() -> FieldDataType {
    sketch(
        SketchAlgorithm::Hll,
        SketchParams::Hll {
            precision: 12,
            relative_error: None,
        },
    )
}
fn cms_with_heap() -> FieldDataType {
    sketch(
        SketchAlgorithm::CmsWithHeap,
        SketchParams::CmsWithHeap {
            width: 64,
            depth: 4,
            heap_size: 10,
        },
    )
}
fn merge(children: Vec<Rc<OperatorNode>>) -> Result<Rc<OperatorNode>, String> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryMerge { children }))
        .map_err(|e| e.to_string())
}

/// The family-capability table, row by row: how each family's merge
/// inputs must relate, and which statistics one state answers with the
/// error of each from a merged state. Every statistic kind is probed
/// through `answers` and `merged_error`, so the table and both lookups are
/// pinned together.
#[test]
fn family_capability_table() {
    use MergeInputRule::{Disjoint, DisjointInTime, MayOverlap};
    use MergedError::{Approximate, AsIfBuiltOverUnion as Union};
    use StatisticKind::*;
    let (width, depth, heap_size) = (64, 4, 10);
    // (family, how its merge inputs must relate, its readouts).
    type Row = (
        FieldDataType,
        Option<MergeInputRule>,
        Vec<(StatisticKind, MergedError)>,
    );
    let rows: Vec<Row> = vec![
        (
            exact(ExactKind::Sum, ExactParams::Sum),
            Some(Disjoint),
            vec![],
        ),
        (
            exact(ExactKind::Count, ExactParams::Count),
            Some(Disjoint),
            vec![],
        ),
        (
            exact(ExactKind::Min, ExactParams::Min),
            Some(MayOverlap),
            vec![],
        ),
        (
            exact(ExactKind::Max, ExactParams::Max),
            Some(MayOverlap),
            vec![],
        ),
        // Counter panes merge exactly when ordered and disjoint in time.
        (
            exact(ExactKind::Rate, ExactParams::Rate),
            Some(DisjointInTime),
            vec![],
        ),
        (
            exact(ExactKind::Increase, ExactParams::Increase),
            Some(DisjointInTime),
            vec![],
        ),
        (exact(ExactKind::IRate, ExactParams::IRate), None, vec![]),
        (kll(), Some(Disjoint), vec![(Quantile, Union)]),
        (
            sketch(
                SketchAlgorithm::DDSketch,
                SketchParams::DDSketch { alpha: 0.01 },
            ),
            Some(Disjoint),
            vec![(Quantile, Union), (Total, Union)],
        ),
        (hll(), Some(MayOverlap), vec![(Distinct, Union)]),
        (
            sketch(SketchAlgorithm::Cms, SketchParams::Cms { width, depth }),
            Some(Disjoint),
            vec![(ItemFrequency, Union), (Total, Union)],
        ),
        (
            sketch(
                SketchAlgorithm::CountSketch,
                SketchParams::CountSketch { width, depth },
            ),
            Some(Disjoint),
            vec![(ItemFrequency, Union), (FrequencyL2, Union)],
        ),
        (
            sketch(
                SketchAlgorithm::UnivMon,
                SketchParams::UnivMon {
                    heap_size,
                    sketch_rows: depth,
                    sketch_cols: width,
                    layers: 8,
                    distinct_failure_probability: None,
                },
            ),
            Some(Disjoint),
            vec![
                (Total, Union),
                (Distinct, Union),
                (FrequencyL2, Union),
                (FrequencyEntropy, Approximate),
            ],
        ),
        // KMV keeps the k smallest hashes under one shared seed: the merge
        // is the k smallest of the union and ignores repeated hashes.
        (
            sketch(
                SketchAlgorithm::Kmv,
                SketchParams::Kmv {
                    k: 1024,
                    relative_error: None,
                },
            ),
            Some(MayOverlap),
            vec![(Distinct, Union)],
        ),
        (
            sketch(SketchAlgorithm::Theta, SketchParams::Theta { k: 1024 }),
            None,
            vec![(Distinct, Approximate)],
        ),
        (
            cms_with_heap(),
            Some(Disjoint),
            vec![(TopK, Approximate), (ItemFrequency, Union)],
        ),
        (
            sketch(
                SketchAlgorithm::CountSketchWithHeap,
                SketchParams::CountSketchWithHeap {
                    width,
                    depth,
                    heap_size,
                },
            ),
            Some(Disjoint),
            vec![(TopK, Approximate), (ItemFrequency, Union)],
        ),
        // Hydra over a linear sketch is a grid of linear sketches; Hydra
        // over KLL has no accuracy model and fails closed.
        (
            hydra(
                SketchAlgorithm::Cms,
                SketchParams::Cms { width, depth },
                HydraKind::HydraCms,
                HydraParams::HydraCms {
                    width,
                    depth,
                    shared_rows: depth,
                    shared_columns: width,
                },
            ),
            Some(Disjoint),
            vec![(ItemFrequency, Union)],
        ),
        (
            hydra(
                SketchAlgorithm::CountSketch,
                SketchParams::CountSketch { width, depth },
                HydraKind::HydraCountSketch,
                HydraParams::HydraCountSketch {
                    width,
                    depth,
                    shared_rows: depth,
                    shared_columns: width,
                },
            ),
            Some(Disjoint),
            vec![(ItemFrequency, Union)],
        ),
        (
            hydra(
                SketchAlgorithm::Kll,
                SketchParams::Kll {
                    k: 200,
                    failure_probability: None,
                },
                HydraKind::HydraKll,
                HydraParams::HydraKll {
                    k: 200,
                    shared_buckets: width,
                },
            ),
            None,
            vec![(Quantile, Approximate)],
        ),
        (FieldDataType::Plain(DataType::Float64), None, vec![]),
        (
            FieldDataType::SummarySample(
                SamplingKind::Reservoir,
                SamplingParams::Reservoir { size: 64 },
            ),
            None,
            vec![],
        ),
        (
            FieldDataType::Wavelet(WaveletKind::Haar, WaveletParams::Haar { coefficients: 16 }),
            None,
            vec![],
        ),
        (
            FieldDataType::StatModel(
                StatModelKind::Parametric,
                StatModelParams::Parametric {
                    family: "gaussian_mixture".into(),
                },
            ),
            None,
            vec![],
        ),
    ];
    // One statistic of every kind.
    let statistics = [
        SketchStatistic::PointCount {
            key: ColumnRef::SampleValue,
            value: None,
        },
        SketchStatistic::PointCount {
            key: ColumnRef::Named("item".into()),
            value: Some("a".into()),
        },
        SketchStatistic::Quantile { q: 0.99 },
        SketchStatistic::Cardinality,
        SketchStatistic::FrequencyL2,
        SketchStatistic::FrequencyEntropy,
        SketchStatistic::TopK { k: 5 },
    ];
    for (family, merge_inputs, readouts) in rows {
        let capability = FamilyCapability::of(&family);
        assert_eq!(capability.merge_inputs, merge_inputs, "{family:?}");
        assert_eq!(capability.readouts, readouts.as_slice(), "{family:?}");
        for statistic in &statistics {
            let row = readouts
                .iter()
                .find(|(kind, _)| *kind == statistic.kind())
                .map(|&(_, error)| error);
            assert_eq!(
                capability.answers(statistic),
                row.is_some(),
                "{family:?} {statistic:?}"
            );
            // An answered statistic's merged error is its row's; the
            // default for the others is pinned below.
            if row.is_some() || merge_inputs.is_none() {
                assert_eq!(
                    capability.merged_error(statistic),
                    merge_inputs.and(row),
                    "{family:?} {statistic:?}"
                );
            }
        }
    }
}

/// Heap readouts (entropy, top-k) of a merged state are approximate: each
/// part's heaps keep only that part's heavy items. UnivMon's L2 reads layer
/// 0's Count Sketch, which adds, and its distinct count's bound holds for
/// any heap that keeps `min(|S_j|, k)` of a layer's keys, as the merge's
/// does, so both keep the guarantee of one build over the union. Counter
/// readouts of a heap family (item frequencies) keep their guarantee. A
/// statistic the family does not answer takes the family's default.
#[test]
fn merged_error_depends_on_the_readout() {
    use SketchStatistic::{Cardinality, FrequencyEntropy, FrequencyL2, TopK};
    let point = || SketchStatistic::PointCount {
        key: ColumnRef::Named("k".into()),
        value: None,
    };
    let univmon = FamilyCapability::of(&sketch(
        SketchAlgorithm::UnivMon,
        SketchParams::UnivMon {
            heap_size: 10,
            sketch_rows: 4,
            sketch_cols: 64,
            layers: 8,
            distinct_failure_probability: None,
        },
    ));
    for (readout, error) in [
        (FrequencyL2, MergedError::AsIfBuiltOverUnion),
        (point(), MergedError::AsIfBuiltOverUnion),
        (Cardinality, MergedError::AsIfBuiltOverUnion),
        (FrequencyEntropy, MergedError::Approximate),
    ] {
        assert_eq!(univmon.merged_error(&readout), Some(error), "{readout:?}");
    }
    // A statistic the family does not answer takes its default: a heavy
    // item readout of a family with heaps is approximate.
    assert_eq!(
        univmon.merged_error(&TopK { k: 5 }),
        Some(MergedError::Approximate)
    );
    let heap = FamilyCapability::of(&cms_with_heap());
    assert_eq!(
        heap.merged_error(&TopK { k: 5 }),
        Some(MergedError::Approximate)
    );
    let item = SketchStatistic::PointCount {
        key: ColumnRef::Named("k".into()),
        value: Some("a".into()),
    };
    assert_eq!(
        heap.merged_error(&item),
        Some(MergedError::AsIfBuiltOverUnion)
    );
    // A CMS with a heap answers item frequencies, not its total, whose
    // default is the guarantee of one build over the union.
    assert_eq!(
        heap.merged_error(&point()),
        Some(MergedError::AsIfBuiltOverUnion)
    );
    // Pass 1 offers CountSketch for a count (a bare point count), which
    // its row does not answer: merged panes keep one build's guarantee.
    let count_sketch = FamilyCapability::of(&sketch(
        SketchAlgorithm::CountSketch,
        SketchParams::CountSketch {
            width: 64,
            depth: 5,
        },
    ));
    assert!(!count_sketch.answers(&point()));
    assert_eq!(
        count_sketch.merged_error(&point()),
        Some(MergedError::AsIfBuiltOverUnion)
    );
    // An exact accumulator's merged state is exact.
    let sum = exact(ExactKind::Sum, ExactParams::Sum);
    assert_eq!(
        FamilyCapability::of(&sum).merged_error(&point()),
        Some(MergedError::Exact)
    );
    assert_eq!(
        FamilyCapability::of(&hll()).merged_error(&Cardinality),
        Some(MergedError::AsIfBuiltOverUnion)
    );
    assert_eq!(
        FamilyCapability::of(&kll()).merged_error(&SketchStatistic::Quantile { q: 0.99 }),
        Some(MergedError::AsIfBuiltOverUnion)
    );
    let irate = exact(ExactKind::IRate, ExactParams::IRate);
    assert_eq!(FamilyCapability::of(&irate).merged_error(&point()), None);
}

/// The IR does not reject a merge by family: exact IRate has no sound
/// merge, but only the table's consumers (the planner rules and the
/// executor) decide that.
#[test]
fn summary_merge_is_permissive_about_the_family() {
    let irate = || exact(ExactKind::IRate, ExactParams::IRate);
    let merged = merge(vec![
        family_state(irate(), "us"),
        family_state(irate(), "eu"),
    ])
    .unwrap();
    merged.validate_structure().unwrap();
    assert_eq!(FamilyCapability::of(&irate()).merge_inputs, None);
}

/// Idempotent families (HLL, exact Min/Max) merge states that may share
/// rows, and the union of identical selections is that selection; counting
/// families (exact Sum, KLL) still need disjoint selections.
#[test]
fn idempotent_families_merge_overlapping_states() {
    let same_region = |family: FieldDataType| {
        merge(vec![
            family_state(family.clone(), "us"),
            family_state(family, "us"),
        ])
    };
    for family in [hll(), exact(ExactKind::Max, ExactParams::Max)] {
        let merged = same_region(family.clone()).unwrap_or_else(|e| panic!("{family:?}: {e}"));
        merged.validate_structure().unwrap();
        assert_eq!(
            merged.coverage().unwrap().selection,
            family_state(family, "us").coverage().unwrap().selection
        );
    }
    for family in [exact(ExactKind::Sum, ExactParams::Sum), kll()] {
        let error = same_region(family).unwrap_err();
        assert!(error.contains("not proven disjoint"), "{error}");
    }
}

/// `SummaryMerge` takes the timing of its consumer, and reads pane states
/// stored at ingestion time (#509 Example 3/4 Pattern B1) or built at query
/// time (B2). Merging at ingestion time needs every pane at ingestion time.
#[test]
fn summary_merge_timing() {
    let panes = [state(200, "us"), state(200, "eu")];
    let root = merge(panes.to_vec()).unwrap();
    let timings = |assignment: &MaterializationAssignment| {
        let timed =
            apply_materialization_timings(&root, assignment, &mut TimingMemo::new()).unwrap();
        let children = timed.children();
        (
            timed.timing.unwrap(),
            children
                .iter()
                .map(|c| c.timing.unwrap())
                .collect::<Vec<_>>(),
        )
    };
    use ExecutionTiming::{IngestionTime, QueryTime};
    assert_eq!(
        timings(&MaterializationAssignment::default()),
        (QueryTime, vec![QueryTime, QueryTime])
    );
    let at_ingestion = Materialization::MatAtIngestion(Medium::Memory);
    let mut stored_panes = MaterializationAssignment::default();
    for pane in &panes {
        stored_panes.set(pane, at_ingestion);
    }
    assert_eq!(
        timings(&stored_panes),
        (QueryTime, vec![IngestionTime, IngestionTime])
    );
    // A `SummaryAgg` without an assignment runs at query time, so a merge
    // stored at ingestion time needs its panes there too.
    let mut only_merge = MaterializationAssignment::default();
    only_merge.set(&root, at_ingestion);
    assert!(apply_materialization_timings(&root, &only_merge, &mut TimingMemo::new()).is_err());
    let mut stored_merge = MaterializationAssignment::with_default(at_ingestion);
    stored_merge.set(&root, at_ingestion);
    assert_eq!(
        timings(&stored_merge),
        (IngestionTime, vec![IngestionTime, IngestionTime])
    );
}
