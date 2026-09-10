//! Offline sketch-bench evidence. Measurements describe a particular dataset,
//! configuration and environment; they are neither runtime feedback nor proofs
//! of an accuracy guarantee. CPU quantities are nanoseconds, never CPU operations.

use asap_types::post_asap::{
    GroupingStrategy, SketchAlgorithm, SketchParams, SummaryExpr, SummaryFamilyType, SummaryNode,
};
use asap_types::pre_asap::AggIntent;
use serde::{Deserialize, Serialize};

use crate::cost_model::{Cost, CostModel, DefaultCostModel};
use crate::replacement::{
    accuracy_budget, accuracy_target, default_size_params, ReplacementSubDAG, TargetSubDAG,
};
use crate::summary_maintenance_lifecycle::SummaryMaintenanceLifecycleCostInputs;

pub const EVIDENCE_SCHEMA_VERSION: u32 = 1;
pub const EVIDENCE_MODEL_VERSION: &str = "empirical-update-cpu-v1";

pub use crate::empirical_resources::ResourceMeasurements;
pub use asap_types::resources::Measurement;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DistributionDescriptor {
    pub id: String,
    pub family: String,
    pub sample_count: u64,
    pub distinct_count: Option<u64>,
    /// Generator parameters or trace identity/checksum, including sampling rules.
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentDescriptor {
    pub id: String,
    pub cpu: String,
    pub os: String,
    pub runtime: String,
    pub implementation: String,
    pub implementation_version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasurementProvenance {
    pub command: String,
    pub dataset: String,
    pub source_revision: String,
    pub repetitions: u32,
}

/// Observed error on offline ground truth. No confidence or formal guarantee is
/// inferred from these statistics; `metric` defines the meaning of mean/max.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineError {
    pub metric: String,
    pub mean: Option<f64>,
    pub max: Option<f64>,
    pub trials: u32,
    pub ground_truth_method: String,
    pub query: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineMeasurement {
    pub id: String,
    pub algorithm: SketchAlgorithm,
    pub params: SketchParams,
    pub distribution: DistributionDescriptor,
    pub environment: EnvironmentDescriptor,
    pub measured_at_unix_seconds: u64,
    pub valid_until_unix_seconds: u64,
    pub provenance: MeasurementProvenance,
    pub metrics: ResourceMeasurements,
    pub error: Option<OfflineError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceArtifact {
    pub schema_version: u32,
    pub benchmark_version: String,
    /// Identity of the normalization/cost interpretation, independent of JSON
    /// layout and of the sketch implementation's source revision.
    pub model_version: String,
    pub records: Vec<OfflineMeasurement>,
}

/// The caller explicitly chooses the offline applicability context. Matching
/// all descriptors prevents reusing costs solely because a distribution name
/// or hardware label happens to agree. Time is injected for reproducibility.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceContext {
    pub distribution: DistributionDescriptor,
    pub environment: EnvironmentDescriptor,
    pub now_unix_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvidenceError {
    #[error("unsupported offline evidence schema version {0}")]
    UnsupportedVersion(u32),
    #[error("invalid offline evidence: {0}")]
    Invalid(String),
    #[error("no measurement for algorithm and exact configuration")]
    MissingConfiguration,
    #[error("offline distribution or environment is incompatible")]
    IncompatibleContext,
    #[error("offline measurement is expired or dated in the future")]
    Stale,
    #[error("multiple applicable measurements; select an unambiguous artifact")]
    Ambiguous,
}

pub struct EmpiricalEvidenceProvider {
    artifact: EvidenceArtifact,
    context: EvidenceContext,
}

impl EmpiricalEvidenceProvider {
    pub fn new(
        artifact: EvidenceArtifact,
        context: EvidenceContext,
    ) -> Result<Self, EvidenceError> {
        artifact.validate()?;
        if artifact.model_version != EVIDENCE_MODEL_VERSION {
            return invalid("unsupported offline cost model version");
        }
        validate_context(&context.distribution, &context.environment)?;
        Ok(Self { artifact, context })
    }

    pub fn artifact(&self) -> &EvidenceArtifact {
        &self.artifact
    }
    pub fn context(&self) -> &EvidenceContext {
        &self.context
    }

    /// Never interpolates between configurations, distributions, or machines.
    /// The returned row includes complete provenance for user explanations.
    pub fn lookup(
        &self,
        algorithm: &SketchAlgorithm,
        params: &SketchParams,
    ) -> Result<&OfflineMeasurement, EvidenceError> {
        let configurations: Vec<_> = self
            .artifact
            .records
            .iter()
            .filter(|r| &r.algorithm == algorithm && &r.params == params)
            .collect();
        if configurations.is_empty() {
            return Err(EvidenceError::MissingConfiguration);
        }
        let compatible: Vec<_> = configurations
            .into_iter()
            .filter(|r| {
                r.distribution == self.context.distribution
                    && r.environment == self.context.environment
            })
            .collect();
        if compatible.is_empty() {
            return Err(EvidenceError::IncompatibleContext);
        }
        let mut valid = compatible.into_iter().filter(|r| {
            r.measured_at_unix_seconds <= self.context.now_unix_seconds
                && self.context.now_unix_seconds <= r.valid_until_unix_seconds
        });
        let first = valid.next().ok_or(EvidenceError::Stale)?;
        if valid.next().is_some() {
            return Err(EvidenceError::Ambiguous);
        }
        Ok(first)
    }

    /// Reorders only a fully measured comparison, preserving all candidates.
    /// For deployment-specific sizing call `lookup` with those exact params.
    pub fn rank_candidates(
        &self,
        intent: &AggIntent,
        candidates: &[SketchAlgorithm],
        eps: f64,
        delta: f64,
    ) -> Vec<SketchAlgorithm> {
        let costs: Option<Vec<_>> = candidates
            .iter()
            .map(|algorithm| {
                let params = default_size_params(algorithm.clone(), intent, eps, delta);
                self.lookup(algorithm, &params)
                    .ok()?
                    .metrics
                    .resources
                    .cpu
                    .update_cpu_ns
                    .as_ref()
                    .map(|m| (algorithm.clone(), m.value))
            })
            .collect();
        let Some(mut costs) = costs else {
            return candidates.to_vec();
        };
        costs.sort_by(|a, b| a.1.total_cmp(&b.1));
        costs.into_iter().map(|(algorithm, _)| algorithm).collect()
    }

    /// Costs for one independently instantiated sketch state, in CPU ns.
    /// Unknown retention/retirement remain unavailable; CPU time must not be
    /// mixed with an existing deployment's unitless or CPU-operation costs.
    pub fn lifecycle_cost_inputs(
        &self,
        summary: &SummaryNode,
    ) -> SummaryMaintenanceLifecycleCostInputs {
        let SummaryExpr::SummaryAgg {
            family: SummaryFamilyType::Sketch(kind, GroupingStrategy::PerSubpopulationInstance),
            grouping: GroupingStrategy::PerSubpopulationInstance,
            ..
        } = &summary.expr
        else {
            return SummaryMaintenanceLifecycleCostInputs::default();
        };
        let Ok(row) = self.lookup(kind.algorithm(), kind.params()) else {
            return SummaryMaintenanceLifecycleCostInputs::default();
        };
        SummaryMaintenanceLifecycleCostInputs {
            build_cost: snapshot_build_cpu(row).map(Cost),
            maintenance_cost_per_update: row
                .metrics
                .resources
                .cpu
                .update_cpu_ns
                .as_ref()
                .map(|m| Cost(m.value)),
            // A point-frequency benchmark read does not price a total-count
            // or quantile read. There is no query request in this hook.
            summary_read_cost: None,
            ..Default::default()
        }
    }
}

/// Standalone adapter for the existing planner boundary. Empirical data changes
/// candidate discovery order; final structural scores retain their documented
/// default meaning. Complete plan benefit estimates require downstream raw and
/// summary physical evidence and are deliberately not invented here.
pub struct EmpiricalCostModel {
    pub provider: EmpiricalEvidenceProvider,
}

impl EmpiricalCostModel {
    pub fn new(provider: EmpiricalEvidenceProvider) -> Self {
        Self { provider }
    }
}

impl CostModel for EmpiricalCostModel {
    fn rank_candidates(
        &self,
        intent: &AggIntent,
        candidates: &[SketchAlgorithm],
    ) -> Vec<SketchAlgorithm> {
        let Some(accuracy) = accuracy_target(intent) else {
            return candidates.to_vec();
        };
        let (eps, delta) = accuracy_budget(accuracy);
        self.provider
            .rank_candidates(intent, candidates, eps, delta)
    }

    // The default size_params formula retains its formal guarantee. Observed
    // offline error is insufficient evidence to shrink a sketch safely.
    fn estimate_cost(&self, candidate: &ReplacementSubDAG, target: &TargetSubDAG<'_>) -> f64 {
        DefaultCostModel.estimate_cost(candidate, target)
    }

    fn summary_maintenance_lifecycle_cost_inputs(
        &self,
        summary: &SummaryNode,
    ) -> SummaryMaintenanceLifecycleCostInputs {
        self.provider.lifecycle_cost_inputs(summary)
    }
}

impl EvidenceArtifact {
    pub fn validate(&self) -> Result<(), EvidenceError> {
        if self.schema_version != EVIDENCE_SCHEMA_VERSION {
            return Err(EvidenceError::UnsupportedVersion(self.schema_version));
        }
        if self.benchmark_version.trim().is_empty() || self.model_version.trim().is_empty() {
            return invalid("missing benchmark or model version");
        }
        let mut ids = std::collections::HashSet::new();
        for row in &self.records {
            if row.id.trim().is_empty() || !ids.insert(&row.id) {
                return invalid("empty or duplicate record id");
            }
            validate_context(&row.distribution, &row.environment)?;
            let p = &row.provenance;
            if [&p.command, &p.dataset, &p.source_revision]
                .iter()
                .any(|s| s.trim().is_empty())
                || p.repetitions == 0
            {
                return invalid("missing measurement provenance");
            }
            if row.measured_at_unix_seconds > row.valid_until_unix_seconds {
                return invalid("reversed validity interval");
            }
            if !valid_params(&row.algorithm, &row.params) {
                return invalid("invalid or mismatched sketch parameters");
            }
            let m = &row.metrics.resources;
            for measurement in [
                &m.cpu.build_cpu_ns,
                &m.cpu.update_cpu_ns,
                &m.cpu.merge_cpu_ns,
                &m.cpu.prepare_cpu_ns,
                &m.cpu.read_cpu_ns,
                &m.retained_memory_bytes,
                &m.peak_memory_bytes,
                &m.serialized_bytes,
                &m.disk_bytes,
                &m.scan_bytes,
            ]
            .into_iter()
            .flatten()
            {
                if !nonnegative(measurement.value)
                    || measurement.samples == 0
                    || measurement.stddev.is_some_and(|v| !nonnegative(v))
                {
                    return invalid("invalid measurement or uncertainty");
                }
            }
            if let Some(error) = &row.error {
                if error.metric.trim().is_empty()
                    || error.ground_truth_method.trim().is_empty()
                    || error.trials == 0
                    || [error.mean, error.max]
                        .into_iter()
                        .flatten()
                        .any(|v| !nonnegative(v))
                {
                    return invalid("invalid offline error evidence");
                }
            }
        }
        Ok(())
    }
}

fn invalid<T>(message: &str) -> Result<T, EvidenceError> {
    Err(EvidenceError::Invalid(message.into()))
}
fn nonnegative(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

fn snapshot_build_cpu(row: &OfflineMeasurement) -> Option<f64> {
    let cpu = row.metrics.resources.cpu.build_cpu_ns.as_ref()?.value
        + row.metrics.resources.cpu.update_cpu_ns.as_ref()?.value
            * row.distribution.sample_count as f64
        + snapshot_prepare_cpu(row)?;
    nonnegative(cpu).then_some(cpu)
}

/// The existing fixed-snapshot CMS/CountSketch contract needs no separate
/// preparation. Other families must measure that phase, including an explicit
/// zero when no preparation is necessary; absence is not free work.
pub(crate) fn snapshot_prepare_cpu(row: &OfflineMeasurement) -> Option<f64> {
    match &row.metrics.resources.cpu.prepare_cpu_ns {
        Some(measurement) => nonnegative(measurement.value).then_some(measurement.value),
        None if matches!(
            row.algorithm,
            SketchAlgorithm::Cms | SketchAlgorithm::CountSketch
        ) =>
        {
            Some(0.0)
        }
        None => None,
    }
}

fn validate_context(
    distribution: &DistributionDescriptor,
    environment: &EnvironmentDescriptor,
) -> Result<(), EvidenceError> {
    if [
        &distribution.id,
        &distribution.family,
        &environment.id,
        &environment.cpu,
        &environment.os,
        &environment.runtime,
        &environment.implementation,
        &environment.implementation_version,
    ]
    .iter()
    .any(|s| s.trim().is_empty())
    {
        return invalid("missing distribution or environment identity");
    }
    if distribution.sample_count == 0
        || distribution
            .distinct_count
            .is_some_and(|n| n > distribution.sample_count)
        || !distribution.parameters.is_object()
    {
        return invalid("invalid distribution descriptors");
    }
    Ok(())
}

fn valid_params(algorithm: &SketchAlgorithm, params: &SketchParams) -> bool {
    match (algorithm, params) {
        (
            SketchAlgorithm::UnivMon,
            SketchParams::UnivMon {
                heap_size,
                sketch_rows,
                sketch_cols,
                layers,
            },
        ) => *heap_size > 0 && *sketch_rows > 0 && *sketch_cols > 0 && (1..=64).contains(layers),
        (SketchAlgorithm::Kll, SketchParams::Kll { k })
        | (SketchAlgorithm::Kmv, SketchParams::Kmv { k })
        | (SketchAlgorithm::Theta, SketchParams::Theta { k }) => *k > 0,
        (SketchAlgorithm::Hll, SketchParams::Hll { precision }) => (4..=18).contains(precision),
        (SketchAlgorithm::DDSketch, SketchParams::DDSketch { alpha }) => {
            alpha.is_finite() && *alpha > 0.0 && *alpha < 1.0
        }
        (SketchAlgorithm::Cms, SketchParams::Cms { width, depth })
        | (SketchAlgorithm::CountSketch, SketchParams::CountSketch { width, depth }) => {
            *width > 0 && *depth > 0
        }
        (
            SketchAlgorithm::CmsWithHeap,
            SketchParams::CmsWithHeap {
                width,
                depth,
                heap_size,
            },
        )
        | (
            SketchAlgorithm::CountSketchWithHeap,
            SketchParams::CountSketchWithHeap {
                width,
                depth,
                heap_size,
            },
        ) => *width > 0 && *depth > 0 && *heap_size > 0,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replacement::{implementations_for_with, Implementation};
    use asap_types::types::AccuracyTarget;

    /// The documented synthetic wire-format example remains importable and
    /// explicitly identifiable as a test fixture.
    #[test]
    fn checked_in_synthetic_example_is_valid() {
        let artifact: EvidenceArtifact = serde_json::from_str(include_str!(
            "../tests/data/offline-evidence-synthetic.json"
        ))
        .unwrap();
        artifact.validate().unwrap();
        assert!(artifact.records[0]
            .environment
            .implementation
            .contains("SYNTHETIC"));
        let schema: serde_json::Value = serde_json::from_str(include_str!(
            "../../../docs/developer_docs/offline-sketch-evidence.schema.json"
        ))
        .unwrap();
        assert_eq!(
            schema["properties"]["schema_version"]["const"],
            artifact.schema_version
        );
        assert_eq!(
            schema["properties"]["model_version"]["const"],
            EVIDENCE_MODEL_VERSION
        );
    }

    /// Lifecycle build includes all measured snapshot updates, not just an empty
    /// allocation. A missing update measurement cannot become free ingestion.
    #[test]
    fn lifecycle_build_requires_complete_snapshot_ingestion() {
        let (mut artifact, _, _) = fixture();
        let row = &mut artifact.records[0];
        row.metrics.resources.cpu.build_cpu_ns = Some(Measurement {
            value: 10.0,
            stddev: None,
            samples: 1,
            method: None,
        });
        assert_eq!(snapshot_build_cpu(row), Some(20010.0));
        row.metrics.resources.cpu.prepare_cpu_ns = Some(Measurement {
            value: 17.0,
            stddev: None,
            samples: 1,
            method: None,
        });
        assert_eq!(snapshot_build_cpu(row), Some(20027.0));
        row.metrics.resources.cpu.update_cpu_ns = None;
        assert_eq!(snapshot_build_cpu(row), None);
    }

    /// Newly shared optional dimensions receive the same numeric validation.
    #[test]
    fn optional_prepare_and_scan_measurements_are_validated() {
        for prepare in [true, false] {
            let (mut artifact, _, _) = fixture();
            let resources = &mut artifact.records[0].metrics.resources;
            let field = if prepare {
                &mut resources.cpu.prepare_cpu_ns
            } else {
                &mut resources.scan_bytes
            };
            *field = Some(Measurement {
                value: -1.0,
                stddev: None,
                samples: 1,
                method: None,
            });
            assert!(artifact.validate().is_err());
        }
    }

    /// Only the established frequency-sketch contract can omit preparation.
    #[test]
    fn unmeasured_preparation_for_other_families_keeps_build_unknown() {
        let (mut artifact, _, _) = fixture();
        let row = &mut artifact.records[0];
        row.metrics.resources.cpu.build_cpu_ns = Some(Measurement {
            value: 10.0,
            stddev: None,
            samples: 1,
            method: None,
        });
        assert_eq!(snapshot_build_cpu(row), Some(20010.0));
        row.algorithm = SketchAlgorithm::CountSketch;
        assert_eq!(snapshot_prepare_cpu(row), Some(0.0));
        row.algorithm = SketchAlgorithm::Kll;
        row.params = SketchParams::Kll { k: 269 };
        assert_eq!(snapshot_build_cpu(row), None);
        row.metrics.resources.cpu.prepare_cpu_ns = Some(Measurement {
            value: 17.0,
            stddev: None,
            samples: 1,
            method: None,
        });
        assert_eq!(snapshot_build_cpu(row), Some(20027.0));
    }

    fn fixture() -> (EvidenceArtifact, EvidenceContext, AggIntent) {
        let distribution = DistributionDescriptor {
            id: "unit-test-uniform".into(),
            family: "uniform".into(),
            sample_count: 1000,
            distinct_count: Some(100),
            parameters: serde_json::json!({"seed": 7}),
        };
        let environment = EnvironmentDescriptor {
            id: "unit-test-machine".into(),
            cpu: "test CPU".into(),
            os: "test OS".into(),
            runtime: "test runtime".into(),
            implementation: "synthetic test fixture".into(),
            implementation_version: "test-v1".into(),
        };
        let intent = AggIntent::Count {
            accuracy: AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            },
        };
        let records = [
            (SketchAlgorithm::Cms, 20.0),
            (SketchAlgorithm::CountSketch, 10.0),
            (SketchAlgorithm::UnivMon, 100.0),
        ]
        .into_iter()
        .map(|(algorithm, cost)| OfflineMeasurement {
            id: format!("test-{algorithm:?}"),
            params: default_size_params(algorithm.clone(), &intent, 0.01, 0.01),
            algorithm,
            distribution: distribution.clone(),
            environment: environment.clone(),
            measured_at_unix_seconds: 100,
            valid_until_unix_seconds: 200,
            provenance: MeasurementProvenance {
                command: "unit test fixture; not a measured benchmark".into(),
                dataset: "synthetic fixture".into(),
                source_revision: "test".into(),
                repetitions: 3,
            },
            metrics: ResourceMeasurements {
                resources: asap_types::resources::MeasuredResources {
                    cpu: asap_types::resources::MeasuredCpu {
                        update_cpu_ns: Some(Measurement {
                            value: cost,
                            stddev: Some(1.0),
                            samples: 3,
                            method: None,
                        }),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            },
            error: Some(OfflineError {
                metric: "mean absolute relative frequency error".into(),
                mean: Some(0.001),
                max: None,
                trials: 3,
                ground_truth_method: "test exact counter fixture".into(),
                query: serde_json::json!({"kind":"point_frequency"}),
            }),
        })
        .collect();
        (
            EvidenceArtifact {
                schema_version: EVIDENCE_SCHEMA_VERSION,
                benchmark_version: "test-fixture-v1".into(),
                model_version: "empirical-update-cpu-v1".into(),
                records,
            },
            EvidenceContext {
                distribution,
                environment,
                now_unix_seconds: 150,
            },
            intent,
        )
    }

    /// Real replacement generation follows measured update ranking while keeping
    /// every candidate and the same formally sized parameter configurations.
    #[test]
    fn public_cost_model_changes_replacement_order_without_changing_guarantees() {
        let (artifact, context, intent) = fixture();
        let model =
            EmpiricalCostModel::new(EmpiricalEvidenceProvider::new(artifact, context).unwrap());
        let default = implementations_for_with(&intent, &DefaultCostModel);
        let measured = implementations_for_with(&intent, &model);
        assert_eq!(default.len(), measured.len());
        let Implementation::Sketch(first_default) = &default[0] else {
            panic!("expected sketch")
        };
        let Implementation::Sketch(first_measured) = &measured[0] else {
            panic!("expected sketch")
        };
        assert_eq!(first_default.algorithm(), &SketchAlgorithm::Cms);
        assert_eq!(first_measured.algorithm(), &SketchAlgorithm::CountSketch);
        for candidate in &measured {
            assert!(default.contains(candidate));
        }
        assert_eq!(
            model.size_params(SketchAlgorithm::Cms, &intent, 0.001, 0.001),
            DefaultCostModel.size_params(SketchAlgorithm::Cms, &intent, 0.001, 0.001)
        );
    }

    /// Missing, mismatched and expired evidence preserve the original ranking;
    /// a measurement from another configuration is never extrapolated.
    #[test]
    fn unavailable_evidence_falls_back_with_specific_reasons() {
        let (artifact, context, intent) = fixture();
        let candidates = vec![SketchAlgorithm::Cms, SketchAlgorithm::CountSketch];
        for (mut evidence, mut request, expected) in [
            (
                artifact.clone(),
                context.clone(),
                EvidenceError::MissingConfiguration,
            ),
            (
                artifact.clone(),
                context.clone(),
                EvidenceError::IncompatibleContext,
            ),
            (artifact.clone(), context.clone(), EvidenceError::Stale),
        ] {
            match expected {
                EvidenceError::MissingConfiguration => {
                    evidence.records.remove(1);
                }
                EvidenceError::IncompatibleContext => {
                    request.distribution.parameters = serde_json::json!({"seed":8});
                }
                EvidenceError::Stale => {
                    request.now_unix_seconds = 201;
                }
                _ => unreachable!(),
            }
            let provider = EmpiricalEvidenceProvider::new(evidence, request).unwrap();
            assert_eq!(
                provider
                    .lookup(&artifact.records[1].algorithm, &artifact.records[1].params)
                    .unwrap_err(),
                expected
            );
            assert_eq!(
                provider.rank_candidates(&intent, &candidates, 0.01, 0.01),
                candidates
            );
        }
        let provider = EmpiricalEvidenceProvider::new(artifact, context).unwrap();
        assert_eq!(
            provider.rank_candidates(&intent, &candidates, 0.001, 0.01),
            candidates
        );
    }

    /// Null remains unknown across serialization; zero is accepted only as an
    /// explicit valid measurement, and no point-frequency error becomes a bound.
    #[test]
    fn serialization_preserves_unknown_zero_and_provenance() {
        let (mut artifact, context, _) = fixture();
        artifact.records[0].metrics.resources.disk_bytes = Some(Measurement {
            value: 0.0,
            stddev: None,
            samples: 1,
            method: None,
        });
        let decoded: EvidenceArtifact =
            serde_json::from_str(&serde_json::to_string(&artifact).unwrap()).unwrap();
        let provider = EmpiricalEvidenceProvider::new(decoded, context).unwrap();
        let row = provider
            .lookup(&artifact.records[0].algorithm, &artifact.records[0].params)
            .unwrap();
        assert!(row.metrics.resources.peak_memory_bytes.is_none());
        assert_eq!(
            row.metrics.resources.disk_bytes.as_ref().unwrap().value,
            0.0
        );
        assert_eq!(row.provenance, artifact.records[0].provenance);
        assert_eq!(row.error.as_ref().unwrap().query["kind"], "point_frequency");
    }

    /// Malformed values, schema versions and ambiguous live records cannot
    /// silently become plausible costs.
    #[test]
    fn invalid_and_ambiguous_artifacts_are_rejected() {
        let (artifact, context, _) = fixture();
        let mut bad = artifact.clone();
        bad.schema_version = 2;
        assert_eq!(bad.validate(), Err(EvidenceError::UnsupportedVersion(2)));
        let mut bad = artifact.clone();
        bad.records[0]
            .metrics
            .resources
            .cpu
            .update_cpu_ns
            .as_mut()
            .unwrap()
            .value = f64::NAN;
        assert!(bad.validate().is_err());
        let mut bad = artifact.clone();
        bad.records[0].params = SketchParams::Hll { precision: 14 };
        assert!(bad.validate().is_err());
        let mut bad = artifact.clone();
        bad.records[0].provenance.repetitions = 0;
        assert!(bad.validate().is_err());
        let mut duplicate = artifact.records[0].clone();
        duplicate.id = "another-live-measurement".into();
        let mut ambiguous = artifact.clone();
        ambiguous.records.push(duplicate);
        let provider = EmpiricalEvidenceProvider::new(ambiguous, context).unwrap();
        assert_eq!(
            provider.lookup(&artifact.records[0].algorithm, &artifact.records[0].params),
            Err(EvidenceError::Ambiguous)
        );
    }
}
