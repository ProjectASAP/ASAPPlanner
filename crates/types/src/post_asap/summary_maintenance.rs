//! Physical construction mode for a materialized summary.
//!
//! A [`super::SummaryNode`] is a logical summary expression and deliberately
//! does not carry this choice: the same candidate may be built directly for
//! one workload or maintained incrementally for another. Physical planning
//! attaches the selected mode to its deployment guarantee.

/// How a physical summary deployment obtains its state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SummaryMaintenanceMode {
    /// Rebuild the summary from its complete input when the deployment needs
    /// a value. No update stream is required.
    DirectBuild,
    /// Create the state once and apply input changes as they arrive.
    Incremental,
}

impl SummaryMaintenanceMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DirectBuild => "direct_build",
            Self::Incremental => "incremental",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_modes_have_stable_export_names() {
        assert_eq!(SummaryMaintenanceMode::DirectBuild.as_str(), "direct_build");
        assert_eq!(SummaryMaintenanceMode::Incremental.as_str(), "incremental");
    }
}
