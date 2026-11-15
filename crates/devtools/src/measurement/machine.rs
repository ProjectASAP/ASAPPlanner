//! The machine a calibration is measured on, and the key its tables are
//! stored under: CPU model, logical cores and memory, never the hostname.
//!
//! Example: an "Intel(R) Xeon(R) Gold 6142 CPU @ 2.60GHz" with 56 logical
//! cores and 263,724,000 KiB of memory is `xeon-gold-6142-56c-251g`.

use serde::{Deserialize, Serialize};

/// What `machine.json` records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineDescription {
    pub cpu_model: String,
    pub logical_cores: u32,
    /// `MemTotal`, in KiB.
    pub memory_kib: u64,
    pub os: String,
}

impl MachineDescription {
    /// The machine `/proc/cpuinfo` and `/proc/meminfo` describe.
    pub fn from_proc(cpuinfo: &str, meminfo: &str, os: &str) -> Result<Self, String> {
        let _ = (cpuinfo, meminfo, os);
        todo!("asap_run: implement after test approval")
    }

    /// This machine.
    pub fn detect() -> Result<Self, String> {
        todo!("asap_run: implement after test approval")
    }

    /// The directory name its tables are stored under: `[a-z0-9-]+`.
    pub fn machine_key(&self) -> String {
        todo!("asap_run: implement after test approval")
    }
}
