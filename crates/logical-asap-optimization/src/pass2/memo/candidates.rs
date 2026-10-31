//! Logical implementation candidates attached to stable memo groups.
//!
//! Rule expansion owns no pricing or solver state. Shared computations are
//! explicit state groups connected through candidate child dependencies.

/// A Pass 1 inventory does not belong to the initial logical memo.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CandidatePopulationError {
    #[error("Pass 1 root dependencies differ from the initial logical memo")]
    RootDependenciesChanged,
    #[error("Pass 1 target {target} is not a valid measure of an initial aggregate group")]
    InvalidTarget { target: usize },
    #[error("Pass 1 target {target} references missing absorbed target {absorbed}")]
    InvalidAbsorption { target: usize, absorbed: usize },
}
