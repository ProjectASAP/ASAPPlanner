//! Stage 1, Pass 2's output, which Stage 2 implements.

use super::MemoCandidates;

/// Stage 1, Pass 2's output: the Pass 1 groups plus the groups Pass 2 adds
/// (shared producers, shared summary states), each with its Pass 1
/// candidates plus every Pass 2 rule's (by `origin`), including the
/// candidates reading a shared summary state.
#[derive(Debug, Clone)]
pub struct LogicalASAPMemoCandidates {
    /// The expanded memo: groups, query roots (passed through from Pass 1)
    /// and each group's candidates.
    pub memo: MemoCandidates,
}
