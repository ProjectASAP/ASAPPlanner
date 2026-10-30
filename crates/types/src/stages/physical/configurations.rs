//! Finite, unpriced execution configurations over typed immediate candidates:
//! each candidate's timing and materialization.
use super::graph::PhysicalCandidateId;
use super::inventory::CandidateInventory;
use crate::ir::properties::ExecutionTiming;
use crate::physical::Materialization;

#[derive(Debug, Clone)]
pub struct CandidateConfiguration {
    pub source_candidate: PhysicalCandidateId,
    pub materialization: Materialization,
    pub timing: ExecutionTiming,
    /// One entry per workload query, in the order of the planning context's
    /// `entries`: whether Stage 2 lets this pane chain be kept across
    /// evaluations when that query is its only reader. Filled only for a
    /// `MatAtQueryKeptAcrossEvaluations(Memory)` pane chain in a deployment
    /// that keeps query-time results (see `may_keep_with_sole_reader` in
    /// `asap-physical-asap-optimization`);
    /// empty otherwise. Stage 3 pricing reads it instead of asking Stage 2.
    pub may_keep_with_sole_reader: Vec<bool>,
}

#[derive(Debug)]
pub struct CandidateConfigurationInventory {
    /// Costs are zero placeholders. Price/configure resource models before
    /// treating a selected solution as a deployment-cost optimization result.
    pub inventory: CandidateInventory,
    pub configurations: Vec<CandidateConfiguration>,
}
