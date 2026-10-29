use asap_types::ir::physical_export::WindowEdgeCompatibility;

// The renamed window-edge compatibility still reads and emits the deployed
// wire value. (Pane coverage names are checked in physical-asap-optimization.)
#[test]
fn window_edge_compatibility_names_preserve_wire_values() {
    let compatibility = WindowEdgeCompatibility::RequiresAlignedPanePhaseOrExactWindowEdgeResidual;
    let wire = serde_json::json!("RequiresAlignedPanePhaseOrExactBoundaryResidual");
    assert_eq!(serde_json::to_value(compatibility).unwrap(), wire);
    assert_eq!(
        serde_json::from_value::<WindowEdgeCompatibility>(wire).unwrap(),
        compatibility
    );
}
