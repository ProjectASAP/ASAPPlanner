//! What a runtime derives from an installable plan's DAG alone: its
//! materialized views and its raw-data retention (issue #970).

use std::collections::HashSet;

use asap_planner::{
    InstallablePlan, InstallablePlanError, MaterializedViewRefresh, INSTALLABLE_PLAN_WIRE_VERSION,
};
use asap_types::ir::operator::Source;
use asap_types::ir::physical_export::PhysicalASAPDAG;

use asap_types::ir::{NonASAPOp, Operator};

use serde_json::Value;

/// A plan file of wire version 10 (assembled from the parts of a plan
/// written before #970: the versioned DAG envelope, `queries`,
/// `stored_outputs`, `query_time_raw_windows`, `selection`; its DAG nodes
/// carry today's node fields, e.g. `lifetime`) is refused for its version,
/// not for a missing field; its DAG with today's version reads back.
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
