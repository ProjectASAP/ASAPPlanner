//! Query-matched, fixed-snapshot offline recommendations. Observed error is an
//! explicit acceptance criterion, never a replacement for formal guarantees.

use asap_types::post_asap::{SketchAlgorithm, SketchParams};
use serde::{Deserialize, Serialize};

use crate::empirical_cost::{
    DistributionDescriptor, EmpiricalEvidenceProvider, EnvironmentDescriptor, EvidenceArtifact,
    EvidenceContext, Measurement, MeasurementProvenance, OfflineMeasurement,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineQueryDescriptor {
    pub kind: String,
    pub value_type: String,
    /// Identifies the exact probe population used for timing and observed error.
    pub probe_set: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasurementQueryBinding {
    pub record_id: String,
    pub query: OfflineQueryDescriptor,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExactResourceMeasurements {
    pub empty_build_cpu_ns: Option<Measurement>,
    pub update_cpu_ns: Option<Measurement>,
    /// One prepare pass after ingesting the complete snapshot, before any read.
    pub prepare_cpu_ns: Option<Measurement>,
    pub read_cpu_ns: Option<Measurement>,
    pub retained_bytes: Option<Measurement>,
    pub peak_bytes: Option<Measurement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineExactMeasurement {
    pub id: String,
    pub distribution: DistributionDescriptor,
    pub environment: EnvironmentDescriptor,
    pub query: OfflineQueryDescriptor,
    pub measured_at_unix_seconds: u64,
    pub valid_until_unix_seconds: u64,
    pub provenance: MeasurementProvenance,
    pub metrics: ExactResourceMeasurements,
}

/// The companion format binds otherwise query-agnostic sketch primitives to
/// their measured readout and exact reference. Bindings describe state after
/// ingestion, without merges or intervening updates during the read sequence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineComparisonEvidence {
    pub schema_version: u32,
    /// Every CPU phase excludes other phases and destruction of retained state.
    pub timing_contract: String,
    pub sketch_evidence: EvidenceArtifact,
    pub query_bindings: Vec<MeasurementQueryBinding>,
    pub exact_records: Vec<OfflineExactMeasurement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmpiricalAccuracyRequirement {
    pub metric: String,
    pub max_observed_mean: f64,
    pub minimum_trials: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineWorkload {
    pub input_items_per_state: u64,
    /// Reads of the fixed, fully ingested snapshot; no updates between reads.
    pub reads_per_state: u64,
    pub merges_per_state: u64,
    pub state_instances: u64,
    pub horizon_seconds: f64,
}

/// Explicit scalarization: CPU ns × cpu_ns_weight + byte-seconds ×
/// retained_byte_seconds_weight. Weights must be nonnegative and not both zero.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceWeights {
    pub cpu_ns_weight: f64,
    pub retained_byte_seconds_weight: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SketchConfiguration {
    pub algorithm: SketchAlgorithm,
    pub params: SketchParams,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineComparisonRequest {
    pub context: EvidenceContext,
    pub exact_environment: EnvironmentDescriptor,
    pub query: OfflineQueryDescriptor,
    pub accuracy: EmpiricalAccuracyRequirement,
    pub workload: OfflineWorkload,
    pub weights: ResourceWeights,
    /// `Some` restricts selection to these algorithms and configurations at
    /// least as large as their formally legal deployment parameters. `None`
    /// requests a purely offline recommendation, unsuitable for formal binding.
    pub formal_minimums: Option<Vec<SketchConfiguration>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfflineResourceEstimate {
    pub cpu_ns: f64,
    pub retained_bytes: Option<f64>,
    pub retained_byte_seconds: Option<f64>,
    /// Conservative sum of per-state construction/ingestion peaks.
    pub peak_bytes_upper_bound: Option<f64>,
    /// Per-state snapshot sizes, not charged as writes in this in-memory model.
    pub serialized_bytes_per_state: Option<f64>,
    pub disk_bytes_per_state: Option<f64>,
    pub objective_cost: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfflineCandidateEstimate {
    pub record_id: String,
    pub configuration: Option<SketchConfiguration>,
    pub observed_error_mean: Option<f64>,
    pub resources: Option<OfflineResourceEstimate>,
    pub rejection: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfflineRecommendation {
    pub query: OfflineQueryDescriptor,
    pub selected: OfflineCandidateEstimate,
    pub exact_baseline: OfflineCandidateEstimate,
    pub candidates: Vec<OfflineCandidateEstimate>,
    pub estimated_cpu_savings_ns: f64,
    pub estimated_retained_bytes_savings: Option<f64>,
    /// True only if the request supplied and every selected sketch passed
    /// explicit formal minima; observed error alone cannot authorize binding.
    pub checked_formal_minimums: bool,
    pub assumptions: Vec<String>,
}

impl OfflineRecommendation {
    /// An exact selection deliberately returns no sketch. The caller must
    /// preserve its exact path; do not silently substitute a default sketch.
    pub fn selected_sketch(&self) -> Option<&SketchConfiguration> {
        self.selected.configuration.as_ref()
    }
}

/// Compare complete CPU components for a fixed snapshot and retain every
/// unavailable alternative with its reason. Without an applicable exact
/// baseline no benefit can be established, so this returns an error.
pub fn recommend_offline(
    evidence: &OfflineComparisonEvidence,
    request: &OfflineComparisonRequest,
) -> Result<OfflineRecommendation, String> {
    validate_request(request)?;
    if evidence.schema_version != 1 {
        return Err("unsupported offline comparison schema version".into());
    }
    if evidence.timing_contract != "disjoint_live_state_v1" {
        return Err(
            "comparison requires disjoint CPU phases measured with retained state alive".into(),
        );
    }
    let provider =
        EmpiricalEvidenceProvider::new(evidence.sketch_evidence.clone(), request.context.clone())
            .map_err(|e| e.to_string())?;
    let mut bindings = std::collections::HashMap::new();
    for binding in &evidence.query_bindings {
        if bindings
            .insert(&binding.record_id, &binding.query)
            .is_some()
        {
            return Err("duplicate query binding".into());
        }
        if !evidence
            .sketch_evidence
            .records
            .iter()
            .any(|r| r.id == binding.record_id)
        {
            return Err("query binding refers to missing record".into());
        }
    }
    let exact: Vec<_> = evidence
        .exact_records
        .iter()
        .filter(|r| {
            r.distribution == request.context.distribution
                && r.environment == request.exact_environment
                && r.query == request.query
                && r.measured_at_unix_seconds <= request.context.now_unix_seconds
                && request.context.now_unix_seconds <= r.valid_until_unix_seconds
        })
        .collect();
    if exact.len() != 1 {
        return Err("missing, stale, incompatible or ambiguous exact baseline".into());
    }
    let exact = exact[0];
    validate_exact(exact)?;
    let baseline_resources = estimate_exact(exact, request)?;
    let exact_baseline = OfflineCandidateEstimate {
        record_id: exact.id.clone(),
        configuration: None,
        observed_error_mean: Some(0.0),
        resources: Some(baseline_resources),
        rejection: None,
    };
    let mut candidates = Vec::new();
    for row in &evidence.sketch_evidence.records {
        let mut candidate = OfflineCandidateEstimate {
            record_id: row.id.clone(),
            configuration: Some(SketchConfiguration {
                algorithm: row.algorithm.clone(),
                params: row.params.clone(),
            }),
            observed_error_mean: row.error.as_ref().and_then(|e| e.mean),
            resources: None,
            rejection: None,
        };
        let result = (|| {
            let matched = provider
                .lookup(&row.algorithm, &row.params)
                .map_err(|e| e.to_string())?;
            if matched.id != row.id {
                return Err("record belongs to another applicability context".into());
            }
            if bindings.get(&row.id).copied() != Some(&request.query) {
                return Err("missing or incompatible measured query binding".into());
            }
            if let Some(minimums) = &request.formal_minimums {
                if !minimums.iter().any(|m| {
                    m.algorithm == row.algorithm && parameters_at_least(&row.params, &m.params)
                }) {
                    return Err(
                        "configuration does not meet the deployment's formal minimum".into(),
                    );
                }
            }
            let error = row
                .error
                .as_ref()
                .ok_or("missing offline error observation")?;
            if error.query.get("kind").and_then(|v| v.as_str()) != Some(request.query.kind.as_str())
                || error.query.get("value_type").and_then(|v| v.as_str())
                    != Some(request.query.value_type.as_str())
            {
                return Err("offline error observation has incompatible readout semantics".into());
            }
            if error.metric != request.accuracy.metric
                || error.trials < request.accuracy.minimum_trials
            {
                return Err("incompatible error metric or insufficient offline trials".into());
            }
            let mean = error.mean.ok_or("missing observed mean error")?;
            if mean > request.accuracy.max_observed_mean {
                return Err("observed error exceeds explicit offline acceptance budget".into());
            }
            estimate_sketch(row, request)
        })();
        match result {
            Ok(resources) => candidate.resources = Some(resources),
            Err(reason) => candidate.rejection = Some(reason),
        }
        candidates.push(candidate);
    }
    let mut selected = exact_baseline.clone();
    for candidate in &candidates {
        if let Some(resources) = &candidate.resources {
            if resources.objective_cost < selected.resources.as_ref().unwrap().objective_cost {
                selected = candidate.clone();
            }
        }
    }
    let baseline = exact_baseline.resources.as_ref().unwrap();
    let chosen = selected.resources.as_ref().unwrap();
    Ok(OfflineRecommendation { query: request.query.clone(),
        estimated_cpu_savings_ns: baseline.cpu_ns - chosen.cpu_ns,
        estimated_retained_bytes_savings: baseline.retained_bytes.zip(chosen.retained_bytes).map(|(a,b)| a-b),
        selected, exact_baseline, candidates, checked_formal_minimums: request.formal_minimums.is_some(),
        assumptions: vec![
            "Fixed snapshot: construct, ingest all measured input, prepare exact index once, then read without updates or merges".into(),
            "Read CPU is the measured average over all distinct keys; individual key latency and error can differ".into(),
            "CPU sums disjoint measured construction/update/prepare/read phases while state remains alive; the comparison ends with retained state and excludes retirement".into(),
            "No serialization or disk-write CPU is modeled; reported disk/serialized bytes describe one optional persisted snapshot only".into(),
            "Offline mean-error acceptance is restricted to the measured input and probe population; it is not a formal or runtime error guarantee".into(),
        ] })
}

fn validate_request(request: &OfflineComparisonRequest) -> Result<(), String> {
    let w = &request.workload;
    if request.query.kind != "point_frequency"
        || request.query.value_type != "i64"
        || request.query.probe_set != "all_distinct_keys"
    {
        return Err("unsupported offline query contract".into());
    }
    if request.accuracy.metric.trim().is_empty()
        || !nonnegative(request.accuracy.max_observed_mean)
        || request.accuracy.minimum_trials == 0
    {
        return Err("invalid empirical accuracy requirement".into());
    }
    if w.input_items_per_state != request.context.distribution.sample_count
        || w.state_instances == 0
        || !w.horizon_seconds.is_finite()
        || w.horizon_seconds <= 0.0
    {
        return Err(
            "workload must match the measured snapshot and have positive states/horizon".into(),
        );
    }
    if w.merges_per_state != 0 {
        return Err("no post-merge error or exact merge baseline was measured".into());
    }
    if !nonnegative(request.weights.cpu_ns_weight)
        || !nonnegative(request.weights.retained_byte_seconds_weight)
        || request.weights.cpu_ns_weight == 0.0
            && request.weights.retained_byte_seconds_weight == 0.0
    {
        return Err("invalid resource objective weights".into());
    }
    let a = &request.context.environment;
    let b = &request.exact_environment;
    if a.cpu != b.cpu || a.os != b.os || a.runtime != b.runtime {
        return Err(
            "sketch and exact measurements must share hardware, OS and benchmark runtime".into(),
        );
    }
    Ok(())
}

fn validate_exact(row: &OfflineExactMeasurement) -> Result<(), String> {
    let p = &row.provenance;
    if row.id.trim().is_empty()
        || [
            &p.command,
            &p.dataset,
            &p.source_revision,
            &row.environment.id,
            &row.environment.implementation,
            &row.environment.implementation_version,
        ]
        .iter()
        .any(|s| s.trim().is_empty())
        || p.repetitions == 0
        || row.measured_at_unix_seconds > row.valid_until_unix_seconds
    {
        return Err("invalid exact baseline provenance".into());
    }
    let m = &row.metrics;
    for measurement in [
        &m.empty_build_cpu_ns,
        &m.update_cpu_ns,
        &m.prepare_cpu_ns,
        &m.read_cpu_ns,
        &m.retained_bytes,
        &m.peak_bytes,
    ]
    .into_iter()
    .flatten()
    {
        if !nonnegative(measurement.value)
            || measurement.samples == 0
            || measurement.stddev.is_some_and(|s| !nonnegative(s))
        {
            return Err("invalid exact baseline measurement".into());
        }
    }
    Ok(())
}

fn charge(measurement: &Option<Measurement>, count: u64, name: &str) -> Result<f64, String> {
    if count == 0 {
        return Ok(0.0);
    }
    let value = measurement
        .as_ref()
        .ok_or_else(|| format!("missing {name}"))?
        .value
        * count as f64;
    if !nonnegative(value) {
        return Err(format!("invalid or overflowing {name}"));
    }
    Ok(value)
}

fn estimate_sketch(
    row: &OfflineMeasurement,
    request: &OfflineComparisonRequest,
) -> Result<OfflineResourceEstimate, String> {
    let m = &row.metrics;
    let w = &request.workload;
    let cpu = charge(&m.build_cpu_ns, 1, "empty sketch construction CPU")?
        + charge(
            &m.update_cpu_ns,
            w.input_items_per_state,
            "sketch update CPU",
        )?
        + charge(
            &m.read_cpu_ns,
            w.reads_per_state,
            "query-matched sketch read CPU",
        )?
        + charge(&m.merge_cpu_ns, w.merges_per_state, "sketch merge CPU")?;
    estimate_resources(
        cpu,
        &m.retained_bytes,
        &m.peak_bytes,
        m.serialized_bytes.as_ref().map(|m| m.value),
        m.disk_bytes.as_ref().map(|m| m.value),
        request,
    )
}

fn estimate_exact(
    row: &OfflineExactMeasurement,
    request: &OfflineComparisonRequest,
) -> Result<OfflineResourceEstimate, String> {
    let m = &row.metrics;
    let w = &request.workload;
    let cpu = charge(&m.empty_build_cpu_ns, 1, "exact empty construction CPU")?
        + charge(
            &m.update_cpu_ns,
            w.input_items_per_state,
            "exact update CPU",
        )?
        + charge(&m.prepare_cpu_ns, 1, "exact snapshot preparation CPU")?
        + charge(&m.read_cpu_ns, w.reads_per_state, "exact read CPU")?;
    estimate_resources(cpu, &m.retained_bytes, &m.peak_bytes, None, None, request)
}

fn estimate_resources(
    per_state_cpu: f64,
    retained: &Option<Measurement>,
    peak: &Option<Measurement>,
    serialized: Option<f64>,
    disk: Option<f64>,
    request: &OfflineComparisonRequest,
) -> Result<OfflineResourceEstimate, String> {
    let count = request.workload.state_instances as f64;
    let cpu_ns = per_state_cpu * count;
    let retained_bytes = retained.as_ref().map(|m| m.value * count);
    let retained_byte_seconds = retained_bytes.map(|v| v * request.workload.horizon_seconds);
    let peak_bytes_upper_bound = peak.as_ref().map(|m| m.value * count);
    let memory_cost = if request.weights.retained_byte_seconds_weight == 0.0 {
        0.0
    } else {
        retained_byte_seconds.ok_or("missing retained memory for weighted resource objective")?
            * request.weights.retained_byte_seconds_weight
    };
    let objective_cost = cpu_ns * request.weights.cpu_ns_weight + memory_cost;
    if [
        Some(cpu_ns),
        retained_bytes,
        retained_byte_seconds,
        peak_bytes_upper_bound,
        Some(objective_cost),
    ]
    .into_iter()
    .flatten()
    .any(|v| !nonnegative(v))
    {
        return Err("overflowing resource estimate".into());
    }
    Ok(OfflineResourceEstimate {
        cpu_ns,
        retained_bytes,
        retained_byte_seconds,
        peak_bytes_upper_bound,
        serialized_bytes_per_state: serialized,
        disk_bytes_per_state: disk,
        objective_cost,
    })
}

fn nonnegative(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

/// Conservative componentwise dominance for known planner sizing families.
/// A deployment still checks its own catalog/layout constraints before binding.
pub fn parameters_at_least(candidate: &SketchParams, minimum: &SketchParams) -> bool {
    match (candidate, minimum) {
        (SketchParams::Cms { width: a, depth: b }, SketchParams::Cms { width: c, depth: d })
        | (
            SketchParams::CountSketch { width: a, depth: b },
            SketchParams::CountSketch { width: c, depth: d },
        ) => a >= c && b >= d,
        (SketchParams::Kll { k: a }, SketchParams::Kll { k: b })
        | (SketchParams::Kmv { k: a }, SketchParams::Kmv { k: b })
        | (SketchParams::Theta { k: a }, SketchParams::Theta { k: b }) => a >= b,
        (SketchParams::Hll { precision: a }, SketchParams::Hll { precision: b }) => a >= b,
        (SketchParams::DDSketch { alpha: a }, SketchParams::DDSketch { alpha: b }) => {
            a.is_finite() && *a > 0.0 && a <= b
        }
        (
            SketchParams::CmsWithHeap {
                width: a,
                depth: b,
                heap_size: c,
            },
            SketchParams::CmsWithHeap {
                width: d,
                depth: e,
                heap_size: f,
            },
        )
        | (
            SketchParams::CountSketchWithHeap {
                width: a,
                depth: b,
                heap_size: c,
            },
            SketchParams::CountSketchWithHeap {
                width: d,
                depth: e,
                heap_size: f,
            },
        ) => a >= d && b >= e && c >= f,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(value: f64) -> Option<Measurement> {
        Some(Measurement {
            value,
            stddev: None,
            samples: 3,
            method: Some("synthetic test fixture, not measured".into()),
        })
    }

    fn fixture() -> (OfflineComparisonEvidence, OfflineComparisonRequest) {
        let mut sketches: EvidenceArtifact = serde_json::from_str(include_str!(
            "../tests/data/offline-evidence-synthetic.json"
        ))
        .unwrap();
        let query = OfflineQueryDescriptor {
            kind: "point_frequency".into(),
            value_type: "i64".into(),
            probe_set: "all_distinct_keys".into(),
        };
        let row = &mut sketches.records[0];
        row.metrics.build_cpu_ns = m(1.0);
        row.metrics.update_cpu_ns = m(1.0);
        row.metrics.read_cpu_ns = m(1.0);
        row.metrics.retained_bytes = m(100.0);
        row.error.as_mut().unwrap().mean = Some(0.5);
        row.error.as_mut().unwrap().query =
            serde_json::json!({"kind":"point_frequency","value_type":"i64"});
        let mut wide = row.clone();
        wide.id = "synthetic-wide-cms".into();
        wide.params = SketchParams::Cms {
            width: 544,
            depth: 5,
        };
        wide.metrics.update_cpu_ns = m(2.0);
        wide.metrics.retained_bytes = m(200.0);
        wide.error.as_mut().unwrap().mean = Some(0.001);
        let context = EvidenceContext {
            distribution: row.distribution.clone(),
            environment: row.environment.clone(),
            now_unix_seconds: 150,
        };
        let mut exact_environment = row.environment.clone();
        exact_environment.id = "synthetic-exact".into();
        exact_environment.implementation = "synthetic exact baseline".into();
        let exact = OfflineExactMeasurement {
            id: "exact".into(),
            distribution: row.distribution.clone(),
            environment: exact_environment.clone(),
            query: query.clone(),
            measured_at_unix_seconds: 100,
            valid_until_unix_seconds: 200,
            provenance: row.provenance.clone(),
            metrics: ExactResourceMeasurements {
                empty_build_cpu_ns: m(1.0),
                update_cpu_ns: m(5.0),
                prepare_cpu_ns: m(1000.0),
                read_cpu_ns: m(5.0),
                retained_bytes: m(1000.0),
                peak_bytes: None,
            },
        };
        let metric = row.error.as_ref().unwrap().metric.clone();
        sketches.records.push(wide);
        let bindings = sketches
            .records
            .iter()
            .map(|r| MeasurementQueryBinding {
                record_id: r.id.clone(),
                query: query.clone(),
            })
            .collect();
        let request = OfflineComparisonRequest {
            context,
            exact_environment,
            query,
            accuracy: EmpiricalAccuracyRequirement {
                metric,
                max_observed_mean: 0.01,
                minimum_trials: 1,
            },
            workload: OfflineWorkload {
                input_items_per_state: 1000,
                reads_per_state: 1000,
                merges_per_state: 0,
                state_instances: 1,
                horizon_seconds: 10.0,
            },
            weights: ResourceWeights {
                cpu_ns_weight: 1.0,
                retained_byte_seconds_weight: 0.0,
            },
            formal_minimums: None,
        };
        (
            OfflineComparisonEvidence {
                schema_version: 1,
                timing_contract: "disjoint_live_state_v1".into(),
                sketch_evidence: sketches,
                query_bindings: bindings,
                exact_records: vec![exact],
            },
            request,
        )
    }

    /// Observed acceptance rejects a cheap inaccurate rung and chooses a larger
    /// measured configuration, with every CPU component and state counted.
    #[test]
    fn accuracy_requirement_changes_selected_configuration_and_cost() {
        let (evidence, mut request) = fixture();
        request.formal_minimums = Some(vec![SketchConfiguration {
            algorithm: SketchAlgorithm::Cms,
            params: SketchParams::Cms {
                width: 512,
                depth: 5,
            },
        }]);
        let chosen = recommend_offline(&evidence, &request).unwrap();
        assert_eq!(chosen.selected.record_id, "synthetic-wide-cms");
        assert_eq!(chosen.selected.resources.as_ref().unwrap().cpu_ns, 3001.0);
        assert_eq!(
            chosen.exact_baseline.resources.as_ref().unwrap().cpu_ns,
            11001.0
        );
        assert_eq!(chosen.estimated_cpu_savings_ns, 8000.0);
        assert_eq!(chosen.estimated_retained_bytes_savings, Some(800.0));
        assert!(chosen.checked_formal_minimums);
        request.formal_minimums = None;
        request.accuracy.max_observed_mean = 1.0;
        assert_eq!(
            recommend_offline(&evidence, &request)
                .unwrap()
                .selected
                .record_id,
            "synthetic-test-cms"
        );
    }

    /// Missing required measurements and failing observed-error budgets return
    /// the applicable exact baseline, never an optimistically free sketch.
    #[test]
    fn missing_cost_or_failed_error_acceptance_selects_exact() {
        let (mut evidence, mut request) = fixture();
        request.accuracy.max_observed_mean = 0.0;
        assert!(recommend_offline(&evidence, &request)
            .unwrap()
            .selected_sketch()
            .is_none());
        request.accuracy.max_observed_mean = 0.01;
        evidence.sketch_evidence.records[1].metrics.build_cpu_ns = None;
        let chosen = recommend_offline(&evidence, &request).unwrap();
        assert!(chosen.selected_sketch().is_none());
        assert!(chosen.candidates[1]
            .rejection
            .as_ref()
            .unwrap()
            .contains("construction"));
        evidence.exact_records[0].metrics.prepare_cpu_ns = None;
        assert!(recommend_offline(&evidence, &request)
            .unwrap_err()
            .contains("preparation"));
    }

    /// Query, environment, snapshot cardinality, metric, trial count and
    /// validity are required independently; nearby evidence is not extrapolated.
    #[test]
    fn applicability_is_checked_before_recommendation() {
        let (evidence, request) = fixture();
        for case in ["query", "environment", "cardinality", "expired", "merges"] {
            let mut request = request.clone();
            match case {
                "query" => request.query.kind = "total_count".into(),
                "environment" => request.exact_environment.cpu = "other CPU".into(),
                "cardinality" => request.workload.input_items_per_state = 1001,
                "expired" => request.context.now_unix_seconds = 201,
                "merges" => request.workload.merges_per_state = 1,
                _ => unreachable!(),
            }
            assert!(recommend_offline(&evidence, &request).is_err(), "{case}");
        }
        for case in ["metric", "trials", "binding", "readout"] {
            let mut evidence = evidence.clone();
            let mut request = request.clone();
            match case {
                "metric" => request.accuracy.metric = "rank_error".into(),
                "trials" => request.accuracy.minimum_trials = 100,
                "binding" => evidence.query_bindings.clear(),
                "readout" => {
                    for row in &mut evidence.sketch_evidence.records {
                        row.error.as_mut().unwrap().query["kind"] =
                            serde_json::json!("total_count");
                    }
                }
                _ => unreachable!(),
            }
            assert!(
                recommend_offline(&evidence, &request)
                    .unwrap()
                    .selected_sketch()
                    .is_none(),
                "{case}"
            );
        }
    }

    /// Resource weights have explicit dimensions; missing memory blocks a
    /// memory-weighted objective but does not become a zero-memory estimate.
    #[test]
    fn resource_objective_and_unknown_memory_are_explicit() {
        let (mut evidence, mut request) = fixture();
        evidence.sketch_evidence.records[1].metrics.retained_bytes = None;
        let chosen = recommend_offline(&evidence, &request).unwrap();
        assert!(chosen.selected.resources.unwrap().retained_bytes.is_none());
        request.weights.retained_byte_seconds_weight = 1.0;
        assert!(recommend_offline(&evidence, &request)
            .unwrap()
            .selected_sketch()
            .is_none());
        evidence.sketch_evidence.records[1].metrics.retained_bytes = m(10000.0);
        assert!(recommend_offline(&evidence, &request)
            .unwrap()
            .selected_sketch()
            .is_none());
        request.weights.cpu_ns_weight = f64::NAN;
        assert!(recommend_offline(&evidence, &request).is_err());
    }

    /// A measured rung below a deployment's formal sizing floor cannot be
    /// selected even if its error happened to be zero on the offline input.
    #[test]
    fn formal_minimums_cannot_be_relaxed_by_observed_accuracy() {
        let (evidence, mut request) = fixture();
        request.formal_minimums = Some(vec![SketchConfiguration {
            algorithm: SketchAlgorithm::Cms,
            params: SketchParams::Cms {
                width: 1024,
                depth: 5,
            },
        }]);
        assert!(recommend_offline(&evidence, &request)
            .unwrap()
            .selected_sketch()
            .is_none());
        assert!(!parameters_at_least(
            &SketchParams::Cms {
                width: 1024,
                depth: 4
            },
            &SketchParams::Cms {
                width: 512,
                depth: 5
            }
        ));
        assert!(!parameters_at_least(
            &SketchParams::CountSketch {
                width: 1024,
                depth: 5
            },
            &SketchParams::Cms {
                width: 512,
                depth: 5
            }
        ));
    }

    /// Consuming-wrapper CPU phases and ambiguous exact generations cannot
    /// establish a complete fixed-snapshot comparison.
    #[test]
    fn comparison_requires_disjoint_phases_and_one_exact_generation() {
        let (mut evidence, request) = fixture();
        evidence.timing_contract = "consuming_upstream_wrappers".into();
        assert!(recommend_offline(&evidence, &request)
            .unwrap_err()
            .contains("disjoint"));
        evidence.timing_contract = "disjoint_live_state_v1".into();
        evidence
            .exact_records
            .push(evidence.exact_records[0].clone());
        assert!(recommend_offline(&evidence, &request)
            .unwrap_err()
            .contains("ambiguous"));
    }
}
