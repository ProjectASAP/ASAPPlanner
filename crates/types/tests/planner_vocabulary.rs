use asap_types::plan::physical_asap_dag::WindowEdgeCompatibility;

// The renamed window-edge compatibility still reads and emits the deployed
// wire value. (Tumbling-window coverage names are checked in asap-stage2-physical-optimization.)
#[test]
fn window_edge_compatibility_names_preserve_wire_values() {
    let compatibility =
        WindowEdgeCompatibility::RequiresAlignedTumblingWindowPhaseOrExactWindowEdgeResidual;
    let wire = serde_json::json!("RequiresAlignedPanePhaseOrExactBoundaryResidual");
    assert_eq!(serde_json::to_value(compatibility).unwrap(), wire);
    assert_eq!(
        serde_json::from_value::<WindowEdgeCompatibility>(wire).unwrap(),
        compatibility
    );
}
