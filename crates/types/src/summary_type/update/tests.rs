use super::*;

#[test]
fn keyed_summary_update_round_trips() {
    for input in [
        SummaryUpdate {
            item: Some(SummaryInputExpr::EntityIdentity(
                EntityIdentity::PromqlLabelSet { excluding: vec![] },
            )),
            weight: SummaryInputExpr::Constant(1.0),
        },
        SummaryUpdate {
            item: Some(SummaryInputExpr::Tuple(vec![SummaryInputExpr::Column(
                ColumnRef::Named("service".into()),
            )])),
            weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
        },
    ] {
        let input_json = serde_json::to_string(&input).unwrap();
        assert_eq!(
            serde_json::from_str::<SummaryUpdate>(&input_json).unwrap(),
            input
        );
    }
}
