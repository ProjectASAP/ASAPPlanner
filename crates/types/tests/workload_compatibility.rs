use asap_types::workload::PlanningWorkload;

// A SQL workload over ingesting data omits the PromQL-only sample interval.
// Validation never requires it, for PromQL either: the PromQL frontend
// checks a metric's interval only for a bare instant selector over it.
#[test]
fn ingesting_payload_without_sample_interval_validates() {
    for language in [
        serde_json::json!("datafusion"),
        serde_json::json!({"sql": "datafusion_sql"}),
        serde_json::json!("promql"),
    ] {
        let payload = serde_json::json!({
            "query_workload": {"language": language, "query_batch": null,
                "repeating_queries": null},
            "data_workload": {"arrival": {"ingesting": {"rows_per_second": 10.0}},
                "sources": {"m": {"series_count": 10}}}
        });
        let workload = serde_json::from_value::<PlanningWorkload>(payload).unwrap();
        workload.validate().unwrap();
        assert_eq!(workload.sample_interval_of("m"), None);
    }
}
