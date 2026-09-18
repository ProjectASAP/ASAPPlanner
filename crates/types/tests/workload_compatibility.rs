use asap_types::workload::{
    DataWorkload, PlanningWorkload, QueryLanguage, QueryWorkload, SqlDialect,
};

// Non-PromQL wire workloads should not require a PromQL-only cadence fact.
#[test]
fn non_promql_payload_accepts_absent_ingestion_interval() {
    for language in [
        QueryLanguage::DataFusion,
        QueryLanguage::SQL(SqlDialect::DataFusionSQL),
    ] {
        let workload = PlanningWorkload {
            query_workload: QueryWorkload {
                language,
                query_batch: None,
                repeating_queries: None,
            },
            data_workload: Some(DataWorkload::default()),
        };
        workload.validate().unwrap();
        let mut payload = serde_json::to_value(&workload).unwrap();
        payload["data_workload"]
            .as_object_mut()
            .unwrap()
            .remove("data_ingestion_interval");
        let decoded = serde_json::from_value::<PlanningWorkload>(payload.clone()).unwrap();
        decoded.validate().unwrap();
        payload["query_workload"]["language"] = serde_json::json!("promql");
        let promql = serde_json::from_value::<PlanningWorkload>(payload).unwrap();
        assert_eq!(
            promql.validate(),
            Err(asap_types::workload::WorkloadError::MissingDataIngestionInterval)
        );
    }
}
