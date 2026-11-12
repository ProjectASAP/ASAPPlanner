use super::*;

/// An unknown failure probability never evaluates to zero.
#[test]
fn an_unknown_failure_probability_never_evaluates_to_zero() {
    let unknown = ProbabilityExpr::Unknown {
        statistic: "hll_failure_probability".into(),
    };
    assert_eq!(unknown.evaluate(), None);
    assert_eq!(ProbabilityExpr::Zero.evaluate(), Some(0.0));
}

#[test]
fn invalid_numeric_leaves_fail_closed() {
    assert_eq!(BoundExpr::Constant { value: -0.1 }.evaluate(), None);
    assert_eq!(BoundExpr::Constant { value: f64::NAN }.evaluate(), None);
    assert_eq!(ProbabilityExpr::Constant { value: -0.1 }.evaluate(), None);
    assert_eq!(ProbabilityExpr::Constant { value: 1.1 }.evaluate(), None);
}

#[test]
fn guarantee_round_trips_through_json() {
    let g = AccuracyGuarantee::new(
        AccuracyMetric::ABSOLUTE_STREAM_L1,
        BoundExpr::Constant { value: 0.01 },
        ProbabilityExpr::Constant { value: 0.01 },
    );
    let json = serde_json::to_value(&g).unwrap();
    assert_eq!(json["spec"]["accuracy_metric"], "absolute_stream_l1");
    assert_eq!(json["spec"]["accuracy_scope"], "per_value");
    assert_eq!(json["spec"]["error"]["op"], "constant");
    let back: AccuracyGuarantee = serde_json::from_value(json).unwrap();
    assert_eq!(back, g);
}
