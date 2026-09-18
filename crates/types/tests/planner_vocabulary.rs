use asap_types::post_asap::{
    validate_pane_coverage, PaneLayout, WindowEdgeCompatibility, WindowEdgeCoverage,
};
use asap_types::pre_asap::{SchemaResolver, Source, UnresolvedQueryExpr};
use asap_types::resources::{PhysicalHandoffBytes, PhysicalHandoffKind};

// Renamed pane APIs still read and emit the deployed wire contract.
#[test]
fn window_edge_names_preserve_wire_values() {
    let layout: PaneLayout = serde_json::from_value(serde_json::json!({
        "pane_width_ms": 1000, "pane_origin_ms": 0
    }))
    .unwrap();
    let json = serde_json::json!({
        "kind": "exact_boundary_residual", "executor": "runtime", "source": "raw"
    });
    let coverage: WindowEdgeCoverage = serde_json::from_value(json.clone()).unwrap();
    assert!(matches!(
        coverage,
        WindowEdgeCoverage::ExactWindowEdgeResidual { .. }
    ));
    assert_eq!(serde_json::to_value(&coverage).unwrap(), json);
    assert!(validate_pane_coverage(&layout, Some(1500), &coverage).is_ok());
    let compatibility = WindowEdgeCompatibility::RequiresAlignedPanePhaseOrExactWindowEdgeResidual;
    let wire = serde_json::json!("RequiresAlignedPanePhaseOrExactBoundaryResidual");
    assert_eq!(serde_json::to_value(compatibility).unwrap(), wire);
    assert_eq!(
        serde_json::from_value::<WindowEdgeCompatibility>(wire).unwrap(),
        compatibility
    );
}

// External consumers can use the new resolver and resource names without changing behavior.
#[test]
fn renamed_schema_and_handoff_apis_are_public() {
    let tree = UnresolvedQueryExpr::Scan {
        source: Source::TimeSeries {
            metric: "requests".into(),
        },
        predicates: vec![],
        schema: None,
    };
    let schema = SchemaResolver::new().resolve_schema(&tree);
    assert!(schema.column_id("value").is_some());
    let bytes = PhysicalHandoffBytes {
        network_bytes: 12,
        materialization_bytes: 4,
    };
    assert_eq!(
        serde_json::to_value(bytes).unwrap(),
        serde_json::json!({
            "network_bytes": 12, "materialization_bytes": 4
        })
    );
    let kind: PhysicalHandoffKind = serde_json::from_value(serde_json::json!({
        "kind": "network", "source_location": "edge", "destination_location": "backend"
    }))
    .unwrap();
    assert!(matches!(kind, PhysicalHandoffKind::Network { .. }));
}
