//! Measured resource payloads and compatibility with the v1 benchmark wire format.
//!
//! Physical dimensions live in `asap_types::resources`; flat wire structs below
//! exist only to keep archived artifacts readable and preserve their field names.

use asap_types::resources::PhysicalResources;
use serde::{Deserialize, Serialize};

pub use asap_types::resources::{MeasuredCpu, MeasuredResources, Measurement};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "SketchWire", into = "SketchWire")]
pub struct ResourceMeasurements {
    pub resources: MeasuredResources,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "ExactWire", into = "ExactWire")]
pub struct ExactResourceMeasurements {
    pub resources: MeasuredResources,
}

// Keep the archived flat v1 schema at the serialization boundary only. New
// optional dimensions are omitted when absent, so legacy snapshots round-trip.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SketchWire {
    build_cpu_ns: Option<Measurement>,
    update_cpu_ns: Option<Measurement>,
    merge_cpu_ns: Option<Measurement>,
    read_cpu_ns: Option<Measurement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prepare_cpu_ns: Option<Measurement>,
    retained_bytes: Option<Measurement>,
    peak_bytes: Option<Measurement>,
    serialized_bytes: Option<Measurement>,
    disk_bytes: Option<Measurement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scan_bytes: Option<Measurement>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactWire {
    empty_build_cpu_ns: Option<Measurement>,
    update_cpu_ns: Option<Measurement>,
    prepare_cpu_ns: Option<Measurement>,
    read_cpu_ns: Option<Measurement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    merge_cpu_ns: Option<Measurement>,
    retained_bytes: Option<Measurement>,
    peak_bytes: Option<Measurement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    serialized_bytes: Option<Measurement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disk_bytes: Option<Measurement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scan_bytes: Option<Measurement>,
}

impl From<SketchWire> for ResourceMeasurements {
    fn from(w: SketchWire) -> Self {
        Self {
            resources: PhysicalResources {
                cpu: MeasuredCpu {
                    build_cpu_ns: w.build_cpu_ns,
                    update_cpu_ns: w.update_cpu_ns,
                    merge_cpu_ns: w.merge_cpu_ns,
                    prepare_cpu_ns: w.prepare_cpu_ns,
                    read_cpu_ns: w.read_cpu_ns,
                },
                retained_memory_bytes: w.retained_bytes,
                peak_memory_bytes: w.peak_bytes,
                serialized_bytes: w.serialized_bytes,
                disk_bytes: w.disk_bytes,
                scan_bytes: w.scan_bytes,
            },
        }
    }
}

impl From<ResourceMeasurements> for SketchWire {
    fn from(value: ResourceMeasurements) -> Self {
        let r = value.resources;
        Self {
            build_cpu_ns: r.cpu.build_cpu_ns,
            update_cpu_ns: r.cpu.update_cpu_ns,
            merge_cpu_ns: r.cpu.merge_cpu_ns,
            prepare_cpu_ns: r.cpu.prepare_cpu_ns,
            read_cpu_ns: r.cpu.read_cpu_ns,
            retained_bytes: r.retained_memory_bytes,
            peak_bytes: r.peak_memory_bytes,
            serialized_bytes: r.serialized_bytes,
            disk_bytes: r.disk_bytes,
            scan_bytes: r.scan_bytes,
        }
    }
}

impl From<ExactWire> for ExactResourceMeasurements {
    fn from(w: ExactWire) -> Self {
        Self {
            resources: PhysicalResources {
                cpu: MeasuredCpu {
                    build_cpu_ns: w.empty_build_cpu_ns,
                    update_cpu_ns: w.update_cpu_ns,
                    merge_cpu_ns: w.merge_cpu_ns,
                    prepare_cpu_ns: w.prepare_cpu_ns,
                    read_cpu_ns: w.read_cpu_ns,
                },
                retained_memory_bytes: w.retained_bytes,
                peak_memory_bytes: w.peak_bytes,
                serialized_bytes: w.serialized_bytes,
                disk_bytes: w.disk_bytes,
                scan_bytes: w.scan_bytes,
            },
        }
    }
}

impl From<ExactResourceMeasurements> for ExactWire {
    fn from(value: ExactResourceMeasurements) -> Self {
        let r = value.resources;
        Self {
            empty_build_cpu_ns: r.cpu.build_cpu_ns,
            update_cpu_ns: r.cpu.update_cpu_ns,
            merge_cpu_ns: r.cpu.merge_cpu_ns,
            prepare_cpu_ns: r.cpu.prepare_cpu_ns,
            read_cpu_ns: r.cpu.read_cpu_ns,
            retained_bytes: r.retained_memory_bytes,
            peak_bytes: r.peak_memory_bytes,
            serialized_bytes: r.serialized_bytes,
            disk_bytes: r.disk_bytes,
            scan_bytes: r.scan_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measured(value: f64) -> Option<Measurement> {
        Some(Measurement {
            value,
            stddev: Some(0.5),
            samples: 5,
            method: Some("test only".into()),
        })
    }

    /// The same resource dimensions retain uncertainty through either wire adapter.
    #[test]
    fn shared_resources_round_trip_without_losing_dimensions() {
        let resources = PhysicalResources {
            cpu: MeasuredCpu {
                build_cpu_ns: measured(1.0),
                update_cpu_ns: measured(2.0),
                merge_cpu_ns: measured(3.0),
                prepare_cpu_ns: measured(4.0),
                read_cpu_ns: measured(5.0),
            },
            peak_memory_bytes: measured(100.0),
            retained_memory_bytes: measured(70.0),
            scan_bytes: measured(200.0),
            serialized_bytes: measured(40.0),
            disk_bytes: measured(4096.0),
        };
        let sketch = ResourceMeasurements {
            resources: resources.clone(),
        };
        let exact = ExactResourceMeasurements { resources };
        assert_eq!(
            serde_json::from_value::<ResourceMeasurements>(serde_json::to_value(&sketch).unwrap())
                .unwrap(),
            sketch
        );
        assert_eq!(
            serde_json::from_value::<ExactResourceMeasurements>(
                serde_json::to_value(&exact).unwrap()
            )
            .unwrap(),
            exact
        );
    }

    /// Archived names remain accepted, but physical fields use canonical names.
    #[test]
    fn legacy_fields_map_to_shared_resources() {
        let value = serde_json::json!({"empty_build_cpu_ns": measured(3.0), "peak_bytes": measured(100.0), "retained_bytes": measured(70.0)});
        let exact: ExactResourceMeasurements = serde_json::from_value(value).unwrap();
        assert_eq!(exact.resources.cpu.build_cpu_ns, measured(3.0));
        assert_eq!(exact.resources.peak_memory_bytes, measured(100.0));
        assert_eq!(exact.resources.retained_memory_bytes, measured(70.0));
        assert!(exact.resources.scan_bytes.is_none());
        assert!(exact.resources.disk_bytes.is_none());
        assert!(
            serde_json::from_value::<ResourceMeasurements>(serde_json::json!({"cpu_ops": 42}))
                .is_err()
        );
    }
}
