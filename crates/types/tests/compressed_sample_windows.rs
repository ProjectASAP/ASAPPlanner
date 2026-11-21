//! Compressed-sample windows: the state type's family, the decode
//! operator's schema, and the unsealed-window capability (design doc
//! `compressed-sample-windows.md`, sections 2.3 and 2.5; tests T1–T3).
use std::rc::Rc;
use std::time::Duration;

use asap_types::deployment_capabilities::{
    CandidateDescription, CandidateExclusion, DeploymentCapabilities, ExclusionSource,
};
use asap_types::ir::operator::arguments::{Reduction, Source};
use asap_types::ir::operator::OperatorKind;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::series_identity::with_promql_series_identity;
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, TimeRangeKind};
use asap_types::stages::{Lifetime, MemoGroupId, PhysicalProperties};
use asap_types::summary_type::{SummaryFamily, SummaryUpdate};
use asap_types::workload::DataWorkload;

/// `Scan data` with `labels`, its series identity realized as the planner
/// realizes it before Stage 0.
fn scan(labels: &[&str]) -> Rc<OperatorNode> {
    let fields = [
        Field::plain("ts", DataType::Timestamp, false),
        Field::plain("value", DataType::Float64, false),
    ]
    .into_iter()
    .chain(
        labels
            .iter()
            .map(|label| Field::plain(*label, DataType::Utf8, false)),
    )
    .collect();
    let open = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::TimeSeries {
            metric: "data".into(),
        },
        predicates: vec![],
        schema: Schema::with_time_index(fields, 0, vec![]),
    }))
    .unwrap();
    with_promql_series_identity(&open).unwrap()
}

/// The 5 min compressed-samples chain of `data`, keyed by series identity
/// only.
fn chain() -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
        child: scan(&[]),
        summary_type: FieldDataType::CompressedSamples,
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
        tumbling_window_width_ms: 300_000,
    }))
    .unwrap()
}

/// T1. The state's family is `CompressedSamples`, written
/// `"compressed_samples"` in JSON both ways, and an exclusion naming it is
/// valid input. Fails if the family is missing, mapped to `None` (a
/// deployment could then not exclude the state), or named differently.
#[test]
fn compressed_samples_family_json() {
    assert_eq!(
        SummaryFamily::of(&FieldDataType::CompressedSamples),
        Some(SummaryFamily::CompressedSamples)
    );
    assert_eq!(
        serde_json::to_string(&SummaryFamily::CompressedSamples).unwrap(),
        r#""compressed_samples""#
    );
    assert_eq!(
        serde_json::from_str::<SummaryFamily>(r#""compressed_samples""#).unwrap(),
        SummaryFamily::CompressedSamples
    );
    let capabilities: DeploymentCapabilities = serde_json::from_str(
        r#"{"excluded_candidates": [{"summary_family": "compressed_samples"}]}"#,
    )
    .unwrap();
    assert!(capabilities.validate(&DataWorkload::default()).is_ok());
    assert_eq!(
        capabilities.excluded_candidates[0].family_excluded_outright(),
        Some(SummaryFamily::CompressedSamples)
    );
}

/// T2. `DecodeSamplesInRange` over the identity-keyed chain has exactly the
/// schema of `TimeRange(5m) ← Scan` for a scan projecting `job`, and for one
/// projecting `region, service`: the labels come from the series identity.
/// Fails if the decode's schema is the chain's (window end and state), if
/// it drops or reorders the labels, or if it loses the time index or the
/// (identity, time) key that raw rows have.
#[test]
fn decode_schema_equals_time_range_schema() {
    for labels in [&["job"][..], &["region", "service"][..]] {
        let raw = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(300),
            kind: TimeRangeKind::Range,
            child: scan(labels),
        }))
        .unwrap();
        for reads_unsealed_window in [true, false] {
            let decoded = OperatorNode::new_shared(Operator::ASAP(ASAPOp::DecodeSamplesInRange {
                child: chain(),
                range: Duration::from_secs(300),
                kind: TimeRangeKind::Range,
                labels: labels.iter().map(|label| label.to_string()).collect(),
                reads_unsealed_window,
            }))
            .unwrap();
            assert_eq!(decoded.schema, raw.schema, "{labels:?}");
            assert_eq!(decoded.result_kind, raw.result_kind, "{labels:?}");
        }
    }
}

/// The physical properties of a query-time node discarded after use.
fn query_time() -> PhysicalProperties {
    PhysicalProperties::new(ExecutionTiming::QueryTime, Lifetime::None).unwrap()
}

/// A decode of chain group 0 in read mode `reads_unsealed_window`.
fn decode(reads_unsealed_window: bool) -> Operator<MemoGroupId> {
    Operator::ASAP(ASAPOp::DecodeSamplesInRange {
        child: MemoGroupId(0),
        range: Duration::from_secs(300),
        kind: TimeRangeKind::Range,
        labels: vec!["job".into()],
        reads_unsealed_window,
    })
}

/// T3. The default capabilities read unsealed windows and expand into no
/// exclusion of decodes; `unsealed_window_reads: false` expands into one
/// exclusion, from `UnsealedWindowsNotReadable`, that matches the decode
/// reading the unsealed window and not the sealed-read decode, so a
/// deployment without the runtime contract keeps the aligned widths.
/// Fails if the capability is missing, defaults to `false`, or excludes
/// every decode.
#[test]
fn unsealed_window_reads_false_excludes_unsealed_decodes() {
    let default = DeploymentCapabilities::default();
    assert!(default.unsealed_window_reads);
    assert!(!default
        .exclusions()
        .iter()
        .any(|exclusion| exclusion.source == ExclusionSource::UnsealedWindowsNotReadable));

    let parsed: DeploymentCapabilities =
        serde_json::from_str(r#"{"unsealed_window_reads": false}"#).unwrap();
    assert!(!parsed.unsealed_window_reads);
    let from_capability: Vec<CandidateExclusion> = parsed
        .exclusions()
        .into_iter()
        .filter(|exclusion| exclusion.source == ExclusionSource::UnsealedWindowsNotReadable)
        .map(|exclusion| exclusion.exclusion)
        .collect();
    assert_eq!(
        from_capability,
        vec![CandidateExclusion {
            operator: Some(OperatorKind::DecodeSamplesInRange),
            reads_unsealed_window: Some(true),
            ..Default::default()
        }]
    );
    assert_eq!(
        ExclusionSource::UnsealedWindowsNotReadable.to_string(),
        "unsealed_windows_not_readable"
    );
    let described = |operator| CandidateDescription {
        operator,
        parameters: None,
        physical_properties: query_time(),
        read_summary_type: None,
    };
    let unsealed = decode(true);
    let sealed = decode(false);
    assert!(from_capability[0].matches(&described(&unsealed)));
    assert!(!from_capability[0].matches(&described(&sealed)));
}
