//! Helpers that only tests call (this crate's integration tests and other
//! crates' tests), exposed under the `test-support` feature so the normal
//! public API stays as narrow as its production users need. Each forwards
//! to the crate-private function of the same name.
use std::rc::Rc;
use std::time::Instant;

use crate::ir::operator::pane_chain::{PaneChainExpansion, PaneChainExpansionError};
use crate::ir::physical_export::{PhysicalASAPCompilationError, PhysicalASAPDAGCompilation};
use crate::ir::properties::summary_coverage::SummaryCoverage;
use crate::ir::properties::ExecutionDataStateError;
use crate::ir::{OperatorNode, QueryRoot, SchemaDerivationError};

/// `SummaryCoverage::definition_id`.
pub fn definition_id(coverage: &SummaryCoverage) -> String {
    coverage.definition_id()
}

/// `pane_chain::whole_window_build`.
pub fn whole_window_build(chain: &OperatorNode) -> Result<OperatorNode, SchemaDerivationError> {
    crate::ir::operator::pane_chain::whole_window_build(chain)
}

/// `pane_chain::pane`.
pub fn pane(chain: &OperatorNode, index: u32) -> Result<OperatorNode, SchemaDerivationError> {
    crate::ir::operator::pane_chain::pane(chain, index)
}

/// `pane_chain::expand_pane_chains_in_workload_before`.
pub fn expand_pane_chains_in_workload_before(
    roots: &[QueryRoot],
    deadline: Option<Instant>,
) -> Result<PaneChainExpansion, PaneChainExpansionError> {
    crate::ir::operator::pane_chain::expand_pane_chains_in_workload_before(roots, deadline)
}

/// `physical_export::compile_physical_asap_dag_with_node_ids`.
pub fn compile_physical_asap_dag_with_node_ids(
    root: &Rc<OperatorNode>,
) -> Result<PhysicalASAPDAGCompilation, ExecutionDataStateError> {
    crate::ir::physical_export::compile_physical_asap_dag_with_node_ids(root)
}

/// `physical_export::compile_physical_asap_workload_with_node_ids_before`.
pub fn compile_physical_asap_workload_with_node_ids_before(
    roots: &[QueryRoot],
    deadline: Option<Instant>,
) -> Result<PhysicalASAPDAGCompilation, PhysicalASAPCompilationError> {
    crate::ir::physical_export::compile_physical_asap_workload_with_node_ids_before(roots, deadline)
}
