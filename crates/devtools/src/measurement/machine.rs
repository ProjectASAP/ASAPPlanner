//! The machine a calibration is measured on, and the key its tables are
//! stored under: CPU architecture, CPU model, logical cores and memory
//! type. Never the hostname and never the memory size.
//!
//! Example: an "Intel(R) Xeon(R) CPU E5-2683 v3 @ 2.00GHz" on x86_64 with 56
//! logical cores and DDR4 registered DIMMs is
//! `x86_64-intel-xeon-e5-2683-v3-56c-registered-ddr4`.

use serde::{Deserialize, Serialize};

/// The memory type of a machine whose DIMM types cannot be read (no EDAC,
/// as in a VM) and that has no override.
pub const UNKNOWN_MEMORY_TYPE: &str = "unknown-memory";

/// What `machine.json` records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineDescription {
    /// `std::env::consts::ARCH`, for example `x86_64`.
    pub cpu_architecture: String,
    pub cpu_model: String,
    pub logical_cores: u32,
    /// The DIMM type, for example `Registered-DDR4`; DIMM types that differ
    /// are the sorted unique values joined by `-`; `UNKNOWN_MEMORY_TYPE`
    /// when unreadable and not overridden.
    pub memory_type: String,
    /// `MemTotal`, in KiB. Recorded for information; not part of the key.
    pub memory_kib: u64,
    pub os: String,
}

impl MachineDescription {
    /// The machine `/proc/cpuinfo`, `/proc/meminfo` and the EDAC DIMM types
    /// (`/sys/devices/system/edac/mc/mc*/dimm*/dimm_mem_type`, one string per
    /// DIMM, empty without EDAC) describe. `memory_type_override`
    /// (`--memory-type` / `ASAP_MEMORY_TYPE`) replaces the EDAC types.
    pub fn from_proc(
        cpu_architecture: &str,
        cpuinfo: &str,
        meminfo: &str,
        dimm_memory_types: &[&str],
        memory_type_override: Option<&str>,
        os: &str,
    ) -> Result<Self, String> {
        let _ = (
            cpu_architecture,
            cpuinfo,
            meminfo,
            dimm_memory_types,
            memory_type_override,
            os,
        );
        todo!("asap_run: implement after test approval")
    }

    /// This machine.
    pub fn detect(memory_type_override: Option<&str>) -> Result<Self, String> {
        let _ = memory_type_override;
        todo!("asap_run: implement after test approval")
    }

    /// The directory name its tables are stored under: `[a-z0-9-]+`, from
    /// architecture, model, cores and memory type.
    pub fn machine_key(&self) -> String {
        todo!("asap_run: implement after test approval")
    }

    /// A warning when the memory type is `UNKNOWN_MEMORY_TYPE`, naming it
    /// and the `--memory-type` / `ASAP_MEMORY_TYPE` override.
    pub fn warning(&self) -> Option<String> {
        todo!("asap_run: implement after test approval")
    }
}
