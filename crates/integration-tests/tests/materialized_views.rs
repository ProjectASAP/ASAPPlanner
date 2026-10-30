//! What a runtime derives from an installable plan's DAG alone: its
//! materialized views and its raw-data retention (issue #970).

use std::collections::HashSet;

use asap_logical_asap_optimization::DeploymentCapabilities;
use asap_planner::{
    e2e_plan, installable_plan_of, plan_installable, FrontendInput, InstallablePlan,
    InstallablePlanError, MaterializedViewRefresh, PlanningModels, RawDataRetention, UserInput,
    INSTALLABLE_PLAN_WIRE_VERSION,
};
use asap_types::ir::operator::Source;
use asap_types::ir::physical_export::{compile_physical_asap_workload, PhysicalASAPDAG};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator};
use asap_types::physical::Materialization;
use asap_types::workload::PlanningWorkload;
use serde_json::Value;

/// A repeating PromQL workload over ingesting `m`, each query every minute.
fn repeating(queries: &[&str]) -> PlanningWorkload {
    repeating_with(queries, "")
}

/// [`repeating`], each entry with `extra` fields (e.g. a range evaluation).
fn repeating_with(queries: &[&str], extra: &str) -> PlanningWorkload {
    let entries: Vec<String> = queries
        .iter()
        .map(|q| {
            format!(
                r#"{{"query": {q:?}, "demand": {{"fixed_interval": 60000}},
                "requirements": {{"accuracy": {{"max_error": 0.01, "failure_probability": 0.01}}}},
                "predictability": {{"predictable": {{"known_at": null}}}}{extra}}}"#
            )
        })
        .collect();
    serde_json::from_str(&format!(
        r#"{{"query_workload": {{"language": "promql", "repeating_queries": [{}]}},
            "data_workload": {{"arrival": {{"ingesting": {{"rows_per_second": 1000.0}}}},
              "sources": {{"m": {{"series_count": 1000, "sample_interval": 1000}}}}}}}}"#,
        entries.join(",")
    ))
    .unwrap()
}

/// Stage 3's priced raw lookback of `dag` (the selected candidate's
/// physical DAG, pane chains unexpanded), per source: each query-time
/// scan's `PhysicalASAPDAG::scan_window` with every chain kept across
/// evaluations reading its newest pane, as `asap-plan-selection` prices
/// it, the longest per source.
fn stage3_raw_lookback_ms(dag: &PhysicalASAPDAG) -> Vec<(Source, Option<u64>)> {
    let kept_chains: HashSet<_> = dag
        .nodes
        .iter()
        .filter(|node| {
            matches!(
                (&node.payload, node.materialization),
                (
                    Operator::ASAP(ASAPOp::PaneChain { .. }),
                    Materialization::MatAtQueryKeptAcrossEvaluations(_)
                )
            )
        })
        .map(|node| node.id)
        .collect();
    let mut lookback: Vec<(Source, Option<u64>)> = Vec::new();
    for node in &dag.nodes {
        let Operator::NonASAP(NonASAPOp::Scan { source, .. }) = &node.payload else {
            continue;
        };
        if node.output_state.timing != ExecutionTiming::QueryTime {
            continue;
        }
        let scan = dag.scan_window(node.id, &kept_chains).lookback_ms();
        match lookback.iter_mut().find(|(kept, _)| kept == source) {
            Some((_, kept)) => *kept = kept.zip(scan).map(|(a, b)| a.max(b)),
            None => lookback.push((source.clone(), scan)),
        }
    }
    lookback
}

/// `rate(m[5m])` and `rate(m[5m] offset 1h)`, both at query time: the
/// runtime keeps 65 minutes of `m`, the raw lookback Stage 3 prices for the
/// same plan. With no raw data kept, both run from views refreshed at
/// ingestion and no source is read at query time.
#[tokio::test(flavor = "multi_thread")]
async fn raw_data_retention_is_the_lookback_stage3_prices() {
    let workload = repeating(&["rate(m[5m])", "rate(m[5m] offset 1h)"]);
    let input = || {
        UserInput::new(
            &workload,
            FrontendInput::Promql { histograms: None },
            PlanningModels::builtin(),
        )
    };
    let (plan, _) = plan_installable(input()).await.unwrap();
    let m = Source::TimeSeries { metric: "m".into() };
    assert_eq!(
        plan.raw_data_retention().unwrap(),
        [RawDataRetention::new(m.clone(), Some(65 * 60_000))]
    );
    let output = e2e_plan(input()).await.unwrap();
    let selected = compile_physical_asap_workload(&output.selection.unwrap().plan).unwrap();
    assert_eq!(stage3_raw_lookback_ms(&selected), [(m, Some(65 * 60_000))]);

    let capabilities = DeploymentCapabilities {
        raw_data_retained: false,
        ..DeploymentCapabilities::default()
    };
    let (plan, _) = plan_installable(UserInput::new(
        &workload,
        FrontendInput::Promql { histograms: None },
        PlanningModels::builtin().with_deployment_capabilities(&capabilities),
    ))
    .await
    .unwrap();
    assert!(!plan.materialized_views().unwrap().is_empty());
    assert_eq!(plan.raw_data_retention().unwrap(), []);
}

/// A range-evaluated query (`max_over_time(m[5m])` over the last hour at
/// 1-minute steps) installed with every node at query time (its frontend
/// DAG, nothing materialized): its oldest step reads `m` 5 minutes before an
/// hour ago, so the runtime keeps 65 minutes of `m`, the scan's window plus
/// the range span, on the scan's node. (Stage 3 prices one evaluation's
/// window, 5 minutes.)
#[test]
fn a_range_evaluated_query_time_scan_keeps_its_window_plus_the_span() {
    let workload = repeating_with(
        &["max_over_time(m[5m])"],
        r#", "range_evaluation": {"step_ms": 60000, "span_ms": 3600000}"#,
    );
    let roots = asap_devtools::planning_stages::lower_promql_roots(&workload).unwrap();
    let entries: Vec<_> = workload.query_workload.entries().collect();
    let (plan, _) = installable_plan_of(&roots, None, &entries).unwrap();
    plan.validate().unwrap();
    assert!(plan.materialized_views().unwrap().is_empty());
    assert_eq!(
        plan.raw_data_retention().unwrap(),
        [RawDataRetention::new(
            Source::TimeSeries { metric: "m".into() },
            Some(5 * 60_000 + 3_600_000)
        )]
    );
    let scan = plan
        .dag
        .nodes
        .iter()
        .find(|node| matches!(node.payload, Operator::NonASAP(NonASAPOp::Scan { .. })))
        .unwrap();
    assert_eq!(scan.retention_ms, Some(3_900_000));
}

/// A plan file of wire version 10 (assembled from the parts of a plan
/// written on `main` before #970: the versioned DAG envelope, `queries`,
/// `stored_outputs`, `query_time_raw_windows`, `selection`) is refused for
/// its version, not for a missing field; a file of this version reads back.
#[test]
fn a_plan_file_of_an_older_wire_version_is_refused_by_version() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/materialized_view_fixtures/plan_wire_version_10.json");
    let old = std::fs::read_to_string(path).unwrap();
    assert!(matches!(
        InstallablePlan::from_json(&old),
        Err(InstallablePlanError::UnsupportedWireVersion(None))
    ));
    let mut other: Value = serde_json::from_str(&old).unwrap();
    other["wire_version"] = serde_json::json!(INSTALLABLE_PLAN_WIRE_VERSION + 1);
    assert!(matches!(
        InstallablePlan::from_json(&other.to_string()),
        Err(InstallablePlanError::UnsupportedWireVersion(Some(v))) if v == INSTALLABLE_PLAN_WIRE_VERSION + 1
    ));
    let current = InstallablePlan {
        wire_version: INSTALLABLE_PLAN_WIRE_VERSION,
        dag: serde_json::from_value(
            serde_json::from_str::<Value>(&old).unwrap()["dag"]["dag"].clone(),
        )
        .unwrap(),
        query_texts: vec!["a".into(), "b".into()],
    };
    let read = InstallablePlan::from_json(&serde_json::to_string(&current).unwrap()).unwrap();
    assert_eq!(read, current);
}

/// For installable plans captured from `main` before #970 (each the plan
/// file's DAG and its `stored_outputs` field), `materialized_views()` of
/// that DAG, with each view's `retention_ms` on its node, equals
/// `stored_outputs` field for field after the renames: `built_at` →
/// `refresh`, `inputs.stored_outputs` → `inputs.materialized_views`, and
/// each raw-sample window `(time_range_node_id, width_ms, end_offset_ms,
/// scan_node_id)` → `(source of the Scan, end_offset_ms, width_ms)`. The
/// cases cover views refreshed at ingestion and on first read, offset
/// windows, and views read by other views.
#[test]
fn materialized_views_equal_the_stored_outputs_main_listed() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/materialized_view_fixtures/stored_outputs_on_main.json");
    let cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert!(!cases.is_empty());
    let mut refreshes = HashSet::new();
    let mut read_by_views = 0;
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let mut dag: PhysicalASAPDAG = serde_json::from_value(case["dag"].clone()).unwrap();
        let stored = case["stored_outputs"].as_array().unwrap();
        for output in stored {
            let id = output["node_id"].as_u64().unwrap() as usize;
            let node = dag.nodes.iter_mut().find(|node| node.id == id).unwrap();
            node.retention_ms = Some(output["retention_ms"].as_u64().unwrap());
        }
        let views = dag.materialized_views().unwrap();
        assert_eq!(views.len(), stored.len(), "{name}");
        let source_of = |scan: u64| match &dag
            .nodes
            .iter()
            .find(|node| node.id == scan as usize)
            .unwrap()
            .payload
        {
            Operator::NonASAP(NonASAPOp::Scan { source, .. }) => source.clone(),
            other => panic!("{name}: node {scan} is not a Scan: {other:?}"),
        };
        for (view, output) in views.iter().zip(stored) {
            let refresh = match output["built_at"].as_str().unwrap() {
                "ingestion_run" => MaterializedViewRefresh::AtIngestion,
                "query_run" => MaterializedViewRefresh::OnFirstRead,
                other => panic!("{name}: {other}"),
            };
            refreshes.insert(format!("{refresh:?}"));
            assert_eq!(
                view.node_id as u64,
                output["node_id"].as_u64().unwrap(),
                "{name}"
            );
            assert_eq!(view.refresh(), refresh, "{name}");
            assert_eq!(
                serde_json::to_value(view.coverage()).unwrap(),
                output["coverage"],
                "{name}"
            );
            assert_eq!(
                view.retention_ms(),
                output["retention_ms"].as_u64().unwrap(),
                "{name}"
            );
            let views_read: Vec<u64> = output["inputs"]["stored_outputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|id| id.as_u64().unwrap())
                .collect();
            read_by_views += usize::from(!views_read.is_empty());
            assert_eq!(
                view.inputs
                    .materialized_views
                    .iter()
                    .map(|&id| id as u64)
                    .collect::<Vec<_>>(),
                views_read,
                "{name}"
            );
            let windows: Vec<(Source, i64, i64)> = output["inputs"]["raw_sample_windows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|window| {
                    (
                        source_of(window["scan_node_id"].as_u64().unwrap()),
                        window["end_offset_ms"].as_i64().unwrap(),
                        window["width_ms"].as_i64().unwrap(),
                    )
                })
                .collect();
            assert_eq!(
                view.inputs
                    .raw_data_windows()
                    .iter()
                    .map(|w| (w.source().clone(), w.end_offset_ms(), w.width_ms()))
                    .collect::<Vec<_>>(),
                windows,
                "{name}"
            );
        }
    }
    assert_eq!(refreshes.len(), 2, "both refresh modes are covered");
    assert!(read_by_views > 0, "a view read by another view is covered");
}
