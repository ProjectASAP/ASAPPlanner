//! Helpers shared by the ASAP lane tests (`asap_lane.rs`, in the `devtools`
//! test binary) and the conformance test, which runs in its own binary.
use std::path::PathBuf;

/// A directory under the build's test temp directory, removed when dropped.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(test: &str) -> Self {
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("asap-lane-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The planning workload of one exact PromQL `query` evaluated every 10 s at
/// phase 0 over gauge `metric` with `series_count` series sampled every
/// `sample_interval_ms`, arriving as `arrival` (`{"ingesting": ...}` or
/// `"at_rest"`).
pub fn planning_workload(
    query: &str,
    metric: &str,
    series_count: u64,
    sample_interval_ms: u64,
    arrival: &str,
) -> String {
    format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{{
              "query": {query:?},
              "demand": {{"fixed_interval_at": {{"interval": 10000, "evaluation_phase": 0}}}},
              "requirements": {{"accuracy": {{"max_error": 0.0, "failure_probability": 0.0}}}},
              "predictability": {{"predictable": {{"known_at": null}}}}}}]}},
            "data_workload": {{"arrival": {arrival},
              "sources": {{{metric:?}: {{"series_count": {series_count}, "metric_type": "gauge",
                  "sample_interval": {sample_interval_ms}}}}}}}}}"#
    )
}
