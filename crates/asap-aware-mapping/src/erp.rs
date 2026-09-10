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

/// Runtime-observable input shape used to match a benchmark scenario. Input
/// volume is a sufficiency gate, not a distance axis: once the benchmark has
/// enough samples, repeating the same stationary distribution adds little
/// information about sketch error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErpDataShape {
    pub cardinality: u64,
    /// Stable distribution family, for example `uniform`, `zipf`,
    /// `power_law`, or `empirical`. Different families are never interpolated.
    pub family: String,
    /// Family-specific numeric parameters. Zipf uses `exponent`; a continuous
    /// power law may use `alpha` and `minimum`. Uniform has no parameters.
    #[serde(default)]
    pub parameters: BTreeMap<String, f64>,
    pub benchmark_events: u64,
}

/// One hypothesis fitted to the same bounded runtime observation. Lower
/// goodness-of-fit is better; confidence is in [0, 1]. Keeping all plausible
/// fits avoids prematurely classifying unknown data as one named family.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErpShapeFit {
    pub family: String,
    #[serde(default)]
    pub parameters: BTreeMap<String, f64>,
    pub goodness_of_fit: f64,
    pub confidence: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErpShapeObservation {
    pub cardinality: u64,
    pub observed_events: u64,
    pub fits: Vec<ErpShapeFit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empirical_fingerprint: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ErpMultiFitSelectionRequest {
    pub selection: ErpSelectionRequest,
    pub observed: ErpShapeObservation,
    pub minimum_benchmark_events: u64,
    pub max_log2_cardinality_distance: f64,
    pub max_parameter_distance: f64,
    pub max_goodness_of_fit: f64,
    pub minimum_confidence: f64,
    /// Required confidence separation between the best and second-best fit.
    pub minimum_confidence_margin: f64,
}

#[derive(Debug, Clone)]
pub struct ErpNearestSelectionRequest {
    pub selection: ErpSelectionRequest,
    pub observed: ErpDataShape,
    pub minimum_benchmark_events: u64,
    pub max_log2_cardinality_distance: f64,
    /// Maximum normalized distance for every common distribution parameter.
    pub max_parameter_distance: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErpWindowWorkload {
    pub input_updates: u64,
    pub query_executions: u64,
    pub panes_per_query: u64,
    pub retained_panes: u64,
    pub materializations: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErpOperationCounts {
    pub updates: u64,
    pub merges: u64,
    pub queries: u64,
    pub retained_sketches: u64,
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

    /// Select against the nearest compatible measured shape. Shape metadata is
    /// read from `distribution.erp_shape`, keeping ERP v1 wire compatibility.
    pub fn select_nearest(
        &self,
        request: &ErpNearestSelectionRequest,
    ) -> Result<ErpSelection<'_>, ErpError> {
        self.validate()?;
        if !request.selection.valid() || !request.valid() {
            return Err(ErpError::Invalid("invalid nearest-shape request"));
        }
        self.records
            .iter()
            .filter_map(|row| Some((row, data_shape(&row.distribution)?)))
            .filter(|(_, shape)| shape.benchmark_events >= request.minimum_benchmark_events)
            .filter_map(|(row, shape)| Some((row, request.distance(shape)?)))
            .filter(|(_, distance)| *distance <= 1.0)
            .filter(|(row, _)| {
                request
                    .selection
                    .implementation
                    .as_ref()
                    .is_none_or(|wanted| &row.implementation == wanted)
                    && (request.selection.allowed_sketches.is_empty()
                        || request
                            .selection
                            .allowed_sketches
                            .iter()
                            .any(|name| name == &row.sketch))
                    && row.trials >= request.selection.min_trials
            })
            .filter_map(|(row, distance)| {
                let observed_error = *row.error_metrics.get(&request.selection.error_metric)?;
                (observed_error <= request.selection.max_error).then_some((
                    distance,
                    ErpSelection {
                        record: row,
                        observed_error,
                        estimated_cost: request.selection.cost(&row.resources),
                        accuracy_mode: request.selection.mode,
                    },
                ))
            })
            .min_by(|(left_distance, left), (right_distance, right)| {
                left_distance
                    .total_cmp(right_distance)
                    .then_with(|| left.estimated_cost.total_cmp(&right.estimated_cost))
                    .then_with(|| left.record.id.cmp(&right.record.id))
            })
            .map(|(_, selection)| selection)
            .ok_or(ErpError::NoApplicableConfiguration)
    }

    /// Select using every statistically plausible fit for one observation.
    /// Ambiguous and poor fits fail closed so callers can use their
    /// theoretical-then-exact fallback policy.
    pub fn select_multi_fit(
        &self,
        request: &ErpMultiFitSelectionRequest,
    ) -> Result<ErpSelection<'_>, ErpError> {
        self.validate()?;
        if !request.selection.valid() || !request.valid() {
            return Err(ErpError::Invalid("invalid multi-fit request"));
        }
        if let Some(selected) =
            request
                .observed
                .empirical_fingerprint
                .as_deref()
                .and_then(|wanted| {
                    self.records
                        .iter()
                        .filter(|row| benchmark_fingerprint(&row.distribution) == Some(wanted))
                        .filter_map(|row| request.selection.evaluate(row).map(|value| (row, value)))
                        .min_by(|(left_row, left), (right_row, right)| {
                            left.estimated_cost
                                .total_cmp(&right.estimated_cost)
                                .then_with(|| left_row.id.cmp(&right_row.id))
                        })
                        .map(|(_, selected)| selected)
                })
        {
            return Ok(selected);
        }
        let mut fits = request
            .observed
            .fits
            .iter()
            .filter(|fit| {
                fit.confidence >= request.minimum_confidence
                    && fit.goodness_of_fit <= request.max_goodness_of_fit
            })
            .collect::<Vec<_>>();
        fits.sort_by(|left, right| right.confidence.total_cmp(&left.confidence));
        let Some(best) = fits.first() else {
            return Err(ErpError::NoApplicableConfiguration);
        };
        if fits.get(1).is_some_and(|second| {
            best.confidence - second.confidence < request.minimum_confidence_margin
        }) {
            return Err(ErpError::NoApplicableConfiguration);
        }
        // Compare every plausible fit with every compatible benchmark shape.
        // Confidence and fit quality contribute to the joint score; neither
        // field first collapses the observation to one family.
        self.records
            .iter()
            .filter_map(|row| Some((row, data_shape(&row.distribution)?)))
            .filter(|(_, shape)| shape.benchmark_events >= request.minimum_benchmark_events)
            .flat_map(|(row, shape)| {
                fits.iter().filter_map(move |fit| {
                    let nearest = ErpNearestSelectionRequest {
                        selection: request.selection.clone(),
                        observed: ErpDataShape {
                            cardinality: request.observed.cardinality,
                            family: fit.family.clone(),
                            parameters: fit.parameters.clone(),
                            benchmark_events: request.observed.observed_events,
                        },
                        minimum_benchmark_events: request.minimum_benchmark_events,
                        max_log2_cardinality_distance: request.max_log2_cardinality_distance,
                        max_parameter_distance: request.max_parameter_distance,
                    };
                    let shape_distance = nearest.distance(shape.clone())?;
                    (shape_distance <= 1.0).then_some((row, fit, shape_distance))
                })
            })
            .filter_map(|(row, fit, shape_distance)| {
                let selected = request.selection.evaluate(row)?;
                let fit_distance =
                    fit.goodness_of_fit / request.max_goodness_of_fit.max(f64::EPSILON);
                let confidence_distance = 1.0 - fit.confidence;
                Some((
                    shape_distance.max(fit_distance).max(confidence_distance),
                    selected,
                ))
            })
            .min_by(|(left_distance, left), (right_distance, right)| {
                left_distance
                    .total_cmp(right_distance)
                    .then_with(|| left.estimated_cost.total_cmp(&right.estimated_cost))
                    .then_with(|| left.record.id.cmp(&right.record.id))
            })
            .map(|(_, selected)| selected)
            .ok_or(ErpError::NoApplicableConfiguration)
    }
}

fn benchmark_fingerprint(distribution: &serde_json::Value) -> Option<&str> {
    distribution
        .pointer("/erp_shape/empirical_fingerprint")
        .or_else(|| distribution.pointer("/workload/external/fingerprint"))
        .and_then(serde_json::Value::as_str)
}

fn data_shape(distribution: &serde_json::Value) -> Option<ErpDataShape> {
    serde_json::from_value(distribution.get("erp_shape")?.clone()).ok()
}

impl ErpNearestSelectionRequest {
    fn valid(&self) -> bool {
        self.minimum_benchmark_events > 0
            && self.observed.cardinality > 0
            && self.max_log2_cardinality_distance.is_finite()
            && self.max_log2_cardinality_distance > 0.0
            && !self.observed.family.trim().is_empty()
            && self
                .observed
                .parameters
                .values()
                .all(|value| value.is_finite())
            && self.max_parameter_distance.is_finite()
            && self.max_parameter_distance > 0.0
    }

    fn distance(&self, candidate: ErpDataShape) -> Option<f64> {
        if candidate.cardinality == 0
            || candidate.family != self.observed.family
            || candidate
                .parameters
                .keys()
                .ne(self.observed.parameters.keys())
        {
            return None;
        }
        let cardinality = ((candidate.cardinality as f64).log2()
            - (self.observed.cardinality as f64).log2())
        .abs()
            / self.max_log2_cardinality_distance;
        let parameters = candidate
            .parameters
            .iter()
            .map(|(name, value)| {
                let observed = self.observed.parameters.get(name)?;
                (value.is_finite() && observed.is_finite())
                    .then_some((value - observed).abs() / self.max_parameter_distance)
            })
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .fold(0.0_f64, f64::max);
        Some(cardinality.max(parameters))
    }
}

impl ErpMultiFitSelectionRequest {
    fn valid(&self) -> bool {
        self.observed.cardinality > 0
            && self.observed.observed_events > 0
            && (!self.observed.fits.is_empty() || self.observed.empirical_fingerprint.is_some())
            && self.max_goodness_of_fit.is_finite()
            && self.max_goodness_of_fit >= 0.0
            && self.minimum_confidence.is_finite()
            && (0.0..=1.0).contains(&self.minimum_confidence)
            && self.minimum_confidence_margin.is_finite()
            && (0.0..=1.0).contains(&self.minimum_confidence_margin)
            && self.observed.fits.iter().all(|fit| {
                !fit.family.trim().is_empty()
                    && fit.goodness_of_fit.is_finite()
                    && fit.goodness_of_fit >= 0.0
                    && fit.confidence.is_finite()
                    && (0.0..=1.0).contains(&fit.confidence)
                    && fit.parameters.values().all(|value| value.is_finite())
            })
    }
}

impl ErpOperationCounts {
    /// Compose atomic benchmark costs with a pane-based window plan. A query
    /// over one pane needs no merge; N panes need N-1 merges.
    pub fn from_window(workload: ErpWindowWorkload) -> Self {
        Self {
            updates: workload
                .input_updates
                .saturating_mul(workload.materializations),
            merges: workload
                .query_executions
                .saturating_mul(workload.panes_per_query.saturating_sub(1)),
            queries: workload.query_executions,
            retained_sketches: workload
                .retained_panes
                .saturating_mul(workload.materializations),
        }
    }

    pub fn cpu_seconds(self, resources: &ErpResourceProfile) -> f64 {
        self.updates as f64 * resources.update_cpu_seconds
            + self.merges as f64 * resources.merge_cpu_seconds
            + self.queries as f64 * resources.query_cpu_seconds
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
    fn evaluate<'a>(&self, row: &'a ErpRecord) -> Option<ErpSelection<'a>> {
        if self
            .implementation
            .as_ref()
            .is_some_and(|wanted| &row.implementation != wanted)
            || (!self.allowed_sketches.is_empty()
                && !self.allowed_sketches.iter().any(|name| name == &row.sketch))
            || row.trials < self.min_trials
        {
            return None;
        }
        let observed_error = *row.error_metrics.get(&self.error_metric)?;
        (observed_error <= self.max_error).then(|| ErpSelection {
            record: row,
            observed_error,
            estimated_cost: self.cost(&row.resources),
            accuracy_mode: self.mode,
        })
    }

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

    fn multi_fit_request() -> ErpMultiFitSelectionRequest {
        ErpMultiFitSelectionRequest {
            selection: request(),
            observed: ErpShapeObservation {
                cardinality: 1_000,
                observed_events: 20_000,
                fits: vec![ErpShapeFit {
                    family: "zipf".into(),
                    parameters: BTreeMap::from([("exponent".into(), 1.2)]),
                    goodness_of_fit: 0.03,
                    confidence: 0.95,
                }],
                empirical_fingerprint: None,
            },
            minimum_benchmark_events: 10_000,
            max_log2_cardinality_distance: 1.0,
            max_parameter_distance: 0.2,
            max_goodness_of_fit: 0.1,
            minimum_confidence: 0.8,
            minimum_confidence_margin: 0.1,
        }
    }

    #[test]
    fn multi_fit_selects_only_confident_well_fitting_family() {
        let mut measured = row("zipf", 512, 0.009, 12_288.0);
        measured.distribution = serde_json::json!({"erp_shape": {
            "cardinality": 1000, "family": "zipf",
            "parameters": {"exponent": 1.22}, "benchmark_events": 20000
        }});
        let artifact = ErpArtifact {
            schema_version: ERP_SCHEMA_VERSION,
            producer_version: "bench-1".into(),
            records: vec![measured],
        };
        assert_eq!(
            artifact
                .select_multi_fit(&multi_fit_request())
                .unwrap()
                .record
                .id,
            "zipf"
        );
    }

    #[test]
    fn multi_fit_rejects_ambiguous_or_poor_observations() {
        let mut measured = row("zipf", 512, 0.009, 12_288.0);
        measured.distribution = serde_json::json!({"erp_shape": {
            "cardinality": 1000, "family": "zipf",
            "parameters": {"exponent": 1.2}, "benchmark_events": 20000
        }});
        let artifact = ErpArtifact {
            schema_version: ERP_SCHEMA_VERSION,
            producer_version: "bench-1".into(),
            records: vec![measured],
        };
        let mut ambiguous = multi_fit_request();
        ambiguous.observed.fits.push(ErpShapeFit {
            family: "normal".into(),
            parameters: BTreeMap::from([("mean".into(), 0.0), ("stddev".into(), 1.0)]),
            goodness_of_fit: 0.04,
            confidence: 0.90,
        });
        assert_eq!(
            artifact.select_multi_fit(&ambiguous),
            Err(ErpError::NoApplicableConfiguration)
        );
        ambiguous.observed.fits.truncate(1);
        ambiguous.observed.fits[0].goodness_of_fit = 0.5;
        assert_eq!(
            artifact.select_multi_fit(&ambiguous),
            Err(ErpError::NoApplicableConfiguration)
        );
    }

    #[test]
    fn multi_fit_jointly_ranks_all_plausible_families() {
        let mut distant_high_confidence = row("zipf-distant", 512, 0.009, 1.0);
        distant_high_confidence.distribution = serde_json::json!({"erp_shape": {
            "cardinality": 1000, "family": "zipf",
            "parameters": {"exponent": 1.39}, "benchmark_events": 20000
        }});
        let mut close_lower_confidence = row("normal-close", 512, 0.009, 100.0);
        close_lower_confidence.distribution = serde_json::json!({"erp_shape": {
            "cardinality": 1000, "family": "normal",
            "parameters": {"mean": 4.0, "stddev": 1.0}, "benchmark_events": 20000
        }});
        let artifact = ErpArtifact {
            schema_version: ERP_SCHEMA_VERSION,
            producer_version: "bench-1".into(),
            records: vec![distant_high_confidence, close_lower_confidence],
        };
        let mut request = multi_fit_request();
        request.observed.fits.push(ErpShapeFit {
            family: "normal".into(),
            parameters: BTreeMap::from([("mean".into(), 4.0), ("stddev".into(), 1.0)]),
            goodness_of_fit: 0.01,
            confidence: 0.82,
        });
        request.minimum_confidence_margin = 0.1;
        assert_eq!(
            artifact.select_multi_fit(&request).unwrap().record.id,
            "normal-close"
        );
    }

    #[test]
    fn exact_empirical_fingerprint_precedes_fits_and_requires_identity() {
        let mut exact = row("exact-trace", 512, 0.009, 100.0);
        exact.distribution = serde_json::json!({"workload": {"external": {
            "dataset": "trace-a", "fingerprint": "sha256:abc"
        }}});
        let artifact = ErpArtifact {
            schema_version: ERP_SCHEMA_VERSION,
            producer_version: "bench-1".into(),
            records: vec![exact],
        };
        let mut request = multi_fit_request();
        request.observed.empirical_fingerprint = Some("sha256:abc".into());
        request.observed.fits.clear();
        assert_eq!(
            artifact.select_multi_fit(&request).unwrap().record.id,
            "exact-trace"
        );
        request.observed.empirical_fingerprint = Some("sha256:different".into());
        assert_eq!(
            artifact.select_multi_fit(&request),
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

    #[test]
    fn nearest_shape_prefers_cardinality_and_skew_then_cost() {
        let mut close = row("close", 512, 0.009, 12_288.0);
        close.distribution = serde_json::json!({"erp_shape": {
            "cardinality": 1000, "family": "zipf", "parameters": {"exponent": 1.2}, "benchmark_events": 100000
        }});
        let mut cheap_but_far = row("far", 256, 0.009, 1.0);
        cheap_but_far.distribution = serde_json::json!({"erp_shape": {
            "cardinality": 8000, "family": "zipf", "parameters": {"exponent": 1.2}, "benchmark_events": 100000
        }});
        let artifact = ErpArtifact {
            schema_version: ERP_SCHEMA_VERSION,
            producer_version: "bench-1".into(),
            records: vec![cheap_but_far, close],
        };
        let selected = artifact
            .select_nearest(&ErpNearestSelectionRequest {
                selection: request(),
                observed: ErpDataShape {
                    cardinality: 1200,
                    family: "zipf".into(),
                    parameters: BTreeMap::from([("exponent".into(), 1.1)]),
                    benchmark_events: 0,
                },
                minimum_benchmark_events: 10_000,
                max_log2_cardinality_distance: 4.0,
                max_parameter_distance: 0.5,
            })
            .unwrap();
        assert_eq!(selected.record.id, "close");
    }

    #[test]
    fn nearest_shape_rejects_distribution_family_and_small_benchmarks() {
        let mut row = row("uniform", 512, 0.009, 12_288.0);
        row.distribution = serde_json::json!({"erp_shape": {
            "cardinality": 1000, "family": "uniform", "parameters": {}, "benchmark_events": 999
        }});
        let artifact = ErpArtifact {
            schema_version: ERP_SCHEMA_VERSION,
            producer_version: "bench-1".into(),
            records: vec![row],
        };
        let nearest = ErpNearestSelectionRequest {
            selection: request(),
            observed: ErpDataShape {
                cardinality: 1000,
                family: "zipf".into(),
                parameters: BTreeMap::from([("exponent".into(), 1.0)]),
                benchmark_events: 0,
            },
            minimum_benchmark_events: 1_000,
            max_log2_cardinality_distance: 1.0,
            max_parameter_distance: 0.5,
        };
        assert_eq!(
            artifact.select_nearest(&nearest),
            Err(ErpError::NoApplicableConfiguration)
        );
    }

    #[test]
    fn pane_window_composes_atomic_operation_costs() {
        let counts = ErpOperationCounts::from_window(ErpWindowWorkload {
            input_updates: 1_000,
            query_executions: 10,
            panes_per_query: 12,
            retained_panes: 24,
            materializations: 2,
        });
        assert_eq!(counts.updates, 2_000);
        assert_eq!(counts.merges, 110);
        assert_eq!(counts.queries, 10);
        assert_eq!(counts.retained_sketches, 48);
        assert!((counts.cpu_seconds(&row("cost", 1, 0.0, 0.0).resources) - 0.00131).abs() < 1e-12);
    }
}
