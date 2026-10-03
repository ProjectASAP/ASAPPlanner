//! Coverage composition preserves gaps and rejects duplicate observations.
use asap_types::{
    ir::operator_properties::Reduction,
    ir::summary_coverage::*,
    post_asap::SummaryUpdate,
    pre_asap::{ColumnRef, Source},
};
fn table(name: &str) -> Source {
    Source::Table {
        table_ref: name.into(),
    }
}
fn coverage(start: i64, end: i64, population: &[(&str, &str)]) -> SummaryCoverage {
    SummaryCoverage {
        source: table("flows"),
        regions: vec![CoverageRegion {
            time_ms: Some(start..end),
            population: population
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }],
    }
}
/// Adjacent panes coalesce; gaps remain disconnected rather than becoming a hull.
#[test]
fn time_union_preserves_gaps() {
    let merged =
        SummaryCoverage::merge_disjoint(&[coverage(0, 1, &[]), coverage(1, 2, &[])]).unwrap();
    assert_eq!(merged.regions[0].time_ms, Some(0..2));
    assert_eq!(merged.regions.len(), 1);
    let gapped =
        SummaryCoverage::merge_disjoint(&[coverage(0, 1, &[]), coverage(2, 3, &[])]).unwrap();
    assert_eq!(gapped.regions.len(), 2);
}
/// Population partitions can overlap in time without sharing observations.
#[test]
fn population_and_joint_union() {
    let merged = SummaryCoverage::merge_disjoint(&[
        coverage(0, 2, &[("region", "us")]),
        coverage(0, 2, &[("region", "eu")]),
    ])
    .unwrap();
    assert_eq!(merged.regions.len(), 2);
    let joint = SummaryCoverage::merge_disjoint(&[
        coverage(0, 1, &[("region", "us")]),
        coverage(1, 2, &[("region", "eu")]),
    ])
    .unwrap();
    assert_eq!(joint.regions.len(), 2);
    let decoded: SummaryCoverage =
        serde_json::from_str(&serde_json::to_string(&joint).unwrap()).unwrap();
    assert_eq!(decoded, joint);
}
/// Intersecting predicates and windows cannot authorize once-per-observation merge.
#[test]
fn overlap_and_identity_fail_closed() {
    assert_eq!(
        SummaryCoverage::merge_disjoint(&[coverage(0, 2, &[]), coverage(1, 3, &[])]),
        Err(CoverageError::PossibleOverlap)
    );
    assert_eq!(
        SummaryCoverage::merge_disjoint(&[
            coverage(0, 2, &[("region", "us")]),
            coverage(0, 2, &[("tier", "premium")])
        ]),
        Err(CoverageError::PossibleOverlap)
    );
    assert_eq!(
        SummaryCoverage::merge_disjoint(&[
            coverage(0, 2, &[("region", "us")]),
            coverage(0, 2, &[("region", "us")])
        ]),
        Err(CoverageError::PossibleOverlap)
    );
    let mut other = coverage(1, 2, &[]);
    other.source = table("other-flows");
    assert_eq!(
        SummaryCoverage::merge_disjoint(&[coverage(0, 1, &[]), other]),
        Err(CoverageError::SourceMismatch)
    );
    assert_eq!(
        coverage(2, 1, &[]).validate(),
        Err(CoverageError::InvalidInterval)
    );
}

/// Coverage is logical state metadata, and input rewrites invalidate its proof.
#[test]
fn node_coverage_is_required_checked_and_cleared_by_rewrites() {
    use asap_types::{
        ir::{ASAPOp, NonASAPOp, Operator, OperatorNode},
        post_asap::{SketchAlgorithm, SketchKind, SketchParams},
        pre_asap::{DataType, Field, FieldDataType, Schema},
    };
    let raw = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: table("flows"),
        predicates: vec![],
        schema: Schema::new(vec![Field::plain("latency", DataType::Float64, false)]),
    }))
    .unwrap();
    let declared = coverage(0, 1, &[]);
    assert!((*raw).clone().with_coverage(declared.clone()).is_err());
    let state = OperatorNode::new(Operator::ASAP(ASAPOp::SummaryAgg {
        child: raw,
        family: FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
            Default::default(),
        ),
        input: SummaryUpdate::column(ColumnRef::Named("latency".into())),
        reduction: Reduction::by(vec![]),
        grouping: Default::default(),
        filter: None,
    }))
    .unwrap();
    // Summary nodes cannot validate without coverage.
    assert!(matches!(
        std::rc::Rc::new(state.clone()).validate_structure(),
        Err(asap_types::ir::SchemaDerivationError::Coverage(
            CoverageError::Missing
        ))
    ));
    let state = state.with_coverage(declared.clone()).unwrap();
    std::rc::Rc::new(state.clone())
        .validate_structure()
        .unwrap();
    let rebuilt = state.map_children(Clone::clone).unwrap();
    assert!(rebuilt.coverage.is_none());
}

/// Sources without a time column declare no time bounds; such a region overlaps
/// any region it is not population-disjoint from.
#[test]
fn regions_without_time_bounds() {
    let mut tabular = coverage(0, 1, &[("region", "us")]);
    tabular.regions[0].time_ms = None;
    let mut other = coverage(0, 1, &[("region", "eu")]);
    other.regions[0].time_ms = None;
    assert_eq!(
        SummaryCoverage::merge_disjoint(&[tabular.clone(), other])
            .unwrap()
            .regions
            .len(),
        2
    );
    assert_eq!(
        SummaryCoverage::merge_disjoint(&[tabular, coverage(5, 6, &[("region", "us")])]),
        Err(CoverageError::PossibleOverlap)
    );
}
