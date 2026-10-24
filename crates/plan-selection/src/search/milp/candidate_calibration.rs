//! Calibration policy for candidate selection. Unknown calibration is never
//! fitted to a timeout-dependent subset and then treated as a fixed price model.
use crate::{CalibrationFit, PlanningModels};

#[derive(Debug, Clone, Copy)]
pub(super) enum CandidateCalibration {
    Frozen(CalibrationFit),
    CompleteCatalog,
}

impl CandidateCalibration {
    pub fn for_models(models: PlanningModels<'_>) -> Result<Self, String> {
        if let Some(factor) = models.plan_selection.frozen_calibration_factor {
            return CalibrationFit::from_factor(factor)
                .map(Self::Frozen)
                .map_err(|error| error.to_string());
        }
        if models
            .plan_selection
            .measured_operation_costs
            .operators
            .is_empty()
        {
            Ok(Self::Frozen(CalibrationFit::default()))
        } else {
            Ok(Self::CompleteCatalog)
        }
    }
}
