//! The candidate inventory: the dependency graph over immediate physical
//! candidates together with each candidate's operator template.
use std::rc::Rc;

use super::graph::{CandidateDependencyGraph, PhysicalCandidateId};
use crate::ir::OperatorNode;

/// Templates indexed by globally unique candidate ID. Each template describes
/// one immediate operator; its ordered `Operator::map_children` positions correspond one-for-one to the
/// candidate's producer requirements. Children serve as typed input contracts,
/// never as implicit selected dependencies.
pub type CandidateOperatorTemplates = Vec<Option<Rc<OperatorNode>>>;

#[derive(Debug)]
pub struct CandidateInventory {
    pub graph: CandidateDependencyGraph,
    pub templates: CandidateOperatorTemplates,
    /// Root implementation IDs, excluding each implementation's private nodes.
    pub group_candidates: Vec<Vec<PhysicalCandidateId>>,
}
