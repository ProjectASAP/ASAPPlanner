// cargo run -p asap-lower --bin dag_export -- \
//     --sql "SELECT service, COUNT(*) FROM metrics GROUP BY service" --name q1 \
//     --promql "topk(5, rate(http_requests_total[5m]))" --name q2
//
// Lowers each given SQL/PromQL query to pre-ASAP IR and prints a single
// `asap_types::dag_export::WorkloadGraph` as JSON on stdout — the input format
// for `tools/dag-viewer` (issue #133). Redirect to a file and load it there:
//   cargo run -p asap-lower --bin dag_export -- --sql "..." --name q1 > /tmp/dag.json
//
// `--name` is optional; an unnamed query defaults to `q<n>` (1-indexed).
//
// `--epsilon <f64>` is optional and applies to every query in the run: it
// lowers with `AccuracyTarget::Epsilon(<f64>)` instead of the default
// `AccuracyTarget::Exact`. Without it, every `AggIntent` lowers exact and
// `asap_aware_mapping::SketchAlgorithmStrategy` never has a genuine sketch
// alternative to report — so no node ever picks up a `SketchApproximation`
// note. Pass it to actually exercise that path, e.g.:
//   cargo run -p asap-lower --bin dag_export -- \
//       --epsilon 0.01 --sql "SELECT quantile(0.99, latency) FROM metrics" --name p99
//
// `--post-asap` is optional and off by default. When passed, this binary
// additionally runs `asap_aware_mapping::replacement::search_workload` (this
// binary took no strategies of its own — `default_strategies()` already
// includes `AvgToSumOverCountStrategy` as of #282) over every lowered query
// and ranks each discovered `MemoGroup` via `PlanSpace::cost_sorted`. The
// best-ranked
// candidate per group feeds two additive outputs:
//
//   - one `asap_types::dag_export::TargetReplacement` per group on whichever
//     query's `NamedGraph.replacements` contains that target node (matched
//     by `DagNode::hash` + structural equality, the same collision-safe
//     pattern `annotate_with_explanations` below already uses for notes) —
//     a small, self-contained "before -> after" pair per replacement site;
//   - one merged `NamedGraph.post_graph`: a single flattened graph per
//     query with every winning candidate spliced directly into the query's
//     own pre-ASAP shape in place, built via
//     `asap_types::dag_export::export_post_asap`.
//
// Together these surface every one of the four concrete replacement kinds:
// the sketch family `SketchAlgorithmStrategy`/`HydraGroupingStrategy` bound,
// the CSE share/recompute choice `SharedSubtreeStrategy` found, the
// workload-aware roll-up `RollupStrategy` derived, and the `avg ->
// sum/count` rewrite `AvgToSumOverCountStrategy` proposes. Without
// `--post-asap`, every existing invocation of this binary produces
// byte-identical output to before (`NamedGraph.replacements` is empty and
// `post_graph` is `None`, both skipped from the JSON entirely in that
// case). E.g.:
//   cargo run -p asap-lower --bin dag_export -- \
//       --post-asap --epsilon 0.01 \
//       --sql "SELECT quantile(0.95, latency) FROM metrics" --name q1

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use asap_aware_mapping::analytical_cost::{AnalyticalCostError, ResourceCalibration};
use asap_aware_mapping::analytical_lowering::{
    PhysicalDag, PhysicalNodeEvidence, PhysicalNodeRequest,
};
use asap_aware_mapping::analytical_planner::{
    AnalyticalPlannerCostModel, PlannerPhysicalPlanProvider,
};
use asap_aware_mapping::analytical_statistics::ComparisonScope;
use asap_aware_mapping::cost_model::DefaultCostModel;
use asap_aware_mapping::cost_model::{Cost, CostModel};
use asap_aware_mapping::replacement::{
    default_strategies_with_evidence, search_workload, search_workload_with, Replacement,
    ReplacementSubDAG,
};
use asap_aware_mapping::{AccuracyEvidenceProvider, PropagationStats};
use asap_types::cost::{BaselineRef, CostAnnotation, CostInput, CostSource, CostUnit};
use asap_types::dag_export::{
    self, DagDecision, DagGraph, DagNote, NamedGraph, PostAsapSubstitution, TargetRejection,
    TargetReplacement, TargetReplacementAfter, WorkloadGraph,
};
use asap_types::post_asap::SummaryExpr;
use asap_types::post_asap::SummaryNode;
use asap_types::post_asap::{CompositionOperator, SketchQuery, SummaryFamilyType};
use asap_types::pre_asap::cse::{structural_hash, HashCache};
use asap_types::pre_asap::query_expr::QueryExpr;
use asap_types::pre_asap::schema::{Column, DataType, Schema};
use asap_types::types::AccuracyTarget;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PlannerCostDocument {
    calibration: ResourceCalibration,
    targets: Vec<TargetPhysicalEvidence>,
}

fn parse_planner_cost_document(raw: &str) -> Result<PlannerCostDocument, String> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|error| format!("planner cost evidence is invalid JSON: {error}"))?;
    if value.get("targets").is_none() {
        return Err("the compact analytical-cost payload is no longer supported; migrate to complete per-operator physical DAG evidence".into());
    }
    let mut document: PlannerCostDocument = serde_json::from_value(value)
        .map_err(|error| format!("planner cost evidence is invalid: {error}"))?;
    for target in &mut document.targets {
        for candidate in &mut target.candidates {
            normalize_replacement_identity(&mut candidate.replacement.plan);
        }
    }
    Ok(document)
}

fn normalize_replacement_identity(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            // Accuracy guarantees are derived from the selected summary and
            // target and may round-trip through decimal JSON by one ULP. They
            // are validated before costing, but are not physical identity.
            fields.remove("guarantee");
            for child in fields.values_mut() {
                normalize_replacement_identity(child);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                normalize_replacement_identity(child);
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct TargetPhysicalEvidence {
    target: QueryExpr,
    scope: ComparisonScopeEvidence,
    query_nodes: Vec<QueryNodePhysicalEvidence>,
    candidates: Vec<CandidatePhysicalEvidence>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ComparisonScopeEvidence {
    data_arrival: asap_types::workload::DataArrival,
    planning_time_ms: u64,
    horizon_ms: u64,
    evaluation_count: u64,
    time_scope: String,
    lookback_ms: Option<u64>,
    as_of_ms: Option<u64>,
    sources: Vec<asap_aware_mapping::analytical_statistics::SourceCoverage>,
}

impl ComparisonScopeEvidence {
    fn resolve(&self) -> Result<ComparisonScope, AnalyticalCostError> {
        use asap_types::workload::{
            DurationMs, QueryRecurrence, QueryTimeScope, TimeSelection, TimestampMs,
        };
        let scope = match self.time_scope.as_str() {
            "real_time" => QueryTimeScope::RealTime,
            "longitudinal" => QueryTimeScope::Longitudinal,
            "mixed" => QueryTimeScope::Mixed,
            "unknown" => QueryTimeScope::Unknown,
            _ => return Err(AnalyticalCostError::MissingComparisonScope("time_scope")),
        };
        let comparison = ComparisonScope {
            data_arrival: self.data_arrival,
            planning_time: TimestampMs(self.planning_time_ms),
            horizon: DurationMs(self.horizon_ms),
            recurrence: QueryRecurrence::OneTime {
                invocations: self.evaluation_count,
                execute_at: None,
            },
            time_selection: TimeSelection {
                scope,
                lookback: self.lookback_ms.map(DurationMs),
                as_of: self.as_of_ms.map(TimestampMs),
            },
            sources: self.sources.clone(),
        };
        comparison.validate()?;
        Ok(comparison)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct QueryNodePhysicalEvidence {
    logical_node: QueryExpr,
    operator: asap_aware_mapping::analytical_cost::PhysicalOperator,
    occurrence: usize,
    synthetic: bool,
    evidence: PhysicalNodeEvidence,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CandidatePhysicalEvidence {
    replacement: CandidateReplacementSelector,
    dag: PhysicalDag,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CandidateReplacementSelector {
    /// Exact canonical exported replacement DAG. This is compared in full;
    /// hashes and strategy labels are never treated as identity.
    plan: serde_json::Value,
}

impl CandidateReplacementSelector {
    fn matches(&self, candidate: &ReplacementSubDAG) -> bool {
        let actual = match &candidate.replacement {
            Replacement::Summary(summary) => {
                serde_json::to_value(dag_export::export_summary(summary))
            }
            Replacement::Rewrite(query) => serde_json::to_value(dag_export::export(query)),
        };
        actual.is_ok_and(|mut actual| {
            normalize_replacement_identity(&mut actual);
            actual == self.plan
        })
    }
}

struct ExportPhysicalProvider<'a> {
    target: &'a TargetPhysicalEvidence,
    candidate: &'a CandidatePhysicalEvidence,
}

impl PlannerPhysicalPlanProvider for ExportPhysicalProvider<'_> {
    fn comparison_scope(
        &self,
        _target: &asap_aware_mapping::replacement::TargetSubDAG<'_>,
    ) -> Result<ComparisonScope, AnalyticalCostError> {
        self.target.scope.resolve()
    }

    fn query_node_evidence(
        &self,
        request: PhysicalNodeRequest<'_>,
    ) -> Result<PhysicalNodeEvidence, AnalyticalCostError> {
        let mut matches = self.target.query_nodes.iter().filter(|entry| {
            entry.logical_node == *request.logical_node
                && entry.operator == request.operator
                && entry.occurrence == request.occurrence
                && entry.synthetic == request.synthetic
        });
        let evidence = matches.next().ok_or_else(|| {
            AnalyticalCostError::MissingOperatorStatistics(format!(
                "logical occurrence {}",
                request.occurrence
            ))
        })?;
        if matches.next().is_some() {
            return Err(AnalyticalCostError::InvalidPhysicalDag(
                "duplicate query-node evidence key",
            ));
        }
        Ok(evidence.evidence.clone())
    }

    fn summary_physical_dag(
        &self,
        _summary: &Rc<SummaryNode>,
        _target: &asap_aware_mapping::replacement::TargetSubDAG<'_>,
        _scope: &ComparisonScope,
    ) -> Result<PhysicalDag, AnalyticalCostError> {
        Ok(self.candidate.dag.clone())
    }
}

struct ExportPlannerCostModel<'a> {
    document: &'a PlannerCostDocument,
}

impl ExportPlannerCostModel<'_> {
    fn bound<'a>(
        &'a self,
        candidate: &ReplacementSubDAG,
        target: &asap_aware_mapping::replacement::TargetSubDAG<'_>,
    ) -> Option<(ExportPhysicalProvider<'a>, &'a ResourceCalibration)> {
        let mut targets = self
            .document
            .targets
            .iter()
            .filter(|entry| entry.target == **target.root);
        let target_evidence = targets.next()?;
        if targets.next().is_some() {
            return None;
        }
        let mut candidates = target_evidence
            .candidates
            .iter()
            .filter(|entry| entry.replacement.matches(candidate));
        let candidate_evidence = candidates.next()?;
        if candidates.next().is_some() {
            return None;
        }
        Some((
            ExportPhysicalProvider {
                target: target_evidence,
                candidate: candidate_evidence,
            },
            &self.document.calibration,
        ))
    }

    fn annotations(
        &self,
        candidate: &ReplacementSubDAG,
        target: &Rc<QueryExpr>,
    ) -> (CostAnnotation, CostAnnotation, CostAnnotation) {
        let target = asap_aware_mapping::replacement::TargetSubDAG::new(target);
        let Some((provider, calibration)) = self.bound(candidate, &target) else {
            return winner_cost_annotations();
        };
        let Ok(model) = AnalyticalPlannerCostModel::new(&provider, calibration.clone()) else {
            return winner_cost_annotations();
        };
        let Ok(estimate) = model.estimate_candidate(candidate, &target) else {
            return winner_cost_annotations();
        };
        let version = format!("physical-dag+{}", calibration.version);
        let inputs = |resources: asap_aware_mapping::analytical_cost::ResourceEstimate| {
            vec![
                CostInput {
                    name: "estimated_cpu_ops".into(),
                    value: resources.cpu_ops,
                    unit: Some("operations".into()),
                },
                CostInput {
                    name: "estimated_peak_memory".into(),
                    value: resources.peak_memory_bytes as f64,
                    unit: Some("bytes".into()),
                },
                CostInput {
                    name: "estimated_scan".into(),
                    value: resources.scan_bytes as f64,
                    unit: Some("bytes".into()),
                },
            ]
        };
        let baseline = CostAnnotation::modeled(
            estimate.raw_cost.0,
            CostUnit::CostUnits,
            &version,
            inputs(estimate.resources.raw),
        );
        let selected = CostAnnotation::modeled(
            estimate.candidate_cost.0,
            CostUnit::CostUnits,
            &version,
            inputs(estimate.resources.candidate),
        )
        .with_baseline(BaselineRef::PreAsapRecomputation, estimate.raw_cost.0);
        let benefit = CostAnnotation {
            value: selected.delta,
            unit: CostUnit::CostUnits,
            source: CostSource::Modeled,
            baseline: Some(BaselineRef::PreAsapRecomputation),
            delta: None,
            benefit_ratio: selected.benefit_ratio,
            model_version: Some(version),
            benchmark_id: None,
            inputs: Vec::new(),
        };
        (baseline, selected, benefit)
    }
}

impl CostModel for ExportPlannerCostModel<'_> {
    fn candidate_cost_covers_complete_plan(&self) -> bool {
        true
    }

    fn candidate_cost(
        &self,
        candidate: &ReplacementSubDAG,
        target: &asap_aware_mapping::replacement::TargetSubDAG<'_>,
    ) -> Option<Cost> {
        let (provider, calibration) = self.bound(candidate, target)?;
        AnalyticalPlannerCostModel::new(&provider, calibration.clone())
            .ok()?
            .candidate_cost(candidate, target)
    }

    fn rank_candidates(
        &self,
        intent: &asap_types::pre_asap::AggIntent,
        candidates: &[asap_types::post_asap::SketchAlgorithm],
    ) -> Vec<asap_types::post_asap::SketchAlgorithm> {
        DefaultCostModel.rank_candidates(intent, candidates)
    }

    fn estimate_cost(
        &self,
        candidate: &ReplacementSubDAG,
        target: &asap_aware_mapping::replacement::TargetSubDAG<'_>,
    ) -> f64 {
        self.candidate_cost(candidate, target)
            .map_or(f64::NAN, |cost| cost.0)
    }
}

/// Baseline/selected/benefit [`CostAnnotation`]s for one [`Winner`] — issue
/// #286's "replacement-region baseline cost, selected cost, and benefit"
/// granularity item, reused verbatim for [`TargetReplacement`] and for the
/// [`DagDecision`] carried by every node the winning candidate produced or
/// carried.
///
/// `dag_export` has no deployment-owned physical evidence provider. It must
/// therefore expose costs as unavailable instead of guessing operator
/// statistics or falling back to structural node counts. Callers that have
/// complete evidence use `AnalyticalPlannerCostModel` before export and may
/// attach its dimensional comparison to these fields.
#[allow(dead_code)]
fn winner_cost_annotations() -> (CostAnnotation, CostAnnotation, CostAnnotation) {
    (
        CostAnnotation::unavailable(CostUnit::CostUnits),
        CostAnnotation::unavailable(CostUnit::CostUnits),
        CostAnnotation::unavailable(CostUnit::CostUnits),
    )
}

use asap_devtools::{lower_promql, lower_sql, SqlCatalog};

enum Lang {
    Sql,
    PromQl,
}

fn default_catalog() -> SqlCatalog {
    SqlCatalog::new()
        .with_table(
            "metrics",
            Schema::with_time_index(
                vec![
                    Column::new("ts", DataType::Timestamp, false),
                    Column::new("service", DataType::Utf8, false),
                    Column::new("region", DataType::Utf8, false),
                    Column::new("latency", DataType::Float64, false),
                    Column::new("bytes", DataType::Int64, false),
                ],
                0,
                vec![],
            ),
        )
        .with_table(
            "hosts",
            Schema::new(vec![
                Column::new("service", DataType::Utf8, false),
                Column::new("region", DataType::Utf8, false),
            ]),
        )
}

fn catalog(custom: &[String]) -> SqlCatalog {
    let mut catalog = default_catalog();
    for raw in custom {
        let value: serde_json::Value = serde_json::from_str(raw)
            .unwrap_or_else(|error| panic!("--table-schema must be valid JSON: {error}"));
        let name = value["name"]
            .as_str()
            .expect("--table-schema.name must be a string");
        let columns = value["columns"]
            .as_array()
            .expect("--table-schema.columns must be an array");
        let columns: Vec<Column> = columns
            .iter()
            .map(|column| {
                let column_name = column["name"]
                    .as_str()
                    .expect("column.name must be a string");
                let data_type = match column["type"]
                    .as_str()
                    .expect("column.type must be a string")
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "timestamp" => DataType::Timestamp,
                    "utf8" | "string" => DataType::Utf8,
                    "float64" | "double" => DataType::Float64,
                    "int64" | "bigint" => DataType::Int64,
                    other => panic!("unsupported column type {other:?}"),
                };
                Column::new(
                    column_name,
                    data_type,
                    column["nullable"].as_bool().unwrap_or(true),
                )
            })
            .collect();
        let schema = match value.get("time_index").and_then(|index| index.as_u64()) {
            Some(index) => Schema::with_time_index(columns, index as usize, vec![]),
            None => Schema::new(columns),
        };
        catalog = catalog.with_table(name, schema);
    }
    catalog
}

/// Parses `--sql "<query>" --name "<label>"` / `--promql "<query>" --name
/// "<label>"` pairs off argv, in the order given, plus one optional global
/// `--epsilon <f64>` and one optional global `--post-asap` flag. `--name` is
/// optional and applies to the immediately preceding `--sql`/`--promql`.
struct ParsedArgs {
    entries: Vec<(String, Lang, String)>,
    accuracy: AccuracyTarget,
    post_asap: bool,
    progress: bool,
    table_schemas: Vec<String>,
    planner_cost: Option<PlannerCostDocument>,
    topk_margin: Option<TopKMarginEvidence>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct TopKMarginEvidence {
    selected_lower_bound: f64,
    excluded_upper_bound: f64,
    interval_failure_probability: f64,
}

impl TopKMarginEvidence {
    fn validate(&self) -> Result<(), &'static str> {
        if !self.selected_lower_bound.is_finite() || !self.excluded_upper_bound.is_finite() {
            return Err("Top-K bounds must be finite");
        }
        if self.selected_lower_bound <= self.excluded_upper_bound {
            return Err("selected_lower_bound must exceed excluded_upper_bound");
        }
        if !(self.interval_failure_probability.is_finite()
            && self.interval_failure_probability >= 0.0
            && self.interval_failure_probability < 1.0)
        {
            return Err("interval_failure_probability must be in [0, 1)");
        }
        Ok(())
    }
}

impl AccuracyEvidenceProvider for TopKMarginEvidence {
    fn propagation_stats(
        &self,
        op: &CompositionOperator,
        _family: &SummaryFamilyType,
        _query: Option<&SketchQuery>,
    ) -> PropagationStats {
        if matches!(op, CompositionOperator::TopKSelection) {
            PropagationStats {
                topk_selected_lower_bound: Some(self.selected_lower_bound),
                topk_excluded_upper_bound: Some(self.excluded_upper_bound),
                topk_interval_failure_probability: Some(self.interval_failure_probability),
                ..Default::default()
            }
        } else {
            PropagationStats::default()
        }
    }
}

fn parse_args() -> ParsedArgs {
    let mut entries: Vec<(String, Lang, String)> = Vec::new();
    let mut pending: Option<(Lang, String)> = None;
    let mut accuracy = AccuracyTarget::Exact;
    let mut post_asap = false;
    let mut progress = false;
    let mut table_schemas = Vec::new();
    let mut planner_cost_json = None;
    let mut topk_margin_json = None;
    let mut args = std::env::args().skip(1);

    fn flush(entries: &mut Vec<(String, Lang, String)>, pending: &mut Option<(Lang, String)>) {
        if let Some((lang, query)) = pending.take() {
            entries.push((format!("q{}", entries.len() + 1), lang, query));
        }
    }

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sql" | "--promql" => {
                flush(&mut entries, &mut pending);
                let query = args
                    .next()
                    .unwrap_or_else(|| panic!("{arg} requires a query string"));
                let lang = if arg == "--sql" {
                    Lang::Sql
                } else {
                    Lang::PromQl
                };
                pending = Some((lang, query));
            }
            "--name" => {
                let name = args.next().expect("--name requires a value");
                let (lang, query) = pending.take().expect("--name must follow --sql/--promql");
                entries.push((name, lang, query));
            }
            "--epsilon" => {
                let raw = args.next().expect("--epsilon requires a value");
                let epsilon: f64 = raw
                    .parse()
                    .unwrap_or_else(|_| panic!("--epsilon must be a float, got {raw:?}"));
                accuracy = AccuracyTarget::Epsilon(epsilon);
            }
            "--post-asap" => {
                post_asap = true;
            }
            "--progress" => {
                progress = true;
            }
            "--table-schema" => {
                table_schemas.push(args.next().expect("--table-schema requires JSON"));
            }
            "--planner-cost-json" | "--analytical-cost-json" => {
                planner_cost_json = Some(
                    args.next()
                        .expect("planner cost evidence requires a JSON document"),
                );
            }
            "--topk-margin-json" => {
                topk_margin_json = Some(
                    args.next()
                        .expect("--topk-margin-json requires a JSON object"),
                );
            }
            other => panic!("unrecognized argument: {other}"),
        }
    }
    flush(&mut entries, &mut pending);
    let planner_cost = planner_cost_json
        .map(|raw| parse_planner_cost_document(&raw).unwrap_or_else(|error| panic!("{error}")));
    let topk_margin = topk_margin_json.map(|raw| {
        let evidence: TopKMarginEvidence = serde_json::from_str(&raw)
            .unwrap_or_else(|error| panic!("--topk-margin-json is invalid: {error}"));
        evidence
            .validate()
            .unwrap_or_else(|error| panic!("invalid Top-K margin evidence: {error}"));
        evidence
    });
    ParsedArgs {
        entries,
        accuracy,
        post_asap,
        progress,
        table_schemas,
        planner_cost,
        topk_margin,
    }
}

/// Attach workload-wide replacement explanations to their exact graph nodes.
/// `node_hash` is only a narrowing filter; `source_expr == Some(target)` is
/// the collision-safe identity check (`source_expr` is `None` only for a
/// post-ASAP-originated node inside a `--post-asap` `post_graph`, which this
/// function is never called on — every node it sees, from an ordinary
/// [`dag_export::export`], carries `Some`).
fn annotate_with_explanations(
    graph: &mut DagGraph,
    explanations: &[asap_aware_mapping::ReplacementExplanation],
    matched: &mut [bool],
) {
    for (i, explanation) in explanations.iter().enumerate() {
        for node in graph.nodes.iter_mut() {
            if node.hash == Some(explanation.node_hash)
                && node.source_expr.as_ref() == Some(explanation.target.as_ref())
            {
                node.notes.push(DagNote {
                    kind: format!("{:?}", explanation.kind),
                    reason: explanation.reason.clone(),
                });
                matched[i] = true;
            }
        }
    }
}

/// One `MemoGroup`'s best-ranked candidate, kept alongside its own `target`
/// — the unit both [`PostAsapResults::replacements`] and
/// [`PostAsapResults::post_graphs`] are built from, so the two outputs can
/// never disagree about which candidate won for a given target.
#[allow(dead_code)]
struct Winner<'a> {
    target: &'a Rc<QueryExpr>,
    candidate: &'a ReplacementSubDAG,
    costs: (CostAnnotation, CostAnnotation, CostAnnotation),
}

/// Short explanation intended for a selected winner in node-level UI. The
/// candidate's full rationale remains available in pre-ASAP applicability
/// notes; repeating that exhaustive prose on every post-ASAP region node
/// obscures the actual decision.
#[allow(dead_code)]
fn decision_rationale(winner: &Winner<'_>) -> String {
    match winner.candidate.strategy {
        "AvgToSumOverCountStrategy" => {
            "Rewrites AVG into SUM and COUNT under the same grouping, then divides SUM by COUNT."
                .to_string()
        }
        "SharedSubtreeStrategy" => match winner.candidate.provenance {
            asap_aware_mapping::replacement::ReplacementProvenance::CseShare => {
                "Builds the repeated subtree once and shares it across consumers.".to_string()
            }
            asap_aware_mapping::replacement::ReplacementProvenance::CseRecompute => {
                "Recomputes the subtree per consumer because that has the lower estimated cost."
                    .to_string()
            }
            _ => "Chooses the lowest-cost handling of the repeated subtree.".to_string(),
        },
        "RollupStrategy" => {
            "Answers this aggregate from a compatible finer-grained aggregate.".to_string()
        }
        "TopKLimitReuseStrategy" => {
            "Derives this smaller top-k from a compatible larger top-k result shared by the workload."
                .to_string()
        }
        _ => {
            let summary = winner
                .candidate
                .rationale
                .split_once(" — ")
                .map_or(winner.candidate.rationale.as_str(), |(summary, _)| summary);
            summary
                .split_once(" (asap_")
                .map_or(summary, |(plain, _)| plain)
                .to_string()
        }
    }
}

/// Find the index into `winners` (if any) whose `target` is structurally
/// identical to `expr` — `by_hash` is only a narrowing filter (keyed by
/// [`structural_hash`]); `expr == target` is the collision-safe identity
/// check, the same two-step discipline [`annotate_with_explanations`] and
/// this file's original single-target matching already use. Returns an
/// index rather than a `&Winner` directly so a caller can both use the
/// match and record it (e.g. `matched[i] = true`) without juggling a second
/// way to name the same winner.
#[allow(dead_code)]
fn lookup_winner(
    by_hash: &HashMap<u64, Vec<usize>>,
    winners: &[Winner<'_>],
    cache: &mut HashCache,
    expr: &QueryExpr,
) -> Option<usize> {
    let hash = structural_hash(expr, cache);
    by_hash
        .get(&hash)?
        .iter()
        .copied()
        .find(|&i| expr == winners[i].target.as_ref())
}

/// One `(decision.id, baseline_cost, selected_cost)` triple per *distinct*
/// [`DagDecision`] carried anywhere in `graph` — collapsing every node that
/// shares one `decision.id` (a replacement region can span many nodes, all
/// carrying an identical clone of the same decision) down to a single
/// entry, so a caller summing these never counts one decision's cost once
/// per node it happens to touch.
fn decision_cost_entries(graph: &DagGraph) -> Vec<(u32, CostAnnotation, CostAnnotation)> {
    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();
    for node in &graph.nodes {
        let Some(decision) = &node.decision else {
            continue;
        };
        if !seen.insert(decision.id) {
            continue;
        }
        let (Some(baseline), Some(selected)) = (&decision.baseline_cost, &decision.selected_cost)
        else {
            continue;
        };
        entries.push((decision.id, baseline.clone(), selected.clone()));
    }
    entries
}

/// Build a [`TargetReplacement`] for `winner`, matching this file's own
/// per-target `before`/`after` construction.
#[allow(dead_code)]
fn target_replacement(
    decision_id: u32,
    target_pre_id: u32,
    winner: &Winner<'_>,
) -> TargetReplacement {
    let strategy = winner.candidate.strategy.to_string();
    let before = dag_export::export(winner.target);
    let after = match &winner.candidate.replacement {
        Replacement::Summary(node) => {
            TargetReplacementAfter::Summary(dag_export::export_summary(node))
        }
        Replacement::Rewrite(rewritten) => {
            TargetReplacementAfter::Rewrite(dag_export::export(rewritten))
        }
    };
    let (baseline_cost, selected_cost, benefit) = winner.costs.clone();
    // Derived from `selected_cost` so the legacy scalar field and the
    // structured annotation can never drift apart.
    let cost = selected_cost.value.unwrap_or(f64::NAN);
    TargetReplacement {
        decision_id,
        target_pre_id,
        strategy,
        rationale: decision_rationale(winner),
        rank: 0,
        cost,
        before,
        after,
        baseline_cost: Some(baseline_cost),
        selected_cost: Some(selected_cost),
        benefit: Some(benefit),
    }
}

/// The two additive `--post-asap` outputs — see this file's top-of-file
/// usage doc for what each is for.
struct PostAsapResults {
    /// One `(query_name, TargetReplacement)` pair per discovered replacement
    /// site whose target node is found in that query's own exported graph. A
    /// target can in principle be reachable from more than one query's root
    /// after CSE (a shared subtree), in which case it yields one pair per
    /// matching query, each with that query's own `target_pre_id`.
    replacements: Vec<(String, TargetReplacement)>,
    /// One merged, whole-query [`DagGraph`] per query, built via
    /// [`dag_export::export_post_asap`] — every winning candidate spliced
    /// directly into that query's own pre-ASAP shape in place.
    post_graphs: Vec<(String, DagGraph)>,
    /// One `(query_name, TargetRejection)` per accuracy-illegal candidate
    /// the search refused (`MemoGroup::rejected`, issue #172) whose target
    /// node is found in that query's own exported graph.
    rejections: Vec<(String, TargetRejection)>,
}

fn raw_only_post_asap_results() -> PostAsapResults {
    PostAsapResults {
        replacements: Vec::new(),
        post_graphs: Vec::new(),
        rejections: Vec::new(),
    }
}

/// Assign collision-free, explicit identities to structurally equal nodes
/// across a set of exported query graphs. The full canonical subtree string
/// is the equality key; the compact integer is what JSON consumers receive.
/// Consequently the viewer never needs to guess identity from labels,
/// hashes, or a client-side node signature.
fn assign_workload_node_ids(graphs: &mut [&mut DagGraph]) {
    fn key_for(id: u32, graph: &DagGraph, memo: &mut HashMap<u32, String>) -> String {
        if let Some(key) = memo.get(&id) {
            return key.clone();
        }
        let node = &graph.nodes[id as usize];
        let child_keys: Vec<_> = node
            .children
            .iter()
            .map(|child| key_for(*child, graph, memo))
            .collect();
        let key = serde_json::to_string(&(node.kind, &node.detail, &node.schema, child_keys))
            .expect("exported DAG node content is serializable");
        memo.insert(id, key.clone());
        key
    }

    let mut ids = HashMap::<String, u32>::new();
    let mut next_id = 0_u32;
    for graph in graphs.iter_mut() {
        let mut memo = HashMap::new();
        let keys: Vec<_> = graph
            .nodes
            .iter()
            .map(|node| key_for(node.id, graph, &mut memo))
            .collect();
        for (node, key) in graph.nodes.iter_mut().zip(keys) {
            let id = *ids.entry(key).or_insert_with(|| {
                let id = next_id;
                next_id += 1;
                id
            });
            node.workload_node_id = Some(id);
        }
    }
}

/// Run `asap_aware_mapping::replacement::search_workload` (its own
/// `default_strategies()` — which includes `AvgToSumOverCountStrategy` as of
/// #282 — is exactly the strategy set this binary wants; no custom list
/// needed) over every lowered query, rank each discovered `MemoGroup` via
/// `PlanSpace::cost_sorted`, and build both `--post-asap` outputs from the
/// exact same set of winning candidates (see [`Winner`]), so the flat
/// `replacements` list and the merged `post_graph` can never disagree about
/// which candidate won for a given target.
#[allow(dead_code)]
fn run_post_asap_with_progress(
    lowered_queries: &[(String, String, QueryExpr)],
    progress: bool,
    cost_model: &dyn CostModel,
    export_model: Option<&ExportPlannerCostModel<'_>>,
    evidence: Option<&dyn AccuracyEvidenceProvider>,
) -> PostAsapResults {
    let mapping_started = Instant::now();
    if progress {
        eprintln!("[3/4] ASAP-aware mapping is running…");
    }
    let roots: Vec<(String, Rc<QueryExpr>)> = lowered_queries
        .iter()
        .map(|(name, _, qe)| (name.clone(), Rc::new(qe.clone())))
        .collect();
    let strategies;
    let space = if let Some(evidence) = evidence {
        strategies = default_strategies_with_evidence(cost_model, evidence);
        search_workload_with(roots, &strategies)
    } else {
        search_workload(roots)
    };
    let ranked_groups = space.cost_sorted(cost_model);

    // A group's top candidate can be `keep_pre_asap`'s own conservative
    // fallback — `Replacement::Summary(SummaryNode { expr:
    // KeepPreAsap(Rc::new(target.clone())), .. })` — the *whole target*
    // wrapped as unbound, e.g. for a multi-measure/`HAVING`-bearing
    // aggregate, or (the case that actually surfaces this: `STDDEV_POP`/
    // `AVG`/`VARIANCE` dispatch to `Implementation::PassThrough` with no
    // alternative at all, per `implementations_for_with`'s own doc) an
    // intent with no summary realization whatsoever. This isn't a
    // replacement decision — it's `SketchAlgorithmStrategy` saying "nothing
    // to bind here" — the identical "no-op candidate" concept
    // `explanation.rs`'s own `sketch_finding_reason` already excludes from
    // being reported as a finding ("a candidate list containing only the
    // trivial no-op realization... isn't an opportunity, it's just the
    // target's existing shape reflected back"). Filtered out here for a
    // second, load-bearing reason beyond just matching that precedent:
    // `export_post_asap`'s `find_winner` re-checks every node reached
    // inside a spliced-in `KeepPreAsap` payload (by design, so a target
    // nested underneath one still gets found) — if that payload structurally
    // *is* the enclosing target, `find_winner` immediately matches the same
    // winner again, forever. Treating this candidate as "no winner" (same
    // as an empty candidate list) avoids ever handing `export_post_asap` a
    // winner that can't help but recurse into itself.
    let winners: Vec<Winner<'_>> = ranked_groups
        .iter()
        .filter_map(|group| {
            let target = asap_aware_mapping::replacement::TargetSubDAG::with_consumer_count(
                group.target,
                group.consumer_count,
            );
            let candidate = group
                .candidates
                .iter()
                .copied()
                .find(|candidate| cost_model.candidate_cost(candidate, &target).is_some())?;
            if matches!(
                &candidate.replacement,
                Replacement::Summary(node) if matches!(node.expr, SummaryExpr::KeepPreAsap(_))
            ) {
                return None;
            }
            Some(Winner {
                target: group.target,
                candidate,
                costs: export_model.map_or_else(winner_cost_annotations, |model| {
                    model.annotations(candidate, group.target)
                }),
            })
        })
        .collect();

    // `by_hash` only narrows the search; `lookup_winner`'s own structural
    // equality check is the real decision — see that function's doc.
    let mut by_hash_cache = HashCache::new();
    let mut by_hash: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, winner) in winners.iter().enumerate() {
        let hash = structural_hash(winner.target, &mut by_hash_cache);
        by_hash.entry(hash).or_default().push(i);
    }
    if progress {
        eprintln!(
            "[3/4] ASAP-aware mapping done in {:.2} ms",
            mapping_started.elapsed().as_secs_f64() * 1000.0
        );
    }

    // ---- Merged, whole-query `post_graph`, one per query ---------------
    //
    // Built *before* the flat `replacements` pass below, not after: a
    // winner whose target only exists inside another winner's own
    // `Replacement::Rewrite` output (e.g. the `sum`/`count` aggregates
    // `AvgToSumOverCountStrategy`'s rewrite exposes, which
    // `default_strategies()` — #282 — now discovers and independently
    // sketch-ranks in the same search pass) can never appear in any query's
    // *original*, pre-rewrite `graph` — there's nothing wrong with that
    // winner, it's just nested. `post_graph` is where it's expected to
    // surface instead (`export_post_asap`'s recursive `find_winner`
    // threading walks straight through a rewritten subtree and re-checks
    // every node inside it too), so the flat-`replacements` pass below
    // checks there before deciding a miss is a real anomaly worth a
    // warning.
    if progress {
        eprintln!("[4/4] Post-ASAP DAG generation is running…");
    }
    let post_started = Instant::now();
    let mut post_graph_cache = HashCache::new();
    let mut find_winner = |expr: &QueryExpr| -> Option<PostAsapSubstitution> {
        let i = lookup_winner(&by_hash, &winners, &mut post_graph_cache, expr)?;
        let winner = &winners[i];
        let (baseline_cost, selected_cost, benefit) = winner.costs.clone();
        // Derived from `selected_cost`; see `target_replacement`'s identical
        // derivation.
        let cost = selected_cost.value.unwrap_or(f64::NAN);
        let decision = DagDecision {
            id: i as u32,
            strategy: winner.candidate.strategy.to_string(),
            rationale: decision_rationale(winner),
            rank: 0,
            cost,
            role: "replacement_region",
            baseline_cost: Some(baseline_cost),
            selected_cost: Some(selected_cost),
            benefit: Some(benefit),
        };
        Some(match &winners[i].candidate.replacement {
            Replacement::Rewrite(rc) => PostAsapSubstitution::Rewrite {
                replacement: Rc::clone(rc),
                decision,
            },
            Replacement::Summary(rc) => PostAsapSubstitution::Summary {
                replacement: Rc::clone(rc),
                decision,
            },
        })
    };
    let post_graphs: Vec<(String, DagGraph)> = lowered_queries
        .iter()
        .map(|(name, _, qe)| {
            (
                name.clone(),
                dag_export::export_post_asap(qe, &mut find_winner),
            )
        })
        .collect();

    // ---- Flat per-target `replacements`, one list per query -----------
    //
    // Independently re-export every query's own graph for matching — a
    // fresh `export` per query, not reused from `main`'s own already-built
    // `NamedGraph`s, so this function stays self-contained and callable on
    // its own (see this file's `#[cfg(test)]` module). Deliberately anchored
    // to the *original* `graph` only (never `post_graph`) — `target_pre_id`
    // is documented as an id into `NamedGraph.graph.nodes`, so a nested
    // secondary target (see above) never gets a flat entry of its own here:
    // it's already visible, in place, inside its parent's own `after`
    // subtree and inside `post_graph` as a whole.
    let mut lookup_cache = HashCache::new();
    let mut replacements = Vec::new();
    let mut rejections = Vec::new();
    let mut matched = vec![false; winners.len()];
    // Groups with accuracy-refused candidates (issue #172): matched to a
    // query's graph nodes the same hash-then-structural-equality way.
    let rejected_groups: Vec<_> = space
        .groups()
        .filter(|group| !group.rejected.is_empty())
        .collect();
    let mut rejected_by_hash: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, group) in rejected_groups.iter().enumerate() {
        let hash = structural_hash(&group.target, &mut by_hash_cache);
        rejected_by_hash.entry(hash).or_default().push(i);
    }
    for (name, _, qe) in lowered_queries {
        let graph = dag_export::export(qe);
        for node in &graph.nodes {
            let Some(source_expr) = node.source_expr.as_ref() else {
                continue; // never true for a plain `export` — defensive only.
            };
            if let Some(i) = lookup_winner(&by_hash, &winners, &mut lookup_cache, source_expr) {
                replacements.push((
                    name.clone(),
                    target_replacement(i as u32, node.id, &winners[i]),
                ));
                matched[i] = true;
            }
            let hash = structural_hash(source_expr, &mut lookup_cache);
            for &i in rejected_by_hash.get(&hash).into_iter().flatten() {
                let group = rejected_groups[i];
                if *source_expr != *group.target {
                    continue;
                }
                rejections.extend(group.rejected.iter().map(|rejected| {
                    (
                        name.clone(),
                        TargetRejection {
                            target_pre_id: node.id,
                            strategy: rejected.strategy.to_string(),
                            description: rejected.description.clone(),
                            error: rejected.error.clone(),
                        },
                    )
                }));
            }
        }
    }

    // A winner can legitimately stay unmatched here: `default_strategies()`
    // (#282) means `AvgToSumOverCountStrategy`'s rewrite output (and
    // similarly `RollupStrategy`'s) can expose a brand-new `sum`/`count`
    // descendant that the *same* search pass then independently discovers
    // and ranks — a real winner, but one with no node anywhere in any
    // query's original, pre-rewrite `graph` to attach a flat entry to
    // (`target_pre_id` is documented as an id into `graph.nodes`
    // specifically). This isn't a data loss: `export_post_asap` still
    // splices that winner in, in place, inside `post_graph` — see this
    // function's own construction of `post_graphs` above, which walks
    // straight through a rewritten subtree and resolves every nested
    // winner too, recursively. So an unmatched winner here is expected,
    // not necessarily a bug, whenever it's downstream of some other
    // winner's own `Replacement::Rewrite` — logged as an FYI rather than a
    // warning, since telling the two cases apart precisely would mean
    // reimplementing `search`'s own private descendant-discovery walk
    // (`discover_new_descendant_targets` in `asap_aware_mapping::replacement`,
    // not exposed) a second time here just to double-check something
    // `post_graph`'s own construction already handled correctly.
    for (winner, matched) in winners.iter().zip(&matched) {
        if !matched {
            let strategy = winner.candidate.strategy;
            eprintln!(
                "dag_export: post-asap replacement ({strategy}) has no node in any query's \
                 original graph — expected for a winner exposed only inside another winner's \
                 own rewrite output (e.g. a sum/count descendant of an avg rewrite); still \
                 present in that query's post_graph"
            );
        }
    }
    if progress {
        eprintln!(
            "[4/4] Post-ASAP DAG generation done in {:.2} ms",
            post_started.elapsed().as_secs_f64() * 1000.0
        );
    }

    PostAsapResults {
        replacements,
        post_graphs,
        rejections,
    }
}

#[cfg(test)]
fn run_post_asap(lowered_queries: &[(String, String, QueryExpr)]) -> PostAsapResults {
    run_post_asap_with_progress(lowered_queries, false, &DefaultCostModel, None, None)
}

#[tokio::main]
async fn main() {
    let ParsedArgs {
        entries,
        accuracy,
        post_asap,
        progress,
        table_schemas,
        planner_cost,
        topk_margin,
    } = parse_args();
    let sql_catalog = catalog(&table_schemas);
    let planner_started = Instant::now();
    if entries.is_empty() {
        eprintln!(
            "usage: dag_export --sql \"<query>\" [--name <label>] [--epsilon <f64>] [--post-asap] ..."
        );
        std::process::exit(1);
    }

    let mut lowered_queries = Vec::new();
    let lowering_started = Instant::now();
    if progress {
        eprintln!("[1/4] Parsing and lowering SQL/PromQL queries…");
    }
    for (name, lang, query) in entries {
        let lowered = match lang {
            Lang::Sql => lower_sql(&query, &sql_catalog, accuracy.clone())
                .await
                .map_err(|e| e.to_string()),
            Lang::PromQl => lower_promql(&query, accuracy.clone()).map_err(|e| e.to_string()),
        };
        match lowered {
            Ok(qe) => {
                lowered_queries.push((name, query, qe));
            }
            Err(e) => eprintln!("skipping {name:?} — lowering failed: {e}"),
        }
    }
    if progress {
        eprintln!(
            "[1/4] Parsing and lowering done in {:.2} ms",
            lowering_started.elapsed().as_secs_f64() * 1000.0
        );
    }

    let pre_started = Instant::now();
    if progress {
        eprintln!("[2/4] Pre-ASAP DAG generation is running…");
    }
    let explanations = asap_aware_mapping::explain_replacements(
        lowered_queries
            .iter()
            .map(|(name, _, qe)| (name.clone(), qe.clone()))
            .collect(),
    );
    let mut matched = vec![false; explanations.len()];
    let mut queries = Vec::new();
    for (name, source, qe) in &lowered_queries {
        let mut graph = dag_export::export(qe);
        annotate_with_explanations(&mut graph, &explanations, &mut matched);
        queries.push(NamedGraph {
            name: name.clone(),
            source: Some(source.clone()),
            graph,
            replacements: Vec::new(),
            post_graph: None,
            workload_cost: None,
            rejections: Vec::new(),
        });
    }
    for (explanation, matched) in explanations.iter().zip(matched) {
        if !matched {
            eprintln!(
                "dag_export: explanation at {} ({:?}, node_hash={}) matched no DagNode",
                explanation.location, explanation.kind, explanation.node_hash
            );
        }
    }
    if progress {
        eprintln!(
            "[2/4] Pre-ASAP DAG generation done in {:.2} ms",
            pre_started.elapsed().as_secs_f64() * 1000.0
        );
    }

    if post_asap {
        let results = if let Some(document) = planner_cost.as_ref() {
            let model = ExportPlannerCostModel { document };
            run_post_asap_with_progress(
                &lowered_queries,
                progress,
                &model,
                Some(&model),
                topk_margin
                    .as_ref()
                    .map(|evidence| evidence as &dyn AccuracyEvidenceProvider),
            )
        } else {
            eprintln!(
                "dag_export: --post-asap requires complete deployment-owned physical-plan evidence; exporting the raw plan only"
            );
            raw_only_post_asap_results()
        };
        for (query_name, replacement) in results.replacements {
            if let Some(named) = queries.iter_mut().find(|q| q.name == query_name) {
                named.replacements.push(replacement);
            }
        }
        for (query_name, post_graph) in results.post_graphs {
            if let Some(named) = queries.iter_mut().find(|q| q.name == query_name) {
                named.post_graph = Some(post_graph);
            }
        }
        for (query_name, rejection) in results.rejections {
            if let Some(named) = queries.iter_mut().find(|q| q.name == query_name) {
                named.rejections.push(rejection);
            }
        }
    }

    {
        let mut pre_graphs: Vec<_> = queries.iter_mut().map(|query| &mut query.graph).collect();
        assign_workload_node_ids(&mut pre_graphs);
    }
    {
        let mut post_graphs: Vec<_> = queries
            .iter_mut()
            .filter_map(|query| query.post_graph.as_mut())
            .collect();
        assign_workload_node_ids(&mut post_graphs);
    }

    // Whole selected-workload cost/benefit (issue #286) — per query, and
    // for the whole selected workload. `decision.id` (== the winning
    // `Winner`'s own index — see `run_post_asap_with_progress`) is already
    // a collision-free dedup key for a decision shared across multiple
    // nodes (a replacement region spans several nodes, all carrying the
    // same `decision.id`) and across multiple queries (a CSE-shared target
    // reachable from more than one query's root) alike, so it's reused
    // directly as `sum_workload_costs`'s dedup key — no separate lookup
    // needed.
    let mut workload_entries = Vec::new();
    for query in &mut queries {
        let Some(post_graph) = &query.post_graph else {
            continue;
        };
        let entries = decision_cost_entries(post_graph);
        if entries.is_empty() {
            continue;
        }
        match asap_types::cost::workload_cost_summary(
            entries
                .iter()
                .map(|(id, baseline, selected)| (Some(*id), baseline, selected)),
            "dag_export-workload-cost-v1",
        ) {
            Ok(summary) => query.workload_cost = Some(summary),
            Err(mismatch) => eprintln!(
                "dag_export: workload cost aggregation for {:?} skipped — {mismatch}",
                query.name
            ),
        }
        workload_entries.extend(entries);
    }
    let workload_cost = if workload_entries.is_empty() {
        None
    } else {
        match asap_types::cost::workload_cost_summary(
            workload_entries
                .iter()
                .map(|(id, baseline, selected)| (Some(*id), baseline, selected)),
            "dag_export-workload-cost-v1",
        ) {
            Ok(summary) => Some(summary),
            Err(mismatch) => {
                eprintln!("dag_export: workload-wide cost aggregation skipped — {mismatch}");
                None
            }
        }
    };

    let workload = WorkloadGraph {
        queries,
        workload_cost,
    };
    if progress {
        eprintln!(
            "Total planner time: {:.2} ms",
            planner_started.elapsed().as_secs_f64() * 1000.0
        );
    }
    println!("{}", serde_json::to_string_pretty(&workload).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    use asap_aware_mapping::analytical_cost::{
        ExecutionMultiplicity, PhysicalDagNode, PhysicalOperator,
    };
    use asap_aware_mapping::analytical_lowering::lower_query_physical_dag;
    use asap_aware_mapping::analytical_statistics::{
        EdgeStatistics, OperatorStatistics, SourceCoverage,
    };
    use asap_types::pre_asap::{Column, DataType, Reduction, Schema, Source};

    fn non_topk_query() -> QueryExpr {
        QueryExpr::Aggregate {
            reduction: Reduction::by(vec![]),
            measures: vec![asap_types::pre_asap::AggIntent::Count {
                accuracy: AccuracyTarget::Epsilon(0.1),
            }],
            output_names: vec![],
            having: None,
            child: Rc::new(QueryExpr::Scan {
                source: Source::Table {
                    table_ref: "events".into(),
                },
                predicates: vec![],
                schema: Schema::new(vec![Column::new("v", DataType::Int64, false)]),
            }),
        }
    }

    fn test_scope() -> ComparisonScopeEvidence {
        ComparisonScopeEvidence {
            data_arrival: asap_types::workload::DataArrival::AtRest,
            planning_time_ms: 1_000,
            horizon_ms: 10_000,
            evaluation_count: 10,
            time_scope: "longitudinal".into(),
            lookback_ms: Some(10_000),
            as_of_ms: Some(1_000),
            sources: vec![SourceCoverage {
                source: Source::Table {
                    table_ref: "events".into(),
                },
                snapshot_id: "snapshot-1".into(),
                predicates: vec![],
            }],
        }
    }

    fn edge(rows: u64, bytes: u64) -> EdgeStatistics {
        EdgeStatistics { rows, bytes }
    }

    fn query_evidence(query: &QueryExpr) -> Vec<QueryNodePhysicalEvidence> {
        let entries = RefCell::new(Vec::new());
        let scope = test_scope().resolve().unwrap();
        let provider = |request: PhysicalNodeRequest<'_>| {
            let (statistics, output_buffer_bytes) = match request.operator {
                PhysicalOperator::Scan => (
                    OperatorStatistics {
                        source_scan_bytes: 64_000,
                        inputs: vec![edge(1_000, 64_000)],
                        output: edge(1_000, 64_000),
                        group_count: None,
                        key_bytes: None,
                        aggregate_value_bytes: None,
                        k: None,
                        hash_join_build_side: None,
                        promql: None,
                    },
                    64_000,
                ),
                PhysicalOperator::HashAggregate => (
                    OperatorStatistics {
                        source_scan_bytes: 0,
                        inputs: vec![edge(1_000, 64_000)],
                        output: edge(100, 2_400),
                        group_count: Some(100),
                        key_bytes: Some(8),
                        aggregate_value_bytes: Some(8),
                        k: None,
                        hash_join_build_side: None,
                        promql: None,
                    },
                    2_400,
                ),
                _ => return Err(AnalyticalCostError::UnsupportedQueryOperator),
            };
            let evidence = PhysicalNodeEvidence {
                physical_id: format!("{:?}-{}", request.operator, request.occurrence),
                statistics,
                output_buffer_bytes,
            };
            entries.borrow_mut().push(QueryNodePhysicalEvidence {
                logical_node: request.logical_node.clone(),
                operator: request.operator,
                occurrence: request.occurrence,
                synthetic: request.synthetic,
                evidence: evidence.clone(),
            });
            Ok(evidence)
        };
        lower_query_physical_dag(&Rc::new(query.clone()), &scope, &provider).unwrap();
        entries.into_inner()
    }

    fn cheap_candidate_dag() -> PhysicalDag {
        let coverage = test_scope().sources[0].clone();
        let statistics = OperatorStatistics {
            source_scan_bytes: 64,
            inputs: vec![edge(100, 2_400)],
            output: edge(100, 2_400),
            group_count: None,
            key_bytes: None,
            aggregate_value_bytes: None,
            k: None,
            hash_join_build_side: None,
            promql: None,
        };
        PhysicalDag {
            nodes: vec![PhysicalDagNode {
                id: "summary-read".into(),
                operator: PhysicalOperator::Scan,
                children: vec![],
                source_coverage: Some(coverage),
                output_buffer_bytes: 2_400,
                retained_bytes: 0,
                execution: ExecutionMultiplicity::PerEvaluation,
            }],
            root: "summary-read".into(),
            evidence: [(
                "summary-read".into(),
                PhysicalNodeEvidence {
                    physical_id: "summary-read".into(),
                    statistics,
                    output_buffer_bytes: 2_400,
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    fn cost_fixture() -> (QueryExpr, ReplacementSubDAG, PlannerCostDocument) {
        let query = non_topk_query();
        let root = Rc::new(query.clone());
        let space = search_workload(vec![(String::from("q"), Rc::clone(&root))]);
        let group = space
            .groups()
            .find(|group| *group.target == query)
            .expect("aggregate memo group");
        let candidate = group
            .candidates
            .iter()
            .find(|candidate| {
                !matches!(
                    &candidate.replacement,
                    Replacement::Summary(node)
                        if matches!(node.expr, SummaryExpr::KeepPreAsap(_))
                )
            })
            .expect("summary candidate")
            .clone();
        let plan = match &candidate.replacement {
            Replacement::Summary(summary) => {
                serde_json::to_value(dag_export::export_summary(summary)).unwrap()
            }
            Replacement::Rewrite(rewrite) => {
                serde_json::to_value(dag_export::export(rewrite)).unwrap()
            }
        };
        let document = PlannerCostDocument {
            calibration: ResourceCalibration {
                cost_per_cpu_op: 1.0,
                cost_per_scan_byte: 1.0,
                cost_per_retained_byte: 1.0,
                version: "test".into(),
            },
            targets: vec![TargetPhysicalEvidence {
                target: query.clone(),
                scope: test_scope(),
                query_nodes: query_evidence(&query),
                candidates: vec![CandidatePhysicalEvidence {
                    replacement: CandidateReplacementSelector { plan },
                    dag: cheap_candidate_dag(),
                }],
            }],
        };
        (query, candidate, document)
    }

    #[test]
    fn planner_json_costs_and_exports_a_generic_non_topk_winner() {
        let (query, candidate, document) = cost_fixture();
        let json = serde_json::to_string(&document).unwrap();
        let parsed = parse_planner_cost_document(&json).unwrap();
        assert_eq!(parsed.targets[0].target, query);
        assert!(parsed.targets[0].candidates[0]
            .replacement
            .matches(&candidate));
        let model = ExportPlannerCostModel { document: &parsed };
        let target_rc = Rc::new(query.clone());
        let target = asap_aware_mapping::replacement::TargetSubDAG::new(&target_rc);
        let (provider, calibration) = model.bound(&candidate, &target).expect("exact binding");
        let estimate = AnalyticalPlannerCostModel::new(&provider, calibration.clone())
            .unwrap()
            .estimate_candidate(&candidate, &target)
            .unwrap();
        assert!(estimate.candidate_cost < estimate.raw_cost);
        let (baseline, selected, benefit) = model.annotations(&candidate, &Rc::new(query));
        assert!(baseline.value.is_some());
        assert!(selected.value.is_some());
        assert!(benefit.value.is_some());
        assert!(selected
            .inputs
            .iter()
            .any(|input| input.name == "estimated_scan"));
        assert!(!selected.inputs.iter().any(|input| input.name == "topk_k"));
    }

    #[test]
    fn duplicate_target_candidate_and_query_evidence_each_fail_closed() {
        let (query, candidate, document) = cost_fixture();
        let target_rc = Rc::new(query);
        let target = asap_aware_mapping::replacement::TargetSubDAG::new(&target_rc);

        let mut duplicate_target = document.clone();
        duplicate_target
            .targets
            .push(duplicate_target.targets[0].clone());
        assert!(ExportPlannerCostModel {
            document: &duplicate_target
        }
        .candidate_cost(&candidate, &target)
        .is_none());

        let mut duplicate_candidate = document.clone();
        let candidate_entry = duplicate_candidate.targets[0].candidates[0].clone();
        duplicate_candidate.targets[0]
            .candidates
            .push(candidate_entry);
        assert!(ExportPlannerCostModel {
            document: &duplicate_candidate
        }
        .candidate_cost(&candidate, &target)
        .is_none());

        let mut duplicate_query = document;
        let query_entry = duplicate_query.targets[0].query_nodes[0].clone();
        duplicate_query.targets[0].query_nodes.push(query_entry);
        assert!(ExportPlannerCostModel {
            document: &duplicate_query
        }
        .candidate_cost(&candidate, &target)
        .is_none());
    }

    #[test]
    fn cost_annotations_fail_closed_without_physical_evidence() {
        let (baseline, selected, benefit) = winner_cost_annotations();
        for annotation in [baseline, selected, benefit] {
            assert!(annotation.value.is_none());
            assert_eq!(annotation.source, asap_types::cost::CostSource::Unavailable);
            assert_eq!(annotation.unit, CostUnit::CostUnits);
        }
    }

    #[test]
    fn missing_physical_evidence_keeps_the_export_raw_only() {
        let results = raw_only_post_asap_results();
        assert!(results.replacements.is_empty());
        assert!(results.post_graphs.is_empty());
    }

    #[test]
    fn compact_analytical_payload_has_an_explicit_migration_error() {
        let error = parse_planner_cost_document(r#"{"inputs":{"group_count":10}}"#)
            .expect_err("legacy payload must fail");
        assert!(error.contains("compact analytical-cost payload"));
    }

    #[test]
    fn exact_candidate_selector_does_not_confuse_the_same_strategy() {
        let selected_query = lower_promql("up", AccuracyTarget::Exact).unwrap();
        let other_query = lower_promql("process_cpu_seconds_total", AccuracyTarget::Exact).unwrap();
        let selected = ReplacementSubDAG {
            replacement: Replacement::Rewrite(Rc::new(selected_query.clone())),
            strategy: "same-strategy",
            provenance: asap_aware_mapping::replacement::ReplacementProvenance::LogicalRewrite,
            rationale: String::new(),
        };
        let other = ReplacementSubDAG {
            replacement: Replacement::Rewrite(Rc::new(other_query)),
            strategy: "same-strategy",
            provenance: asap_aware_mapping::replacement::ReplacementProvenance::LogicalRewrite,
            rationale: String::new(),
        };
        let selector = CandidateReplacementSelector {
            plan: serde_json::to_value(dag_export::export(&selected_query)).unwrap(),
        };
        assert!(selector.matches(&selected));
        assert!(!selector.matches(&other));
    }

    #[test]
    fn workload_wide_explanations_annotate_cross_query_reuse() {
        let query = "sum by (job) (rate(http_requests_total[5m]))";
        let a = lower_promql(query, AccuracyTarget::Exact).unwrap();
        let b = lower_promql(query, AccuracyTarget::Exact).unwrap();
        let explanations =
            asap_aware_mapping::explain_replacements(vec![("a", a.clone()), ("b", b.clone())]);
        assert!(explanations
            .iter()
            .any(|e| { e.kind == asap_aware_mapping::ExplanationKind::CommonSubexpressionReuse }));

        let mut matched = vec![false; explanations.len()];
        let mut graph_a = dag_export::export(&a);
        let mut graph_b = dag_export::export(&b);
        annotate_with_explanations(&mut graph_a, &explanations, &mut matched);
        annotate_with_explanations(&mut graph_b, &explanations, &mut matched);
        assert!(graph_a.nodes.iter().any(|n| !n.notes.is_empty()));
        assert!(graph_b.nodes.iter().any(|n| !n.notes.is_empty()));
    }

    #[test]
    fn hash_collision_without_structural_equality_does_not_attach_a_note() {
        let target = lower_promql(
            "quantile(0.99, rate(http_requests_total[5m]))",
            AccuracyTarget::Epsilon(0.01),
        )
        .unwrap();
        let explanations = asap_aware_mapping::explain_replacements(vec![("target", target)]);
        let explanation = explanations
            .iter()
            .find(|e| e.kind == asap_aware_mapping::ExplanationKind::SketchApproximation)
            .unwrap();

        let unrelated =
            lower_promql("sum(rate(other_metric[5m]))", AccuracyTarget::Epsilon(0.01)).unwrap();
        let mut graph = dag_export::export(&unrelated);
        for node in &mut graph.nodes {
            node.hash = Some(explanation.node_hash);
        }
        let mut matched = vec![false; explanations.len()];
        annotate_with_explanations(&mut graph, &explanations, &mut matched);
        assert!(graph.nodes.iter().all(|n| n.notes.is_empty()));
    }

    /// The `--post-asap` code path, exercised directly (not through the CLI):
    /// a workload with one sketch-approximable quantile aggregate and one
    /// `avg` aggregate must produce at least one `TargetReplacement` with
    /// `after: TargetReplacementAfter::Summary(..)` (the bound sketch) and
    /// at least one with `after: TargetReplacementAfter::Rewrite(..)` (the
    /// `avg -> sum/count` rewrite) — see [`run_post_asap`]. Also checks that
    /// a non-empty `post_graph` comes back for every query, since that's the
    /// other `--post-asap` output `main` wires up.
    #[tokio::test]
    async fn post_asap_run_produces_both_summary_and_rewrite_replacements() {
        let cat = default_catalog();
        let sketch_query = lower_sql(
            "SELECT approx_percentile_cont(latency, 0.95) FROM metrics",
            &cat,
            AccuracyTarget::Epsilon(0.01),
        )
        .await
        .unwrap();
        let avg_query = lower_sql(
            "SELECT service, AVG(latency) FROM metrics GROUP BY service",
            &cat,
            AccuracyTarget::Epsilon(0.01),
        )
        .await
        .unwrap();

        let lowered_queries = vec![
            (
                "sketch".to_string(),
                "SELECT approx_percentile_cont(latency, 0.95) FROM metrics".to_string(),
                sketch_query,
            ),
            (
                "avg".to_string(),
                "SELECT service, AVG(latency) FROM metrics GROUP BY service".to_string(),
                avg_query,
            ),
        ];

        let results = run_post_asap(&lowered_queries);

        assert!(
            results
                .replacements
                .iter()
                .any(|(_, r)| matches!(r.after, TargetReplacementAfter::Summary(_))),
            "expected at least one Summary replacement (the sketch-bound quantile aggregate): {:?}",
            results
                .replacements
                .iter()
                .map(|(name, r)| (name.clone(), r.strategy.clone()))
                .collect::<Vec<_>>()
        );
        assert!(
            results
                .replacements
                .iter()
                .any(|(_, r)| matches!(r.after, TargetReplacementAfter::Rewrite(_))),
            "expected at least one Rewrite replacement (the avg -> sum/count rewrite): {:?}",
            results
                .replacements
                .iter()
                .map(|(name, r)| (name.clone(), r.strategy.clone()))
                .collect::<Vec<_>>()
        );
        // The avg rewrite's own strategy label must disambiguate correctly
        // (not fall through to "Rollup", the other LogicalRewrite shape).
        assert!(results
            .replacements
            .iter()
            .any(|(_, r)| r.strategy == "AvgToSumOverCountStrategy"));

        assert_eq!(results.post_graphs.len(), 2, "one post_graph per query");
        for (name, graph) in &results.post_graphs {
            assert!(
                !graph.nodes.is_empty(),
                "post_graph for {name:?} must not be empty"
            );
        }
        let avg_post = &results
            .post_graphs
            .iter()
            .find(|(name, _)| name == "avg")
            .expect("avg post graph")
            .1;
        assert_eq!(
            avg_post
                .nodes
                .iter()
                .filter(|node| node.kind == "Scan")
                .count(),
            1,
            "AVG's SUM and COUNT branches must retain their shared input as one DAG node"
        );
        let decisions: Vec<_> = results
            .post_graphs
            .iter()
            .flat_map(|(_, graph)| graph.nodes.iter().filter_map(|node| node.decision.as_ref()))
            .collect();
        assert!(
            !decisions.is_empty(),
            "post_graph nodes must carry explicit strategy metadata"
        );
        for decision in decisions {
            assert!(!decision.strategy.is_empty());
            assert!(!decision.rationale.is_empty());
        }
    }

    #[test]
    fn workload_node_ids_make_smaller_topk_reuse_explicit() {
        let small = lower_promql(
            "topk(5, rate(http_requests_total[5m]))",
            AccuracyTarget::Epsilon(0.01),
        )
        .unwrap();
        let large = lower_promql(
            "topk(10, rate(http_requests_total[5m]))",
            AccuracyTarget::Epsilon(0.01),
        )
        .unwrap();
        let lowered = vec![
            (
                "q3".into(),
                "topk(5, rate(http_requests_total[5m]))".into(),
                small,
            ),
            (
                "q4".into(),
                "topk(10, rate(http_requests_total[5m]))".into(),
                large,
            ),
        ];
        let mut results = run_post_asap(&lowered);
        let mut graph_refs: Vec<_> = results
            .post_graphs
            .iter_mut()
            .map(|(_, graph)| graph)
            .collect();
        assign_workload_node_ids(&mut graph_refs);

        let q3 = &results
            .post_graphs
            .iter()
            .find(|(name, _)| name == "q3")
            .unwrap()
            .1;
        let q4 = &results
            .post_graphs
            .iter()
            .find(|(name, _)| name == "q4")
            .unwrap()
            .1;
        let q3_root = &q3.nodes[q3.root as usize];
        assert_eq!(q3_root.label, "Limit(5)");
        assert_eq!(
            q3_root
                .decision
                .as_ref()
                .map(|decision| decision.strategy.as_str()),
            Some("TopKLimitReuseStrategy")
        );
        let q3_large = &q3.nodes[q3_root.children[0] as usize];
        let q4_large = &q4.nodes[q4.root as usize];
        assert_eq!(q3_large.label, "Limit(10)");
        assert_eq!(q3_large.workload_node_id, q4_large.workload_node_id);
    }

    #[tokio::test]
    async fn join_demo_explicitly_shares_metrics_scan_with_grouped_query() {
        let cat = default_catalog();
        let q1 = lower_sql(
            "SELECT service, COUNT(*) FROM metrics GROUP BY service",
            &cat,
            AccuracyTarget::Exact,
        )
        .await
        .unwrap();
        let q6 = lower_sql(
            "SELECT metrics.service, COUNT(*) FROM metrics JOIN hosts ON metrics.service = hosts.service GROUP BY metrics.service",
            &cat,
            AccuracyTarget::Exact,
        )
        .await
        .unwrap();
        let mut q1_graph = dag_export::export(&q1);
        let mut q6_graph = dag_export::export(&q6);
        assign_workload_node_ids(&mut [&mut q1_graph, &mut q6_graph]);
        let q1_scan = q1_graph
            .nodes
            .iter()
            .find(|node| node.label == "Scan(metrics)")
            .unwrap();
        let q6_scan = q6_graph
            .nodes
            .iter()
            .find(|node| node.label == "Scan(metrics)")
            .unwrap();
        assert_eq!(q1_scan.workload_node_id, q6_scan.workload_node_id);
        assert!(q6_graph.nodes.iter().any(|node| node.kind == "Join"));
    }

    /// Regression test for a real stack overflow found via manual testing
    /// against real corpus queries (a `STDDEV_POP` aggregate, which — like
    /// `AVG` — dispatches to `Implementation::PassThrough` with no
    /// alternative strategy of its own, so its *only* candidate is
    /// `keep_pre_asap`'s conservative fallback: `Replacement::Summary`
    /// wrapping the *entire target* as `SummaryExpr::KeepPreAsap`).
    /// `run_post_asap` must not treat that as a real winner: splicing it
    /// into `export_post_asap` would recurse forever, since `find_winner`
    /// re-checks every node inside a spliced `KeepPreAsap` payload by
    /// design, and this payload structurally *is* the enclosing target — a
    /// fresh `find_winner` call finds the identical winner again,
    /// unconditionally, every time. Filtering this shape out of `winners`
    /// (same "no-op candidate" concept `explanation.rs`'s own
    /// `sketch_finding_reason` already excludes from being a finding) is
    /// what keeps this terminating: this test's only assertion that matters
    /// is that `run_post_asap` returns at all instead of overflowing the
    /// stack.
    #[tokio::test]
    async fn post_asap_does_not_recurse_forever_on_a_trivial_keep_pre_asap_winner() {
        let cat = default_catalog();
        let stddev_query = lower_sql(
            "SELECT STDDEV_POP(latency) FROM metrics",
            &cat,
            AccuracyTarget::Epsilon(0.01),
        )
        .await
        .unwrap();
        let lowered_queries = vec![(
            "stddev".to_string(),
            "SELECT STDDEV_POP(latency) FROM metrics".to_string(),
            stddev_query,
        )];

        let results = run_post_asap(&lowered_queries);

        // A trivial keep_pre_asap winner must be filtered before it ever
        // becomes a flat `TargetReplacement` — there's no real replacement
        // to report for a target with no alternative at all.
        assert!(
            results.replacements.is_empty(),
            "a target whose only candidate is the trivial keep_pre_asap fallback \
             shouldn't produce a flat replacement entry: {:?}",
            results
                .replacements
                .iter()
                .map(|(name, r)| (name.clone(), r.strategy.clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(results.post_graphs.len(), 1);
        assert!(!results.post_graphs[0].1.nodes.is_empty());
    }
}
