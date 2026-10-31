//! Calibration policy for candidate selection. Unknown calibration is never
//! fitted to a timeout-dependent subset and then treated as a fixed price model.
use crate::SelectionModels;
use asap_cost::CalibrationFit;

#[derive(Debug, Clone, Copy)]
pub(in crate::search) enum CandidateCalibration {
    Frozen(CalibrationFit),
    CompleteCatalog,
}

impl CandidateCalibration {
    pub fn for_models(models: SelectionModels<'_>) -> Result<Self, String> {
        if let Some(factor) = models.cost_inputs.frozen_calibration_factor {
            return CalibrationFit::from_factor(factor)
                .map(Self::Frozen)
                .map_err(|error| error.to_string());
        }
        if models
            .cost_inputs
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
