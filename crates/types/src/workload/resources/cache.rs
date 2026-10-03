//! Shared cache assumptions. Numerical estimation belongs to the cost model.

use serde::{Deserialize, Serialize};

/// Versioned deployment evidence describing query-result and buffer caching.
/// These are deployment assumptions, not additive CPU or byte consumption.
/// Estimators validate and interpret them against a comparison workload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "profile", rename_all = "snake_case", deny_unknown_fields)]
pub enum CacheProfile {
    NoCache { version: String },
    Evidence(CacheEvidence),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheEvidence {
    pub version: String,
    /// Evaluations whose parameterization is not identical to a preceding
    /// evaluation and therefore cannot use the result cache.
    pub distinct_evaluations: u64,
    /// Evaluations identical to a preceding evaluation and eligible for a
    /// result-cache hit.
    pub repeated_identical_evaluations: u64,
    pub result_cache: CacheCapacityEvidence,
    pub buffer_cache: CacheCapacityEvidence,
    /// Fraction of otherwise-resident result entries invalidated by arriving
    /// data. Required for continuously ingesting data; `AtRest` permits only
    /// zero or omitted invalidation evidence.
    pub result_invalidation_ratio: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheCapacityEvidence {
    pub working_set_bytes: u64,
    pub capacity_bytes: u64,
}

impl CacheProfile {
    pub fn no_cache() -> Self {
        Self::NoCache {
            version: "no-cache-v1".into(),
        }
    }

    pub fn version(&self) -> &str {
        match self {
            Self::NoCache { version } => version,
            Self::Evidence(evidence) => &evidence.version,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Moving the definitions preserves the established tagged JSON contract.
    #[test]
    fn existing_cache_json_round_trips_through_shared_types() {
        let no_cache = json!({"profile": "no_cache", "version": "no-cache-v1"});
        assert_eq!(
            serde_json::to_value(CacheProfile::no_cache()).unwrap(),
            no_cache
        );
        assert_eq!(
            serde_json::from_value::<CacheProfile>(no_cache)
                .unwrap()
                .version(),
            "no-cache-v1"
        );
        let evidence = json!({
            "profile": "evidence", "version": "cache-evidence-7",
            "distinct_evaluations": 2, "repeated_identical_evaluations": 4,
            "result_cache": {"working_set_bytes": 100, "capacity_bytes": 100},
            "buffer_cache": {"working_set_bytes": 9007199254740993_u64, "capacity_bytes": 50},
            "result_invalidation_ratio": null
        });
        let decoded: CacheProfile = serde_json::from_value(evidence.clone()).unwrap();
        assert_eq!(decoded.version(), "cache-evidence-7");
        assert_eq!(serde_json::to_value(decoded).unwrap(), evidence);
    }

    // Unknown invalidation stays unknown; required capacity cannot default to zero.
    #[test]
    fn shared_cache_json_preserves_unknowns_and_rejects_missing_or_extra_dimensions() {
        let mut evidence = json!({
            "profile": "evidence", "version": "v1",
            "distinct_evaluations": 1, "repeated_identical_evaluations": 0,
            "result_cache": {"working_set_bytes": 100, "capacity_bytes": 0},
            "buffer_cache": {"working_set_bytes": 100, "capacity_bytes": 0}
        });
        let CacheProfile::Evidence(decoded) =
            serde_json::from_value::<CacheProfile>(evidence.clone()).unwrap()
        else {
            panic!("wrong variant")
        };
        assert_eq!(decoded.result_invalidation_ratio, None);
        evidence["result_cache"]
            .as_object_mut()
            .unwrap()
            .remove("capacity_bytes");
        assert!(serde_json::from_value::<CacheProfile>(evidence).is_err());
        assert!(serde_json::from_value::<CacheCapacityEvidence>(json!({
            "working_set_bytes": 100, "capacity_bytes": 10, "cpu_ops": 1
        }))
        .is_err());
        assert!(serde_json::from_value::<CacheProfile>(json!({
            "profile": "no_cache", "version": "v1", "scan_bytes": 0
        }))
        .is_err());
    }
}
