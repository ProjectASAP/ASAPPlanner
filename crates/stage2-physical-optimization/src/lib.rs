//! #509 Stage 2, physical ASAP-aware optimization (concepts doc,
//! section 3, "Stage 2: physical alternatives").
//!
//! The planner facade calls [`generate_physical_asap_memo_candidates`] on
//! Pass 2's output (`asap_types::stages::LogicalASAPMemoCandidates`): each
//! logical candidate expanded into one physical candidate per valid
//! $(\tau, \ell)$, each logical edge into its physical edges with
//! $\alpha^{\log}$ and $\alpha^{\mathrm{phys}}$, and the pruned alternatives
//! (`asap_types::stages::PhysicalASAPMemoCandidates`). Stage 3 selects from
//! that output and builds the plan. The crate depends on no other stage
//! crate.

use asap_types::deployment_capabilities::DeploymentCapabilities;
use asap_types::stages::{LogicalASAPMemoCandidates, PhysicalASAPMemoCandidates};
use asap_types::workload::DataWorkload;

/// Stage 2: expand every logical candidate of `logical` that survived Pass 2
/// into its physical alternatives (decision-tree levels 3–4: timing and
/// lifetime) the data and the deployment's `capabilities` allow, and every
/// logical edge into its physical edges with $\alpha^{\log}$ and
/// $\alpha^{\mathrm{phys}}$. Every alternative rejected is recorded in the
/// output's `pruned_candidates` with its `PruneReason`; a value outside a
/// dimension's declared value set (`AcrossEvaluations` on a candidate of a
/// group with no known grid) is not offered and not recorded. Costs nothing, and
/// cannot fail.
pub fn generate_physical_asap_memo_candidates(
    logical: &LogicalASAPMemoCandidates,
    data: &DataWorkload,
    capabilities: &DeploymentCapabilities,
) -> PhysicalASAPMemoCandidates {
    let _ = (logical, data, capabilities);
    todo!("#1207 PR 5: physical expansion and physical edges")
}
