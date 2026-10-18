//! A deployment passes its Stage 3 cost-model settings as JSON (issue #749).

use asap_plan_selection::{
    ClusterLimits, ClusterObjective, CostModel, PlanSelectionSettings, ResourceSpeed,
    ResourceUnitCost,
};

/// Cost model 2 with the deployment's own prices and speed; the fields left
/// out keep their defaults.
#[test]
fn settings_load_from_json_with_defaults_for_omitted_fields() {
    let settings: PlanSelectionSettings = serde_json::from_str(
        r#"{
          "cost_model": {"provisioned_capacity": {
            "cost_per_cpu_op": 2e-6,
            "cost_per_scanned_byte": 1e-7,
            "cost_per_memory_byte_second": 1.25e-7,
            "cost_per_provisioned_memory_byte_second": 3e-7,
            "cost_per_provisioned_cpu_core_second": 900.0,
            "cost_per_disk_byte_second": 1e-9
          }},
          "resource_speed": {"ms_per_cpu_op": 2e-6, "ms_per_scanned_byte": 1e-7}
        }"#,
    )
    .unwrap();
    assert_eq!(
        settings,
        PlanSelectionSettings {
            cost_model: CostModel::ProvisionedCapacity(ResourceUnitCost {
                cost_per_cpu_op: 2e-6,
                cost_per_scanned_byte: 1e-7,
                cost_per_memory_byte_second: 1.25e-7,
                cost_per_provisioned_memory_byte_second: 3e-7,
                cost_per_provisioned_cpu_core_second: 900.0,
                cost_per_disk_byte_second: 1e-9,
            }),
            resource_speed: ResourceSpeed {
                ms_per_cpu_op: 2e-6,
                ms_per_scanned_byte: 1e-7,
            },
            ..PlanSelectionSettings::default()
        }
    );
    settings.validate().unwrap();
    let empty: PlanSelectionSettings = serde_json::from_str("{}").unwrap();
    assert_eq!(empty, PlanSelectionSettings::default());
}

/// Every cost model round-trips, and a misspelled field is refused rather
/// than silently defaulted.
#[test]
fn settings_round_trip_and_refuse_unknown_fields() {
    let cluster = PlanSelectionSettings {
        cost_model: CostModel::ClusterCapacity(ClusterLimits {
            memory_bytes: 64 << 30,
            disk_bytes: 1 << 40,
            cpu_cores: None,
            objective: ClusterObjective::ParetoFront,
        }),
        ..PlanSelectionSettings::default()
    };
    for settings in [PlanSelectionSettings::default(), cluster] {
        let json = serde_json::to_string(&settings).unwrap();
        let back: PlanSelectionSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, settings);
    }
    assert!(serde_json::from_str::<PlanSelectionSettings>(r#"{"costmodel": {}}"#).is_err());
    assert!(serde_json::from_str::<PlanSelectionSettings>(
        r#"{"resource_speed": {"ms_per_cpu_op": 1e-6, "ms_per_scanned_byte": 1e-7, "x": 1}}"#
    )
    .is_err());
}
