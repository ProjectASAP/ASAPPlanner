//! Candidate graph representation and physical implementation construction.
pub(crate) mod configurations;
mod factoring;
pub(crate) mod graph;
pub(crate) mod inventory;
pub(crate) mod physical;
pub(crate) mod reconstruction;
pub(crate) mod scopes;
pub(crate) mod suppliers;

/// Discard partially built catalogs on timeout; they cannot certify coverage.
fn check_deadline(
    deadline: Option<std::time::Instant>,
) -> Result<(), asap_logical_asap_optimization::pass2::memo::CandidateGenerationTimeout> {
    if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
        Err(asap_logical_asap_optimization::pass2::memo::CandidateGenerationTimeout)
    } else {
        Ok(())
    }
}
