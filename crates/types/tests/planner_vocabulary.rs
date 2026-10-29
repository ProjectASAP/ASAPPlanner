use asap_types::ir::physical_export::WindowEdgeCompatibility;

// Renamed pane APIs still read and emit the deployed wire contract.
#[test]
fn window_edge_names_preserve_wire_values() {
    let compatibility = WindowEdgeCompatibility::RequiresAlignedPanePhaseOrExactWindowEdgeResidual;
    let wire = serde_json::json!("RequiresAlignedPanePhaseOrExactBoundaryResidual");
    assert_eq!(serde_json::to_value(compatibility).unwrap(), wire);
    assert_eq!(
        serde_json::from_value::<WindowEdgeCompatibility>(wire).unwrap(),
        compatibility
    );
}
