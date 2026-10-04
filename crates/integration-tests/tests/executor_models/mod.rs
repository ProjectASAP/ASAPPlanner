//! The deployment input these tests plan with: the built-in models and the
//! reference executor's capabilities, as a deployment running the plans on
//! that executor would pass them (C2).
use std::sync::LazyLock;

use asap_plan_selection::{DeploymentCapabilities, PlanningModels};

static EXECUTOR: LazyLock<DeploymentCapabilities> = LazyLock::new(asap_executor::capabilities);

pub fn executor_models() -> PlanningModels<'static> {
    PlanningModels::builtin().with_capabilities(&EXECUTOR)
}
