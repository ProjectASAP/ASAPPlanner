//! Helpers that only tests call (this crate's integration tests and other
//! crates' tests), exposed under the `test-support` feature so the normal
//! public API stays as narrow as its production users need. Each forwards
//! to the crate-private function of the same name.
use std::rc::Rc;

use crate::ir::materialized_view::RawDataWindow;
use crate::ir::operator::maintained_population::CurrentSeriesInput;
use crate::ir::operator::node::Operator;
use crate::ir::physical_export::PhysicalASAPDAGCompilation;
use crate::ir::properties::summary_coverage::SummaryCoverage;
use crate::ir::properties::timing::PhysicalPropertiesAssignment;
use crate::ir::properties::ExecutionDataStateError;
use crate::ir::schema::Schema;
use crate::ir::{ASAPOp, NonASAPOp};
use crate::ir::{OperatorNode, SchemaDerivationError};

/// `SummaryCoverage::definition_id`.
pub fn definition_id(coverage: &SummaryCoverage) -> String {
    coverage.definition_id()
}

/// `tumbling_window_chain::whole_window_build`.
pub fn whole_window_build(chain: &OperatorNode) -> Result<OperatorNode, SchemaDerivationError> {
    crate::ir::operator::tumbling_window_chain::whole_window_build(chain)
}

/// `tumbling_window_chain::tumbling_window`.
pub fn tumbling_window(
    chain: &OperatorNode,
    index: u32,
) -> Result<OperatorNode, SchemaDerivationError> {
    crate::ir::operator::tumbling_window_chain::tumbling_window(chain, index)
}

/// `physical_export::compile_physical_asap_dag_with_node_ids`.
pub fn compile_physical_asap_dag_with_node_ids(
    root: &Rc<OperatorNode>,
    physical_properties: &PhysicalPropertiesAssignment,
) -> Result<PhysicalASAPDAGCompilation, ExecutionDataStateError> {
    crate::ir::physical_export::compile_physical_asap_dag_with_node_ids(root, physical_properties)
}

/// `Operator::output_schema`.
pub fn output_schema(operator: &Operator) -> Result<Schema, SchemaDerivationError> {
    operator.output_schema()
}

/// `NonASAPOp::validate_inputs`.
pub fn validate_non_asap_inputs(op: &NonASAPOp) -> Result<(), SchemaDerivationError> {
    op.validate_inputs()
}

/// `ASAPOp::validate_inputs`.
pub fn validate_asap_inputs(op: &ASAPOp) -> Result<(), SchemaDerivationError> {
    op.validate_inputs()
}

/// `CurrentSeriesInput::matches_node`.
pub fn matches_node(input: &CurrentSeriesInput, node: &OperatorNode) -> bool {
    input.matches_node(node)
}

/// `RawDataWindow::lookback_ms`.
pub fn lookback_ms(window: &RawDataWindow) -> u64 {
    window.lookback_ms()
}
