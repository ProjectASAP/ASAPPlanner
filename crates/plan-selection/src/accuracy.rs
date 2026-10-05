//! Stage 3's accuracy model: whether a summary estimate meets its query's
//! accuracy target. The built-in model delegates to the analytical estimators
//! in `asap_logical_optimizer::accuracy`.

use asap_types::ir::properties::ResultGuarantee;
use asap_types::ir::schema::{FieldDataType, SketchStatistic};
use asap_types::types::AccuracyTarget;

/// The deployment-extensible accuracy model. [`DefaultAccuracyModel`] is the
/// built-in one; a deployment with its own error model for a family
/// implements this trait and passes it in
/// [`PlanningModels`](crate::PlanningModels). Stage 3 rejects an estimate
/// whose family has no model.
pub trait AccuracyModel {
    /// The guarantee of reading `query` out of a summary of family `family`
    /// built over an **exact** input. `None` when this model has no error
    /// model for the family.
    fn local_guarantee(
        &self,
        family: &FieldDataType,
        query: &SketchStatistic,
    ) -> Option<ResultGuarantee>;

    /// Compare the dimensions requested by `target`. Unknown required
    /// dimensions fail.
    fn satisfies(&self, guarantee: &ResultGuarantee, target: &AccuracyTarget) -> bool;

    /// Whether `guarantee` bounds the error a target on `statistic` is
    /// stated in, so that [`Self::satisfies`] compares like with like. An
    /// exact guarantee answers every statistic; otherwise the metric must be
    /// one the statistic's ε is measured in. A deployment with a registered
    /// cross-metric conversion overrides this.
    fn answers(&self, statistic: &SketchStatistic, guarantee: &ResultGuarantee) -> bool {
        asap_logical_optimizer::accuracy::answers(statistic, guarantee)
    }
}

/// The built-in analytical estimator models, with conservative target checks.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultAccuracyModel;

impl AccuracyModel for DefaultAccuracyModel {
    fn local_guarantee(
        &self,
        family: &FieldDataType,
        query: &SketchStatistic,
    ) -> Option<ResultGuarantee> {
        asap_logical_optimizer::accuracy::local_guarantee(family, query)
    }

    fn satisfies(&self, guarantee: &ResultGuarantee, target: &AccuracyTarget) -> bool {
        asap_logical_optimizer::accuracy::satisfies(guarantee, target)
    }

    fn answers(&self, statistic: &SketchStatistic, guarantee: &ResultGuarantee) -> bool {
        asap_logical_optimizer::accuracy::answers(statistic, guarantee)
    }
}
