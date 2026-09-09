//! Distribution-conditioned Error–Resource Profiles (ERP).
//!
//! This is a discrete, auditable profile selector. It does not infer a formal
//! sketch guarantee from benchmark observations and it does not interpolate
//! across distributions, implementations, or parameter points.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

pub const ERP_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErpResourceProfile {
    pub memory_bytes: f64,
    pub update_cpu_seconds: f64,
    pub merge_cpu_seconds: f64,
    pub query_cpu_seconds: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErpRecord {
    pub id: String,
    pub sketch: String,
    pub implementation: String,
    pub parameters: serde_json::Value,
    /// Opaque but equality-matched sketch-bench workload descriptor.
    pub distribution: serde_json::Value,
    pub trials: u32,
    pub error_metrics: BTreeMap<String, f64>,
    pub resources: ErpResourceProfile,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErpArtifact {
    pub schema_version: u32,
    pub producer_version: String,
    pub records: Vec<ErpRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccuracyMode {
    /// ERP may select a smaller empirical configuration. The result is not a
    /// formal guarantee and must be labelled accordingly by the deployment.
    Empirical,
    /// Same selection as empirical mode, but absence of applicable evidence is
    /// reported so the caller can fall back to formal sizing or exact execution.
    Hybrid,
}

#[derive(Debug, Clone)]
pub struct ErpSelectionRequest {
    pub distribution: serde_json::Value,
    pub implementation: Option<String>,
    pub allowed_sketches: Vec<String>,
    pub error_metric: String,
    pub max_error: f64,
    pub min_trials: u32,
    pub expected_updates: f64,
    pub expected_queries: f64,
    pub expected_merges: f64,
    pub retention_seconds: f64,
    pub cpu_weight: f64,
    pub byte_second_weight: f64,
    pub mode: AccuracyMode,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ErpSelection<'a> {
    pub record: &'a ErpRecord,
    pub observed_error: f64,
    pub estimated_cost: f64,
    pub accuracy_mode: AccuracyMode,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ErpError {
    #[error("unsupported ERP schema version {0}")]
    UnsupportedVersion(u32),
    #[error("invalid ERP artifact: {0}")]
    Invalid(&'static str),
    #[error("no applicable ERP configuration satisfies the empirical accuracy requirement")]
    NoApplicableConfiguration,
}

impl ErpArtifact {
    pub fn validate(&self) -> Result<(), ErpError> {
        if self.schema_version != ERP_SCHEMA_VERSION {
            return Err(ErpError::UnsupportedVersion(self.schema_version));
        }
        if self.producer_version.trim().is_empty() {
            return Err(ErpError::Invalid("missing producer version"));
        }
        let mut ids = HashSet::new();
        for row in &self.records {
            if row.id.trim().is_empty() || !ids.insert(row.id.as_str()) {
                return Err(ErpError::Invalid("empty or duplicate record id"));
            }
            if row.sketch.trim().is_empty()
                || row.implementation.trim().is_empty()
                || row.trials == 0
                || !row.resources.valid()
                || row
                    .error_metrics
                    .values()
                    .any(|value| !value.is_finite() || *value < 0.0)
            {
                return Err(ErpError::Invalid("invalid profile record"));
            }
        }
        Ok(())
    }

    /// Select the least-cost measured parameter point satisfying the requested
    /// empirical error. Distribution and implementation matching are exact by
    /// design; a later model may add conservative interpolation explicitly.
    pub fn select(&self, request: &ErpSelectionRequest) -> Result<ErpSelection<'_>, ErpError> {
        self.validate()?;
        if !request.valid() {
            return Err(ErpError::Invalid("invalid selection request"));
        }
        self.records
            .iter()
            .filter(|row| row.distribution == request.distribution)
            .filter(|row| {
                request
                    .implementation
                    .as_ref()
                    .is_none_or(|wanted| &row.implementation == wanted)
            })
            .filter(|row| {
                request.allowed_sketches.is_empty()
                    || request
                        .allowed_sketches
                        .iter()
                        .any(|name| name == &row.sketch)
            })
            .filter(|row| row.trials >= request.min_trials)
            .filter_map(|row| {
                let observed_error = *row.error_metrics.get(&request.error_metric)?;
                (observed_error <= request.max_error).then(|| ErpSelection {
                    record: row,
                    observed_error,
                    estimated_cost: request.cost(&row.resources),
                    accuracy_mode: request.mode,
                })
            })
            .min_by(|left, right| {
                left.estimated_cost
                    .total_cmp(&right.estimated_cost)
                    .then_with(|| left.record.id.cmp(&right.record.id))
            })
            .ok_or(ErpError::NoApplicableConfiguration)
    }
}

impl ErpResourceProfile {
    fn valid(&self) -> bool {
        [
            self.memory_bytes,
            self.update_cpu_seconds,
            self.merge_cpu_seconds,
            self.query_cpu_seconds,
        ]
        .into_iter()
        .all(|value| value.is_finite() && value >= 0.0)
    }
}

impl ErpSelectionRequest {
    fn valid(&self) -> bool {
        !self.error_metric.trim().is_empty()
            && self.max_error.is_finite()
            && self.max_error >= 0.0
            && self.min_trials > 0
            && [
                self.expected_updates,
                self.expected_queries,
                self.expected_merges,
                self.retention_seconds,
                self.cpu_weight,
                self.byte_second_weight,
            ]
            .into_iter()
            .all(|value| value.is_finite() && value >= 0.0)
    }

    fn cost(&self, resources: &ErpResourceProfile) -> f64 {
        self.cpu_weight
            * (self.expected_updates * resources.update_cpu_seconds
                + self.expected_queries * resources.query_cpu_seconds
                + self.expected_merges * resources.merge_cpu_seconds)
            + self.byte_second_weight * self.retention_seconds * resources.memory_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, width: u64, error: f64, memory: f64) -> ErpRecord {
        ErpRecord {
            id: id.into(),
            sketch: "cms".into(),
            implementation: "oxide".into(),
            parameters: serde_json::json!({"width": width, "depth": 3}),
            distribution: serde_json::json!({"synthetic":{"description":{"kind":"zipf"}}}),
            trials: 20,
            error_metrics: BTreeMap::from([("relative_error".into(), error)]),
            resources: ErpResourceProfile {
                memory_bytes: memory,
                update_cpu_seconds: 1e-7,
                merge_cpu_seconds: 1e-5,
                query_cpu_seconds: 1e-6,
            },
        }
    }

    fn request() -> ErpSelectionRequest {
        ErpSelectionRequest {
            distribution: serde_json::json!({"synthetic":{"description":{"kind":"zipf"}}}),
            implementation: Some("oxide".into()),
            allowed_sketches: vec!["cms".into()],
            error_metric: "relative_error".into(),
            max_error: 0.01,
            min_trials: 10,
            expected_updates: 1_000.0,
            expected_queries: 100.0,
            expected_merges: 0.0,
            retention_seconds: 60.0,
            cpu_weight: 1.0,
            byte_second_weight: 1e-9,
            mode: AccuracyMode::Hybrid,
        }
    }

    #[test]
    fn selects_cheapest_applicable_accurate_configuration() {
        let artifact = ErpArtifact {
            schema_version: ERP_SCHEMA_VERSION,
            producer_version: "bench-1".into(),
            records: vec![
                row("too-small", 256, 0.02, 6_144.0),
                row("winner", 512, 0.009, 12_288.0),
                row("overprovisioned", 2048, 0.001, 49_152.0),
            ],
        };
        let selected = artifact.select(&request()).unwrap();
        assert_eq!(selected.record.id, "winner");
    }

    #[test]
    fn distribution_mismatch_fails_closed() {
        let artifact = ErpArtifact {
            schema_version: ERP_SCHEMA_VERSION,
            producer_version: "bench-1".into(),
            records: vec![row("cms", 512, 0.009, 12_288.0)],
        };
        let mut request = request();
        request.distribution = serde_json::json!({"external":{"dataset":"production"}});
        assert_eq!(
            artifact.select(&request),
            Err(ErpError::NoApplicableConfiguration)
        );
    }

    /// Verifies the JSON contract emitted by sketch-bench is directly
    /// consumable without a backend-specific translation layer.
    #[test]
    fn deserializes_sketch_bench_wire_format() {
        let json = serde_json::json!({
            "schema_version": 1,
            "producer_version": "sketch-bench-rev",
            "records": [{
                "id": "erp-0",
                "sketch": "cms",
                "implementation": "oxide",
                "parameters": {"width": 512, "depth": 3},
                "distribution": {"synthetic": {"description": {"kind": "zipf"}}},
                "trials": 20,
                "error_metrics": {"relative_error": 0.009},
                "resources": {
                    "memory_bytes": 12288.0,
                    "update_cpu_seconds": 1e-7,
                    "merge_cpu_seconds": 1e-5,
                    "query_cpu_seconds": 1e-6
                }
            }]
        });
        let artifact: ErpArtifact = serde_json::from_value(json).unwrap();
        let selected = artifact.select(&request()).unwrap();
        assert_eq!(selected.record.id, "erp-0");
        assert_eq!(selected.accuracy_mode, AccuracyMode::Hybrid);
    }
}
