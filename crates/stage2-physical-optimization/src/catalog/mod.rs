//! Stage 2's physical candidates: expand each logical candidate into one
//! physical candidate per valid $(\tau, \ell)$, and each logical edge
//! into the physical edges between the two candidates' alternatives
//! ([`generate_physical_asap_memo_candidates`]). The output type,
//! `PhysicalASAPMemoCandidates`, lives in `asap_types::stages`; Stage 3
//! selects from it.

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
