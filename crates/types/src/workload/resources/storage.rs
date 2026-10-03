//! Storage request-count dimensions, independent of estimation and calibration.

use serde::{Deserialize, Serialize};

/// Explicit counts of physical data requests, not transferred bytes or CPU work.
/// Unavailable storage evidence is represented by the enclosing optional profile,
/// not by constructing an all-zero count vector.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageResources {
    pub disk_reads: u64,
    pub disk_writes: u64,
    pub object_gets: u64,
    pub object_puts: u64,
}

impl StorageResources {
    pub fn terms(self) -> [(&'static str, u64); 4] {
        [
            ("disk_read_operations", self.disk_reads),
            ("disk_write_operations", self.disk_writes),
            ("object_get_operations", self.object_gets),
            ("object_put_operations", self.object_puts),
        ]
    }

    /// Compose exact counts without wrapping or partially updating an input.
    pub fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            disk_reads: self.disk_reads.checked_add(other.disk_reads)?,
            disk_writes: self.disk_writes.checked_add(other.disk_writes)?,
            object_gets: self.object_gets.checked_add(other.object_gets)?,
            object_puts: self.object_puts.checked_add(other.object_puts)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Moving the type preserves its four required integer wire fields exactly.
    #[test]
    fn storage_counts_keep_the_existing_wire_shape() {
        let wire = serde_json::json!({
            "disk_reads": 1, "disk_writes": 2, "object_gets": 3, "object_puts": 4,
        });
        let counts: StorageResources = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(counts).unwrap(), wire);
        assert_eq!(counts.terms()[2], ("object_get_operations", 3));
        for invalid in [
            serde_json::json!(null),
            serde_json::json!({}),
            serde_json::json!({
                "disk_reads": 1.5, "disk_writes": 2, "object_gets": 3, "object_puts": 4,
            }),
        ] {
            assert!(serde_json::from_value::<StorageResources>(invalid).is_err());
        }
    }

    /// Every independent dimension fails closed on integer addition overflow.
    #[test]
    fn storage_count_addition_is_checked_in_each_dimension() {
        let one = StorageResources {
            disk_reads: 1,
            disk_writes: 1,
            object_gets: 1,
            object_puts: 1,
        };
        let zero = StorageResources::default();
        assert_eq!(zero.checked_add(one), Some(one));
        for counts in [
            StorageResources {
                disk_reads: u64::MAX,
                ..zero
            },
            StorageResources {
                disk_writes: u64::MAX,
                ..zero
            },
            StorageResources {
                object_gets: u64::MAX,
                ..zero
            },
            StorageResources {
                object_puts: u64::MAX,
                ..zero
            },
        ] {
            assert_eq!(counts.checked_add(one), None);
            assert_eq!(counts.checked_add(zero), Some(counts));
        }
    }
}
