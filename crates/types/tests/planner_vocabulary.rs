use asap_types::ir::physical_export::WindowEdgeCompatibility;
use asap_types::physical::{validate_pane_coverage, PaneLayout, WindowEdgeCoverage};

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
