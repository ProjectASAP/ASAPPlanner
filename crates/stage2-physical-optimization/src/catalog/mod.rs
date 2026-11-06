//! Stage 2's physical candidates: expand each logical candidate into one
//! physical candidate per valid $(\tau, \ell, \sigma)$, and each logical edge
//! into the physical edges between the two candidates' alternatives
//! ([`generate_physical_asap_memo_candidates`]); and realize one selected
//! DAG as a physical plan (#1047). The output type,
//! `PhysicalASAPMemoCandidates`, lives in `asap_types::stages`; Stage 3
//! selects from it.
mod realize;

pub use realize::Stage2AssignmentRealizer;

use asap_types::stages::{LogicalASAPMemoCandidates, PhysicalASAPMemoCandidates};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

/// Why [`generate_physical_asap_memo_candidates`] failed.
#[derive(Debug, thiserror::Error)]
pub enum ErrorGeneratingPhysicalASAPMemoCandidates {
    /// Structural pruning found the candidates malformed.
    #[error("{0}")]
    Pruning(String),
}

/// Stage 2: expand every logical candidate of `logical` into its physical
/// alternatives (decision-tree levels 4–6: timing, lifetime, schedule) the
/// data and deployment allow (`query_time_results_retained`; no query-time
/// raw scans unless `raw_data_retained`), and every logical edge into its
/// physical edges with $\alpha^{\log}$ and $\alpha^{\mathrm{phys}}$. Every
/// alternative not generated is recorded in the output's
/// `pruned_candidates` with its `PruneReason`. Prices nothing.
pub fn generate_physical_asap_memo_candidates(
    logical: &LogicalASAPMemoCandidates,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    query_time_results_retained: bool,
    raw_data_retained: bool,
) -> Result<PhysicalASAPMemoCandidates, ErrorGeneratingPhysicalASAPMemoCandidates> {
    let _ = (
        logical,
        entries,
        data,
        query_time_results_retained,
        raw_data_retained,
    );
    todo!("#1207 PR 5: physical expansion and physical edges")
}
