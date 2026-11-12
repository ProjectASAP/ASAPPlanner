use super::*;

/// Both execution phases use raw values, distinct from maintained state.
#[test]
fn raw_primitive_labels() {
    assert_eq!(
        ExecutionDataState::INGESTION_ROWS.primitive,
        DataPrimitive::Raw
    );
    assert_eq!(ExecutionDataState::QUERY_ROWS.primitive, DataPrimitive::Raw);
    assert_eq!(
        ExecutionDataState::INGESTION_ROWS.to_string(),
        "ingestion_time/raw"
    );
    assert_eq!(ExecutionDataState::QUERY_ROWS.to_string(), "query_time/raw");
    assert_eq!(DataPrimitive::SummaryState.as_str(), "summary_state");
}

#[test]
fn execution_phase_wire_names_are_ingestion_and_query_time() {
    for (phase, name) in [
        (ExecutionTiming::IngestionTime, "ingestion_time"),
        (ExecutionTiming::QueryTime, "query_time"),
    ] {
        assert_eq!(phase.as_str(), name);
        assert_eq!(serde_json::to_value(phase).unwrap(), name);
        assert_eq!(
            serde_json::from_value::<ExecutionTiming>(serde_json::json!(name)).unwrap(),
            phase
        );
    }
    assert!(serde_json::from_str::<ExecutionTiming>("\"maintenance_time\"").is_err());
    assert!(serde_json::from_str::<ExecutionTiming>("\"MaintenanceTime\"").is_err());
}
