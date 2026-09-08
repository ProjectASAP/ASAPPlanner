//! Physical transfer and materialization dimensions, independent of planner policy.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BoundaryKind {
    Network {
        source_location: String,
        destination_location: String,
    },
    Materialization {
        medium: MaterializationMedium,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterializationMedium {
    Memory,
    Disk,
    ObjectStore,
}

/// Byte work at explicitly declared physical actions, not storage occupancy.
/// Network traffic and materialization writes remain separate dimensions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryResources {
    pub network_bytes: u64,
    pub materialization_bytes: u64,
}

impl BoundaryResources {
    pub fn terms(self) -> [(&'static str, u64); 2] {
        [
            ("network_bytes", self.network_bytes),
            ("materialization_bytes", self.materialization_bytes),
        ]
    }

    /// Add independent dimensions without wrapping or partially mutating either input.
    pub fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            network_bytes: self.network_bytes.checked_add(other.network_bytes)?,
            materialization_bytes: self
                .materialization_bytes
                .checked_add(other.materialization_bytes)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared vocabulary retains the original flat counters and tagged kind JSON.
    #[test]
    fn boundary_json_format_is_unchanged() {
        let bytes = BoundaryResources {
            network_bytes: 480,
            materialization_bytes: 40,
        };
        let json = serde_json::json!({"network_bytes": 480, "materialization_bytes": 40});
        assert_eq!(serde_json::to_value(bytes).unwrap(), json);
        assert_eq!(
            serde_json::from_value::<BoundaryResources>(json).unwrap(),
            bytes
        );
        let network = BoundaryKind::Network {
            source_location: "edge".into(),
            destination_location: "backend".into(),
        };
        assert_eq!(
            serde_json::to_value(network).unwrap(),
            serde_json::json!({
                "kind": "network", "source_location": "edge", "destination_location": "backend"
            })
        );
        for (medium, name) in [
            (MaterializationMedium::Memory, "memory"),
            (MaterializationMedium::Disk, "disk"),
            (MaterializationMedium::ObjectStore, "object_store"),
        ] {
            let kind = BoundaryKind::Materialization { medium };
            let json = serde_json::json!({"kind": "materialization", "medium": name});
            assert_eq!(serde_json::to_value(&kind).unwrap(), json);
            assert_eq!(serde_json::from_value::<BoundaryKind>(json).unwrap(), kind);
        }
    }

    /// Either dimension overflowing returns None without changing the original counters.
    #[test]
    fn checked_add_preserves_dimensions_and_rejects_overflow() {
        let first = BoundaryResources {
            network_bytes: 2,
            materialization_bytes: 3,
        };
        let second = BoundaryResources {
            network_bytes: 5,
            materialization_bytes: 7,
        };
        assert_eq!(
            first.checked_add(second),
            Some(BoundaryResources {
                network_bytes: 7,
                materialization_bytes: 10,
            })
        );
        for overflowing in [
            BoundaryResources {
                network_bytes: u64::MAX,
                materialization_bytes: 0,
            },
            BoundaryResources {
                network_bytes: 0,
                materialization_bytes: u64::MAX,
            },
        ] {
            assert!(first.checked_add(overflowing).is_none());
        }
        assert_eq!(
            first.terms(),
            [("network_bytes", 2), ("materialization_bytes", 3)]
        );
    }
}
