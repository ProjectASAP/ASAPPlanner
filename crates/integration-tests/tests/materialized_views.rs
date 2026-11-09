//! An installable plan file of an older wire version is refused by its
//! version (issue #970).

use asap_planner::{InstallablePlan, InstallablePlanError, INSTALLABLE_PLAN_WIRE_VERSION};

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
